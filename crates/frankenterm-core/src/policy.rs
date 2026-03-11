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
use crate::connector_credential_broker::{
    ConnectorCredentialBroker, CredentialBrokerConfig, CredentialScope, CredentialSensitivity,
};
use crate::identity_graph::{AuthAction, PrincipalId, PrincipalKind, ResourceId, ResourceKind};
use crate::policy_audit_chain::{AuditChain, AuditEntryKind};
use crate::policy_compliance::ComplianceEngine;
use crate::policy_decision_log::{DecisionLogConfig, DecisionOutcome, PolicyDecisionLog};
use crate::policy_metrics::{
    PolicyMetricsCollector, PolicyMetricsDashboard, PolicyMetricsThresholds, PolicySubsystemInput,
};
use crate::policy_quarantine::QuarantineRegistry;
use crate::trauma_guard::TraumaDecision;
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
    /// Dispatch a connector notification (Slack/email/webhook)
    ConnectorNotify,
    /// Dispatch a connector ticket action (Jira/Linear/GitHub issue)
    ConnectorTicket,
    /// Trigger an external workflow via a connector
    ConnectorTriggerWorkflow,
    /// Emit an external audit/log action via a connector
    ConnectorAuditLog,
    /// Invoke a generic connector RPC/action
    ConnectorInvoke,
    /// Rotate or revoke credentials via a connector
    ConnectorCredentialAction,
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
            Self::Close
                | Self::DeleteFile
                | Self::SendCtrlC
                | Self::SendCtrlD
                | Self::ConnectorCredentialAction
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
                | Self::ConnectorNotify
                | Self::ConnectorTicket
                | Self::ConnectorTriggerWorkflow
                | Self::ConnectorAuditLog
                | Self::ConnectorInvoke
                | Self::ConnectorCredentialAction
        )
    }

    /// Returns true if this is a connector-scoped action.
    #[must_use]
    pub const fn is_connector_action(&self) -> bool {
        matches!(
            self,
            Self::ConnectorNotify
                | Self::ConnectorTicket
                | Self::ConnectorTriggerWorkflow
                | Self::ConnectorAuditLog
                | Self::ConnectorInvoke
                | Self::ConnectorCredentialAction
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
            Self::ConnectorNotify => "connector_notify",
            Self::ConnectorTicket => "connector_ticket",
            Self::ConnectorTriggerWorkflow => "connector_trigger_workflow",
            Self::ConnectorAuditLog => "connector_audit_log",
            Self::ConnectorInvoke => "connector_invoke",
            Self::ConnectorCredentialAction => "connector_credential_action",
        }
    }

    /// Map a policy action onto the coarse least-privilege action used by the
    /// identity graph.
    #[must_use]
    pub fn auth_action(&self) -> AuthAction {
        match self {
            Self::ReadOutput | Self::SearchOutput => AuthAction::Read,
            Self::Spawn | Self::Split => AuthAction::Create,
            Self::Close | Self::DeleteFile => AuthAction::Delete,
            Self::Activate
            | Self::BrowserAuth
            | Self::WorkflowRun
            | Self::ExecCommand
            | Self::ConnectorTriggerWorkflow => AuthAction::Execute,
            Self::ConnectorCredentialAction => AuthAction::Admin,
            Self::SendText
            | Self::SendCtrlC
            | Self::SendCtrlD
            | Self::SendCtrlZ
            | Self::SendControl
            | Self::ReservePane
            | Self::ReleasePane
            | Self::WriteFile
            | Self::ConnectorNotify
            | Self::ConnectorTicket
            | Self::ConnectorAuditLog
            | Self::ConnectorInvoke => AuthAction::Write,
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

    /// Map policy actors to the corresponding identity-graph principal kind.
    #[must_use]
    pub const fn principal_kind(&self) -> PrincipalKind {
        match self {
            Self::Human => PrincipalKind::Human,
            Self::Robot => PrincipalKind::Agent,
            Self::Mcp => PrincipalKind::Mcp,
            Self::Workflow => PrincipalKind::Workflow,
        }
    }
}

// ============================================================================
// Policy Surface
// ============================================================================

/// High-level subsystem surface where a policy action originates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySurface {
    /// Surface could not be determined.
    #[default]
    Unknown,
    /// Native mux/session control plane.
    Mux,
    /// Swarm orchestration runtime.
    Swarm,
    /// Robot mode command/API surface.
    Robot,
    /// Connector fabric execution surface.
    Connector,
    /// Workflow automation runtime.
    Workflow,
    /// MCP tool-serving surface.
    Mcp,
    /// Local IPC surface.
    Ipc,
}

impl PolicySurface {
    /// Returns a stable string identifier for this policy surface.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Mux => "mux",
            Self::Swarm => "swarm",
            Self::Robot => "robot",
            Self::Connector => "connector",
            Self::Workflow => "workflow",
            Self::Mcp => "mcp",
            Self::Ipc => "ipc",
        }
    }

    /// Best-effort default surface for an actor when no explicit surface is provided.
    ///
    /// Human actions intentionally remain `Unknown` until call-sites annotate the
    /// concrete subsystem (mux/swarm/ipc/etc.) to avoid over-asserting context.
    #[must_use]
    pub const fn default_for_actor(actor: ActorKind) -> Self {
        match actor {
            ActorKind::Human => Self::Unknown,
            ActorKind::Robot => Self::Robot,
            ActorKind::Mcp => Self::Mcp,
            ActorKind::Workflow => Self::Workflow,
        }
    }
}

fn scoped_identity_id(domain: Option<&str>, local_id: impl std::fmt::Display) -> String {
    match domain {
        Some(domain) if !domain.is_empty() => format!("{domain}:{local_id}"),
        _ => local_id.to_string(),
    }
}

fn derive_identity_principal(
    actor: ActorKind,
    workflow_id: Option<&str>,
    actor_id: Option<&str>,
) -> PrincipalId {
    let principal_id = actor_id
        .map(str::to_owned)
        .or_else(|| {
            if actor == ActorKind::Workflow {
                workflow_id.map(str::to_owned)
            } else {
                None
            }
        })
        .unwrap_or_else(|| actor.as_str().to_string());

    match actor {
        ActorKind::Human => PrincipalId::human(principal_id),
        ActorKind::Robot => PrincipalId::agent(principal_id),
        ActorKind::Mcp => PrincipalId::new(PrincipalKind::Mcp, principal_id),
        ActorKind::Workflow => PrincipalId::workflow(principal_id),
    }
}

fn derive_identity_resource(
    action: ActionKind,
    pane_id: Option<u64>,
    domain: Option<&str>,
    workflow_id: Option<&str>,
) -> ResourceId {
    if let Some(pane_id) = pane_id {
        return ResourceId::pane(scoped_identity_id(domain, pane_id));
    }

    if action == ActionKind::WorkflowRun {
        return ResourceId::new(
            ResourceKind::Workflow,
            workflow_id.unwrap_or("*").to_string(),
        );
    }

    match action {
        ActionKind::WriteFile | ActionKind::DeleteFile => {
            ResourceId::new(ResourceKind::File, scoped_identity_id(domain, "*"))
        }
        ActionKind::ConnectorCredentialAction => {
            ResourceId::credential(scoped_identity_id(domain, "*"))
        }
        ActionKind::BrowserAuth | ActionKind::ExecCommand => {
            ResourceId::new(ResourceKind::Capability, action.as_str())
        }
        ActionKind::ConnectorNotify
        | ActionKind::ConnectorTicket
        | ActionKind::ConnectorTriggerWorkflow
        | ActionKind::ConnectorAuditLog
        | ActionKind::ConnectorInvoke => ResourceId::new(
            ResourceKind::Capability,
            scoped_identity_id(domain, "connector"),
        ),
        _ => domain
            .map(|domain| ResourceId::session(domain.to_string()))
            .unwrap_or_else(ResourceId::fleet),
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
    /// High-level subsystem surface for this decision
    #[serde(default)]
    pub surface: PolicySurface,
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
            surface: PolicySurface::Unknown,
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

    /// Derive the requesting principal identity for least-privilege checks.
    #[must_use]
    pub fn identity_principal(&self, actor_id: Option<&str>) -> PrincipalId {
        derive_identity_principal(self.actor, self.workflow_id.as_deref(), actor_id)
    }

    /// Derive the target resource identity for least-privilege checks.
    #[must_use]
    pub fn identity_resource(&self) -> ResourceId {
        derive_identity_resource(
            self.action,
            self.pane_id,
            self.domain.as_deref(),
            self.workflow_id.as_deref(),
        )
    }

    /// Derive the coarse authorization action used by the identity graph.
    #[must_use]
    pub fn identity_action(&self) -> AuthAction {
        self.action.auth_action()
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
    /// Scope string (`per_pane:<id>` or `global`)
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

// ── Approval Workflow Tracker ──────────────────────────────────────

/// Status of a pending approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    /// Awaiting operator decision.
    Pending,
    /// Approved by an operator.
    Approved,
    /// Rejected by an operator.
    Rejected,
    /// Expired without a decision.
    Expired,
    /// Revoked after being approved.
    Revoked,
}

impl ApprovalStatus {
    /// Returns a stable string tag.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }

    /// Returns true if the approval grants access.
    #[must_use]
    pub const fn grants_access(&self) -> bool {
        matches!(self, Self::Approved)
    }
}

/// A tracked approval entry in the workflow queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalEntry {
    /// Unique approval ID.
    pub approval_id: String,
    /// The action kind that triggered the approval request.
    pub action: String,
    /// Actor who requested the action.
    pub actor: String,
    /// Domain/connector/resource involved.
    pub resource: String,
    /// Human-readable reason the approval is required.
    pub reason: String,
    /// Rule that triggered the approval requirement.
    pub rule_id: String,
    /// Timestamp when the approval was requested (epoch ms).
    pub requested_at_ms: u64,
    /// Expiration timestamp (epoch ms). 0 = no expiry.
    pub expires_at_ms: u64,
    /// Current status.
    pub status: ApprovalStatus,
    /// Who approved/rejected (empty if still pending).
    pub decided_by: String,
    /// Timestamp of the decision (0 if pending).
    pub decided_at_ms: u64,
}

/// Bounded approval workflow tracker.
///
/// Maintains a queue of pending, approved, rejected, and revoked approvals.
/// Integrates with the audit chain for governance traceability.
#[derive(Debug, Clone)]
pub struct ApprovalTracker {
    entries: Vec<ApprovalEntry>,
    max_entries: usize,
    next_id: u64,
}

impl Default for ApprovalTracker {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            max_entries: 4096,
            next_id: 1,
        }
    }
}

impl ApprovalTracker {
    /// Create a new tracker with a given capacity.
    #[must_use]
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: Vec::new(),
            max_entries,
            next_id: 1,
        }
    }

    /// Submit a new approval request. Returns the generated approval ID.
    pub fn submit(
        &mut self,
        action: &str,
        actor: &str,
        resource: &str,
        reason: &str,
        rule_id: &str,
        requested_at_ms: u64,
        expires_at_ms: u64,
    ) -> String {
        let approval_id = format!("appr-{}", self.next_id);
        self.next_id += 1;
        let entry = ApprovalEntry {
            approval_id: approval_id.clone(),
            action: action.to_string(),
            actor: actor.to_string(),
            resource: resource.to_string(),
            reason: reason.to_string(),
            rule_id: rule_id.to_string(),
            requested_at_ms,
            expires_at_ms,
            status: ApprovalStatus::Pending,
            decided_by: String::new(),
            decided_at_ms: 0,
        };
        self.entries.push(entry);
        // Evict oldest if over capacity
        if self.entries.len() > self.max_entries {
            self.entries.remove(0);
        }
        approval_id
    }

    /// Approve a pending request. Returns true if the approval was found and updated.
    pub fn approve(&mut self, approval_id: &str, decided_by: &str, now_ms: u64) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.approval_id == approval_id) {
            if entry.status == ApprovalStatus::Pending {
                entry.status = ApprovalStatus::Approved;
                entry.decided_by = decided_by.to_string();
                entry.decided_at_ms = now_ms;
                return true;
            }
        }
        false
    }

    /// Reject a pending request. Returns true if found and updated.
    pub fn reject(&mut self, approval_id: &str, decided_by: &str, now_ms: u64) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.approval_id == approval_id) {
            if entry.status == ApprovalStatus::Pending {
                entry.status = ApprovalStatus::Rejected;
                entry.decided_by = decided_by.to_string();
                entry.decided_at_ms = now_ms;
                return true;
            }
        }
        false
    }

    /// Revoke a previously approved request. Returns true if found and revoked.
    pub fn revoke(&mut self, approval_id: &str, decided_by: &str, now_ms: u64) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.approval_id == approval_id) {
            if entry.status == ApprovalStatus::Approved {
                entry.status = ApprovalStatus::Revoked;
                entry.decided_by = decided_by.to_string();
                entry.decided_at_ms = now_ms;
                return true;
            }
        }
        false
    }

    /// Expire all pending approvals past their deadline.
    /// Returns the count of newly expired entries.
    pub fn expire_stale(&mut self, now_ms: u64) -> usize {
        let mut count = 0;
        for entry in &mut self.entries {
            if entry.status == ApprovalStatus::Pending
                && entry.expires_at_ms > 0
                && now_ms >= entry.expires_at_ms
            {
                entry.status = ApprovalStatus::Expired;
                count += 1;
            }
        }
        count
    }

    /// Look up an approval by ID.
    #[must_use]
    pub fn get(&self, approval_id: &str) -> Option<&ApprovalEntry> {
        self.entries.iter().find(|e| e.approval_id == approval_id)
    }

    /// All pending approvals.
    #[must_use]
    pub fn pending(&self) -> Vec<&ApprovalEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == ApprovalStatus::Pending)
            .collect()
    }

    /// Total number of tracked approvals (all statuses).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the tracker has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Count entries by status.
    #[must_use]
    pub fn count_by_status(&self, status: &ApprovalStatus) -> usize {
        self.entries.iter().filter(|e| &e.status == status).count()
    }

    /// Entries within a time range.
    #[must_use]
    pub fn by_time_range(&self, start_ms: u64, end_ms: u64) -> Vec<&ApprovalEntry> {
        self.entries
            .iter()
            .filter(|e| e.requested_at_ms >= start_ms && e.requested_at_ms <= end_ms)
            .collect()
    }

    /// Diagnostic snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ApprovalTrackerSnapshot {
        ApprovalTrackerSnapshot {
            total: self.entries.len(),
            pending: self.count_by_status(&ApprovalStatus::Pending),
            approved: self.count_by_status(&ApprovalStatus::Approved),
            rejected: self.count_by_status(&ApprovalStatus::Rejected),
            expired: self.count_by_status(&ApprovalStatus::Expired),
            revoked: self.count_by_status(&ApprovalStatus::Revoked),
            max_entries: self.max_entries,
        }
    }
}

/// Snapshot of the approval tracker state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalTrackerSnapshot {
    pub total: usize,
    pub pending: usize,
    pub approved: usize,
    pub rejected: usize,
    pub expired: usize,
    pub revoked: usize,
    pub max_entries: usize,
}

// ── Revocation Engine ──────────────────────────────────────────────

/// A revocation record for a credential, session, or connector access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationRecord {
    /// Unique revocation ID.
    pub revocation_id: String,
    /// Type of resource revoked (e.g., "credential", "session", "connector").
    pub resource_type: String,
    /// Identifier of the revoked resource.
    pub resource_id: String,
    /// Reason for revocation.
    pub reason: String,
    /// Who initiated the revocation.
    pub revoked_by: String,
    /// Timestamp of revocation (epoch ms).
    pub revoked_at_ms: u64,
    /// Whether the revocation is currently active (can be un-revoked).
    pub active: bool,
}

/// Bounded revocation registry.
///
/// Tracks revoked credentials, sessions, and connector access grants.
/// Active revocations are checked during authorization to deny access.
#[derive(Debug, Clone)]
pub struct RevocationRegistry {
    records: Vec<RevocationRecord>,
    max_records: usize,
    next_id: u64,
}

impl Default for RevocationRegistry {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            max_records: 4096,
            next_id: 1,
        }
    }
}

impl RevocationRegistry {
    /// Create with a given capacity.
    #[must_use]
    pub fn new(max_records: usize) -> Self {
        Self {
            records: Vec::new(),
            max_records,
            next_id: 1,
        }
    }

    /// Revoke a resource. Returns the revocation ID.
    pub fn revoke(
        &mut self,
        resource_type: &str,
        resource_id: &str,
        reason: &str,
        revoked_by: &str,
        now_ms: u64,
    ) -> String {
        let revocation_id = format!("rev-{}", self.next_id);
        self.next_id += 1;
        self.records.push(RevocationRecord {
            revocation_id: revocation_id.clone(),
            resource_type: resource_type.to_string(),
            resource_id: resource_id.to_string(),
            reason: reason.to_string(),
            revoked_by: revoked_by.to_string(),
            revoked_at_ms: now_ms,
            active: true,
        });
        if self.records.len() > self.max_records {
            self.records.remove(0);
        }
        revocation_id
    }

    /// Un-revoke (reinstate) a resource. Returns true if found and reinstated.
    pub fn reinstate(&mut self, revocation_id: &str) -> bool {
        if let Some(record) = self
            .records
            .iter_mut()
            .find(|r| r.revocation_id == revocation_id && r.active)
        {
            record.active = false;
            return true;
        }
        false
    }

    /// Check if a resource is currently revoked.
    #[must_use]
    pub fn is_revoked(&self, resource_type: &str, resource_id: &str) -> bool {
        self.records
            .iter()
            .any(|r| r.active && r.resource_type == resource_type && r.resource_id == resource_id)
    }

    /// Get the active revocation for a resource, if any.
    #[must_use]
    pub fn active_revocation(
        &self,
        resource_type: &str,
        resource_id: &str,
    ) -> Option<&RevocationRecord> {
        self.records
            .iter()
            .find(|r| r.active && r.resource_type == resource_type && r.resource_id == resource_id)
    }

    /// All active revocations.
    #[must_use]
    pub fn active_revocations(&self) -> Vec<&RevocationRecord> {
        self.records.iter().filter(|r| r.active).collect()
    }

    /// Total records (active + inactive).
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns true if empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Count of currently active revocations.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.records.iter().filter(|r| r.active).count()
    }

    /// Diagnostic snapshot.
    #[must_use]
    pub fn snapshot(&self) -> RevocationRegistrySnapshot {
        RevocationRegistrySnapshot {
            total_records: self.records.len(),
            active_revocations: self.active_count(),
            max_records: self.max_records,
        }
    }
}

/// Snapshot of the revocation registry state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationRegistrySnapshot {
    pub total_records: usize,
    pub active_revocations: usize,
    pub max_records: usize,
}

// ── Forensic Report ────────────────────────────────────────────────

/// A unified forensic report aggregating evidence from all governance
/// subsystems.  Designed for compliance export, incident review, and
/// action chain reconstruction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForensicReport {
    /// Report generation timestamp (epoch ms).
    pub generated_at_ms: u64,
    /// Time range covered (start, end) in epoch ms.
    pub time_range: (u64, u64),
    /// Decision log entries matching the query.
    pub decisions: Vec<crate::policy_decision_log::PolicyDecisionEntry>,
    /// Audit chain entries (subset by time range).
    pub audit_trail: Vec<ForensicAuditEntry>,
    /// Active and recent revocations.
    pub revocations: Vec<RevocationRecord>,
    /// Approval workflow entries.
    pub approvals: Vec<ApprovalEntry>,
    /// Namespace boundary violations.
    pub namespace_violations: Vec<ForensicNamespaceEvent>,
    /// Compliance summary.
    pub compliance_summary: ForensicComplianceSummary,
    /// Quarantine events in the time range.
    pub quarantine_active: Vec<String>,
    /// Kill switch status at report time.
    pub kill_switch_active: bool,
}

/// A serializable audit chain entry for forensic reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForensicAuditEntry {
    pub timestamp_ms: u64,
    pub kind: String,
    pub actor: String,
    pub description: String,
    pub surface: String,
    pub hash: String,
}

/// A namespace boundary crossing event for forensic reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForensicNamespaceEvent {
    pub timestamp_ms: u64,
    pub source_namespace: String,
    pub target_namespace: String,
    pub resource_kind: String,
    pub resource_id: String,
    pub decision: String,
    pub reason: String,
}

/// Compliance summary for forensic reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForensicComplianceSummary {
    pub total_evaluations: u64,
    pub total_denials: u64,
    pub denial_rate_percent: f64,
}

/// Query parameters for forensic report generation.
#[derive(Debug, Clone, Default)]
pub struct ForensicQuery {
    /// Start of time range (epoch ms). 0 = beginning.
    pub start_ms: u64,
    /// End of time range (epoch ms). 0 = now.
    pub end_ms: u64,
    /// Filter by actor kind.
    pub actor: Option<ActorKind>,
    /// Filter by action kind.
    pub action: Option<ActionKind>,
    /// Filter by pane ID.
    pub pane_id: Option<u64>,
    /// Filter by connector domain.
    pub domain: Option<String>,
    /// Include only denied decisions.
    pub denials_only: bool,
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
    /// High-level subsystem surface where this request originates
    #[serde(default)]
    pub surface: PolicySurface,
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
    /// Optional Trauma Guard decision context for command-loop protection.
    #[serde(skip)]
    pub trauma_decision: Option<TraumaDecision>,
    /// Pane title for rule matching (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_title: Option<String>,
    /// Pane working directory for rule matching (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_cwd: Option<String>,
    /// Inferred agent type for rule matching (e.g., "claude", "cursor", "shell")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,

    /// Actor's namespace for multi-tenant isolation checks.
    /// When set, the policy engine checks whether the actor's namespace
    /// has cross-tenant access to the target resource's namespace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_namespace: Option<crate::namespace_isolation::TenantNamespace>,
}

impl PolicyInput {
    /// Create a new policy input
    #[must_use]
    pub fn new(action: ActionKind, actor: ActorKind) -> Self {
        Self {
            action,
            actor,
            surface: PolicySurface::default_for_actor(actor),
            pane_id: None,
            domain: None,
            capabilities: PaneCapabilities::default(),
            text_summary: None,
            workflow_id: None,
            command_text: None,
            trauma_decision: None,
            pane_title: None,
            pane_cwd: None,
            agent_type: None,
            actor_namespace: None,
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

    /// Set request surface
    #[must_use]
    pub fn with_surface(mut self, surface: PolicySurface) -> Self {
        self.surface = surface;
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

    /// Attach Trauma Guard decision metadata for this command evaluation.
    #[must_use]
    pub fn with_trauma_decision(mut self, decision: TraumaDecision) -> Self {
        self.trauma_decision = Some(decision);
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

    /// Set the actor's namespace for multi-tenant isolation checks.
    #[must_use]
    pub fn with_namespace(mut self, ns: crate::namespace_isolation::TenantNamespace) -> Self {
        self.actor_namespace = Some(ns);
        self
    }

    /// Derive the requesting principal identity for least-privilege checks.
    #[must_use]
    pub fn identity_principal(&self, actor_id: Option<&str>) -> PrincipalId {
        derive_identity_principal(self.actor, self.workflow_id.as_deref(), actor_id)
    }

    /// Derive the target resource identity for least-privilege checks.
    #[must_use]
    pub fn identity_resource(&self) -> ResourceId {
        derive_identity_resource(
            self.action,
            self.pane_id,
            self.domain.as_deref(),
            self.workflow_id.as_deref(),
        )
    }

    /// Derive the coarse authorization action used by the identity graph.
    #[must_use]
    pub fn identity_action(&self) -> AuthAction {
        self.action.auth_action()
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
            surface: input.surface,
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
        ctx.add_evidence("surface", input.surface.as_str());
        let identity_principal = input.identity_principal(None);
        let identity_resource = input.identity_resource();
        let identity_action = input.identity_action();
        ctx.add_evidence("identity_principal", identity_principal.stable_key());
        ctx.add_evidence("identity_resource", identity_resource.stable_key());
        ctx.add_evidence("identity_action", identity_action.as_str().to_string());
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

        if let Some(decision) = input.trauma_decision.as_ref() {
            ctx.add_evidence("trauma_decision_present", "true");
            ctx.add_evidence(
                "trauma_should_intervene",
                decision.should_intervene.to_string(),
            );
            if let Some(reason_code) = decision.reason_code.as_deref() {
                ctx.add_evidence("trauma_reason_code", reason_code.to_string());
            }
            ctx.add_evidence("trauma_repeat_count", decision.repeat_count.to_string());
        } else {
            ctx.add_evidence("trauma_decision_present", "false");
        }

        ctx
    }

    /// Build the standard audit-trace context shape for non-policy-originated
    /// audit records that still need structured policy metadata.
    #[must_use]
    pub fn new_audit(
        timestamp_ms: i64,
        action: ActionKind,
        actor: ActorKind,
        surface: PolicySurface,
        pane_id: Option<u64>,
        domain: Option<String>,
        text_summary: Option<String>,
        workflow_id: Option<String>,
    ) -> Self {
        Self {
            timestamp_ms,
            action,
            actor,
            surface,
            pane_id,
            domain,
            capabilities: PaneCapabilities::default(),
            text_summary,
            workflow_id,
            rules_evaluated: Vec::new(),
            determining_rule: None,
            evidence: Vec::new(),
            rate_limit: None,
            risk: None,
        }
    }
}

/// Parse a serialized decision context emitted in audit records.
#[must_use]
pub fn parse_serialized_decision_context(serialized: Option<&str>) -> Option<DecisionContext> {
    serde_json::from_str(serialized?).ok()
}

/// Extract the best-known policy surface from serialized decision context.
///
/// This prefers typed [`DecisionContext`] payloads but tolerates older audit
/// records that only stored a raw JSON object with a `"surface"` field.
#[must_use]
pub fn parse_serialized_decision_surface(serialized: Option<&str>) -> Option<PolicySurface> {
    parse_serialized_decision_context(serialized)
        .map(|context| context.surface)
        .or_else(|| {
            let surface = serde_json::from_str::<serde_json::Value>(serialized?)
                .ok()?
                .get("surface")
                .cloned()?;
            serde_json::from_value(surface).ok()
        })
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

static VAR_ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^[a-zA-Z_][a-zA-Z0-9_]*=(?:'[^']*'|"[^"]*"|\$\([^)]*\)|`[^`]*`|\\.|[^\s;&|<>])*(\s+|$)"#,
    )
    .expect("var assign regex")
});

const COMMAND_TOKENS: &[&str] = &[
    "git",
    "rm",
    "sudo",
    "doas",
    "su",
    "env",
    "time",
    "nohup",
    "xargs",
    "watch",
    "timeout",
    "exec",
    "command",
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
    "time",
    "nohup",
    "nice",
    "timeout",
    "doas",
    "su",
    "stdbuf",
    "taskset",
    "ionice",
    "strace",
    "valgrind",
    "watch",
    "command",
    "builtin",
    "awk",
    "sed",
    // Network/Download
    "curl",
    "wget",
    "nc",
    "ncat",
    "netcat",
    "socat",
    "telnet",
    "ftp",
    "sftp",
    "scp",
    "rsync",
    // Destructive / System utilities
    "mkfs",
    "shred",
    "wipe",
    "scrub",
    "format",
    "mount",
    "umount",
    // Additional cloud and orchestration tools
    "terraform",
    "pulumi",
    "helm",
    "gcloud",
    "az",
    "kustomize",
    "podman",
    "docker-compose",
    // Additional database tools
    "mongosh",
    "redis-cli",
    // Database destructive commands
    "drop",
    "truncate",
    "delete",
    "alter",
    // System tools
    "kill",
    "killall",
    "pkill",
    "reboot",
    "shutdown",
    "halt",
    "poweroff",
    "init",
    // Command prefixes
    "unbuffer",
    "ltrace",
    // Package managers
    "pip",
    "pip3",
    "gem",
    "brew",
    "apt",
    "apt-get",
    "dpkg",
    "yum",
    "dnf",
    "apk",
    "snap",
    "pacman",
    "zypper",
    // Common file utils
    "tar",
    "unzip",
    "gzip",
    "zip",
    "jq",
    "yq",
    "mke2fs",
];

const TRAUMA_BYPASS_ENV: &str = "FT_BYPASS_TRAUMA";
const TRAUMA_LOOP_BLOCK_RULE_ID: &str = "policy.trauma_guard.loop_block";
const TRAUMA_FEEDBACK_PREFIX: &str = "[ft::TraumaGuard]";
const RCH_HEAVY_COMPUTE_RULE_ID: &str = "policy.rch.heavy_compute";

/// Determine whether the text looks like a shell command
#[must_use]
pub fn is_command_candidate(text: &str) -> bool {
    for line in text.lines() {
        let mut trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
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

        // Check for special shell characters before stripping variable assignments
        // because variable assignments might contain subshells (e.g. FOO=$(rm -rf /))
        // which should be treated as command candidates.
        if trimmed.contains("&&")
            || trimmed.contains("||")
            || trimmed.contains('|')
            || trimmed.contains('>')
            || trimmed.contains(';')
            || trimmed.contains("$(")
            || trimmed.contains('`')
        {
            return true;
        }

        // Strip leading variable assignments (e.g. FOO=bar, FOO="a b")
        while let Some(mat) = VAR_ASSIGN.find(trimmed) {
            trimmed = &trimmed[mat.end()..];
        }

        if trimmed.is_empty() {
            continue;
        }

        // Helper to check if a token matches any known command, handling paths
        let is_match = |token: &str| {
            let clean_token = token.replace(['"', '\''], "");
            let lower = clean_token.to_ascii_lowercase();

            // Always treat path-like tokens as candidates (e.g. ./script.sh, /bin/destroy)
            // We defer to DCG to determine if the script/binary is actually dangerous.
            if lower.contains('/') || lower.contains('\\') {
                return true;
            }

            // For bare commands, check the known token list
            if COMMAND_TOKENS.contains(&lower.as_str()) {
                return true;
            }

            // Prefix match for things like mkfs.ext4, docker-compose, etc.
            COMMAND_TOKENS.iter().any(|&cmd| {
                if let Some(rest) = lower.strip_prefix(cmd) {
                    // If it starts with the command, the next character must be non-alphanumeric
                    // (e.g., . in mkfs.ext4, - in docker-compose) to prevent matching "good" with "go"
                    rest.starts_with(|c: char| !c.is_alphanumeric())
                } else {
                    false
                }
            })
        };

        let mut parts = trimmed.split_whitespace();
        let token = parts.next().unwrap_or("");

        if is_match(token) {
            return true;
        }
    }

    false
}

#[must_use]
fn has_trauma_bypass_prefix(text: &str) -> bool {
    for line in text.lines() {
        let mut trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(stripped) = trimmed.strip_prefix('$') {
            trimmed = stripped.trim_start();
        }

        while let Some(mat) = VAR_ASSIGN.find(trimmed) {
            let assignment = &trimmed[..mat.end()];
            if assignment.contains("FT_BYPASS_TRAUMA=1")
                || assignment.contains("FT_BYPASS_TRAUMA=\"1\"")
                || assignment.contains("FT_BYPASS_TRAUMA='1'")
            {
                return true;
            }
            trimmed = &trimmed[mat.end()..];
        }
    }

    false
}

#[must_use]
fn trauma_feedback_comment(decision: &PolicyDecision) -> Option<String> {
    if decision.rule_id() != Some(TRAUMA_LOOP_BLOCK_RULE_ID) {
        return None;
    }

    let reason = decision
        .reason()
        .unwrap_or("recurring failure loop detected");
    let normalized_reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");

    Some(format!(
        "# {TRAUMA_FEEDBACK_PREFIX} EXECUTION BLOCKED: {normalized_reason}. Next: fix-forward (edit code/tests) or prefix with {TRAUMA_BYPASS_ENV}=1 once."
    ))
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

    let mut worst_outcome: Option<CommandGateOutcome> = None;

    let mut logical_lines = Vec::new();
    let mut current_logical_line = String::new();

    for line in text.lines() {
        let trimmed = line.trim_end();
        if let Some(stripped) = trimmed.strip_suffix('\\') {
            current_logical_line.push_str(stripped);
            current_logical_line.push(' ');
        } else {
            current_logical_line.push_str(line);
            logical_lines.push(current_logical_line.clone());
            current_logical_line.clear();
        }
    }
    if !current_logical_line.is_empty() {
        logical_lines.push(current_logical_line);
    }

    for line_ref in &logical_lines {
        let line = line_ref.as_str();
        if line.trim().is_empty() {
            continue;
        }

        if !is_command_candidate(line) {
            continue;
        }

        let builtin_result = match crate::command_guard::evaluate_stateless(line) {
            Some((rule_id, _pack, reason, _suggestions)) => {
                let is_hard_deny = rule_id == "core.filesystem:rm-rf-root";
                let full_rule = if is_hard_deny {
                    "command.rm_rf_root".to_string()
                } else {
                    format!("dcg.{}", rule_id)
                };

                let policy = if is_hard_deny {
                    DcgDenyPolicy::Deny
                } else {
                    config.dcg_deny_policy
                };

                Some(match policy {
                    DcgDenyPolicy::Deny => CommandGateOutcome::Deny {
                        reason,
                        rule_id: full_rule,
                    },
                    DcgDenyPolicy::RequireApproval => CommandGateOutcome::RequireApproval {
                        reason,
                        rule_id: full_rule,
                    },
                })
            }
            None => None,
        };

        if let Some(result) = builtin_result {
            match result {
                CommandGateOutcome::Deny { .. } => return result,
                CommandGateOutcome::RequireApproval { .. } => {
                    if !matches!(worst_outcome, Some(CommandGateOutcome::Deny { .. })) {
                        worst_outcome = Some(result);
                    }
                }
                CommandGateOutcome::Allow => {}
            }
        }

        let dcg_result = match config.dcg_mode {
            DcgMode::Disabled | DcgMode::Native => CommandGateOutcome::Allow,
            DcgMode::Opportunistic | DcgMode::Required => match dcg_runner(line) {
                Ok(DcgDecision::Allow) => CommandGateOutcome::Allow,
                Ok(DcgDecision::Deny { rule_id }) => {
                    let rule = rule_id.unwrap_or_else(|| "unknown".to_string());
                    let rule_id = format!("dcg.{}", rule);
                    let reason = format!("Command safety gate blocked by dcg (rule {})", rule);
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
                            DcgError::Failed(detail) => format!("dcg error: {}", detail),
                        };
                        CommandGateOutcome::RequireApproval {
                            reason: format!(
                                "Command safety gate requires dcg but it is unavailable ({})",
                                detail
                            ),
                            rule_id: "command_gate.dcg_unavailable".to_string(),
                        }
                    }
                    _ => CommandGateOutcome::Allow,
                },
            },
        };

        match dcg_result {
            CommandGateOutcome::Deny { .. } => return dcg_result,
            CommandGateOutcome::RequireApproval { .. } => {
                if !matches!(worst_outcome, Some(CommandGateOutcome::Deny { .. })) {
                    worst_outcome = Some(dcg_result);
                }
            }
            CommandGateOutcome::Allow => {}
        }
    }

    worst_outcome.unwrap_or(CommandGateOutcome::Allow)
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
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DcgError::NotAvailable
            } else {
                DcgError::Failed(e.to_string())
            }
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(payload.to_string().as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DcgError::Failed(e.to_string()));
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| DcgError::Failed(e.to_string()))?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            format!("stdout: {}", stdout.trim())
        } else {
            "no stdout/stderr output".to_string()
        };
        return Err(DcgError::Failed(format!(
            "dcg exited unsuccessfully ({detail})"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(DcgDecision::Allow);
    }

    let parsed: DcgResponse =
        serde_json::from_str(stdout.trim()).map_err(|e| DcgError::Failed(e.to_string()))?;

    if parsed
        .hook_specific_output
        .permission_decision
        .eq_ignore_ascii_case("deny")
    {
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
/// use frankenterm_core::policy::Redactor;
///
/// let redactor = Redactor::new();
/// let input = "My API key is sk-abc123456789012345678901234567890123456789012345678901";
/// let output = redactor.redact(input);
/// assert!(output.contains("[REDACTED]"));
/// assert!(!output.contains("sk-abc"));
/// ```
#[derive(Debug, Default, Clone)]
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
    /// All rules that matched before priority/specificity tie-breaking.
    pub matched_rule_ids: Vec<String>,
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
            matched_rule_ids: Vec::new(),
        };
    }

    let rules_checked = rules_config
        .rules
        .iter()
        .map(|rule| rule.id.clone())
        .collect::<Vec<_>>();
    let dsl_rules = rules_config
        .rules
        .iter()
        .map(crate::policy_dsl::compile_policy_rule)
        .collect::<Vec<_>>();
    let dsl_result = crate::policy_dsl::evaluate_dsl_rules(&dsl_rules, input);

    for (rule, dsl_rule) in rules_config.rules.iter().zip(dsl_rules.iter()) {
        debug_assert_eq!(
            matches_rule(&rule.match_on, input),
            crate::policy_dsl::evaluate_predicate(&dsl_rule.predicate, input),
            "policy_dsl bridge diverged for rule {}",
            rule.id
        );
    }

    let matched_rule_ids = dsl_result
        .evaluations
        .iter()
        .filter(|evaluation| evaluation.matched)
        .map(|evaluation| evaluation.rule_id.clone())
        .collect::<Vec<_>>();
    let matching_rule = dsl_result.matched_rule.as_ref().and_then(|matched| {
        rules_config
            .rules
            .iter()
            .find(|rule| rule.id == matched.rule_id)
            .cloned()
    });

    RuleEvaluationResult {
        decision: matching_rule.as_ref().map(|rule| rule.decision),
        matching_rule,
        rules_checked,
        matched_rule_ids,
    }
}

fn config_rule_trace_reason(
    rule: &PolicyRule,
    selected: bool,
    selected_rule_id: Option<&str>,
) -> String {
    let prefix = if selected {
        "rule matched and selected".to_string()
    } else {
        let winner = selected_rule_id.unwrap_or("unknown");
        format!("rule matched but '{winner}' won tie-breaking")
    };

    rule.message
        .as_ref()
        .map_or_else(|| prefix.clone(), |message| format!("{prefix}: {message}"))
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

    // Check policy surface
    if !match_on.surfaces.is_empty()
        && !match_on
            .surfaces
            .iter()
            .any(|s| s.eq_ignore_ascii_case(input.surface.as_str()))
    {
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

    // Check command patterns (regex) — cached to avoid recompilation on every policy check
    if !match_on.command_patterns.is_empty() {
        match &input.command_text {
            Some(text) => {
                thread_local! {
                    static CMD_REGEX_CACHE: std::cell::RefCell<crate::lru_cache::LruCache<String, Regex>> =
                        std::cell::RefCell::new(crate::lru_cache::LruCache::new(128));
                }
                let matches_any = match_on.command_patterns.iter().any(|pattern| {
                    CMD_REGEX_CACHE.with(|cache| {
                        let mut cache = cache.borrow_mut();
                        if let Some(re) = cache.get(pattern) {
                            return re.is_match(text);
                        }
                        if let Ok(re) = Regex::new(pattern) {
                            let is_match = re.is_match(text);
                            cache.put(pattern.clone(), re);
                            return is_match;
                        }
                        false
                    })
                });
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
///
/// Uses the fast O(n) byte-level matching algorithm from `crate::events::match_rule_glob`.
fn glob_match(pattern: &str, text: &str) -> bool {
    crate::events::match_rule_glob(pattern, text)
}

// ============================================================================
// Composable Rule Predicate AST
// ============================================================================

/// A composable boolean predicate tree for policy rule matching.
///
/// Unlike `PolicyRuleMatch` which uses flat AND/OR within categories,
/// `RulePredicate` supports recursive boolean logic via `And`/`Or`/`Not`
/// combinators. This enables complex policies like:
///
/// ```text
/// (action=spawn AND actor=robot) OR (action=delete AND pane_title="*critical*")
/// ```
///
/// Individual leaf predicates match against `PolicyInput` fields.
/// Multiple values in a leaf are OR'd (any match succeeds).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RulePredicate {
    /// Matches if the action kind is in the given list (OR'd).
    Action { values: Vec<String> },
    /// Matches if the actor kind is in the given list (OR'd).
    Actor { values: Vec<String> },
    /// Matches if the policy surface is in the given list (OR'd, case-insensitive).
    Surface { values: Vec<String> },
    /// Matches if the pane ID is in the given list (OR'd).
    PaneId { values: Vec<u64> },
    /// Matches if the pane title matches any glob pattern (OR'd).
    PaneTitle { patterns: Vec<String> },
    /// Matches if the pane CWD matches any glob pattern (OR'd).
    PaneCwd { patterns: Vec<String> },
    /// Matches if the pane domain is in the given list (OR'd).
    PaneDomain { values: Vec<String> },
    /// Matches if the command text matches any regex pattern (OR'd).
    CommandPattern { patterns: Vec<String> },
    /// Matches if the agent type is in the given list (OR'd, case-insensitive).
    AgentType { values: Vec<String> },
    /// All child predicates must match (AND combinator).
    And { children: Vec<RulePredicate> },
    /// Any child predicate must match (OR combinator).
    Or { children: Vec<RulePredicate> },
    /// Negates the child predicate.
    Not { child: Box<RulePredicate> },
    /// Always matches.
    True,
    /// Never matches.
    False,
}

impl RulePredicate {
    /// Evaluate this predicate against a `PolicyInput`.
    #[must_use]
    pub fn evaluate(&self, input: &PolicyInput) -> bool {
        match self {
            Self::Action { values } => {
                !values.is_empty() && values.iter().any(|v| v == input.action.as_str())
            }
            Self::Actor { values } => {
                !values.is_empty() && values.iter().any(|v| v == input.actor.as_str())
            }
            Self::Surface { values } => {
                !values.is_empty()
                    && values
                        .iter()
                        .any(|v| v.eq_ignore_ascii_case(input.surface.as_str()))
            }
            Self::PaneId { values } => match input.pane_id {
                Some(id) => !values.is_empty() && values.contains(&id),
                None => false,
            },
            Self::PaneTitle { patterns } => match &input.pane_title {
                Some(title) => {
                    !patterns.is_empty() && patterns.iter().any(|p| glob_match(p, title))
                }
                None => false,
            },
            Self::PaneCwd { patterns } => match &input.pane_cwd {
                Some(cwd) => !patterns.is_empty() && patterns.iter().any(|p| glob_match(p, cwd)),
                None => false,
            },
            Self::PaneDomain { values } => match &input.domain {
                Some(domain) => !values.is_empty() && values.iter().any(|v| v == domain),
                None => false,
            },
            Self::CommandPattern { patterns } => match &input.command_text {
                Some(text) => {
                    !patterns.is_empty()
                        && patterns
                            .iter()
                            .any(|p| Regex::new(p).is_ok_and(|re| re.is_match(text)))
                }
                None => false,
            },
            Self::AgentType { values } => match &input.agent_type {
                Some(agent) => {
                    !values.is_empty() && values.iter().any(|v| v.eq_ignore_ascii_case(agent))
                }
                None => false,
            },
            Self::And { children } => children.iter().all(|c| c.evaluate(input)),
            Self::Or { children } => children.iter().any(|c| c.evaluate(input)),
            Self::Not { child } => !child.evaluate(input),
            Self::True => true,
            Self::False => false,
        }
    }

    /// Returns the depth of the predicate tree.
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Self::And { children } | Self::Or { children } => {
                1 + children.iter().map(|c| c.depth()).max().unwrap_or(0)
            }
            Self::Not { child } => 1 + child.depth(),
            _ => 1,
        }
    }

    /// Returns the total number of leaf predicates in the tree.
    #[must_use]
    pub fn leaf_count(&self) -> usize {
        match self {
            Self::And { children } | Self::Or { children } => {
                children.iter().map(|c| c.leaf_count()).sum()
            }
            Self::Not { child } => child.leaf_count(),
            _ => 1,
        }
    }

    /// Convert a flat `PolicyRuleMatch` into a composable predicate tree.
    ///
    /// The resulting predicate AND's all non-empty criteria (same semantics
    /// as the flat `matches_rule` function). A catch-all match becomes `True`.
    #[must_use]
    pub fn from_flat_match(m: &PolicyRuleMatch) -> Self {
        let mut parts = Vec::new();

        if !m.actions.is_empty() {
            parts.push(Self::Action {
                values: m.actions.clone(),
            });
        }
        if !m.actors.is_empty() {
            parts.push(Self::Actor {
                values: m.actors.clone(),
            });
        }
        if !m.surfaces.is_empty() {
            parts.push(Self::Surface {
                values: m.surfaces.clone(),
            });
        }
        if !m.pane_ids.is_empty() {
            parts.push(Self::PaneId {
                values: m.pane_ids.clone(),
            });
        }
        if !m.pane_titles.is_empty() {
            parts.push(Self::PaneTitle {
                patterns: m.pane_titles.clone(),
            });
        }
        if !m.pane_cwds.is_empty() {
            parts.push(Self::PaneCwd {
                patterns: m.pane_cwds.clone(),
            });
        }
        if !m.pane_domains.is_empty() {
            parts.push(Self::PaneDomain {
                values: m.pane_domains.clone(),
            });
        }
        if !m.command_patterns.is_empty() {
            parts.push(Self::CommandPattern {
                patterns: m.command_patterns.clone(),
            });
        }
        if !m.agent_types.is_empty() {
            parts.push(Self::AgentType {
                values: m.agent_types.clone(),
            });
        }

        match parts.len() {
            0 => Self::True,
            1 => parts.into_iter().next().unwrap(),
            _ => Self::And { children: parts },
        }
    }
}

/// Evaluates a `RulePredicate` against a `PolicyInput`.
///
/// Convenience function that delegates to `RulePredicate::evaluate`.
#[must_use]
pub fn evaluate_predicate(predicate: &RulePredicate, input: &PolicyInput) -> bool {
    predicate.evaluate(input)
}

// ============================================================================
// Unified telemetry snapshot
// ============================================================================

/// Unified telemetry snapshot aggregating all PolicyEngine subsystem snapshots.
///
/// Provides a single serializable struct for `ft doctor --json`, dashboard feeds,
/// and the unified telemetry schema (ft-3681t.7.1 precursor). Each field is an
/// `Option` so subsystems that fail to snapshot don't block the aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyEngineTelemetrySnapshot {
    /// Capture timestamp (epoch ms).
    pub captured_at_ms: u64,
    /// Decision log snapshot.
    pub decision_log: crate::policy_decision_log::DecisionLogSnapshot,
    /// Quarantine registry snapshot.
    pub quarantine: crate::policy_quarantine::QuarantineTelemetrySnapshot,
    /// Audit chain snapshot.
    pub audit_chain: crate::policy_audit_chain::AuditChainTelemetrySnapshot,
    /// Compliance engine snapshot.
    pub compliance: crate::policy_compliance::ComplianceSnapshot,
    /// Credential broker snapshot.
    pub credential_broker: crate::connector_credential_broker::CredentialBrokerTelemetrySnapshot,
    /// Connector governor snapshot.
    pub connector_governor: crate::connector_governor::GovernorSnapshot,
    /// Connector registry telemetry snapshot.
    pub connector_registry: crate::connector_registry::RegistryTelemetrySnapshot,
    /// Connector reliability snapshots (one per connector).
    pub connector_reliability: Vec<crate::connector_reliability::ConnectorReliabilitySnapshot>,
    /// Bundle registry snapshot.
    pub bundle_registry: crate::connector_bundles::BundleRegistrySnapshot,
    /// Connector mesh telemetry snapshot.
    pub connector_mesh: crate::connector_mesh::MeshTelemetrySnapshot,
    /// Ingestion pipeline snapshot.
    pub ingestion_pipeline: crate::connector_bundles::IngestionTelemetrySnapshot,
    /// Namespace registry snapshot.
    pub namespace_registry: crate::namespace_isolation::NamespaceRegistrySnapshot,
    /// Approval tracker snapshot.
    pub approval_tracker: ApprovalTrackerSnapshot,
    /// Revocation registry snapshot.
    pub revocation_registry: RevocationRegistrySnapshot,
    /// Whether namespace isolation is enabled.
    pub namespace_isolation_enabled: bool,
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
    /// Whether trauma-guard intervention is enabled.
    trauma_guard_enabled: bool,
    /// Custom policy rules configuration
    policy_rules: PolicyRulesConfig,
    /// Risk scoring configuration
    risk_config: RiskConfig,
    /// Append-only decision log for forensics and compliance
    decision_log: PolicyDecisionLog,
    /// Quarantine registry for component isolation
    quarantine_registry: QuarantineRegistry,
    /// Tamper-evident hash-linked audit chain
    audit_chain: AuditChain,
    /// Compliance engine for violation tracking and reporting
    compliance_engine: ComplianceEngine,
    /// Credential broker for JIT secret provisioning and access control
    credential_broker: ConnectorCredentialBroker,
    /// Credential broker configuration (sensitivity ceiling, etc.)
    credential_broker_config: CredentialBrokerConfig,
    /// Connector lifecycle manager for install/update/enable/disable/restart
    lifecycle_manager: crate::connector_lifecycle::ConnectorLifecycleManager,
    /// Connector data classifier for sensitivity tagging and redaction
    data_classifier: crate::connector_data_classification::ConnectorDataClassifier,
    /// Connector governor for rate-limit, quota, and cost governance
    connector_governor: crate::connector_governor::ConnectorGovernor,
    /// Connector registry client for package trust verification
    connector_registry: crate::connector_registry::ConnectorRegistryClient,
    /// Connector host runtime for execution environment management
    connector_host_runtime: crate::connector_host_runtime::ConnectorHostRuntime,
    /// Connector reliability registry (circuit breakers + DLQ)
    reliability_registry: crate::connector_reliability::ReliabilityRegistry,
    /// Connector bundle registry for tier-based connector packaging and trust gating
    bundle_registry: crate::connector_bundles::BundleRegistry,
    /// Connector mesh for multi-host/zone routing federation
    connector_mesh: crate::connector_mesh::ConnectorMesh,
    /// Ingestion pipeline for connector event recording and audit trails
    ingestion_pipeline: crate::connector_bundles::IngestionPipeline,
    /// Namespace registry for multi-tenant isolation and cross-tenant guardrails
    namespace_registry: crate::namespace_isolation::NamespaceRegistry,
    /// Whether namespace isolation enforcement is enabled
    namespace_isolation_enabled: bool,
    /// Approval workflow tracker for gating sensitive actions
    approval_tracker: ApprovalTracker,
    /// Revocation registry for credential/session/connector access revocation
    revocation_registry: RevocationRegistry,
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
            trauma_guard_enabled: true,
            policy_rules: PolicyRulesConfig::default(),
            risk_config: RiskConfig::default(),
            decision_log: PolicyDecisionLog::with_defaults(),
            quarantine_registry: QuarantineRegistry::new(),
            audit_chain: AuditChain::new(1024),
            compliance_engine: ComplianceEngine::new(500, 3_600_000),
            credential_broker: ConnectorCredentialBroker::new(),
            credential_broker_config: CredentialBrokerConfig::default(),
            lifecycle_manager: crate::connector_lifecycle::ConnectorLifecycleManager::new(
                crate::connector_lifecycle::LifecycleManagerConfig::default(),
            ),
            data_classifier: crate::connector_data_classification::ConnectorDataClassifier::new(
                crate::connector_data_classification::ClassifierConfig::default(),
            ),
            connector_governor: crate::connector_governor::ConnectorGovernor::new(
                crate::connector_governor::ConnectorGovernorConfig::default(),
            ),
            connector_registry: crate::connector_registry::ConnectorRegistryClient::new(
                crate::connector_registry::ConnectorRegistryConfig::default(),
            ),
            connector_host_runtime: crate::connector_host_runtime::ConnectorHostRuntime::new(
                crate::connector_host_runtime::ConnectorHostConfig::default(),
            )
            .expect("default ConnectorHostConfig should be valid"),
            reliability_registry: crate::connector_reliability::ReliabilityRegistry::new(
                crate::connector_reliability::ConnectorReliabilityConfig::default(),
            ),
            bundle_registry: crate::connector_bundles::BundleRegistry::new(
                crate::connector_bundles::BundleRegistryConfig::default(),
            ),
            connector_mesh: crate::connector_mesh::ConnectorMesh::new(
                crate::connector_mesh::ConnectorMeshConfig::default(),
            ),
            ingestion_pipeline: crate::connector_bundles::IngestionPipeline::new(
                crate::connector_bundles::IngestionPipelineConfig::default(),
            ),
            namespace_registry: crate::namespace_isolation::NamespaceRegistry::new(),
            namespace_isolation_enabled: false,
            approval_tracker: ApprovalTracker::default(),
            revocation_registry: RevocationRegistry::default(),
        }
    }

    /// Create a policy engine from a `SafetyConfig`.
    ///
    /// This is the recommended constructor when building from TOML config,
    /// since it wires command gate, policy rules, decision log, and trauma
    /// guard settings in a single call.
    #[must_use]
    pub fn from_safety_config(config: &crate::config::SafetyConfig) -> Self {
        let mut engine = Self::new(
            config.rate_limit_per_pane,
            config.rate_limit_global,
            config.require_prompt_active,
        )
        .with_command_gate_config(config.command_gate.clone())
        .with_policy_rules(config.rules.clone())
        .with_decision_log_config(config.decision_log.clone());
        engine.quarantine_registry = QuarantineRegistry::from_config(&config.quarantine);
        engine.audit_chain = AuditChain::from_config(&config.audit_chain);
        engine.compliance_engine = ComplianceEngine::from_config(&config.compliance);
        engine.credential_broker =
            ConnectorCredentialBroker::from_config(&config.credential_broker);
        engine.credential_broker_config = config.credential_broker.clone();
        engine.lifecycle_manager = crate::connector_lifecycle::ConnectorLifecycleManager::new(
            config.lifecycle_manager.clone(),
        );
        engine.data_classifier = crate::connector_data_classification::ConnectorDataClassifier::new(
            config.data_classifier.clone(),
        );
        engine.connector_governor =
            crate::connector_governor::ConnectorGovernor::new(config.connector_governor.clone());
        engine.connector_registry = crate::connector_registry::ConnectorRegistryClient::new(
            config.connector_registry.clone(),
        );
        engine.connector_host_runtime = crate::connector_host_runtime::ConnectorHostRuntime::new(
            config.connector_host_runtime.clone(),
        )
        .expect("ConnectorHostConfig from SafetyConfig should be valid");
        engine.reliability_registry = crate::connector_reliability::ReliabilityRegistry::new(
            config.connector_reliability.clone(),
        );
        engine.bundle_registry =
            crate::connector_bundles::BundleRegistry::new(config.bundle_registry.clone());
        engine.connector_mesh =
            crate::connector_mesh::ConnectorMesh::new(config.connector_mesh.clone());
        engine.ingestion_pipeline =
            crate::connector_bundles::IngestionPipeline::new(config.ingestion_pipeline.clone());
        engine.namespace_registry =
            crate::namespace_isolation::NamespaceRegistry::from_config(&config.namespace_isolation);
        engine.namespace_isolation_enabled = config.namespace_isolation.enabled;
        engine
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

    /// Enable or disable trauma-guard intervention.
    #[must_use]
    pub fn with_trauma_guard_enabled(mut self, enabled: bool) -> Self {
        self.trauma_guard_enabled = enabled;
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

    /// Set decision log configuration
    #[must_use]
    pub fn with_decision_log_config(mut self, config: DecisionLogConfig) -> Self {
        self.decision_log = PolicyDecisionLog::new(config);
        self
    }

    /// Access the quarantine registry.
    #[must_use]
    pub fn quarantine_registry(&self) -> &QuarantineRegistry {
        &self.quarantine_registry
    }

    /// Access the quarantine registry mutably (for quarantine/release operations).
    pub fn quarantine_registry_mut(&mut self) -> &mut QuarantineRegistry {
        &mut self.quarantine_registry
    }

    /// Access the tamper-evident audit chain.
    #[must_use]
    pub fn audit_chain(&self) -> &AuditChain {
        &self.audit_chain
    }

    /// Access the audit chain mutably (for verification, export, etc.).
    pub fn audit_chain_mut(&mut self) -> &mut AuditChain {
        &mut self.audit_chain
    }

    /// Access the compliance engine for violation queries and snapshots.
    #[must_use]
    pub fn compliance_engine(&self) -> &ComplianceEngine {
        &self.compliance_engine
    }

    /// Access the compliance engine mutably (for recording violations, remediations).
    pub fn compliance_engine_mut(&mut self) -> &mut ComplianceEngine {
        &mut self.compliance_engine
    }

    /// Access the credential broker.
    #[must_use]
    pub fn credential_broker(&self) -> &ConnectorCredentialBroker {
        &self.credential_broker
    }

    /// Access the credential broker mutably (for registering providers, rules, etc.).
    pub fn credential_broker_mut(&mut self) -> &mut ConnectorCredentialBroker {
        &mut self.credential_broker
    }

    /// Access the connector lifecycle manager.
    #[must_use]
    pub fn lifecycle_manager(&self) -> &crate::connector_lifecycle::ConnectorLifecycleManager {
        &self.lifecycle_manager
    }

    /// Access the connector lifecycle manager mutably.
    pub fn lifecycle_manager_mut(
        &mut self,
    ) -> &mut crate::connector_lifecycle::ConnectorLifecycleManager {
        &mut self.lifecycle_manager
    }

    /// Access the data classifier.
    #[must_use]
    pub fn data_classifier(
        &self,
    ) -> &crate::connector_data_classification::ConnectorDataClassifier {
        &self.data_classifier
    }

    /// Access the data classifier mutably.
    pub fn data_classifier_mut(
        &mut self,
    ) -> &mut crate::connector_data_classification::ConnectorDataClassifier {
        &mut self.data_classifier
    }

    /// Access the connector governor.
    #[must_use]
    pub fn connector_governor(&self) -> &crate::connector_governor::ConnectorGovernor {
        &self.connector_governor
    }

    /// Access the connector governor mutably.
    pub fn connector_governor_mut(&mut self) -> &mut crate::connector_governor::ConnectorGovernor {
        &mut self.connector_governor
    }

    /// Access the connector registry.
    #[must_use]
    pub fn connector_registry(&self) -> &crate::connector_registry::ConnectorRegistryClient {
        &self.connector_registry
    }

    /// Access the connector registry mutably.
    pub fn connector_registry_mut(
        &mut self,
    ) -> &mut crate::connector_registry::ConnectorRegistryClient {
        &mut self.connector_registry
    }

    /// Access the connector host runtime.
    #[must_use]
    pub fn connector_host_runtime(&self) -> &crate::connector_host_runtime::ConnectorHostRuntime {
        &self.connector_host_runtime
    }

    /// Access the connector host runtime mutably.
    pub fn connector_host_runtime_mut(
        &mut self,
    ) -> &mut crate::connector_host_runtime::ConnectorHostRuntime {
        &mut self.connector_host_runtime
    }

    /// Access the reliability registry.
    #[must_use]
    pub fn reliability_registry(&self) -> &crate::connector_reliability::ReliabilityRegistry {
        &self.reliability_registry
    }

    /// Access the reliability registry mutably.
    pub fn reliability_registry_mut(
        &mut self,
    ) -> &mut crate::connector_reliability::ReliabilityRegistry {
        &mut self.reliability_registry
    }

    /// Access the bundle registry.
    #[must_use]
    pub fn bundle_registry(&self) -> &crate::connector_bundles::BundleRegistry {
        &self.bundle_registry
    }

    /// Access the bundle registry mutably.
    pub fn bundle_registry_mut(&mut self) -> &mut crate::connector_bundles::BundleRegistry {
        &mut self.bundle_registry
    }

    /// Access the connector mesh.
    #[must_use]
    pub fn connector_mesh(&self) -> &crate::connector_mesh::ConnectorMesh {
        &self.connector_mesh
    }

    /// Access the connector mesh mutably.
    pub fn connector_mesh_mut(&mut self) -> &mut crate::connector_mesh::ConnectorMesh {
        &mut self.connector_mesh
    }

    /// Access the ingestion pipeline.
    #[must_use]
    pub fn ingestion_pipeline(&self) -> &crate::connector_bundles::IngestionPipeline {
        &self.ingestion_pipeline
    }

    /// Access the ingestion pipeline mutably.
    pub fn ingestion_pipeline_mut(&mut self) -> &mut crate::connector_bundles::IngestionPipeline {
        &mut self.ingestion_pipeline
    }

    /// Access the namespace registry.
    #[must_use]
    pub fn namespace_registry(&self) -> &crate::namespace_isolation::NamespaceRegistry {
        &self.namespace_registry
    }

    /// Access the namespace registry mutably.
    pub fn namespace_registry_mut(&mut self) -> &mut crate::namespace_isolation::NamespaceRegistry {
        &mut self.namespace_registry
    }

    /// Returns whether namespace isolation enforcement is enabled.
    #[must_use]
    pub fn namespace_isolation_enabled(&self) -> bool {
        self.namespace_isolation_enabled
    }

    /// Bind a resource to a namespace with audit chain recording.
    ///
    /// Records the binding in both the namespace registry and the audit chain
    /// for governance traceability.
    pub fn bind_resource_to_namespace(
        &mut self,
        kind: crate::namespace_isolation::NamespacedResourceKind,
        resource_id: &str,
        namespace: crate::namespace_isolation::TenantNamespace,
        actor: &str,
        now_ms: u64,
    ) -> Option<crate::namespace_isolation::TenantNamespace> {
        let prev = self
            .namespace_registry
            .bind(kind, resource_id, namespace.clone());
        self.audit_chain.append(
            AuditEntryKind::PolicyDecision,
            actor,
            &format!(
                "bound {kind} '{resource_id}' to namespace '{namespace}'{}",
                prev.as_ref()
                    .map(|p| format!(" (previously in '{p}')"))
                    .unwrap_or_default(),
                kind = kind.as_str(),
            ),
            &format!("namespace.bind.{}", kind.as_str()),
            now_ms,
        );
        self.compliance_engine.record_evaluation(false);
        prev
    }

    /// Check cross-tenant access with audit chain recording.
    ///
    /// Performs a namespace boundary check and records the result in both
    /// the namespace registry's audit log and the policy audit chain.
    /// Returns [`BoundaryCheckResult`](crate::namespace_isolation::BoundaryCheckResult).
    pub fn check_cross_tenant_access(
        &mut self,
        source_ns: &crate::namespace_isolation::TenantNamespace,
        target_ns: &crate::namespace_isolation::TenantNamespace,
        resource_kind: crate::namespace_isolation::NamespacedResourceKind,
        resource_id: &str,
        actor: &str,
        now_ms: u64,
    ) -> crate::namespace_isolation::BoundaryCheckResult {
        let result = self.namespace_registry.check_and_audit(
            source_ns,
            target_ns,
            resource_kind,
            resource_id,
            now_ms,
        );

        if result.crosses_boundary {
            let decision_str = if result.is_allowed() { "allow" } else { "deny" };
            self.audit_chain.append(
                AuditEntryKind::PolicyDecision,
                actor,
                &format!(
                    "cross-tenant {decision_str}: {src} -> {tgt} for {kind}:'{rid}'{rule}",
                    src = source_ns,
                    tgt = target_ns,
                    kind = resource_kind.as_str(),
                    rid = resource_id,
                    rule = result
                        .matched_rule
                        .as_deref()
                        .map(|r| format!(" (rule: {r})"))
                        .unwrap_or_default(),
                ),
                "policy.namespace_isolation",
                now_ms,
            );

            if !result.is_allowed() {
                self.compliance_engine.record_evaluation(true);
            } else {
                self.compliance_engine.record_evaluation(false);
            }
        }

        result
    }

    /// Register a connector bundle within an actor's namespace.
    ///
    /// Registers the bundle normally and also binds it to the actor's tenant
    /// namespace so that future cross-tenant access checks apply.  Returns
    /// the result of the bundle registration.
    pub fn register_bundle_in_namespace(
        &mut self,
        bundle: crate::connector_bundles::ConnectorBundle,
        actor_namespace: &crate::namespace_isolation::TenantNamespace,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), crate::connector_bundles::BundleRegistryError> {
        let bundle_id = bundle.bundle_id.clone();
        self.register_bundle(bundle, actor, now_ms)?;
        // Bind all connectors in this bundle to the actor's namespace
        self.bind_resource_to_namespace(
            crate::namespace_isolation::NamespacedResourceKind::Connector,
            &bundle_id,
            actor_namespace.clone(),
            actor,
            now_ms,
        );
        Ok(())
    }

    /// Check whether a connector credential access is allowed under namespace
    /// isolation.  Returns `true` if the access is within the same namespace
    /// (or namespace isolation is disabled).
    pub fn check_credential_namespace(
        &mut self,
        connector_id: &str,
        actor_namespace: &crate::namespace_isolation::TenantNamespace,
        actor: &str,
        now_ms: u64,
    ) -> bool {
        if !self.namespace_isolation_enabled {
            return true;
        }
        let target_ns = self.namespace_registry.lookup(
            crate::namespace_isolation::NamespacedResourceKind::Credential,
            connector_id,
        );
        let result = self.check_cross_tenant_access(
            actor_namespace,
            &target_ns,
            crate::namespace_isolation::NamespacedResourceKind::Credential,
            connector_id,
            actor,
            now_ms,
        );
        result.is_allowed()
    }

    // ── Approval Tracker accessors ────────────────────────────────

    /// Access the approval tracker.
    #[must_use]
    pub fn approval_tracker(&self) -> &ApprovalTracker {
        &self.approval_tracker
    }

    /// Access the approval tracker mutably.
    pub fn approval_tracker_mut(&mut self) -> &mut ApprovalTracker {
        &mut self.approval_tracker
    }

    /// Submit an approval request with audit chain recording.
    ///
    /// Creates a pending approval entry and records it in the audit chain.
    /// Returns the generated approval ID.
    pub fn submit_approval(
        &mut self,
        action: &str,
        actor: &str,
        resource: &str,
        reason: &str,
        rule_id: &str,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> String {
        let approval_id = self.approval_tracker.submit(
            action,
            actor,
            resource,
            reason,
            rule_id,
            now_ms,
            expires_at_ms,
        );
        self.audit_chain.append(
            AuditEntryKind::PolicyDecision,
            actor,
            &format!(
                "approval requested: {approval_id} for {action} on '{resource}' (reason: {reason})",
            ),
            "policy.approval.submit",
            now_ms,
        );
        self.compliance_engine.record_evaluation(false);
        approval_id
    }

    /// Approve a pending request with audit chain recording.
    ///
    /// Returns true if the approval was found and granted.
    pub fn grant_approval(
        &mut self,
        approval_id: &str,
        decided_by: &str,
        now_ms: u64,
    ) -> bool {
        let granted = self.approval_tracker.approve(approval_id, decided_by, now_ms);
        if granted {
            self.audit_chain.append(
                AuditEntryKind::PolicyDecision,
                decided_by,
                &format!("approval granted: {approval_id}"),
                "policy.approval.grant",
                now_ms,
            );
        }
        granted
    }

    /// Reject a pending request with audit chain recording.
    ///
    /// Returns true if the approval was found and rejected.
    pub fn reject_approval(
        &mut self,
        approval_id: &str,
        decided_by: &str,
        now_ms: u64,
    ) -> bool {
        let rejected = self.approval_tracker.reject(approval_id, decided_by, now_ms);
        if rejected {
            self.audit_chain.append(
                AuditEntryKind::PolicyDecision,
                decided_by,
                &format!("approval rejected: {approval_id}"),
                "policy.approval.reject",
                now_ms,
            );
            self.compliance_engine.record_evaluation(true);
        }
        rejected
    }

    /// Revoke a previously granted approval with audit chain recording.
    ///
    /// Returns true if the approval was found and revoked.
    pub fn revoke_approval(
        &mut self,
        approval_id: &str,
        decided_by: &str,
        now_ms: u64,
    ) -> bool {
        let revoked = self.approval_tracker.revoke(approval_id, decided_by, now_ms);
        if revoked {
            self.audit_chain.append(
                AuditEntryKind::QuarantineAction,
                decided_by,
                &format!("approval revoked: {approval_id}"),
                "policy.approval.revoke",
                now_ms,
            );
            self.compliance_engine.record_evaluation(true);
        }
        revoked
    }

    // ── Revocation Registry accessors ─────────────────────────────

    /// Access the revocation registry.
    #[must_use]
    pub fn revocation_registry(&self) -> &RevocationRegistry {
        &self.revocation_registry
    }

    /// Access the revocation registry mutably.
    pub fn revocation_registry_mut(&mut self) -> &mut RevocationRegistry {
        &mut self.revocation_registry
    }

    /// Revoke a resource (credential, session, connector) with audit trail.
    ///
    /// Returns the revocation ID.  The revocation is immediately active and
    /// will cause future authorization checks to deny access.
    pub fn revoke_resource(
        &mut self,
        resource_type: &str,
        resource_id: &str,
        reason: &str,
        actor: &str,
        now_ms: u64,
    ) -> String {
        let rev_id = self.revocation_registry.revoke(
            resource_type,
            resource_id,
            reason,
            actor,
            now_ms,
        );
        self.audit_chain.append(
            AuditEntryKind::QuarantineAction,
            actor,
            &format!(
                "resource revoked: {rev_id} ({resource_type}:'{resource_id}', reason: {reason})",
            ),
            "policy.revocation.revoke",
            now_ms,
        );
        self.compliance_engine.record_evaluation(true);
        rev_id
    }

    /// Reinstate a previously revoked resource with audit trail.
    ///
    /// Returns true if the revocation was found and deactivated.
    pub fn reinstate_resource(
        &mut self,
        revocation_id: &str,
        actor: &str,
        now_ms: u64,
    ) -> bool {
        let reinstated = self.revocation_registry.reinstate(revocation_id);
        if reinstated {
            self.audit_chain.append(
                AuditEntryKind::PolicyDecision,
                actor,
                &format!("resource reinstated: {revocation_id}"),
                "policy.revocation.reinstate",
                now_ms,
            );
            self.compliance_engine.record_evaluation(false);
        }
        reinstated
    }

    /// Check if a resource is currently revoked.
    #[must_use]
    pub fn is_resource_revoked(&self, resource_type: &str, resource_id: &str) -> bool {
        self.revocation_registry
            .is_revoked(resource_type, resource_id)
    }

    // ── Forensic Report ───────────────────────────────────────────

    /// Generate a forensic report covering the specified query parameters.
    ///
    /// Aggregates evidence from the decision log, audit chain, namespace
    /// registry, approval tracker, revocation registry, compliance engine,
    /// and quarantine registry into a unified, exportable report.
    pub fn generate_forensic_report(&mut self, query: &ForensicQuery, now_ms: u64) -> ForensicReport {
        let end = if query.end_ms == 0 { u64::MAX } else { query.end_ms };

        // Decision log entries
        let mut decisions: Vec<crate::policy_decision_log::PolicyDecisionEntry> =
            self.decision_log.by_time_range(query.start_ms, end)
                .into_iter()
                .cloned()
                .collect();

        if let Some(actor) = query.actor {
            decisions.retain(|d| d.actor == actor);
        }
        if let Some(action) = query.action {
            decisions.retain(|d| d.action == action);
        }
        if let Some(pane_id) = query.pane_id {
            decisions.retain(|d| d.pane_id == Some(pane_id));
        }
        if query.denials_only {
            decisions.retain(|d| {
                d.decision == crate::policy_decision_log::DecisionOutcome::Deny
            });
        }

        // Audit chain entries
        let audit_trail: Vec<ForensicAuditEntry> = self
            .audit_chain
            .entries_in_range(query.start_ms, end)
            .into_iter()
            .map(|e| ForensicAuditEntry {
                timestamp_ms: e.timestamp_ms,
                kind: format!("{:?}", e.kind),
                actor: e.actor.clone(),
                description: e.description.clone(),
                surface: e.entity_ref.clone(),
                hash: e.chain_hash.clone(),
            })
            .collect();

        // Revocations
        let revocations: Vec<RevocationRecord> = self
            .revocation_registry
            .active_revocations()
            .into_iter()
            .cloned()
            .collect();

        // Approvals
        let approvals: Vec<ApprovalEntry> = self
            .approval_tracker
            .by_time_range(query.start_ms, end)
            .into_iter()
            .cloned()
            .collect();

        // Namespace violations
        let namespace_violations: Vec<ForensicNamespaceEvent> = self
            .namespace_registry
            .audit_log()
            .iter()
            .filter(|e| e.timestamp_ms >= query.start_ms && e.timestamp_ms <= end)
            .map(|e| ForensicNamespaceEvent {
                timestamp_ms: e.timestamp_ms,
                source_namespace: e.source_namespace.as_str().to_string(),
                target_namespace: e.target_namespace.as_str().to_string(),
                resource_kind: e.resource_kind.clone(),
                resource_id: e.resource_id.clone(),
                decision: format!("{:?}", e.decision),
                reason: e.reason.clone().unwrap_or_default(),
            })
            .collect();

        // Compliance summary
        let comp_snap = self.compliance_engine.snapshot(now_ms);
        let denial_rate = if comp_snap.counters.total_evaluations > 0 {
            (comp_snap.counters.total_denials as f64
                / comp_snap.counters.total_evaluations as f64)
                * 100.0
        } else {
            0.0
        };

        // Quarantine
        let quarantine_active: Vec<String> = self
            .quarantine_registry
            .active_quarantines();

        ForensicReport {
            generated_at_ms: now_ms,
            time_range: (query.start_ms, end),
            decisions,
            audit_trail,
            revocations,
            approvals,
            namespace_violations,
            compliance_summary: ForensicComplianceSummary {
                total_evaluations: comp_snap.counters.total_evaluations,
                total_denials: comp_snap.counters.total_denials,
                denial_rate_percent: denial_rate,
            },
            quarantine_active,
            kill_switch_active: self.quarantine_registry.kill_switch().is_emergency(),
        }
    }

    /// Generate a forensic report and export it as JSON.
    pub fn export_forensic_report_json(
        &mut self,
        query: &ForensicQuery,
        now_ms: u64,
    ) -> Result<String, serde_json::Error> {
        let report = self.generate_forensic_report(query, now_ms);
        serde_json::to_string_pretty(&report)
    }

    /// Generate a forensic report and export it as JSONL (one record per line).
    pub fn export_forensic_report_jsonl(
        &mut self,
        query: &ForensicQuery,
        now_ms: u64,
    ) -> Result<String, serde_json::Error> {
        let report = self.generate_forensic_report(query, now_ms);
        let mut lines = Vec::new();
        // Emit decisions
        for d in &report.decisions {
            lines.push(serde_json::to_string(d)?);
        }
        // Emit audit trail
        for a in &report.audit_trail {
            lines.push(serde_json::to_string(a)?);
        }
        // Emit revocations
        for r in &report.revocations {
            lines.push(serde_json::to_string(r)?);
        }
        // Emit approvals
        for ap in &report.approvals {
            lines.push(serde_json::to_string(ap)?);
        }
        // Emit namespace violations
        for ns in &report.namespace_violations {
            lines.push(serde_json::to_string(ns)?);
        }
        // Emit compliance summary
        lines.push(serde_json::to_string(&report.compliance_summary)?);
        Ok(lines.join("\n"))
    }

    /// Register a connector bundle with audit chain recording and compliance notification.
    pub fn register_bundle(
        &mut self,
        bundle: crate::connector_bundles::ConnectorBundle,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), crate::connector_bundles::BundleRegistryError> {
        let bundle_id = bundle.bundle_id.clone();
        let tier = bundle.tier;
        self.bundle_registry.register(bundle, actor, now_ms)?;
        self.audit_chain.append(
            AuditEntryKind::PolicyDecision,
            actor,
            &format!("registered connector bundle '{bundle_id}' (tier: {tier})"),
            &bundle_id,
            now_ms,
        );
        self.compliance_engine.record_evaluation(false);
        Ok(())
    }

    /// Remove a connector bundle with audit chain recording and compliance notification.
    pub fn remove_bundle(
        &mut self,
        bundle_id: &str,
        actor: &str,
        now_ms: u64,
    ) -> Result<
        crate::connector_bundles::ConnectorBundle,
        crate::connector_bundles::BundleRegistryError,
    > {
        let bundle = self.bundle_registry.remove(bundle_id, actor, now_ms)?;
        self.audit_chain.append(
            AuditEntryKind::QuarantineAction,
            actor,
            &format!(
                "removed connector bundle '{bundle_id}' (tier: {})",
                bundle.tier
            ),
            bundle_id,
            now_ms,
        );
        self.compliance_engine.record_evaluation(false);
        Ok(bundle)
    }

    /// Record a credential broker denial to the compliance engine and audit chain.
    fn credential_broker_record_denial(&mut self, connector_id: &str) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.compliance_engine.record_evaluation(true);
        self.audit_chain.append(
            AuditEntryKind::PolicyDecision,
            connector_id,
            &format!("credential access denied for connector '{connector_id}'"),
            "policy.credential_broker",
            ts,
        );
    }

    /// Get the current risk configuration
    #[must_use]
    pub fn risk_config(&self) -> &RiskConfig {
        &self.risk_config
    }

    /// Access the decision log for querying and export.
    #[must_use]
    pub fn decision_log(&self) -> &PolicyDecisionLog {
        &self.decision_log
    }

    /// Access the decision log mutably (for filtering, export, etc.).
    pub fn decision_log_mut(&mut self) -> &mut PolicyDecisionLog {
        &mut self.decision_log
    }

    /// Generate a unified metrics dashboard aggregating all policy subsystems.
    ///
    /// Collects metrics from the decision log, compliance engine, quarantine
    /// registry, audit chain, and credential broker into a single
    /// `PolicyMetricsDashboard` with health indicators and counters.
    ///
    /// Uses `&mut self` because audit chain verification updates internal
    /// telemetry counters.
    pub fn metrics_dashboard(&mut self, now_ms: u64) -> PolicyMetricsDashboard {
        self.metrics_dashboard_with_thresholds(now_ms, PolicyMetricsThresholds::default())
    }

    /// Generate a metrics dashboard with custom thresholds.
    pub fn metrics_dashboard_with_thresholds(
        &mut self,
        now_ms: u64,
        thresholds: PolicyMetricsThresholds,
    ) -> PolicyMetricsDashboard {
        let mut collector = PolicyMetricsCollector::new(thresholds);

        // Feed decision log counters
        let log_snapshot = self.decision_log.snapshot();
        collector.update_subsystem(
            "decision_log",
            PolicySubsystemInput {
                evaluations: log_snapshot.total_recorded,
                denials: log_snapshot.deny_count,
                active_quarantines: 0,
                active_violations: 0,
            },
        );

        // Feed compliance engine counters
        let compliance_snap = self.compliance_engine.snapshot(now_ms);
        collector.update_subsystem(
            "compliance",
            PolicySubsystemInput {
                evaluations: compliance_snap.counters.total_evaluations,
                denials: compliance_snap.counters.total_denials,
                active_quarantines: 0,
                active_violations: compliance_snap.active_violations.len() as u32,
            },
        );

        // Feed quarantine registry state
        let quarantine_count = self.quarantine_registry.active_quarantines().len() as u32;
        collector.update_subsystem(
            "quarantine",
            PolicySubsystemInput {
                evaluations: 0,
                denials: 0,
                active_quarantines: quarantine_count,
                active_violations: 0,
            },
        );

        // Feed audit chain state
        let chain_verification = self.audit_chain.verify();
        collector.update_audit_chain(self.audit_chain.len() as u64, chain_verification.valid);

        // Feed kill switch state
        let ks = self.quarantine_registry.kill_switch();
        let ks_active = !matches!(
            ks.level,
            crate::policy_quarantine::KillSwitchLevel::Disarmed
        );
        collector.update_kill_switch(ks_active);

        // Feed credential broker counters
        let broker_snap = self.credential_broker.telemetry_snapshot(now_ms);
        collector.update_subsystem(
            "credential_broker",
            PolicySubsystemInput {
                evaluations: broker_snap.counters.leases_issued
                    + broker_snap.counters.access_denied,
                denials: broker_snap.counters.access_denied,
                active_quarantines: 0,
                active_violations: 0,
            },
        );

        // Feed connector governor counters
        let gov_snap = self.connector_governor.snapshot(now_ms);
        collector.update_subsystem(
            "connector_governor",
            PolicySubsystemInput {
                evaluations: gov_snap.telemetry.evaluations,
                denials: gov_snap.telemetry.rejections,
                active_quarantines: 0,
                active_violations: 0,
            },
        );

        // Feed bundle registry counters
        let bundle_tel = self.bundle_registry.telemetry();
        collector.update_subsystem(
            "bundle_registry",
            PolicySubsystemInput {
                evaluations: bundle_tel.bundles_registered
                    + bundle_tel.bundles_updated
                    + bundle_tel.bundles_removed,
                denials: bundle_tel.validation_failures,
                active_quarantines: 0,
                active_violations: 0,
            },
        );

        // Feed connector mesh counters
        let mesh_snap = self.connector_mesh.telemetry().snapshot();
        collector.update_subsystem(
            "connector_mesh",
            PolicySubsystemInput {
                evaluations: mesh_snap.routing_requests,
                denials: mesh_snap.routing_failures,
                active_quarantines: 0,
                active_violations: 0,
            },
        );

        // Feed ingestion pipeline counters
        let ingest_tel = self.ingestion_pipeline.telemetry();
        collector.update_subsystem(
            "ingestion_pipeline",
            PolicySubsystemInput {
                evaluations: ingest_tel.events_received,
                denials: ingest_tel.events_rejected,
                active_quarantines: 0,
                active_violations: 0,
            },
        );

        // Feed namespace isolation counters
        let ns_snap = self.namespace_registry.snapshot();
        collector.update_subsystem(
            "namespace_isolation",
            PolicySubsystemInput {
                evaluations: ns_snap.total_bindings as u64,
                denials: 0, // boundary denials tracked via compliance_engine
                active_quarantines: 0,
                active_violations: 0,
            },
        );

        collector.dashboard(now_ms)
    }

    /// Quarantine a component and record the action in the audit chain.
    pub fn quarantine_component(
        &mut self,
        component_id: &str,
        component_kind: crate::policy_quarantine::ComponentKind,
        severity: crate::policy_quarantine::QuarantineSeverity,
        reason: crate::policy_quarantine::QuarantineReason,
        imposed_by: &str,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<(), crate::policy_quarantine::QuarantineError> {
        self.quarantine_registry.quarantine(
            component_id,
            component_kind,
            severity,
            reason,
            imposed_by,
            now_ms,
            expires_at_ms,
        )?;
        self.audit_chain.append(
            AuditEntryKind::QuarantineAction,
            imposed_by,
            &format!("quarantined {component_id}"),
            component_id,
            now_ms,
        );
        self.compliance_engine.record_quarantine();
        Ok(())
    }

    /// Release a component from quarantine and record in the audit chain.
    pub fn release_component(
        &mut self,
        component_id: &str,
        released_by: &str,
        probation: bool,
        now_ms: u64,
    ) -> Result<(), crate::policy_quarantine::QuarantineError> {
        self.quarantine_registry
            .release(component_id, released_by, probation, now_ms)?;
        let detail = if probation {
            format!("released {component_id} to probation")
        } else {
            format!("released {component_id} from quarantine")
        };
        self.audit_chain.append(
            AuditEntryKind::QuarantineAction,
            released_by,
            &detail,
            component_id,
            now_ms,
        );
        Ok(())
    }

    /// Trip the kill switch and record in the audit chain.
    pub fn trip_kill_switch(
        &mut self,
        level: crate::policy_quarantine::KillSwitchLevel,
        by: &str,
        reason: &str,
        now_ms: u64,
    ) {
        self.quarantine_registry
            .trip_kill_switch(level, by, reason, now_ms);
        self.audit_chain.append(
            AuditEntryKind::KillSwitchAction,
            by,
            &format!("kill switch tripped to {level}: {reason}"),
            "kill_switch",
            now_ms,
        );
        self.compliance_engine.record_kill_switch_trip();
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
    /// use frankenterm_core::policy::{PolicyEngine, PolicyInput, ActionKind, ActorKind, PaneCapabilities};
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
        let decision = self.evaluate_authorization(input, None, None);
        self.record_to_decision_log(input, &decision);
        decision
    }

    /// Authorize a connector credential action using an explicit least-privilege scope.
    pub fn authorize_connector_credential_action(
        &mut self,
        input: &PolicyInput,
        scope: &CredentialScope,
        sensitivity: CredentialSensitivity,
    ) -> PolicyDecision {
        let decision = self.evaluate_authorization(input, Some(scope), Some(sensitivity));
        self.record_to_decision_log(input, &decision);
        decision
    }

    /// Core authorization logic (called by `authorize()`).
    fn evaluate_authorization(
        &mut self,
        input: &PolicyInput,
        credential_scope: Option<&CredentialScope>,
        credential_sensitivity: Option<CredentialSensitivity>,
    ) -> PolicyDecision {
        let mut context = DecisionContext::from_input(input);

        // ---- Quarantine check (earliest gate) ----
        // If the kill switch is active at emergency level, block everything.
        if self.quarantine_registry.kill_switch().is_emergency() {
            context.record_rule(
                "policy.kill_switch",
                true,
                Some("deny"),
                Some("emergency halt active".to_string()),
            );
            context.set_determining_rule("policy.kill_switch");
            return PolicyDecision::deny_with_rule(
                "System kill switch is in emergency halt",
                "policy.kill_switch",
            )
            .with_context(context);
        }

        // If the target pane is quarantined, check blocking semantics.
        if let Some(pane_id) = input.pane_id {
            let component_id = format!("pane-{pane_id}");
            if input.action.is_mutating()
                && self
                    .quarantine_registry
                    .is_blocked_for_writes(&component_id)
            {
                context.record_rule(
                    "policy.quarantine",
                    true,
                    Some("deny"),
                    Some(format!("pane {pane_id} quarantined (writes blocked)")),
                );
                context.set_determining_rule("policy.quarantine");
                return PolicyDecision::deny_with_rule(
                    format!("Pane {pane_id} is quarantined — writes blocked"),
                    "policy.quarantine",
                )
                .with_context(context);
            }
            if self.quarantine_registry.is_blocked_for_all(&component_id) {
                context.record_rule(
                    "policy.quarantine",
                    true,
                    Some("deny"),
                    Some(format!("pane {pane_id} quarantined (all actions blocked)")),
                );
                context.set_determining_rule("policy.quarantine");
                return PolicyDecision::deny_with_rule(
                    format!("Pane {pane_id} is quarantined — all actions blocked"),
                    "policy.quarantine",
                )
                .with_context(context);
            }
        }

        // ---- Revocation check ----
        // If the target connector or credential is revoked, deny immediately.
        if input.action.is_connector_action() {
            if let Some(domain) = &input.domain {
                if self.revocation_registry.is_revoked("connector", domain) {
                    context.record_rule(
                        "policy.revocation",
                        true,
                        Some("deny"),
                        Some(format!("connector '{domain}' has been revoked")),
                    );
                    context.set_determining_rule("policy.revocation");
                    return PolicyDecision::deny_with_rule(
                        format!("Connector '{domain}' has been revoked"),
                        "policy.revocation",
                    )
                    .with_context(context);
                }
            }
        }

        // ---- Namespace isolation check (domain-inferred) ----
        // When namespace isolation is enabled and the actor's namespace is inferred
        // from `domain` (not the explicit `actor_namespace` field), apply boundary
        // checks using raw pane-id lookup.  The explicit `actor_namespace` path is
        // handled by the subsequent block which uses the prefixed resource-id format.
        if self.namespace_isolation_enabled && input.actor_namespace.is_none() {
            if let Some(pane_id) = input.pane_id {
                let actor_ns_opt = input
                    .domain
                    .as_deref()
                    .and_then(crate::namespace_isolation::TenantNamespace::new);
                if let Some(actor_ns) = actor_ns_opt {
                    let target_ns = self.namespace_registry.lookup(
                        crate::namespace_isolation::NamespacedResourceKind::Pane,
                        &pane_id.to_string(),
                    );
                    let boundary = self.namespace_registry.check_boundary(
                        &actor_ns,
                        &target_ns,
                        crate::namespace_isolation::NamespacedResourceKind::Pane,
                    );
                    if boundary.crosses_boundary {
                        if !boundary.is_allowed() {
                            context.record_rule(
                                "policy.namespace_isolation",
                                true,
                                Some("deny"),
                                Some(format!(
                                    "cross-tenant access denied: {} -> {} for pane {pane_id}",
                                    actor_ns, target_ns
                                )),
                            );
                            context.set_determining_rule("policy.namespace_isolation");
                            return PolicyDecision::deny_with_rule(
                                format!(
                                    "Cross-tenant access denied: namespace '{}' cannot access pane {} in namespace '{}'",
                                    actor_ns, pane_id, target_ns
                                ),
                                "policy.namespace_isolation",
                            )
                            .with_context(context);
                        }
                        // Allowed but crosses boundary — record for audit
                        context.record_rule(
                            "policy.namespace_isolation",
                            false,
                            None,
                            Some(format!(
                                "cross-tenant access allowed: {} -> {} ({})",
                                actor_ns,
                                target_ns,
                                boundary.matched_rule.as_deref().unwrap_or("default policy"),
                            )),
                        );
                        context.add_evidence("namespace_boundary", "crossed");
                    } else {
                        context.record_rule(
                            "policy.namespace_isolation",
                            false,
                            None,
                            Some("same-namespace access".to_string()),
                        );
                    }
                }
            }
        }

        // ---- Namespace isolation check (explicit actor_namespace) ----
        if self.namespace_isolation_enabled {
            if let Some(actor_ns) = &input.actor_namespace {
                let (resource_kind, resource_id) = if let Some(pane_id) = input.pane_id {
                    (
                        crate::namespace_isolation::NamespacedResourceKind::Pane,
                        format!("pane-{pane_id}"),
                    )
                } else if input.action.is_connector_action() {
                    (
                        crate::namespace_isolation::NamespacedResourceKind::Connector,
                        input
                            .domain
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string()),
                    )
                } else if input.workflow_id.is_some() {
                    (
                        crate::namespace_isolation::NamespacedResourceKind::Workflow,
                        input
                            .workflow_id
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string()),
                    )
                } else {
                    (
                        crate::namespace_isolation::NamespacedResourceKind::Pane,
                        String::new(),
                    )
                };

                if !resource_id.is_empty() {
                    let target_ns = self.namespace_registry.lookup(resource_kind, &resource_id);
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let boundary = self.namespace_registry.check_and_audit(
                        actor_ns,
                        &target_ns,
                        resource_kind,
                        &resource_id,
                        now_ms,
                    );

                    if boundary.crosses_boundary && !boundary.is_allowed() {
                        context.record_rule(
                            "policy.namespace_isolation",
                            true,
                            Some("deny"),
                            Some(format!(
                                "cross-tenant access denied: {actor_ns} -> {target_ns} for {}:'{resource_id}'",
                                resource_kind.as_str(),
                            )),
                        );
                        context.set_determining_rule("policy.namespace_isolation");
                        self.audit_chain.append(
                            AuditEntryKind::PolicyDecision,
                            &format!("{:?}", input.actor),
                            &format!(
                                "namespace isolation denied: {actor_ns} -> {target_ns} for {}:'{resource_id}'",
                                resource_kind.as_str(),
                            ),
                            "policy.namespace_isolation",
                            now_ms,
                        );
                        self.compliance_engine.record_evaluation(true);
                        return PolicyDecision::deny_with_rule(
                            format!(
                                "Cross-tenant access denied: namespace '{actor_ns}' cannot access resource in namespace '{target_ns}'",
                            ),
                            "policy.namespace_isolation",
                        )
                        .with_context(context);
                    }

                    if boundary.crosses_boundary {
                        context.record_rule(
                            "policy.namespace_isolation",
                            false,
                            None,
                            Some(format!(
                                "cross-tenant access allowed: {actor_ns} -> {target_ns}{}",
                                boundary
                                    .matched_rule
                                    .as_deref()
                                    .map(|r| format!(" (rule: {r})"))
                                    .unwrap_or_default(),
                            )),
                        );
                    } else {
                        context.record_rule(
                            "policy.namespace_isolation",
                            false,
                            None,
                            Some("same-namespace access".to_string()),
                        );
                    }
                }
            }
        }

        // Calculate and attach risk score (wa-upg.6.3)
        let risk = self.calculate_risk(input);
        if risk.score > 0 {
            context.set_risk(risk);
        }

        // ---- Credential broker gate (connector actions) ----
        if self.credential_broker_config.enabled && input.action.is_connector_action() {
            // For connector credential actions, check broker access rules
            if matches!(input.action, ActionKind::ConnectorCredentialAction) {
                let connector_id = input.domain.as_deref().unwrap_or("unknown");
                let Some(scope) = credential_scope else {
                    let reason = "connector credential action missing scope context".to_string();
                    context.record_rule(
                        "policy.credential_broker",
                        true,
                        Some("deny"),
                        Some(reason.clone()),
                    );
                    context.set_determining_rule("policy.credential_broker");
                    self.credential_broker_record_denial(connector_id);
                    return PolicyDecision::deny_with_rule(reason, "policy.credential_broker")
                        .with_context(context);
                };
                let sensitivity = credential_sensitivity.unwrap_or(CredentialSensitivity::Medium);
                if !self
                    .credential_broker
                    .is_authorized(connector_id, scope, sensitivity)
                {
                    let reason = format!(
                        "connector '{connector_id}' not authorized for scope {}:{} at sensitivity {sensitivity}",
                        scope.provider, scope.resource
                    );
                    context.record_rule(
                        "policy.credential_broker",
                        true,
                        Some("deny"),
                        Some(reason.clone()),
                    );
                    context.set_determining_rule("policy.credential_broker");
                    self.credential_broker_record_denial(connector_id);
                    return PolicyDecision::deny_with_rule(reason, "policy.credential_broker")
                        .with_context(context);
                }
                if sensitivity > self.credential_broker_config.max_sensitivity {
                    let reason = format!(
                        "credential sensitivity {sensitivity} exceeds configured ceiling {}",
                        self.credential_broker_config.max_sensitivity
                    );
                    context.record_rule(
                        "policy.credential_broker",
                        true,
                        Some("require_approval"),
                        Some(reason.clone()),
                    );
                    context.set_determining_rule("policy.credential_broker");
                    return PolicyDecision::require_approval_with_rule(
                        reason,
                        "policy.credential_broker",
                    )
                    .with_context(context);
                }
                context.record_rule(
                    "policy.credential_broker",
                    false,
                    None,
                    Some(format!(
                        "credential access authorized for {}:{} at sensitivity {sensitivity}",
                        scope.provider, scope.resource
                    )),
                );
            }
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
            let is_owner = matches!(
                (&input.capabilities.reserved_by, &input.workflow_id),
                (Some(reserved_by), Some(workflow_id)) if reserved_by == workflow_id
            );
            if is_owner {
                // Owning workflow: record and fall through to command safety gate
                context.record_rule(
                    "policy.pane_reserved",
                    false,
                    None,
                    Some("reserved by same workflow".to_string()),
                );
            } else {
                // Non-owner: deny
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
                if let Some(trauma_decision) = input.trauma_decision.as_ref() {
                    if !self.trauma_guard_enabled {
                        context.record_rule(
                            "policy.trauma_guard",
                            false,
                            None,
                            Some("trauma guard disabled by config".to_string()),
                        );
                    } else if trauma_decision.should_intervene {
                        if has_trauma_bypass_prefix(text) {
                            context.record_rule(
                                "policy.trauma_guard",
                                false,
                                None,
                                Some("bypass requested via FT_BYPASS_TRAUMA=1".to_string()),
                            );
                        } else {
                            let reason = trauma_decision
                                .reason_code
                                .as_deref()
                                .map_or_else(
                                    || {
                                        format!(
                                            "Trauma Guard blocked command after {} repeated failures",
                                            trauma_decision.repeat_count
                                        )
                                    },
                                    |reason_code| {
                                        format!(
                                            "Trauma Guard blocked command ({reason_code}) after {} repeated failures. Prefix with FT_BYPASS_TRAUMA=1 to override once.",
                                            trauma_decision.repeat_count
                                        )
                                    },
                                );
                            let rule_id = "policy.trauma_guard.loop_block".to_string();
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
                    } else {
                        context.record_rule(
                            "policy.trauma_guard",
                            false,
                            None,
                            Some("trauma guard allow".to_string()),
                        );
                    }
                } else {
                    context.record_rule(
                        "policy.trauma_guard",
                        false,
                        None,
                        Some("no trauma decision".to_string()),
                    );
                }

                if input.actor != ActorKind::Human && crate::build_coord::requires_rch_offload(text)
                {
                    let prefix = crate::build_coord::recommended_rch_prefix();
                    let reason = format!(
                        "Heavy cargo commands from {} actions must be routed through `{prefix}` to avoid local contention",
                        input.actor.as_str()
                    );
                    context.record_rule(
                        RCH_HEAVY_COMPUTE_RULE_ID,
                        true,
                        Some("require_approval"),
                        Some(reason.clone()),
                    );
                    context.set_determining_rule(RCH_HEAVY_COMPUTE_RULE_ID);
                    context.add_evidence("rch_required", "true");
                    context.add_evidence("rch_recommended_prefix", prefix);
                    return PolicyDecision::require_approval_with_rule(
                        reason,
                        RCH_HEAVY_COMPUTE_RULE_ID,
                    )
                    .with_context(context);
                }

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
        let selected_rule_id = rule_result
            .matching_rule
            .as_ref()
            .map(|rule| rule.id.as_str());
        for rule in &self.policy_rules.rules {
            let matched = rule_result
                .matched_rule_ids
                .iter()
                .any(|matched_id| matched_id == &rule.id);

            let qualified_rule_id = format!("config.rule.{}", rule.id);
            if matched {
                let selected = selected_rule_id == Some(rule.id.as_str());
                context.record_rule(
                    qualified_rule_id,
                    true,
                    Some(rule.decision.as_str()),
                    Some(config_rule_trace_reason(rule, selected, selected_rule_id)),
                );
            } else {
                context.record_rule(
                    qualified_rule_id,
                    false,
                    None,
                    Some("rule checked".to_string()),
                );
            }
        }

        if let (Some(rule), Some(decision)) = (rule_result.matching_rule, rule_result.decision) {
            let rule_id = format!("config.rule.{}", rule.id);
            let reason = rule
                .message
                .clone()
                .unwrap_or_else(|| format!("Rule '{}' matched", rule.id));

            match decision {
                PolicyRuleDecision::Deny => {
                    context.set_determining_rule(&rule_id);
                    return PolicyDecision::deny_with_rule(reason, rule_id).with_context(context);
                }
                PolicyRuleDecision::RequireApproval => {
                    context.set_determining_rule(&rule_id);
                    return PolicyDecision::require_approval_with_rule(reason, rule_id)
                        .with_context(context);
                }
                PolicyRuleDecision::Allow => {
                    // Allow rules short-circuit to allow (skipping default checks)
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

    /// Record a policy decision to the append-only decision log.
    fn record_to_decision_log(&mut self, input: &PolicyInput, decision: &PolicyDecision) {
        let outcome = match decision {
            PolicyDecision::Allow { .. } => DecisionOutcome::Allow,
            PolicyDecision::Deny { .. } => DecisionOutcome::Deny,
            PolicyDecision::RequireApproval { .. } => DecisionOutcome::RequireApproval,
        };
        let rules_evaluated = decision
            .context()
            .map(|ctx| ctx.rules_evaluated.len() as u32)
            .unwrap_or(0);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.decision_log.record(
            ts,
            input.action,
            input.actor,
            input.surface,
            input.pane_id,
            outcome,
            decision.rule_id().map(String::from),
            decision.reason().map(String::from),
            rules_evaluated,
        );

        // Record to tamper-evident audit chain (skip allows unless configured)
        let should_record = match decision {
            PolicyDecision::Allow { .. } => self.audit_chain.records_allows(),
            PolicyDecision::Deny { .. } | PolicyDecision::RequireApproval { .. } => true,
        };
        if should_record {
            let description = format!(
                "{}: {} by {:?} on {:?}",
                decision.as_str(),
                input.action.as_str(),
                input.actor,
                input.surface,
            );
            let entity_ref = decision.rule_id().unwrap_or("policy.authorize");
            self.audit_chain.append(
                AuditEntryKind::PolicyDecision,
                &format!("{:?}", input.actor),
                &description,
                entity_ref,
                ts,
            );
        }

        // Feed compliance engine with evaluation counts
        self.compliance_engine
            .record_evaluation(decision.is_denied());
        self.compliance_engine
            .update_subsystem_eval(&format!("{:?}", input.surface), ts);
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
            .with_surface(PolicySurface::Mux)
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

    /// Capture a unified telemetry snapshot from all subsystems.
    ///
    /// Aggregates snapshots from decision log, quarantine, audit chain,
    /// compliance, credential broker, connector governor, registry,
    /// reliability, bundles, mesh, ingestion, namespace, approvals,
    /// and revocations into a single [`PolicyEngineTelemetrySnapshot`].
    ///
    /// Some subsystem snapshots require `&mut self` (compliance, governor)
    /// because they update internal counters during snapshot capture.
    pub fn telemetry_snapshot(&mut self, now_ms: u64) -> PolicyEngineTelemetrySnapshot {
        PolicyEngineTelemetrySnapshot {
            captured_at_ms: now_ms,
            decision_log: self.decision_log.snapshot(),
            quarantine: self.quarantine_registry.telemetry_snapshot(now_ms),
            audit_chain: self.audit_chain.telemetry_snapshot(now_ms),
            compliance: self.compliance_engine.snapshot(now_ms),
            credential_broker: self.credential_broker.telemetry_snapshot(now_ms),
            connector_governor: self.connector_governor.snapshot(now_ms),
            connector_registry: self.connector_registry.telemetry().snapshot(),
            connector_reliability: self.reliability_registry.all_snapshots(),
            bundle_registry: self.bundle_registry.snapshot(now_ms),
            connector_mesh: self.connector_mesh.telemetry().snapshot(),
            ingestion_pipeline: self.ingestion_pipeline.snapshot(now_ms),
            namespace_registry: self.namespace_registry.snapshot(),
            approval_tracker: self.approval_tracker.snapshot(),
            revocation_registry: self.revocation_registry.snapshot(),
            namespace_isolation_enabled: self.namespace_isolation_enabled,
        }
    }
}

const AUDIT_PREVIEW_CHARS: usize = 80;

/// Redacted summary metadata for SendText audit entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        /// The policy decision that authorized the attempted injection
        decision: PolicyDecision,
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
                    .and_then(|ctx| serde_json::to_string(ctx)
                        .inspect_err(|e| tracing::warn!(error = %e, "policy decision_context serialization failed"))
                        .ok()),
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
                    .and_then(|ctx| serde_json::to_string(ctx)
                        .inspect_err(|e| tracing::warn!(error = %e, "policy decision_context serialization failed"))
                        .ok()),
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
                    .and_then(|ctx| serde_json::to_string(ctx)
                        .inspect_err(|e| tracing::warn!(error = %e, "policy decision_context serialization failed"))
                        .ok()),
                result: "require_approval".to_string(),
            },
            Self::Error {
                decision,
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
                policy_decision: decision.as_str().to_string(),
                decision_reason: decision.reason().map(String::from),
                rule_id: decision.rule_id().map(String::from),
                input_summary: None,
                verification_summary: Some(error.clone()),
                decision_context: decision
                    .context()
                    .and_then(|ctx| serde_json::to_string(ctx)
                        .inspect_err(|e| tracing::warn!(error = %e, "policy decision_context serialization failed"))
                        .ok()),
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
/// ```no_run
/// use frankenterm_core::policy::{
///     PolicyGatedInjector, PolicyEngine, ActorKind,
///     PaneCapabilities, InjectionResult,
/// };
/// use frankenterm_core::wezterm::WeztermClient;
///
/// # async fn example() {
/// let engine = PolicyEngine::permissive();
/// let client = WeztermClient::new();
/// let mut injector = PolicyGatedInjector::new(engine, client);
///
/// // Capabilities are derived from current pane state
/// let caps = PaneCapabilities::prompt();
/// let result = injector.send_text(1, "ls -la", ActorKind::Robot, &caps, None).await;
///
/// match result {
///     InjectionResult::Allowed { .. } => println!("Sent successfully"),
///     InjectionResult::Denied { decision, .. } => println!("Denied: {:?}", decision),
///     InjectionResult::RequiresApproval { decision, .. } => println!("Needs approval"),
///     InjectionResult::Error { error, .. } => println!("Error: {}", error),
/// }
/// # }
/// ```
pub struct PolicyGatedInjector<C = crate::wezterm::WeztermClient> {
    engine: PolicyEngine,
    client: C,
    /// Optional storage handle for audit trail emission
    storage: Option<crate::storage::StorageHandle>,
    /// Optional ingress tap for flight recorder capture (ft-oegrb.2.2)
    ingress_tap: Option<crate::recording::SharedIngressTap>,
    /// Optional replay capture adapter for policy decision provenance.
    decision_capture: Option<crate::replay_capture::SharedCaptureAdapter>,
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
            ingress_tap: None,
            decision_capture: None,
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
            ingress_tap: None,
            decision_capture: None,
        }
    }

    /// Set the storage handle for audit trail emission
    pub fn set_storage(&mut self, storage: crate::storage::StorageHandle) {
        self.storage = Some(storage);
    }

    /// Set the ingress tap for flight recorder capture.
    pub fn set_ingress_tap(&mut self, tap: crate::recording::SharedIngressTap) {
        self.ingress_tap = Some(tap);
    }

    /// Set the replay capture adapter for policy decision provenance.
    pub fn set_decision_capture(&mut self, capture: crate::replay_capture::SharedCaptureAdapter) {
        self.decision_capture = Some(capture);
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

    /// Inject a synthetic Trauma Guard feedback line into the pane when the
    /// deny decision originated from the trauma loop interceptor.
    async fn maybe_inject_trauma_feedback(&self, pane_id: u64, decision: &PolicyDecision) {
        let Some(feedback) = trauma_feedback_comment(decision) else {
            return;
        };

        if let Err(error) = self
            .client
            .send_text_with_options(pane_id, &feedback, true, false)
            .await
        {
            tracing::warn!(
                pane_id,
                rule_id = ?decision.rule_id(),
                error = %error,
                "Failed to inject synthetic trauma guard feedback"
            );
        }
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
        let tap_summary = if self.ingress_tap.is_some() {
            Some(summary.clone())
        } else {
            None
        };

        // Build policy input
        let mut input = PolicyInput::new(action, actor)
            .with_surface(PolicySurface::Mux)
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

        if let Some(adapter) = self.decision_capture.as_ref() {
            let input_text = serde_json::to_string(&input).unwrap_or_else(|_| {
                format!(
                    "action={};pane_id={};actor={}",
                    action.as_str(),
                    pane_id,
                    actor.as_str()
                )
            });
            let output = serde_json::to_value(&decision).unwrap_or_else(|_| {
                serde_json::json!({
                    "decision": decision.as_str(),
                    "rule_id": decision.rule_id(),
                })
            });
            let decision_event = crate::replay_capture::DecisionEvent::new(
                crate::replay_capture::DecisionType::PolicyEvaluation,
                pane_id,
                decision.rule_id().unwrap_or("policy.default_allow"),
                &decision_definition_text(&decision),
                &input_text,
                output,
                workflow_id.map(|id| format!("workflow_execution:{id}")),
                None,
                crate::recording::epoch_ms_now(),
            );
            adapter.capture_decision(
                crate::recording::actor_to_source(actor),
                workflow_id.map(String::from),
                decision_event,
            );
        }

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
                        decision,
                        error: e.to_string(),
                        pane_id,
                        action,
                        audit_action_id: None,
                    },
                }
            }
            PolicyDecision::Deny { .. } => {
                self.maybe_inject_trauma_feedback(pane_id, &decision).await;
                InjectionResult::Denied {
                    decision,
                    summary,
                    pane_id,
                    action,
                    audit_action_id: None,
                }
            }
            PolicyDecision::RequireApproval { .. } => InjectionResult::RequiresApproval {
                decision,
                summary,
                pane_id,
                action,
                audit_action_id: None,
            },
        };

        // Notify ingress tap (ft-oegrb.2.2)
        if let Some(ref tap) = self.ingress_tap {
            use crate::recording::{
                IngressEvent, IngressOutcome, action_to_ingress_kind, actor_to_source, epoch_ms_now,
            };
            let outcome = match &result {
                InjectionResult::Allowed { .. } => IngressOutcome::Allowed,
                InjectionResult::Denied { decision, .. } => IngressOutcome::Denied {
                    reason: format!("{decision:?}"),
                },
                InjectionResult::RequiresApproval { .. } => IngressOutcome::RequiresApproval,
                InjectionResult::Error { error, .. } => IngressOutcome::Error {
                    error: error.clone(),
                },
            };
            if let Some(text) = tap_summary {
                tap.on_ingress(IngressEvent {
                    pane_id,
                    text,
                    source: actor_to_source(actor),
                    ingress_kind: action_to_ingress_kind(action, actor),
                    redaction: crate::recording::RecorderRedactionLevel::Partial,
                    occurred_at_ms: epoch_ms_now(),
                    outcome,
                    workflow_id: workflow_id.map(String::from),
                });
            }
        }

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

fn decision_definition_text(decision: &PolicyDecision) -> String {
    let mut segments = Vec::new();
    if let Some(rule_id) = decision.rule_id() {
        segments.push(format!("rule_id={rule_id}"));
    } else {
        segments.push("rule_id=policy.default_allow".to_string());
    }
    segments.push(format!("decision={}", decision.as_str()));
    if let Some(ctx) = decision.context() {
        if let Some(determining) = &ctx.determining_rule {
            segments.push(format!("determining_rule={determining}"));
        }
        let matched_rules: Vec<&str> = ctx
            .rules_evaluated
            .iter()
            .filter(|rule| rule.matched)
            .map(|rule| rule.rule_id.as_str())
            .collect();
        if !matched_rules.is_empty() {
            segments.push(format!("matched={}", matched_rules.join(",")));
        }
    }
    segments.join("|")
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

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;

        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build policy test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

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
        assert!(is_command_candidate("FOO=bar;rm -rf /tmp"));
        assert!(!is_command_candidate("Please check the logs"));
        assert!(!is_command_candidate("# commented command"));
    }

    #[test]
    fn trauma_bypass_prefix_detection() {
        assert!(has_trauma_bypass_prefix("FT_BYPASS_TRAUMA=1 cargo test"));
        assert!(has_trauma_bypass_prefix(
            "FOO=bar FT_BYPASS_TRAUMA=1 cargo test -p core"
        ));
        assert!(has_trauma_bypass_prefix(
            "FOO=\"with spaces\" FT_BYPASS_TRAUMA=1 cargo test"
        ));
        assert!(has_trauma_bypass_prefix(
            "FT_BYPASS_TRAUMA=\"1\" cargo test"
        ));
        assert!(!has_trauma_bypass_prefix("FT_BYPASS_TRAUMA=0 cargo test"));
        assert!(!has_trauma_bypass_prefix("cargo test FT_BYPASS_TRAUMA=1"));
    }

    #[test]
    fn trauma_feedback_comment_only_for_loop_block_rule() {
        let trauma = PolicyDecision::deny_with_rule(
            "Trauma Guard blocked command (recurring_failure_loop)",
            TRAUMA_LOOP_BLOCK_RULE_ID,
        );
        let non_trauma = PolicyDecision::deny_with_rule("alt screen active", "policy.alt_screen");

        let comment = trauma_feedback_comment(&trauma);
        assert!(comment.is_some(), "expected trauma feedback comment");
        let comment = comment.unwrap_or_default();
        assert!(comment.contains(TRAUMA_FEEDBACK_PREFIX));
        assert!(comment.contains("FT_BYPASS_TRAUMA=1"));

        assert!(
            trauma_feedback_comment(&non_trauma).is_none(),
            "non-trauma deny should not emit trauma feedback"
        );
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
        assert_eq!(decision.rule_id(), Some("dcg.core.git:reset-hard"));
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
    fn command_gate_trauma_blocks_without_bypass() {
        let mut engine = PolicyEngine::permissive();
        let trauma = TraumaDecision {
            should_intervene: true,
            reason_code: Some("recurring_failure_loop".to_string()),
            command_hash: 42,
            repeat_count: 3,
            recurring_signatures: vec!["core.codex:error_loop".to_string()],
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("cargo test -p core")
            .with_trauma_decision(trauma);

        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("policy.trauma_guard.loop_block"));
    }

    #[test]
    fn command_gate_trauma_disabled_skips_trauma_block() {
        let mut engine = PolicyEngine::permissive().with_trauma_guard_enabled(false);
        let trauma = TraumaDecision {
            should_intervene: true,
            reason_code: Some("recurring_failure_loop".to_string()),
            command_hash: 42,
            repeat_count: 3,
            recurring_signatures: vec!["core.codex:error_loop".to_string()],
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("git status")
            .with_trauma_decision(trauma);

        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
        assert_ne!(decision.rule_id(), Some("policy.trauma_guard.loop_block"));
    }

    #[test]
    fn command_gate_trauma_bypass_allows_command_gate_path() {
        let mut engine = PolicyEngine::permissive();
        let trauma = TraumaDecision {
            should_intervene: true,
            reason_code: Some("recurring_failure_loop".to_string()),
            command_hash: 42,
            repeat_count: 3,
            recurring_signatures: vec!["core.codex:error_loop".to_string()],
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("FT_BYPASS_TRAUMA=1 git status")
            .with_trauma_decision(trauma);

        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn robot_heavy_cargo_without_rch_requires_approval() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("cargo test -p frankenterm-core -- --nocapture");

        let decision = engine.authorize(&input);
        assert!(decision.requires_approval());
        assert_eq!(decision.rule_id(), Some(RCH_HEAVY_COMPUTE_RULE_ID));
        assert!(
            decision
                .reason()
                .is_some_and(|reason| reason.contains("rch exec"))
        );
    }

    #[test]
    fn robot_heavy_cargo_with_rch_prefix_is_allowed() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("TMPDIR=/tmp rch exec -- cargo check --help");

        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn robot_light_cargo_without_rch_is_allowed() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("cargo fmt --check");

        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn human_heavy_cargo_without_rch_is_not_forced_through_rch_policy() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Human)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("cargo build --workspace");

        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
        assert_ne!(decision.rule_id(), Some(RCH_HEAVY_COMPUTE_RULE_ID));
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

    #[test]
    fn command_gate_native_blocks_destructive() {
        let config = CommandGateConfig {
            enabled: true,
            dcg_mode: DcgMode::Native,
            dcg_deny_policy: DcgDenyPolicy::Deny,
        };
        // docker system prune is only in native guard packs, not built-in rules
        let outcome = evaluate_command_gate_with_runner("docker system prune -af", &config, |_| {
            panic!("Native mode should not call external dcg runner");
        });
        match outcome {
            CommandGateOutcome::Deny { rule_id, .. } => {
                assert!(
                    rule_id.starts_with("dcg."),
                    "expected dcg. prefix: {rule_id}"
                );
            }
            _ => panic!("Expected deny for docker system prune in native mode"),
        }
    }

    #[test]
    fn command_gate_native_allows_safe() {
        let config = CommandGateConfig {
            enabled: true,
            dcg_mode: DcgMode::Native,
            dcg_deny_policy: DcgDenyPolicy::Deny,
        };
        let outcome = evaluate_command_gate_with_runner("git status", &config, |_| {
            panic!("Native mode should not call external dcg runner");
        });
        assert!(matches!(outcome, CommandGateOutcome::Allow));
    }

    #[test]
    fn injector_emits_policy_decision_to_replay_capture() {
        run_async_test(async {
            let sink = std::sync::Arc::new(crate::replay_capture::CollectingCaptureSink::new());
            let adapter = std::sync::Arc::new(crate::replay_capture::CaptureAdapter::new(
                sink.clone(),
                crate::replay_capture::CaptureConfig::default(),
            ));

            let mut injector = PolicyGatedInjector::new(
                PolicyEngine::strict(),
                crate::wezterm::default_wezterm_handle(),
            );
            injector.set_decision_capture(adapter);

            let mut caps = PaneCapabilities::prompt();
            caps.alt_screen = Some(true);

            let result = injector
                .send_text(1, "echo hi", ActorKind::Robot, &caps, None)
                .await;
            assert!(
                matches!(result, InjectionResult::Denied { .. }),
                "expected deny when alt_screen is active"
            );

            let events = sink.recorder_events();
            assert_eq!(events.len(), 1);
            match &events[0].payload {
                crate::recording::RecorderEventPayload::ControlMarker { details, .. } => {
                    assert_eq!(details["decision_type"], "PolicyEvaluation");
                    assert_eq!(details["rule_id"], "policy.alt_screen");
                }
                other => panic!("expected control marker, got {other:?}"),
            }
        });
    }

    #[test]
    fn injector_policy_context_marks_mux_surface() {
        run_async_test(async {
            let actors = [
                ActorKind::Human,
                ActorKind::Robot,
                ActorKind::Mcp,
                ActorKind::Workflow,
            ];

            for actor in actors {
                let mut injector = PolicyGatedInjector::new(
                    PolicyEngine::strict(),
                    crate::wezterm::default_wezterm_handle(),
                );

                let mut caps = PaneCapabilities::prompt();
                caps.alt_screen = Some(true);

                let result = injector.send_text(1, "echo hi", actor, &caps, None).await;

                match result {
                    InjectionResult::Denied { decision, .. } => {
                        let context = decision
                            .context()
                            .expect("injector deny decision should include context");
                        assert_eq!(
                            context.surface,
                            PolicySurface::Mux,
                            "expected mux surface for actor {}",
                            actor.as_str()
                        );
                    }
                    other => panic!(
                        "expected denied result for actor {}, got {other:?}",
                        actor.as_str()
                    ),
                }
            }
        });
    }

    #[test]
    fn e2e_trauma_guard_deny_injects_synthetic_feedback() {
        run_async_test(async {
            let injector = PolicyGatedInjector::permissive(crate::wezterm::MockWezterm::new());
            let _pane = injector.client.add_default_pane(1).await;
            let decision = PolicyDecision::deny_with_rule(
                "Trauma Guard blocked command (recurring_failure_loop)",
                TRAUMA_LOOP_BLOCK_RULE_ID,
            );

            injector.maybe_inject_trauma_feedback(1, &decision).await;

            let pane = injector
                .client
                .pane_state(1)
                .await
                .expect("pane 1 should exist");
            assert!(
                pane.content.contains(TRAUMA_FEEDBACK_PREFIX),
                "synthetic trauma feedback should be injected into pane content"
            );
            assert!(
                pane.content.contains("EXECUTION BLOCKED"),
                "feedback should include explicit block wording"
            );
        });
    }

    #[test]
    fn e2e_non_trauma_deny_does_not_inject_synthetic_feedback() {
        run_async_test(async {
            let injector = PolicyGatedInjector::permissive(crate::wezterm::MockWezterm::new());
            let _pane = injector.client.add_default_pane(1).await;
            let decision = PolicyDecision::deny_with_rule("alt screen active", "policy.alt_screen");

            injector.maybe_inject_trauma_feedback(1, &decision).await;

            let pane = injector
                .client
                .pane_state(1)
                .await
                .expect("pane 1 should exist");
            assert!(
                pane.content.is_empty(),
                "non-trauma deny should not inject trauma feedback"
            );
        });
    }

    // ========================================================================
    // ActionKind Tests
    // ========================================================================

    #[test]
    fn action_kind_mutating() {
        assert!(ActionKind::SendText.is_mutating());
        assert!(ActionKind::SendCtrlC.is_mutating());
        assert!(ActionKind::Close.is_mutating());
        assert!(!ActionKind::ConnectorNotify.is_mutating());
        assert!(!ActionKind::ReadOutput.is_mutating());
        assert!(!ActionKind::SearchOutput.is_mutating());
    }

    #[test]
    fn action_kind_destructive() {
        assert!(ActionKind::Close.is_destructive());
        assert!(ActionKind::DeleteFile.is_destructive());
        assert!(ActionKind::SendCtrlC.is_destructive());
        assert!(ActionKind::ConnectorCredentialAction.is_destructive());
        assert!(!ActionKind::SendText.is_destructive());
        assert!(!ActionKind::ReadOutput.is_destructive());
    }

    #[test]
    fn action_kind_rate_limited() {
        assert!(ActionKind::SendText.is_rate_limited());
        assert!(ActionKind::WorkflowRun.is_rate_limited());
        assert!(ActionKind::ConnectorNotify.is_rate_limited());
        assert!(!ActionKind::ReadOutput.is_rate_limited());
        assert!(!ActionKind::SearchOutput.is_rate_limited());
    }

    #[test]
    fn action_kind_stable_strings() {
        assert_eq!(ActionKind::SendText.as_str(), "send_text");
        assert_eq!(ActionKind::SendCtrlC.as_str(), "send_ctrl_c");
        assert_eq!(ActionKind::WorkflowRun.as_str(), "workflow_run");
        assert_eq!(ActionKind::ConnectorNotify.as_str(), "connector_notify");
        assert_eq!(
            ActionKind::ConnectorCredentialAction.as_str(),
            "connector_credential_action"
        );
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
            command: "ft approve ABCD1234".to_string(),
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
            command: "ft approve ABCD1234".to_string(),
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
            command: "ft approve ABCD1234".to_string(),
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
        let mut context = DecisionContext::empty();
        context.action = ActionKind::SendText;
        context.actor = ActorKind::Human;
        context.surface = PolicySurface::Mux;
        context.pane_id = Some(99);
        context.record_rule(
            "policy.test.send_mux",
            true,
            Some("allow"),
            Some("Mux send permitted".to_string()),
        );
        context.set_determining_rule("policy.test.send_mux");

        let result = InjectionResult::Error {
            decision: PolicyDecision::allow_with_rule("policy.test.send_mux").with_context(context),
            error: "WezTerm connection failed".to_string(),
            pane_id: 99,
            action: ActionKind::SendText,
            audit_action_id: None,
        };

        let record = result.to_audit_record(ActorKind::Human, None, None);

        assert_eq!(record.actor_kind, "human");
        assert_eq!(record.pane_id, Some(99));
        assert_eq!(record.policy_decision, "allow");
        assert_eq!(record.rule_id, Some("policy.test.send_mux".to_string()));
        assert!(record.input_summary.is_none());
        assert_eq!(
            record.verification_summary,
            Some("WezTerm connection failed".to_string())
        );
        let context_json = record
            .decision_context
            .as_deref()
            .expect("error audit should preserve decision context");
        assert!(context_json.contains("\"surface\":\"mux\""));
        assert!(context_json.contains("\"determining_rule\":\"policy.test.send_mux\""));
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
    fn check_send_compatibility_path_uses_mux_surface() {
        let mut engine = PolicyEngine::strict();
        #[allow(deprecated)]
        let decision = engine.check_send(7, false);
        let context = decision
            .context()
            .expect("compatibility path should capture decision context");
        assert_eq!(context.surface, PolicySurface::Mux);
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
            decision: PolicyDecision::allow(),
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
    fn evaluate_policy_rules_reports_all_matches_before_tie_breaking() {
        let rules = PolicyRulesConfig {
            enabled: true,
            rules: vec![
                PolicyRule {
                    id: "allow-robot-mux".to_string(),
                    description: None,
                    priority: 100,
                    match_on: PolicyRuleMatch {
                        actors: vec!["robot".to_string()],
                        surfaces: vec!["mux".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Allow,
                    message: Some("robot mux reads allowed".to_string()),
                },
                PolicyRule {
                    id: "deny-robot".to_string(),
                    description: None,
                    priority: 10,
                    match_on: PolicyRuleMatch {
                        actors: vec!["robot".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Deny,
                    message: Some("robot actions denied".to_string()),
                },
                PolicyRule {
                    id: "require-mcp".to_string(),
                    description: None,
                    priority: 1,
                    match_on: PolicyRuleMatch {
                        actors: vec!["mcp".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::RequireApproval,
                    message: Some("mcp approval required".to_string()),
                },
            ],
        };

        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_surface(PolicySurface::Mux)
            .with_pane(9);

        let result = evaluate_policy_rules(&rules, &input);
        assert_eq!(
            result.rules_checked,
            vec![
                "allow-robot-mux".to_string(),
                "deny-robot".to_string(),
                "require-mcp".to_string(),
            ]
        );
        assert_eq!(
            result.matched_rule_ids,
            vec!["allow-robot-mux".to_string(), "deny-robot".to_string()]
        );
        assert_eq!(
            result.matching_rule.as_ref().map(|rule| rule.id.as_str()),
            Some("deny-robot")
        );
        assert_eq!(result.decision, Some(PolicyRuleDecision::Deny));
    }

    #[test]
    fn policy_rules_trace_distinguishes_checked_from_matched_candidates() {
        let rules = PolicyRulesConfig {
            enabled: true,
            rules: vec![
                PolicyRule {
                    id: "allow-robot-mux".to_string(),
                    description: None,
                    priority: 100,
                    match_on: PolicyRuleMatch {
                        actors: vec!["robot".to_string()],
                        surfaces: vec!["mux".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Allow,
                    message: Some("robot mux reads allowed".to_string()),
                },
                PolicyRule {
                    id: "deny-robot".to_string(),
                    description: None,
                    priority: 10,
                    match_on: PolicyRuleMatch {
                        actors: vec!["robot".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Deny,
                    message: Some("robot actions denied".to_string()),
                },
                PolicyRule {
                    id: "require-mcp".to_string(),
                    description: None,
                    priority: 1,
                    match_on: PolicyRuleMatch {
                        actors: vec!["mcp".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::RequireApproval,
                    message: Some("mcp approval required".to_string()),
                },
            ],
        };

        let mut engine = PolicyEngine::permissive().with_policy_rules(rules);
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_surface(PolicySurface::Mux)
            .with_pane(9);

        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("config.rule.deny-robot"));

        let context = decision
            .context()
            .expect("decision context should be present");
        let config_rules: Vec<_> = context
            .rules_evaluated
            .iter()
            .filter(|rule| rule.rule_id.starts_with("config.rule."))
            .collect();
        assert_eq!(
            config_rules.len(),
            3,
            "each config rule should be recorded once"
        );
        assert_eq!(
            context.determining_rule.as_deref(),
            Some("config.rule.deny-robot")
        );

        let allow_rule = config_rules
            .iter()
            .find(|rule| rule.rule_id == "config.rule.allow-robot-mux")
            .expect("allow rule trace should be present");
        assert!(allow_rule.matched);
        assert_eq!(allow_rule.decision.as_deref(), Some("allow"));
        assert!(
            allow_rule
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("won tie-breaking"))
        );

        let deny_rule = config_rules
            .iter()
            .find(|rule| rule.rule_id == "config.rule.deny-robot")
            .expect("deny rule trace should be present");
        assert!(deny_rule.matched);
        assert_eq!(deny_rule.decision.as_deref(), Some("deny"));
        assert!(
            deny_rule
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("matched and selected"))
        );

        let require_rule = config_rules
            .iter()
            .find(|rule| rule.rule_id == "config.rule.require-mcp")
            .expect("non-matching rule trace should be present");
        assert!(!require_rule.matched);
        assert_eq!(require_rule.decision, None);
        assert_eq!(require_rule.reason.as_deref(), Some("rule checked"));
    }

    #[test]
    fn policy_rules_dsl_bridge_preserves_selected_rule_and_matches() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![
                PolicyRule {
                    id: "allow-robot".to_string(),
                    description: None,
                    priority: 100,
                    match_on: PolicyRuleMatch {
                        actors: vec!["robot".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Allow,
                    message: None,
                },
                PolicyRule {
                    id: "deny-critical-delete".to_string(),
                    description: None,
                    priority: 50,
                    match_on: PolicyRuleMatch {
                        actions: vec!["delete_file".to_string()],
                        pane_titles: vec!["*critical*".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Deny,
                    message: Some("critical deletes denied".to_string()),
                },
                PolicyRule {
                    id: "require-mcp".to_string(),
                    description: None,
                    priority: 75,
                    match_on: PolicyRuleMatch {
                        actors: vec!["mcp".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::RequireApproval,
                    message: None,
                },
            ],
        };
        let input = PolicyInput::new(ActionKind::DeleteFile, ActorKind::Robot)
            .with_pane_title("critical maintenance")
            .with_surface(PolicySurface::Mux);

        let result = evaluate_policy_rules(&config, &input);
        let dsl_rules = config
            .rules
            .iter()
            .map(crate::policy_dsl::compile_policy_rule)
            .collect::<Vec<_>>();
        let dsl_result = crate::policy_dsl::evaluate_dsl_rules(&dsl_rules, &input);
        let dsl_matched_ids = dsl_result
            .evaluations
            .iter()
            .filter(|evaluation| evaluation.matched)
            .map(|evaluation| evaluation.rule_id.clone())
            .collect::<Vec<_>>();

        assert_eq!(result.matched_rule_ids, dsl_matched_ids);
        assert_eq!(
            result.matching_rule.as_ref().map(|rule| rule.id.as_str()),
            dsl_result
                .matched_rule
                .as_ref()
                .map(|matched| matched.rule_id.as_str())
        );
        assert_eq!(result.decision, Some(PolicyRuleDecision::Deny));
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
            surfaces: vec!["connector".to_string()],
            pane_ids: vec![42],
            command_patterns: vec!["rm.*".to_string()],
            ..Default::default()
        };
        assert_eq!(multi_criteria.specificity(), 7); // 1 + 1 + 1 + 2 + 2
    }

    #[test]
    fn policy_rule_decision_priority() {
        assert_eq!(PolicyRuleDecision::Deny.priority(), 0);
        assert_eq!(PolicyRuleDecision::RequireApproval.priority(), 1);
        assert_eq!(PolicyRuleDecision::Allow.priority(), 2);
    }

    #[test]
    fn policy_rules_match_on_surface() {
        let rules = PolicyRulesConfig {
            enabled: true,
            rules: vec![
                PolicyRule {
                    id: "rule.robot.surface".to_string(),
                    description: None,
                    priority: 100,
                    match_on: PolicyRuleMatch {
                        actions: vec!["read_output".to_string()],
                        actors: vec!["robot".to_string()],
                        surfaces: vec!["robot".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Allow,
                    message: Some("robot surface allowed".to_string()),
                },
                PolicyRule {
                    id: "rule.connector.surface".to_string(),
                    description: None,
                    priority: 10,
                    match_on: PolicyRuleMatch {
                        actions: vec!["read_output".to_string()],
                        actors: vec!["robot".to_string()],
                        surfaces: vec!["connector".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Deny,
                    message: Some("connector surface blocked".to_string()),
                },
            ],
        };

        let mut engine = PolicyEngine::permissive().with_policy_rules(rules);
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_surface(PolicySurface::Connector)
            .with_pane(7);

        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(
            decision.rule_id(),
            Some("config.rule.rule.connector.surface")
        );
        let context = decision
            .context()
            .expect("decision context should be present");
        assert_eq!(context.surface, PolicySurface::Connector);
    }

    #[test]
    fn policy_rules_match_exec_command_surface_case_insensitive() {
        let rules = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "rule.mcp.exec".to_string(),
                description: None,
                priority: 10,
                match_on: PolicyRuleMatch {
                    actions: vec!["exec_command".to_string()],
                    actors: vec!["mcp".to_string()],
                    surfaces: vec!["MCP".to_string()],
                    ..Default::default()
                },
                decision: PolicyRuleDecision::Deny,
                message: Some("mcp exec blocked".to_string()),
            }],
        };

        let mut engine = PolicyEngine::permissive().with_policy_rules(rules);
        let input = PolicyInput::new(ActionKind::ExecCommand, ActorKind::Mcp)
            .with_command_text("caut refresh openai");

        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("config.rule.rule.mcp.exec"));
        let context = decision
            .context()
            .expect("decision context should be present");
        assert_eq!(context.surface, PolicySurface::Mcp);
    }

    #[test]
    fn explicit_surface_override_takes_precedence_over_actor_default() {
        let rules = PolicyRulesConfig {
            enabled: true,
            rules: vec![
                PolicyRule {
                    id: "rule.surface.mcp".to_string(),
                    description: None,
                    priority: 10,
                    match_on: PolicyRuleMatch {
                        actions: vec!["read_output".to_string()],
                        actors: vec!["mcp".to_string()],
                        surfaces: vec!["mcp".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Deny,
                    message: Some("mcp surface blocked".to_string()),
                },
                PolicyRule {
                    id: "rule.surface.mux".to_string(),
                    description: None,
                    priority: 10,
                    match_on: PolicyRuleMatch {
                        actions: vec!["read_output".to_string()],
                        actors: vec!["mcp".to_string()],
                        surfaces: vec!["mux".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Allow,
                    message: Some("mux override allowed".to_string()),
                },
            ],
        };

        let mut engine = PolicyEngine::permissive().with_policy_rules(rules);
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Mcp)
            .with_surface(PolicySurface::Mux)
            .with_pane(3);

        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
        assert_eq!(decision.rule_id(), Some("config.rule.rule.surface.mux"));
        let context = decision
            .context()
            .expect("decision context should be present");
        assert_eq!(context.surface, PolicySurface::Mux);
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

    // ========================================================================
    // Additional Pure Tests
    // ========================================================================

    #[test]
    fn risk_factor_new_clamps_base_weight_to_100() {
        // RiskFactor::new should cap base_weight at 100
        let factor = RiskFactor::new("test.factor", RiskCategory::Content, 200, "over limit");
        assert_eq!(
            factor.base_weight, 100,
            "base_weight above 100 must be clamped"
        );
        assert_eq!(factor.id, "test.factor");
        assert_eq!(factor.category, RiskCategory::Content);

        let factor_exact = RiskFactor::new("test.exact", RiskCategory::State, 100, "at limit");
        assert_eq!(factor_exact.base_weight, 100);

        let factor_normal = RiskFactor::new("test.normal", RiskCategory::Action, 42, "normal");
        assert_eq!(factor_normal.base_weight, 42);
    }

    #[test]
    fn risk_score_level_predicates_cover_all_ranges() {
        // Verify is_low / is_medium / is_elevated / is_high partition 0..=100
        let make = |score: u8| RiskScore {
            score,
            factors: vec![],
            summary: RiskScore::summary_for_score(score),
        };

        // Boundary: 0 is low
        let s0 = make(0);
        assert!(s0.is_low());
        assert!(!s0.is_medium());
        assert!(!s0.is_elevated());
        assert!(!s0.is_high());

        // Boundary: 20 is low
        let s20 = make(20);
        assert!(s20.is_low());
        assert!(!s20.is_medium());

        // Boundary: 21 is medium
        let s21 = make(21);
        assert!(!s21.is_low());
        assert!(s21.is_medium());

        // Boundary: 50 is medium
        let s50 = make(50);
        assert!(s50.is_medium());
        assert!(!s50.is_elevated());

        // Boundary: 51 is elevated
        let s51 = make(51);
        assert!(!s51.is_medium());
        assert!(s51.is_elevated());

        // Boundary: 70 is elevated
        let s70 = make(70);
        assert!(s70.is_elevated());
        assert!(!s70.is_high());

        // Boundary: 71 is high
        let s71 = make(71);
        assert!(!s71.is_elevated());
        assert!(s71.is_high());

        // Boundary: 100 is high
        let s100 = make(100);
        assert!(s100.is_high());
    }

    #[test]
    fn risk_config_get_weight_uses_override_then_clamps() {
        let mut config = RiskConfig::default();

        // No override: returns base_weight
        assert_eq!(config.get_weight("foo", 42), 42);

        // Override with a value
        config.weights.insert("foo".to_string(), 75);
        assert_eq!(config.get_weight("foo", 42), 75);

        // Override above 100 is clamped
        config.weights.insert("bar".to_string(), 200);
        assert_eq!(config.get_weight("bar", 10), 100);

        // Disabled factor always returns 0
        config.disabled.insert("baz".to_string());
        assert_eq!(config.get_weight("baz", 50), 0);
        assert!(config.is_disabled("baz"));
        assert!(!config.is_disabled("foo"));
    }

    #[test]
    fn decision_context_record_rule_and_evidence_roundtrip() {
        let mut ctx = DecisionContext::empty();

        // Verify empty defaults
        assert_eq!(ctx.timestamp_ms, 0);
        assert_eq!(ctx.surface, PolicySurface::Unknown);
        assert!(ctx.rules_evaluated.is_empty());
        assert!(ctx.determining_rule.is_none());
        assert!(ctx.evidence.is_empty());
        assert!(ctx.risk.is_none());

        // Record a rule
        ctx.record_rule(
            "test.rule.1",
            true,
            Some("deny"),
            Some("matched pattern".to_string()),
        );
        ctx.record_rule("test.rule.2", false, None, None);
        assert_eq!(ctx.rules_evaluated.len(), 2);
        assert!(ctx.rules_evaluated[0].matched);
        assert_eq!(ctx.rules_evaluated[0].decision.as_deref(), Some("deny"));
        assert!(!ctx.rules_evaluated[1].matched);

        // Set determining rule
        ctx.set_determining_rule("test.rule.1");
        assert_eq!(ctx.determining_rule.as_deref(), Some("test.rule.1"));

        // Add evidence
        ctx.add_evidence("key1", "value1");
        ctx.add_evidence("key2", "value2");
        assert_eq!(ctx.evidence.len(), 2);
        assert_eq!(ctx.evidence[0].key, "key1");
        assert_eq!(ctx.evidence[0].value, "value1");

        // Set risk
        ctx.set_risk(RiskScore::zero());
        assert!(ctx.risk.is_some());
        assert_eq!(ctx.risk.as_ref().unwrap().score, 0);

        // Serde roundtrip
        let json = serde_json::to_string(&ctx).unwrap();
        let ctx2: DecisionContext = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx2.rules_evaluated.len(), 2);
        assert_eq!(ctx2.evidence.len(), 2);
        assert_eq!(ctx2.determining_rule.as_deref(), Some("test.rule.1"));
        assert_eq!(ctx2.risk.as_ref().unwrap().score, 0);
    }

    #[test]
    fn parse_serialized_decision_context_roundtrips_typed_payload() {
        let mut ctx = DecisionContext::empty();
        ctx.surface = PolicySurface::Workflow;
        ctx.action = ActionKind::SendText;
        ctx.actor = ActorKind::Workflow;
        ctx.workflow_id = Some("wf-123".to_string());

        let serialized = serde_json::to_string(&ctx).unwrap();
        let parsed = parse_serialized_decision_context(Some(&serialized)).unwrap();

        assert_eq!(parsed.surface, PolicySurface::Workflow);
        assert_eq!(parsed.action, ActionKind::SendText);
        assert_eq!(parsed.actor, ActorKind::Workflow);
        assert_eq!(parsed.workflow_id.as_deref(), Some("wf-123"));
    }

    #[test]
    fn parse_serialized_decision_surface_accepts_typed_and_legacy_payloads() {
        let mut ctx = DecisionContext::empty();
        ctx.surface = PolicySurface::Mux;
        let typed = serde_json::to_string(&ctx).unwrap();

        assert_eq!(
            parse_serialized_decision_surface(Some(&typed)),
            Some(PolicySurface::Mux)
        );
        assert_eq!(
            parse_serialized_decision_surface(Some(r#"{"surface":"workflow"}"#)),
            Some(PolicySurface::Workflow)
        );
        assert_eq!(
            parse_serialized_decision_surface(Some(r#"{"other":1}"#)),
            None
        );
        assert_eq!(parse_serialized_decision_surface(Some("{not json")), None);
        assert_eq!(parse_serialized_decision_surface(None), None);
    }

    #[test]
    fn decision_context_new_audit_uses_standard_defaults() {
        let context = DecisionContext::new_audit(
            123,
            ActionKind::ExecCommand,
            ActorKind::Mcp,
            PolicySurface::Mcp,
            Some(7),
            Some("remote".to_string()),
            Some("summary".to_string()),
            Some("wf-9".to_string()),
        );

        assert_eq!(context.timestamp_ms, 123);
        assert_eq!(context.action, ActionKind::ExecCommand);
        assert_eq!(context.actor, ActorKind::Mcp);
        assert_eq!(context.surface, PolicySurface::Mcp);
        assert_eq!(context.pane_id, Some(7));
        assert_eq!(context.domain.as_deref(), Some("remote"));
        assert_eq!(context.text_summary.as_deref(), Some("summary"));
        assert_eq!(context.workflow_id.as_deref(), Some("wf-9"));
        assert_eq!(context.capabilities, PaneCapabilities::default());
        assert!(context.rules_evaluated.is_empty());
        assert!(context.evidence.is_empty());
        assert!(context.rate_limit.is_none());
        assert!(context.risk.is_none());
    }

    // =========================================================================
    // Batch: DarkBadger wa-1u90p.7.1 — trait & edge coverage
    // =========================================================================

    // --- ActionKind ---

    #[test]
    fn action_kind_debug_clone_copy_eq_hash() {
        use std::collections::HashSet;
        let a = ActionKind::SendText;
        let b = a; // Copy
        let c = a;
        assert_eq!(a, b);
        assert_eq!(a, c);

        let mut set = HashSet::new();
        set.insert(ActionKind::SendText);
        set.insert(ActionKind::Spawn);
        set.insert(ActionKind::SendText); // dup
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn action_kind_serde_snake_case() {
        let variants = [
            (ActionKind::SendText, "\"send_text\""),
            (ActionKind::SendCtrlC, "\"send_ctrl_c\""),
            (ActionKind::Close, "\"close\""),
            (ActionKind::BrowserAuth, "\"browser_auth\""),
            (ActionKind::ReadOutput, "\"read_output\""),
        ];
        for (v, expected) in &variants {
            let json = serde_json::to_string(v).unwrap();
            assert_eq!(&json, *expected, "serde mismatch for {:?}", v);
            let parsed: ActionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, v);
        }
    }

    #[test]
    fn action_kind_as_str_roundtrip() {
        let all = [
            ActionKind::SendText,
            ActionKind::SendCtrlC,
            ActionKind::SendCtrlD,
            ActionKind::SendCtrlZ,
            ActionKind::SendControl,
            ActionKind::Spawn,
            ActionKind::Split,
            ActionKind::Activate,
            ActionKind::Close,
            ActionKind::BrowserAuth,
            ActionKind::WorkflowRun,
            ActionKind::ReservePane,
            ActionKind::ReleasePane,
            ActionKind::ReadOutput,
            ActionKind::SearchOutput,
            ActionKind::WriteFile,
            ActionKind::DeleteFile,
            ActionKind::ExecCommand,
        ];
        for a in &all {
            let s = a.as_str();
            assert!(!s.is_empty(), "as_str empty for {:?}", a);
        }
        assert_eq!(all.len(), 18);
    }

    #[test]
    fn action_kind_is_mutating() {
        assert!(ActionKind::SendText.is_mutating());
        assert!(ActionKind::Close.is_mutating());
        assert!(!ActionKind::ReadOutput.is_mutating());
        assert!(!ActionKind::Activate.is_mutating());
    }

    #[test]
    fn action_kind_is_destructive() {
        assert!(ActionKind::Close.is_destructive());
        assert!(ActionKind::SendCtrlC.is_destructive());
        assert!(!ActionKind::SendText.is_destructive());
        assert!(!ActionKind::ReadOutput.is_destructive());
    }

    // --- ActorKind ---

    #[test]
    fn actor_kind_debug_clone_copy_eq_hash() {
        use std::collections::HashSet;
        let a = ActorKind::Human;
        let b = a; // Copy
        assert_eq!(a, b);
        let mut set = HashSet::new();
        set.insert(ActorKind::Human);
        set.insert(ActorKind::Robot);
        set.insert(ActorKind::Mcp);
        set.insert(ActorKind::Workflow);
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn actor_kind_serde_snake_case() {
        let variants = [
            (ActorKind::Human, "\"human\""),
            (ActorKind::Robot, "\"robot\""),
            (ActorKind::Mcp, "\"mcp\""),
            (ActorKind::Workflow, "\"workflow\""),
        ];
        for (v, expected) in &variants {
            let json = serde_json::to_string(v).unwrap();
            assert_eq!(&json, *expected);
            let parsed: ActorKind = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, v);
        }
    }

    #[test]
    fn actor_kind_is_trusted() {
        assert!(ActorKind::Human.is_trusted());
        assert!(!ActorKind::Robot.is_trusted());
        assert!(!ActorKind::Mcp.is_trusted());
        assert!(!ActorKind::Workflow.is_trusted());
    }

    // --- PolicySurface ---

    #[test]
    fn policy_surface_debug_clone_copy_eq_hash_default() {
        use std::collections::HashSet;
        let a = PolicySurface::Robot;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_eq!(PolicySurface::default(), PolicySurface::Unknown);
        let mut set = HashSet::new();
        set.insert(PolicySurface::Unknown);
        set.insert(PolicySurface::Robot);
        set.insert(PolicySurface::Connector);
        set.insert(PolicySurface::Ipc);
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn policy_surface_serde_and_as_str() {
        let variants = [
            (PolicySurface::Unknown, "\"unknown\""),
            (PolicySurface::Mux, "\"mux\""),
            (PolicySurface::Swarm, "\"swarm\""),
            (PolicySurface::Robot, "\"robot\""),
            (PolicySurface::Connector, "\"connector\""),
            (PolicySurface::Workflow, "\"workflow\""),
            (PolicySurface::Mcp, "\"mcp\""),
            (PolicySurface::Ipc, "\"ipc\""),
        ];
        for (v, expected) in variants {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, expected);
            assert_eq!(v.as_str(), expected.trim_matches('"'));
            let parsed: PolicySurface = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, v);
        }
    }

    #[test]
    fn policy_surface_default_for_actor_mapping() {
        assert_eq!(
            PolicySurface::default_for_actor(ActorKind::Human),
            PolicySurface::Unknown
        );
        assert_eq!(
            PolicySurface::default_for_actor(ActorKind::Robot),
            PolicySurface::Robot
        );
        assert_eq!(
            PolicySurface::default_for_actor(ActorKind::Mcp),
            PolicySurface::Mcp
        );
        assert_eq!(
            PolicySurface::default_for_actor(ActorKind::Workflow),
            PolicySurface::Workflow
        );
    }

    #[test]
    fn policy_input_new_defaults_surface_from_actor() {
        let robot = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot);
        assert_eq!(robot.surface, PolicySurface::Robot);

        let mcp = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Mcp);
        assert_eq!(mcp.surface, PolicySurface::Mcp);

        let workflow = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Workflow);
        assert_eq!(workflow.surface, PolicySurface::Workflow);

        let human = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Human);
        assert_eq!(human.surface, PolicySurface::Unknown);
    }

    #[test]
    fn action_kind_identity_action_mapping_is_stable() {
        assert_eq!(ActionKind::ReadOutput.auth_action(), AuthAction::Read);
        assert_eq!(ActionKind::SendText.auth_action(), AuthAction::Write);
        assert_eq!(ActionKind::Spawn.auth_action(), AuthAction::Create);
        assert_eq!(ActionKind::Close.auth_action(), AuthAction::Delete);
        assert_eq!(
            ActionKind::ConnectorCredentialAction.auth_action(),
            AuthAction::Admin
        );
    }

    #[test]
    fn policy_input_identity_mapping_prefers_specific_ids_and_scopes_panes() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Workflow)
            .with_workflow("wf-123")
            .with_domain("local")
            .with_pane(42);

        assert_eq!(
            input.identity_principal(None).stable_key(),
            "workflow:wf-123"
        );
        assert_eq!(input.identity_resource().stable_key(), "pane:local:42");
        assert_eq!(input.identity_action(), AuthAction::Write);

        let robot = PolicyInput::new(ActionKind::ExecCommand, ActorKind::Robot);
        assert_eq!(
            robot.identity_principal(Some("codex-agent-7")).stable_key(),
            "agent:codex-agent-7"
        );
        assert_eq!(
            robot.identity_resource().stable_key(),
            "capability:exec_command"
        );
        assert_eq!(robot.identity_action(), AuthAction::Execute);
    }

    #[test]
    fn decision_context_from_input_records_identity_evidence() {
        let input = PolicyInput::new(ActionKind::WorkflowRun, ActorKind::Workflow)
            .with_workflow("wf-run-9");

        let context = DecisionContext::from_input(&input);
        let identity_principal = context
            .evidence
            .iter()
            .find(|entry| entry.key == "identity_principal")
            .map(|entry| entry.value.as_str());
        let identity_resource = context
            .evidence
            .iter()
            .find(|entry| entry.key == "identity_resource")
            .map(|entry| entry.value.as_str());
        let identity_action = context
            .evidence
            .iter()
            .find(|entry| entry.key == "identity_action")
            .map(|entry| entry.value.as_str());

        assert_eq!(identity_principal, Some("workflow:wf-run-9"));
        assert_eq!(identity_resource, Some("workflow:wf-run-9"));
        assert_eq!(identity_action, Some("execute"));
    }

    #[test]
    fn decision_context_identity_principal_uses_supplied_actor_id() {
        let mut context = DecisionContext::empty();
        context.actor = ActorKind::Mcp;
        context.action = ActionKind::ConnectorCredentialAction;
        context.domain = Some("slack".to_string());

        assert_eq!(
            context
                .identity_principal(Some("remote-admin"))
                .stable_key(),
            "mcp:remote-admin"
        );
        assert_eq!(
            context.identity_resource().stable_key(),
            "credential:slack:*"
        );
        assert_eq!(context.identity_action(), AuthAction::Admin);
    }

    // --- RiskCategory ---

    #[test]
    fn risk_category_debug_clone_copy_eq_hash() {
        use std::collections::HashSet;
        let c = RiskCategory::State;
        let b = c; // Copy
        assert_eq!(c, b);
        let mut set = HashSet::new();
        set.insert(RiskCategory::State);
        set.insert(RiskCategory::Action);
        set.insert(RiskCategory::Context);
        set.insert(RiskCategory::Content);
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn risk_category_serde_snake_case() {
        let variants = [
            (RiskCategory::State, "\"state\""),
            (RiskCategory::Action, "\"action\""),
            (RiskCategory::Context, "\"context\""),
            (RiskCategory::Content, "\"content\""),
        ];
        for (v, expected) in &variants {
            let json = serde_json::to_string(v).unwrap();
            assert_eq!(&json, *expected);
        }
    }

    // --- RiskScore ---

    #[test]
    fn risk_score_zero() {
        let r = RiskScore::zero();
        assert_eq!(r.score, 0);
        assert!(r.factors.is_empty());
    }

    #[test]
    fn risk_score_serde_roundtrip() {
        let r = RiskScore {
            score: 42,
            factors: vec![AppliedRiskFactor {
                id: "test.factor".into(),
                weight: 20,
                explanation: "test reason".into(),
            }],
            summary: "Medium risk".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let parsed: RiskScore = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.score, 42);
        assert_eq!(parsed.factors.len(), 1);
        assert_eq!(parsed.factors[0].weight, 20);
    }

    // --- PolicyDecision ---

    #[test]
    fn policy_decision_allow_eq() {
        let a = PolicyDecision::allow();
        let b = PolicyDecision::allow();
        assert_eq!(a, b);
    }

    #[test]
    fn policy_decision_serde_roundtrip_all_variants() {
        let allow = PolicyDecision::allow();
        let json = serde_json::to_string(&allow).unwrap();
        assert!(json.contains("\"decision\":\"allow\""));
        let parsed: PolicyDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, allow);

        let deny = PolicyDecision::deny("forbidden");
        let json = serde_json::to_string(&deny).unwrap();
        assert!(json.contains("\"decision\":\"deny\""));
        let parsed: PolicyDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, deny);
    }

    // --- RateLimitHit ---

    #[test]
    fn rate_limit_hit_debug_clone() {
        let h = RateLimitHit {
            scope: RateLimitScope::Global,
            action: ActionKind::SendText,
            limit: 60,
            current: 61,
            retry_after: Duration::from_secs(5),
        };
        let cloned = h.clone();
        assert_eq!(cloned.limit, 60);
        let dbg = format!("{:?}", h);
        assert!(dbg.contains("RateLimitHit"));
    }

    #[test]
    fn rate_limit_hit_reason_global() {
        let h = RateLimitHit {
            scope: RateLimitScope::Global,
            action: ActionKind::SendText,
            limit: 10,
            current: 11,
            retry_after: Duration::from_secs(3),
        };
        let reason = h.reason();
        assert!(reason.contains("Global rate limit"));
        assert!(reason.contains("send_text"));
        assert!(reason.contains("retry after"));
    }

    #[test]
    fn rate_limit_hit_reason_per_pane() {
        let h = RateLimitHit {
            scope: RateLimitScope::PerPane { pane_id: 42 },
            action: ActionKind::Close,
            limit: 5,
            current: 6,
            retry_after: Duration::ZERO,
        };
        let reason = h.reason();
        assert!(reason.contains("pane 42"));
        assert!(reason.contains("close"));
    }

    // --- RateLimitOutcome ---

    #[test]
    fn rate_limit_outcome_debug_clone() {
        let allowed = RateLimitOutcome::Allowed;
        let cloned = allowed.clone();
        assert!(cloned.is_allowed());
        let dbg = format!("{:?}", allowed);
        assert!(dbg.contains("Allowed"));

        let limited = RateLimitOutcome::Limited(RateLimitHit {
            scope: RateLimitScope::Global,
            action: ActionKind::Spawn,
            limit: 1,
            current: 2,
            retry_after: Duration::from_secs(1),
        });
        assert!(!limited.is_allowed());
    }

    // --- Redactor ---

    #[test]
    fn redactor_debug_default() {
        let r = Redactor::new();
        let d = Redactor::default();
        let dbg = format!("{:?}", r);
        assert!(dbg.contains("Redactor"));
        let dbg2 = format!("{:?}", d);
        assert!(dbg2.contains("Redactor"));
    }

    #[test]
    fn redactor_no_secrets_passthrough() {
        let r = Redactor::new();
        let text = "just some normal text with no secrets";
        assert_eq!(r.redact(text), text);
        assert!(!r.contains_secrets(text));
    }

    // --- SendTextAuditSummary ---

    #[test]
    fn send_text_audit_summary_debug_clone_serde() {
        let s = SendTextAuditSummary {
            text_length: 42,
            text_preview_redacted: "echo hello".into(),
            text_hash: "abc123".into(),
            command_candidate: true,
            workflow_execution_id: Some("wf-1".into()),
            parent_action_id: Some(99),
        };
        let cloned = s.clone();
        assert_eq!(cloned.text_length, 42);
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("SendTextAuditSummary"));

        let json = serde_json::to_string(&s).unwrap();
        let parsed: SendTextAuditSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.text_length, 42);
        assert!(parsed.command_candidate);
    }

    #[test]
    fn send_text_audit_summary_serde_skip_none() {
        let s = SendTextAuditSummary {
            text_length: 5,
            text_preview_redacted: "ls".into(),
            text_hash: "x".into(),
            command_candidate: false,
            workflow_execution_id: None,
            parent_action_id: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("workflow_execution_id"));
        assert!(!json.contains("parent_action_id"));
    }

    // --- RuleEvaluationResult ---

    #[test]
    fn rule_evaluation_result_debug_clone() {
        let r = RuleEvaluationResult {
            matching_rule: None,
            decision: None,
            rules_checked: vec!["rule.1".into(), "rule.2".into()],
            matched_rule_ids: Vec::new(),
        };
        let cloned = r.clone();
        assert_eq!(cloned.rules_checked.len(), 2);
        assert!(cloned.matching_rule.is_none());
        let dbg = format!("{:?}", r);
        assert!(dbg.contains("RuleEvaluationResult"));
    }

    // --- glob_match ---

    #[test]
    fn glob_match_empty_pattern_matches_empty() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "anything"));
    }

    #[test]
    fn glob_match_star_matches_anything() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_match_question_mark_single_char() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
    }

    #[test]
    fn is_command_candidate_handles_env_assignments() {
        assert!(is_command_candidate("FOO=bar rm -rf /"));
        assert!(is_command_candidate("FOO='bar' rm -rf /"));
        assert!(is_command_candidate("FOO=\"bar\" rm -rf /"));
        assert!(is_command_candidate("FOO=bar /bin/rm -rf /"));
        assert!(!is_command_candidate("FOO=bar"));
        assert!(!is_command_candidate("FOO='bar'"));
        assert!(!is_command_candidate("FOO=\"bar\""));
    }

    #[test]
    fn is_command_candidate_catches_subshell_in_assignment() {
        // THIS TEST WILL FAIL IF THE BUG EXISTS
        assert!(is_command_candidate("FOO=$(rm -rf /)"));
        assert!(is_command_candidate("FOO=`rm -rf /`"));
    }

    #[test]
    fn is_command_candidate_bypasses_simple_text() {
        assert!(!is_command_candidate("Please check the logs"));
        assert!(!is_command_candidate("# commented command"));
    }

    // ========================================================================
    // config_rule_trace_reason regression tests (ft-13l5b)
    // Validates the fix: `rule.message` instead of former `self.message`
    // ========================================================================

    #[test]
    fn trace_reason_selected_with_message() {
        let rule = PolicyRule {
            id: "r1".to_string(),
            description: None,
            priority: 100,
            match_on: PolicyRuleMatch::default(),
            decision: PolicyRuleDecision::Allow,
            message: Some("custom reason".to_string()),
        };
        let reason = config_rule_trace_reason(&rule, true, None);
        assert_eq!(reason, "rule matched and selected: custom reason");
    }

    #[test]
    fn trace_reason_selected_without_message() {
        let rule = PolicyRule {
            id: "r2".to_string(),
            description: None,
            priority: 100,
            match_on: PolicyRuleMatch::default(),
            decision: PolicyRuleDecision::Deny,
            message: None,
        };
        let reason = config_rule_trace_reason(&rule, true, None);
        assert_eq!(reason, "rule matched and selected");
    }

    #[test]
    fn trace_reason_not_selected_with_message() {
        let rule = PolicyRule {
            id: "r3".to_string(),
            description: None,
            priority: 100,
            match_on: PolicyRuleMatch::default(),
            decision: PolicyRuleDecision::Allow,
            message: Some("low priority".to_string()),
        };
        let reason = config_rule_trace_reason(&rule, false, Some("winner-rule"));
        assert_eq!(
            reason,
            "rule matched but 'winner-rule' won tie-breaking: low priority"
        );
    }

    #[test]
    fn trace_reason_not_selected_without_message() {
        let rule = PolicyRule {
            id: "r4".to_string(),
            description: None,
            priority: 100,
            match_on: PolicyRuleMatch::default(),
            decision: PolicyRuleDecision::Allow,
            message: None,
        };
        let reason = config_rule_trace_reason(&rule, false, Some("other"));
        assert_eq!(reason, "rule matched but 'other' won tie-breaking");
    }

    #[test]
    fn trace_reason_not_selected_no_winner_id() {
        let rule = PolicyRule {
            id: "r5".to_string(),
            description: None,
            priority: 100,
            match_on: PolicyRuleMatch::default(),
            decision: PolicyRuleDecision::Deny,
            message: None,
        };
        let reason = config_rule_trace_reason(&rule, false, None);
        assert_eq!(reason, "rule matched but 'unknown' won tie-breaking");
    }

    // ========================================================================
    // matches_rule unit tests (ft-13l5b)
    // Direct tests for the core policy rule matching logic
    // ========================================================================

    #[test]
    fn matches_rule_catch_all() {
        let match_on = PolicyRuleMatch::default();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_action_match() {
        let match_on = PolicyRuleMatch {
            actions: vec!["send_text".to_string()],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_action_mismatch() {
        let match_on = PolicyRuleMatch {
            actions: vec!["close".to_string()],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_actor_match() {
        let match_on = PolicyRuleMatch {
            actors: vec!["robot".to_string()],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_actor_mismatch() {
        let match_on = PolicyRuleMatch {
            actors: vec!["human".to_string()],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_surface_case_insensitive() {
        let match_on = PolicyRuleMatch {
            surfaces: vec!["ROBOT".to_string()],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_surface(PolicySurface::Robot);
        assert!(matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_pane_id_match() {
        let match_on = PolicyRuleMatch {
            pane_ids: vec![42],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(42);
        assert!(matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_pane_id_mismatch() {
        let match_on = PolicyRuleMatch {
            pane_ids: vec![42],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(99);
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_pane_id_none_input() {
        let match_on = PolicyRuleMatch {
            pane_ids: vec![42],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_domain_match() {
        let match_on = PolicyRuleMatch {
            pane_domains: vec!["local".to_string()],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_domain("local");
        assert!(matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_domain_none_input() {
        let match_on = PolicyRuleMatch {
            pane_domains: vec!["local".to_string()],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_title_glob() {
        let match_on = PolicyRuleMatch {
            pane_titles: vec!["*agent*".to_string()],
            ..Default::default()
        };
        let mut input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        input.pane_title = Some("my-agent-pane".to_string());
        assert!(matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_title_glob_no_match() {
        let match_on = PolicyRuleMatch {
            pane_titles: vec!["*agent*".to_string()],
            ..Default::default()
        };
        let mut input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        input.pane_title = Some("my-shell-pane".to_string());
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_title_none_input() {
        let match_on = PolicyRuleMatch {
            pane_titles: vec!["*".to_string()],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_cwd_glob() {
        let match_on = PolicyRuleMatch {
            pane_cwds: vec!["/home/*/projects/*".to_string()],
            ..Default::default()
        };
        let mut input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        input.pane_cwd = Some("/home/user/projects/frankenterm".to_string());
        assert!(matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_cwd_none_input() {
        let match_on = PolicyRuleMatch {
            pane_cwds: vec!["/tmp/*".to_string()],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_command_regex() {
        let match_on = PolicyRuleMatch {
            command_patterns: vec!["^rm\\s+-rf".to_string()],
            ..Default::default()
        };
        let mut input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        input.command_text = Some("rm -rf /tmp/junk".to_string());
        assert!(matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_command_regex_no_match() {
        let match_on = PolicyRuleMatch {
            command_patterns: vec!["^rm\\s+-rf".to_string()],
            ..Default::default()
        };
        let mut input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        input.command_text = Some("ls -la".to_string());
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_command_none_input() {
        let match_on = PolicyRuleMatch {
            command_patterns: vec![".*".to_string()],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_agent_type_case_insensitive() {
        let match_on = PolicyRuleMatch {
            agent_types: vec!["Claude".to_string()],
            ..Default::default()
        };
        let mut input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        input.agent_type = Some("claude".to_string());
        assert!(matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_agent_type_none_input() {
        let match_on = PolicyRuleMatch {
            agent_types: vec!["claude".to_string()],
            ..Default::default()
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_multiple_criteria_all_must_match() {
        let match_on = PolicyRuleMatch {
            actions: vec!["send_text".to_string()],
            actors: vec!["robot".to_string()],
            pane_ids: vec![10],
            ..Default::default()
        };
        // All criteria match
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(10);
        assert!(matches_rule(&match_on, &input));

        // Action matches but pane_id doesn't
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(99);
        assert!(!matches_rule(&match_on, &input));

        // Pane matches but action doesn't
        let input = PolicyInput::new(ActionKind::Close, ActorKind::Robot).with_pane(10);
        assert!(!matches_rule(&match_on, &input));
    }

    #[test]
    fn matches_rule_multiple_values_in_list_are_ored() {
        let match_on = PolicyRuleMatch {
            actions: vec!["send_text".to_string(), "close".to_string()],
            ..Default::default()
        };
        let input1 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let input2 = PolicyInput::new(ActionKind::Close, ActorKind::Robot);
        let input3 = PolicyInput::new(ActionKind::Spawn, ActorKind::Robot);
        assert!(matches_rule(&match_on, &input1));
        assert!(matches_rule(&match_on, &input2));
        assert!(!matches_rule(&match_on, &input3));
    }

    #[test]
    fn matches_rule_invalid_regex_does_not_match() {
        let match_on = PolicyRuleMatch {
            command_patterns: vec!["[invalid".to_string()],
            ..Default::default()
        };
        let mut input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        input.command_text = Some("anything".to_string());
        assert!(!matches_rule(&match_on, &input));
    }

    // ========================================================================
    // RulePredicate tests
    // ========================================================================

    #[test]
    fn predicate_true_always_matches() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(RulePredicate::True.evaluate(&input));
    }

    #[test]
    fn predicate_false_never_matches() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(!RulePredicate::False.evaluate(&input));
    }

    #[test]
    fn predicate_not_inverts() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let pred = RulePredicate::Not {
            child: Box::new(RulePredicate::True),
        };
        assert!(!pred.evaluate(&input));

        let pred2 = RulePredicate::Not {
            child: Box::new(RulePredicate::False),
        };
        assert!(pred2.evaluate(&input));
    }

    #[test]
    fn predicate_action_matches() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let pred = RulePredicate::Action {
            values: vec!["send_text".to_string()],
        };
        assert!(pred.evaluate(&input));

        let pred2 = RulePredicate::Action {
            values: vec!["close".to_string()],
        };
        assert!(!pred2.evaluate(&input));
    }

    #[test]
    fn predicate_actor_matches() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let pred = RulePredicate::Actor {
            values: vec!["robot".to_string()],
        };
        assert!(pred.evaluate(&input));

        let pred2 = RulePredicate::Actor {
            values: vec!["human".to_string()],
        };
        assert!(!pred2.evaluate(&input));
    }

    #[test]
    fn predicate_surface_case_insensitive() {
        let mut input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        input.surface = PolicySurface::Mux;
        let pred = RulePredicate::Surface {
            values: vec!["MUX".to_string()],
        };
        assert!(pred.evaluate(&input));
    }

    #[test]
    fn predicate_and_requires_all() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let pred = RulePredicate::And {
            children: vec![
                RulePredicate::Action {
                    values: vec!["send_text".to_string()],
                },
                RulePredicate::Actor {
                    values: vec!["robot".to_string()],
                },
            ],
        };
        assert!(pred.evaluate(&input));

        let pred2 = RulePredicate::And {
            children: vec![
                RulePredicate::Action {
                    values: vec!["send_text".to_string()],
                },
                RulePredicate::Actor {
                    values: vec!["human".to_string()],
                },
            ],
        };
        assert!(!pred2.evaluate(&input));
    }

    #[test]
    fn predicate_or_requires_any() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let pred = RulePredicate::Or {
            children: vec![
                RulePredicate::Action {
                    values: vec!["close".to_string()],
                },
                RulePredicate::Actor {
                    values: vec!["robot".to_string()],
                },
            ],
        };
        assert!(pred.evaluate(&input));

        let pred_none = RulePredicate::Or {
            children: vec![
                RulePredicate::Action {
                    values: vec!["close".to_string()],
                },
                RulePredicate::Actor {
                    values: vec!["human".to_string()],
                },
            ],
        };
        assert!(!pred_none.evaluate(&input));
    }

    #[test]
    fn predicate_and_empty_children_matches() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let pred = RulePredicate::And { children: vec![] };
        assert!(pred.evaluate(&input));
    }

    #[test]
    fn predicate_or_empty_children_fails() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let pred = RulePredicate::Or { children: vec![] };
        assert!(!pred.evaluate(&input));
    }

    #[test]
    fn predicate_nested_complex() {
        // (action=spawn AND actor=robot) OR (action=send_text AND actor=mcp)
        let pred = RulePredicate::Or {
            children: vec![
                RulePredicate::And {
                    children: vec![
                        RulePredicate::Action {
                            values: vec!["spawn".to_string()],
                        },
                        RulePredicate::Actor {
                            values: vec!["robot".to_string()],
                        },
                    ],
                },
                RulePredicate::And {
                    children: vec![
                        RulePredicate::Action {
                            values: vec!["send_text".to_string()],
                        },
                        RulePredicate::Actor {
                            values: vec!["mcp".to_string()],
                        },
                    ],
                },
            ],
        };

        // spawn + robot → matches first branch
        let input1 = PolicyInput::new(ActionKind::Spawn, ActorKind::Robot);
        assert!(pred.evaluate(&input1));

        // send_text + mcp → matches second branch
        let input2 = PolicyInput::new(ActionKind::SendText, ActorKind::Mcp);
        assert!(pred.evaluate(&input2));

        // send_text + robot → matches neither
        let input3 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(!pred.evaluate(&input3));
    }

    #[test]
    fn predicate_pane_id_matches() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(42);
        let pred = RulePredicate::PaneId {
            values: vec![42, 99],
        };
        assert!(pred.evaluate(&input));

        let pred2 = RulePredicate::PaneId { values: vec![99] };
        assert!(!pred2.evaluate(&input));
    }

    #[test]
    fn predicate_pane_title_glob() {
        let mut input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        input.pane_title = Some("my-critical-pane".to_string());
        let pred = RulePredicate::PaneTitle {
            patterns: vec!["*critical*".to_string()],
        };
        assert!(pred.evaluate(&input));

        let pred2 = RulePredicate::PaneTitle {
            patterns: vec!["*safe*".to_string()],
        };
        assert!(!pred2.evaluate(&input));
    }

    #[test]
    fn predicate_command_pattern_regex() {
        let mut input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        input.command_text = Some("rm -rf /tmp/stuff".to_string());
        let pred = RulePredicate::CommandPattern {
            patterns: vec!["^rm\\s+-rf".to_string()],
        };
        assert!(pred.evaluate(&input));
    }

    #[test]
    fn predicate_empty_values_never_matches() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        assert!(!RulePredicate::Action { values: vec![] }.evaluate(&input));
        assert!(!RulePredicate::Actor { values: vec![] }.evaluate(&input));
        assert!(!RulePredicate::Surface { values: vec![] }.evaluate(&input));
    }

    #[test]
    fn predicate_depth() {
        assert_eq!(RulePredicate::True.depth(), 1);
        assert_eq!(
            RulePredicate::Not {
                child: Box::new(RulePredicate::True)
            }
            .depth(),
            2
        );
        assert_eq!(
            RulePredicate::And {
                children: vec![
                    RulePredicate::True,
                    RulePredicate::Not {
                        child: Box::new(RulePredicate::False)
                    },
                ]
            }
            .depth(),
            3
        );
    }

    #[test]
    fn predicate_leaf_count() {
        assert_eq!(RulePredicate::True.leaf_count(), 1);
        assert_eq!(
            RulePredicate::And {
                children: vec![RulePredicate::True, RulePredicate::False,]
            }
            .leaf_count(),
            2
        );
        assert_eq!(
            RulePredicate::Or {
                children: vec![
                    RulePredicate::True,
                    RulePredicate::And {
                        children: vec![RulePredicate::False, RulePredicate::True,]
                    },
                ]
            }
            .leaf_count(),
            3
        );
    }

    #[test]
    fn predicate_from_flat_match_catch_all() {
        let m = PolicyRuleMatch::default();
        let pred = RulePredicate::from_flat_match(&m);
        assert_eq!(pred, RulePredicate::True);
    }

    #[test]
    fn predicate_from_flat_match_single_criterion() {
        let m = PolicyRuleMatch {
            actions: vec!["send_text".to_string()],
            ..Default::default()
        };
        let pred = RulePredicate::from_flat_match(&m);
        assert_eq!(
            pred,
            RulePredicate::Action {
                values: vec!["send_text".to_string()]
            }
        );
    }

    #[test]
    fn predicate_from_flat_match_multiple_criteria() {
        let m = PolicyRuleMatch {
            actions: vec!["send_text".to_string()],
            actors: vec!["robot".to_string()],
            ..Default::default()
        };
        let pred = RulePredicate::from_flat_match(&m);
        let check = matches!(pred, RulePredicate::And { children } if children.len() == 2);
        assert!(check);
    }

    #[test]
    fn predicate_from_flat_match_parity_with_matches_rule() {
        let m = PolicyRuleMatch {
            actions: vec!["send_text".to_string()],
            actors: vec!["robot".to_string()],
            pane_ids: vec![42],
            ..Default::default()
        };
        let pred = RulePredicate::from_flat_match(&m);

        let input_match = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(42);
        let input_no_match = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(99);

        assert_eq!(matches_rule(&m, &input_match), pred.evaluate(&input_match));
        assert_eq!(
            matches_rule(&m, &input_no_match),
            pred.evaluate(&input_no_match)
        );
    }

    #[test]
    fn predicate_serde_roundtrip() {
        let pred = RulePredicate::And {
            children: vec![
                RulePredicate::Action {
                    values: vec!["send_text".to_string()],
                },
                RulePredicate::Not {
                    child: Box::new(RulePredicate::Actor {
                        values: vec!["human".to_string()],
                    }),
                },
            ],
        };
        let json = serde_json::to_string(&pred).unwrap();
        let back: RulePredicate = serde_json::from_str(&json).unwrap();
        assert_eq!(pred, back);
    }

    #[test]
    fn predicate_serde_tagged_format() {
        let pred = RulePredicate::Action {
            values: vec!["send_text".to_string()],
        };
        let json = serde_json::to_string(&pred).unwrap();
        assert!(json.contains("\"type\":\"action\""));
        assert!(json.contains("\"values\":[\"send_text\"]"));
    }

    // ========================================================================
    // Decision log integration tests
    // ========================================================================

    #[test]
    fn decision_log_records_allow() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Human);
        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());

        let log = engine.decision_log();
        assert_eq!(log.len(), 1);
        let snapshot = log.snapshot();
        assert_eq!(snapshot.total_recorded, 1);
        assert_eq!(snapshot.allow_count, 1);
        assert_eq!(snapshot.deny_count, 0);
    }

    #[test]
    fn decision_log_records_deny() {
        let mut engine = PolicyEngine::new(30, 100, true);
        // Trigger a deny: send text while command is running
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::running());
        let decision = engine.authorize(&input);
        assert!(decision.is_denied());

        let log = engine.decision_log();
        assert_eq!(log.len(), 1);
        let snapshot = log.snapshot();
        assert_eq!(snapshot.deny_count, 1);
    }

    #[test]
    fn decision_log_records_require_approval() {
        let mut engine = PolicyEngine::new(30, 100, false);
        // Trigger require_approval: destructive action by robot
        let input = PolicyInput::new(ActionKind::Close, ActorKind::Robot).with_pane(1);
        let decision = engine.authorize(&input);
        assert!(decision.requires_approval());

        let log = engine.decision_log();
        assert_eq!(log.len(), 1);
        let snapshot = log.snapshot();
        assert_eq!(snapshot.require_approval_count, 1);
    }

    #[test]
    fn decision_log_accumulates_multiple_decisions() {
        let mut engine = PolicyEngine::permissive();

        // 3 allows
        for _ in 0..3 {
            let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Human);
            engine.authorize(&input);
        }

        // 1 deny (send to alt-screen)
        let deny_input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities {
                alt_screen: Some(true),
                ..PaneCapabilities::default()
            });
        engine.authorize(&deny_input);

        let log = engine.decision_log();
        assert_eq!(log.len(), 4);
        let snapshot = log.snapshot();
        assert_eq!(snapshot.allow_count, 3);
        assert_eq!(snapshot.deny_count, 1);
    }

    #[test]
    fn decision_log_captures_rule_id() {
        let mut engine = PolicyEngine::new(30, 100, true);
        // Trigger deny: send text while command running -> rule_id = "policy.prompt_required"
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::running());
        engine.authorize(&input);

        let log = engine.decision_log();
        let entries: Vec<_> = log.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].rule_id.as_deref(),
            Some("policy.prompt_required")
        );
    }

    #[test]
    fn decision_log_captures_pane_id_and_surface() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(42)
            .with_surface(PolicySurface::Robot);
        engine.authorize(&input);

        let log = engine.decision_log();
        let entries: Vec<_> = log.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pane_id, Some(42));
        assert_eq!(entries[0].surface, PolicySurface::Robot);
        assert_eq!(entries[0].actor, ActorKind::Robot);
        assert_eq!(entries[0].action, ActionKind::ReadOutput);
    }

    #[test]
    fn decision_log_respects_config_skip_allows() {
        use crate::policy_decision_log::DecisionLogConfig;
        let config = DecisionLogConfig {
            max_entries: 100,
            record_allows: false,
        };
        let mut engine = PolicyEngine::permissive().with_decision_log_config(config);

        // Allow should be skipped
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Human);
        engine.authorize(&input);
        assert_eq!(engine.decision_log().len(), 0);

        // Deny should still be recorded
        let deny_input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities {
                alt_screen: Some(true),
                ..PaneCapabilities::default()
            });
        engine.authorize(&deny_input);
        assert_eq!(engine.decision_log().len(), 1);
    }

    #[test]
    fn decision_log_records_rules_evaluated_count() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());
        engine.authorize(&input);

        let log = engine.decision_log();
        let entries: Vec<_> = log.entries().collect();
        assert_eq!(entries.len(), 1);
        // authorize evaluates multiple builtin gates (rate_limit, alt_screen, etc.)
        assert!(entries[0].rules_evaluated > 0);
    }

    #[test]
    fn decision_log_export_json() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Human);
        engine.authorize(&input);

        let json = engine.decision_log().export_json().unwrap();
        // export_json uses to_string_pretty, so keys/values may be on separate lines
        assert!(
            json.contains("read_output"),
            "expected read_output in: {json}"
        );
        assert!(json.contains("human"), "expected human in: {json}");
        assert!(json.contains("allow"), "expected allow in: {json}");
    }

    #[test]
    fn decision_log_from_safety_config_wires_decision_log_settings() {
        let mut safety = crate::config::SafetyConfig::default();
        safety.decision_log = crate::policy_decision_log::DecisionLogConfig {
            max_entries: 5,
            record_allows: false,
        };
        let mut engine = PolicyEngine::from_safety_config(&safety);

        // Allow decision should be skipped (record_allows = false)
        let allow_input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Human);
        engine.authorize(&allow_input);
        assert_eq!(engine.decision_log().len(), 0, "allows should be skipped");

        // Deny should still be recorded
        let deny_input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities {
                alt_screen: Some(true),
                ..PaneCapabilities::default()
            });
        engine.authorize(&deny_input);
        assert_eq!(engine.decision_log().len(), 1, "deny should be recorded");

        let snapshot = engine.decision_log().snapshot();
        assert_eq!(snapshot.max_entries, 5);
        assert!(!snapshot.record_allows);
    }

    #[test]
    fn decision_log_records_dsl_custom_rule_match() {
        let deny_rule = PolicyRule {
            id: "test.deny_robot_send".to_string(),
            description: None,
            priority: 10,
            match_on: PolicyRuleMatch {
                actions: vec!["send_text".to_string()],
                actors: vec!["robot".to_string()],
                ..PolicyRuleMatch::default()
            },
            decision: PolicyRuleDecision::Deny,
            message: Some("Robot sends denied by custom rule".to_string()),
        };
        let rules_config = PolicyRulesConfig {
            enabled: true,
            rules: vec![deny_rule],
        };
        let mut engine = PolicyEngine::permissive().with_policy_rules(rules_config);

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        assert!(decision.is_denied());

        let log = engine.decision_log();
        let entries: Vec<_> = log.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].rule_id.as_deref(),
            Some("config.rule.test.deny_robot_send")
        );
        assert_eq!(
            entries[0].decision,
            crate::policy_decision_log::DecisionOutcome::Deny
        );

        // Snapshot counters should reflect the deny
        let snapshot = log.snapshot();
        assert_eq!(snapshot.deny_count, 1);
        assert_eq!(snapshot.total_recorded, 1);
    }

    #[test]
    fn decision_log_snapshot_serializable() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Human);
        engine.authorize(&input);

        let snapshot = engine.decision_log().snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: crate::policy_decision_log::DecisionLogSnapshot =
            serde_json::from_str(&json).unwrap();
        assert_eq!(snapshot, back);
    }

    // ========================================================================
    // Quarantine integration tests
    // ========================================================================

    #[test]
    fn quarantine_registry_initially_empty() {
        let engine = PolicyEngine::permissive();
        assert!(engine.quarantine_registry().active_quarantines().is_empty());
        assert!(
            engine
                .quarantine_registry()
                .kill_switch()
                .allows_new_workflows()
        );
    }

    #[test]
    fn quarantine_blocks_mutating_action_on_pane() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        // Quarantine pane-1
        engine
            .quarantine_registry_mut()
            .quarantine(
                "pane-1",
                ComponentKind::Pane,
                QuarantineSeverity::Restricted,
                QuarantineReason::PolicyViolation {
                    rule_id: "r1".into(),
                    detail: "rate abuse".into(),
                },
                "operator",
                1000,
                0,
            )
            .unwrap();

        // SendText is mutating — should be denied
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("policy.quarantine"));
    }

    #[test]
    fn quarantine_allows_read_on_restricted_pane() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine
            .quarantine_registry_mut()
            .quarantine(
                "pane-1",
                ComponentKind::Pane,
                QuarantineSeverity::Restricted,
                QuarantineReason::AnomalousBehavior {
                    metric: "error_rate".into(),
                    observed: "50%".into(),
                },
                "system",
                1000,
                0,
            )
            .unwrap();

        // ReadOutput is not mutating — should be allowed
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot).with_pane(1);
        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn quarantine_isolated_blocks_all_actions() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine
            .quarantine_registry_mut()
            .quarantine(
                "pane-2",
                ComponentKind::Pane,
                QuarantineSeverity::Isolated,
                QuarantineReason::CredentialCompromise {
                    credential_id: "key-abc".into(),
                },
                "security",
                1000,
                0,
            )
            .unwrap();

        // Even reads should be blocked at Isolated severity
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot).with_pane(2);
        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("policy.quarantine"));
    }

    #[test]
    fn kill_switch_emergency_denies_all() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine.quarantine_registry_mut().trip_kill_switch(
            KillSwitchLevel::EmergencyHalt,
            "incident-commander",
            "critical security breach",
            1000,
        );

        // Any action should be denied
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Human);
        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("policy.kill_switch"));
    }

    #[test]
    fn kill_switch_soft_stop_blocks_writes_on_any_pane() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine.quarantine_registry_mut().trip_kill_switch(
            KillSwitchLevel::SoftStop,
            "operator",
            "maintenance window",
            1000,
        );

        // Write to any pane should be blocked (kill switch blocks at registry level)
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(99)
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("policy.quarantine"));
    }

    #[test]
    fn quarantine_release_restores_access() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine
            .quarantine_registry_mut()
            .quarantine(
                "pane-1",
                ComponentKind::Pane,
                QuarantineSeverity::Isolated,
                QuarantineReason::CircuitBreakerTrip {
                    circuit_id: "cb-1".into(),
                },
                "system",
                1000,
                0,
            )
            .unwrap();

        // Blocked before release
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());
        assert!(engine.authorize(&input).is_denied());

        // Release from quarantine
        engine
            .quarantine_registry_mut()
            .release("pane-1", "operator", false, 2000)
            .unwrap();

        // Should be allowed after release
        assert!(engine.authorize(&input).is_allowed());
    }

    #[test]
    fn quarantine_decision_recorded_in_log() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine
            .quarantine_registry_mut()
            .quarantine(
                "pane-5",
                ComponentKind::Pane,
                QuarantineSeverity::Restricted,
                QuarantineReason::OperatorDirected {
                    operator: "admin".into(),
                    note: "investigation".into(),
                },
                "admin",
                1000,
                0,
            )
            .unwrap();

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(5)
            .with_capabilities(PaneCapabilities::prompt());
        engine.authorize(&input);

        // The denial should be recorded in the decision log
        let log = engine.decision_log();
        assert_eq!(log.len(), 1);
    }

    // ================================================================
    // Audit chain integration tests
    // ================================================================

    #[test]
    fn audit_chain_initially_empty() {
        let engine = PolicyEngine::permissive();
        assert!(engine.audit_chain().is_empty());
        assert_eq!(engine.audit_chain().len(), 0);
    }

    #[test]
    fn audit_chain_records_deny_decisions() {
        let mut engine = PolicyEngine::strict();
        // Strict engine requires prompt active, so non-prompt sends are denied
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::running());
        let decision = engine.authorize(&input);
        assert!(decision.is_denied());

        // Deny should always be recorded in audit chain
        assert_eq!(engine.audit_chain().len(), 1);
        let entry = engine.audit_chain().latest().unwrap();
        assert_eq!(entry.kind, AuditEntryKind::PolicyDecision);
        assert!(entry.description.starts_with("deny:"));
    }

    #[test]
    fn audit_chain_skips_allows_by_default() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Human)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());

        // By default, allows are NOT recorded in audit chain
        assert!(engine.audit_chain().is_empty());
    }

    #[test]
    fn audit_chain_records_allows_when_configured() {
        let config = crate::config::SafetyConfig {
            audit_chain: crate::policy_audit_chain::AuditChainConfig {
                max_entries: 100,
                record_allows: true,
            },
            ..Default::default()
        };
        let mut engine = PolicyEngine::from_safety_config(&config);
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Human)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());

        assert_eq!(engine.audit_chain().len(), 1);
        let entry = engine.audit_chain().latest().unwrap();
        assert!(entry.description.starts_with("allow:"));
    }

    #[test]
    fn audit_chain_records_require_approval() {
        let mut engine = PolicyEngine::strict();
        // Destructive action by robot → require approval
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("git reset --hard");
        let decision = engine.authorize(&input);
        assert!(decision.requires_approval());

        assert_eq!(engine.audit_chain().len(), 1);
        let entry = engine.audit_chain().latest().unwrap();
        assert!(entry.description.starts_with("require_approval:"));
    }

    #[test]
    fn audit_chain_links_entries_correctly() {
        let mut engine = PolicyEngine::strict();

        // Generate two deny decisions
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::running());
        engine.authorize(&input);
        engine.authorize(&input);

        assert_eq!(engine.audit_chain().len(), 2);

        // Verify chain integrity
        let result = engine.audit_chain_mut().verify();
        assert!(result.valid, "audit chain should verify: {result}");
        assert_eq!(result.entries_checked, 2);
    }

    #[test]
    fn audit_chain_quarantine_deny_recorded() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine
            .quarantine_registry_mut()
            .quarantine(
                "pane-7",
                ComponentKind::Pane,
                QuarantineSeverity::Restricted,
                QuarantineReason::OperatorDirected {
                    operator: "admin".into(),
                    note: "review".into(),
                },
                "admin",
                1000,
                0,
            )
            .unwrap();

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(7)
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        assert!(decision.is_denied());

        // Quarantine deny should be in the audit chain
        assert_eq!(engine.audit_chain().len(), 1);
        let entry = engine.audit_chain().latest().unwrap();
        assert_eq!(entry.entity_ref, "policy.quarantine");
    }

    #[test]
    fn audit_chain_from_safety_config_max_entries() {
        let config = crate::config::SafetyConfig {
            audit_chain: crate::policy_audit_chain::AuditChainConfig {
                max_entries: 3,
                record_allows: true,
            },
            ..Default::default()
        };
        let mut engine = PolicyEngine::from_safety_config(&config);

        // Generate 5 allow decisions
        for _ in 0..5 {
            let input = PolicyInput::new(ActionKind::SendText, ActorKind::Human)
                .with_pane(1)
                .with_capabilities(PaneCapabilities::prompt());
            engine.authorize(&input);
        }

        // Bounded to max_entries
        assert_eq!(engine.audit_chain().len(), 3);
    }

    // ================================================================
    // Audited quarantine/kill-switch operation tests
    // ================================================================

    #[test]
    fn quarantine_component_records_to_audit_chain() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine
            .quarantine_component(
                "pane-10",
                ComponentKind::Pane,
                QuarantineSeverity::Restricted,
                QuarantineReason::OperatorDirected {
                    operator: "admin".into(),
                    note: "suspicious".into(),
                },
                "admin",
                5000,
                0,
            )
            .unwrap();

        // Should be in quarantine registry
        assert!(engine.quarantine_registry().is_quarantined("pane-10"));

        // Should be in audit chain
        assert_eq!(engine.audit_chain().len(), 1);
        let entry = engine.audit_chain().latest().unwrap();
        assert_eq!(entry.kind, AuditEntryKind::QuarantineAction);
        assert_eq!(entry.entity_ref, "pane-10");
        assert!(entry.description.contains("quarantined pane-10"));
    }

    #[test]
    fn release_component_records_to_audit_chain() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine
            .quarantine_component(
                "pane-11",
                ComponentKind::Pane,
                QuarantineSeverity::Restricted,
                QuarantineReason::OperatorDirected {
                    operator: "admin".into(),
                    note: "test".into(),
                },
                "admin",
                1000,
                0,
            )
            .unwrap();

        engine
            .release_component("pane-11", "admin", false, 2000)
            .unwrap();

        // Two entries: quarantine + release
        assert_eq!(engine.audit_chain().len(), 2);
        let entry = engine.audit_chain().latest().unwrap();
        assert_eq!(entry.kind, AuditEntryKind::QuarantineAction);
        assert!(
            entry
                .description
                .contains("released pane-11 from quarantine")
        );
    }

    #[test]
    fn trip_kill_switch_records_to_audit_chain() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine.trip_kill_switch(
            KillSwitchLevel::EmergencyHalt,
            "operator",
            "critical incident",
            3000,
        );

        assert!(engine.quarantine_registry().kill_switch().is_emergency());

        assert_eq!(engine.audit_chain().len(), 1);
        let entry = engine.audit_chain().latest().unwrap();
        assert_eq!(entry.kind, AuditEntryKind::KillSwitchAction);
        assert_eq!(entry.entity_ref, "kill_switch");
        assert!(entry.description.contains("emergency_halt"));
    }

    #[test]
    fn full_lifecycle_audit_chain_verifies() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        // Quarantine a component
        engine
            .quarantine_component(
                "pane-20",
                ComponentKind::Pane,
                QuarantineSeverity::Isolated,
                QuarantineReason::OperatorDirected {
                    operator: "admin".into(),
                    note: "audit".into(),
                },
                "admin",
                1000,
                0,
            )
            .unwrap();

        // Deny an action against quarantined pane
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(20)
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        assert!(decision.is_denied());

        // Release the component
        engine
            .release_component("pane-20", "admin", false, 3000)
            .unwrap();

        // Three entries: quarantine, deny, release
        assert_eq!(engine.audit_chain().len(), 3);

        // Chain should verify cleanly
        let result = engine.audit_chain_mut().verify();
        assert!(result.valid, "full lifecycle chain should verify: {result}");
        assert_eq!(result.entries_checked, 3);
    }

    // ================================================================
    // Compliance engine integration tests
    // ================================================================

    #[test]
    fn compliance_engine_initially_empty() {
        let engine = PolicyEngine::permissive();
        assert_eq!(
            engine.compliance_engine().compute_status(),
            crate::policy_compliance::ComplianceStatus::Compliant
        );
        assert_eq!(engine.compliance_engine().active_violation_count(), 0);
    }

    #[test]
    fn compliance_tracks_evaluations_from_authorize() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Human)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());

        engine.authorize(&input);
        engine.authorize(&input);
        engine.authorize(&input);

        let counters = engine.compliance_engine().counters();
        assert_eq!(counters.total_evaluations, 3);
        assert_eq!(counters.total_denials, 0);
    }

    #[test]
    fn compliance_tracks_denials_from_authorize() {
        let mut engine = PolicyEngine::strict();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::running());
        let decision = engine.authorize(&input);
        assert!(decision.is_denied());

        let counters = engine.compliance_engine().counters();
        assert_eq!(counters.total_evaluations, 1);
        assert_eq!(counters.total_denials, 1);
    }

    #[test]
    fn compliance_tracks_quarantine_from_audited_ops() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine
            .quarantine_component(
                "pane-30",
                ComponentKind::Pane,
                QuarantineSeverity::Restricted,
                QuarantineReason::OperatorDirected {
                    operator: "admin".into(),
                    note: "test".into(),
                },
                "admin",
                1000,
                0,
            )
            .unwrap();

        assert_eq!(engine.compliance_engine().counters().total_quarantines, 1);
    }

    #[test]
    fn compliance_tracks_kill_switch_trips() {
        use crate::policy_quarantine::*;
        let mut engine = PolicyEngine::permissive();

        engine.trip_kill_switch(KillSwitchLevel::SoftStop, "admin", "drill", 1000);

        assert_eq!(
            engine
                .compliance_engine()
                .counters()
                .total_kill_switch_trips,
            1
        );
    }

    #[test]
    fn compliance_from_safety_config() {
        let config = crate::config::SafetyConfig {
            compliance: crate::policy_compliance::ComplianceConfig {
                max_violations: 10,
                sla_threshold_ms: 1000,
            },
            ..Default::default()
        };
        let engine = PolicyEngine::from_safety_config(&config);
        assert_eq!(
            engine.compliance_engine().compute_status(),
            crate::policy_compliance::ComplianceStatus::Compliant,
        );
    }

    // ========================================================================
    // Credential Broker Integration Tests
    // ========================================================================

    #[test]
    fn credential_broker_default_in_policy_engine() {
        let engine = PolicyEngine::new(30, 100, true);
        // Broker exists and is empty by default
        assert!(engine.credential_broker().credential_ids().is_empty());
    }

    #[test]
    fn credential_broker_from_safety_config() {
        let config = crate::config::SafetyConfig {
            credential_broker: crate::connector_credential_broker::CredentialBrokerConfig {
                enabled: true,
                max_audit_events: 512,
                max_leases_per_connector: 5,
                max_sensitivity: CredentialSensitivity::Medium,
            },
            ..Default::default()
        };
        let engine = PolicyEngine::from_safety_config(&config);
        assert!(engine.credential_broker().credential_ids().is_empty());
        assert_eq!(
            engine.credential_broker_config.max_sensitivity,
            CredentialSensitivity::Medium,
        );
    }

    #[test]
    fn credential_broker_denies_unauthorized_connector_action() {
        let mut engine = PolicyEngine::permissive();
        // No access rules registered → connector credential action should be denied
        let input = PolicyInput {
            action: ActionKind::ConnectorCredentialAction,
            actor: ActorKind::Robot,
            surface: PolicySurface::Robot,
            pane_id: None,
            domain: Some("slack".to_string()),
            capabilities: PaneCapabilities::default(),
            text_summary: None,
            workflow_id: None,
            command_text: None,
            trauma_decision: None,
            pane_title: None,
            pane_cwd: None,
            agent_type: None,
            actor_namespace: None,
        };
        let scope = crate::connector_credential_broker::CredentialScope {
            provider: "slack".to_string(),
            resource: "channels/alerts".to_string(),
            operations: vec!["write".to_string()],
        };
        let decision = engine.authorize_connector_credential_action(
            &input,
            &scope,
            CredentialSensitivity::Medium,
        );
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("policy.credential_broker"));
    }

    #[test]
    fn credential_broker_allows_authorized_connector_action() {
        use crate::connector_credential_broker::{CredentialAccessRule, CredentialScope};

        let mut engine = PolicyEngine::permissive();
        // Register an access rule that permits "slack" connector at medium sensitivity
        engine
            .credential_broker_mut()
            .add_access_rule(CredentialAccessRule {
                rule_id: "test-slack".to_string(),
                connector_pattern: "slack".to_string(),
                permitted_scope: CredentialScope {
                    provider: "slack".to_string(),
                    resource: "*".to_string(),
                    operations: vec!["*".to_string()],
                },
                max_sensitivity: CredentialSensitivity::High,
                max_lease_ttl_ms: 0,
                max_concurrent_leases: 10,
            });

        // Use Human actor (trusted) to bypass destructive-action approval gate
        let input = PolicyInput {
            action: ActionKind::ConnectorCredentialAction,
            actor: ActorKind::Human,
            surface: PolicySurface::Robot,
            pane_id: None,
            domain: Some("slack".to_string()),
            capabilities: PaneCapabilities::default(),
            text_summary: None,
            workflow_id: None,
            command_text: None,
            trauma_decision: None,
            pane_title: None,
            pane_cwd: None,
            agent_type: None,
            actor_namespace: None,
        };
        let scope = CredentialScope {
            provider: "slack".to_string(),
            resource: "channels/alerts".to_string(),
            operations: vec!["write".to_string()],
        };
        let decision = engine.authorize_connector_credential_action(
            &input,
            &scope,
            CredentialSensitivity::Medium,
        );
        assert!(decision.is_allowed());
    }

    #[test]
    fn credential_broker_denial_feeds_compliance_and_audit() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput {
            action: ActionKind::ConnectorCredentialAction,
            actor: ActorKind::Robot,
            surface: PolicySurface::Robot,
            pane_id: None,
            domain: Some("github".to_string()),
            capabilities: PaneCapabilities::default(),
            text_summary: None,
            workflow_id: None,
            command_text: None,
            trauma_decision: None,
            pane_title: None,
            pane_cwd: None,
            agent_type: None,
            actor_namespace: None,
        };
        let scope = crate::connector_credential_broker::CredentialScope {
            provider: "github".to_string(),
            resource: "repos/frankenterm".to_string(),
            operations: vec!["admin".to_string()],
        };
        let _decision = engine.authorize_connector_credential_action(
            &input,
            &scope,
            CredentialSensitivity::High,
        );
        // Compliance engine should have recorded the evaluation
        let snap = engine.compliance_engine_mut().snapshot(1000);
        assert!(snap.counters.total_evaluations > 0);
        // Audit chain should have an entry
        assert!(!engine.audit_chain().is_empty());
    }

    #[test]
    fn credential_broker_high_sensitivity_requires_approval() {
        use crate::connector_credential_broker::{CredentialAccessRule, CredentialScope};

        let config = crate::config::SafetyConfig {
            credential_broker: crate::connector_credential_broker::CredentialBrokerConfig {
                enabled: true,
                max_sensitivity: CredentialSensitivity::Medium,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut engine = PolicyEngine::from_safety_config(&config);
        engine
            .credential_broker_mut()
            .add_access_rule(CredentialAccessRule {
                rule_id: "test-slack".to_string(),
                connector_pattern: "slack".to_string(),
                permitted_scope: CredentialScope {
                    provider: "slack".to_string(),
                    resource: "*".to_string(),
                    operations: vec!["*".to_string()],
                },
                max_sensitivity: CredentialSensitivity::Critical,
                max_lease_ttl_ms: 0,
                max_concurrent_leases: 10,
            });

        let input = PolicyInput {
            action: ActionKind::ConnectorCredentialAction,
            actor: ActorKind::Human,
            surface: PolicySurface::Robot,
            pane_id: None,
            domain: Some("slack".to_string()),
            capabilities: PaneCapabilities::default(),
            text_summary: None,
            workflow_id: None,
            command_text: None,
            trauma_decision: None,
            pane_title: None,
            pane_cwd: None,
            agent_type: None,
            actor_namespace: None,
        };
        let scope = CredentialScope {
            provider: "slack".to_string(),
            resource: "workspaces/main".to_string(),
            operations: vec!["admin".to_string()],
        };
        let decision = engine.authorize_connector_credential_action(
            &input,
            &scope,
            CredentialSensitivity::High,
        );
        assert_eq!(decision.rule_id(), Some("policy.credential_broker"));
        assert!(decision.requires_approval());
    }

    #[test]
    fn credential_broker_disabled_skips_check() {
        let config = crate::config::SafetyConfig {
            credential_broker: crate::connector_credential_broker::CredentialBrokerConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut engine = PolicyEngine::from_safety_config(&config);
        // No access rules → but broker is disabled, so it should pass through
        let input = PolicyInput {
            action: ActionKind::ConnectorCredentialAction,
            actor: ActorKind::Robot,
            surface: PolicySurface::Robot,
            pane_id: None,
            domain: Some("slack".to_string()),
            capabilities: PaneCapabilities::default(),
            text_summary: None,
            workflow_id: None,
            command_text: None,
            trauma_decision: None,
            pane_title: None,
            pane_cwd: None,
            agent_type: None,
            actor_namespace: None,
        };
        let decision = engine.authorize(&input);
        // Should not be denied by credential broker (may still hit rate limit, etc.)
        assert_ne!(decision.rule_id(), Some("policy.credential_broker"));
    }

    #[test]
    fn credential_broker_non_connector_action_bypasses_broker() {
        let mut engine = PolicyEngine::permissive();
        // SendText is not a connector action, should bypass broker entirely
        let input = PolicyInput {
            action: ActionKind::SendText,
            actor: ActorKind::Human,
            surface: PolicySurface::Robot,
            pane_id: Some(0),
            domain: None,
            capabilities: PaneCapabilities {
                prompt_active: true,
                ..Default::default()
            },
            text_summary: None,
            workflow_id: None,
            command_text: Some("echo hello".to_string()),
            trauma_decision: None,
            pane_title: None,
            pane_cwd: None,
            agent_type: None,
            actor_namespace: None,
        };
        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    // ── Lifecycle Manager integration tests ──────────────────────────

    #[test]
    fn lifecycle_manager_default_in_policy_engine() {
        let engine = PolicyEngine::new(30, 100, true);
        assert_eq!(engine.lifecycle_manager().count(), 0);
    }

    #[test]
    fn lifecycle_manager_from_safety_config() {
        use crate::connector_lifecycle::LifecycleManagerConfig;
        let config = crate::config::SafetyConfig {
            lifecycle_manager: LifecycleManagerConfig {
                max_managed_connectors: 128,
                ..Default::default()
            },
            ..Default::default()
        };
        let engine = PolicyEngine::from_safety_config(&config);
        assert_eq!(engine.lifecycle_manager().count(), 0);
    }

    #[test]
    fn lifecycle_manager_mut_accessible() {
        use crate::connector_host_runtime::ConnectorCapability;
        use crate::connector_lifecycle::{AdminState, LifecycleIntent};
        use crate::connector_registry::ConnectorManifest;
        let mut engine = PolicyEngine::permissive();
        let manifest = ConnectorManifest {
            schema_version: 1,
            package_id: "slack".to_string(),
            version: "1.0.0".to_string(),
            display_name: "Slack".to_string(),
            description: "test".to_string(),
            author: "test".to_string(),
            min_ft_version: None,
            sha256_digest: "a".repeat(64),
            required_capabilities: vec![ConnectorCapability::Invoke],
            publisher_signature: Some("sig".to_string()),
            transparency_token: None,
            created_at_ms: 1000,
            metadata: std::collections::BTreeMap::new(),
        };
        let result = engine
            .lifecycle_manager_mut()
            .execute(LifecycleIntent::Install { manifest }, 1000);
        assert!(result.is_ok());
        assert_eq!(engine.lifecycle_manager().count(), 1);
        let conn = engine.lifecycle_manager().get("slack").unwrap();
        assert_eq!(conn.admin_state, AdminState::Enabled);
    }

    // ── Data Classifier integration tests ────────────────────────────

    #[test]
    fn data_classifier_default_in_policy_engine() {
        let engine = PolicyEngine::new(30, 100, true);
        assert_eq!(engine.data_classifier().telemetry().total_events(), 0);
    }

    #[test]
    fn data_classifier_from_safety_config() {
        use crate::connector_data_classification::ClassifierConfig;
        let config = crate::config::SafetyConfig {
            data_classifier: ClassifierConfig {
                max_audit_entries: 500,
                redaction_marker: "[REDACTED]".to_string(),
                hash_salt: "custom-salt".to_string(),
                detailed_audit: true,
            },
            ..Default::default()
        };
        let engine = PolicyEngine::from_safety_config(&config);
        assert_eq!(engine.data_classifier().telemetry().total_events(), 0);
    }

    #[test]
    fn data_classifier_mut_accessible() {
        use crate::connector_data_classification::ClassificationPolicy;
        let mut engine = PolicyEngine::permissive();
        let policy = ClassificationPolicy {
            policy_id: "slack-policy".to_string(),
            connector_pattern: "slack".to_string(),
            ..Default::default()
        };
        engine.data_classifier_mut().register_policy(policy);
        // Verify the policy was registered (telemetry still zero, but no panic)
        assert_eq!(engine.data_classifier().telemetry().total_events(), 0);
    }

    // ── ConnectorGovernor integration tests ─────────────────────────

    #[test]
    fn connector_governor_default_in_policy_engine() {
        let engine = PolicyEngine::new(30, 100, true);
        let mut engine = engine;
        let snap = engine.connector_governor_mut().snapshot(1000);
        assert_eq!(snap.telemetry.evaluations, 0);
    }

    #[test]
    fn connector_governor_from_safety_config() {
        use crate::connector_governor::ConnectorGovernorConfig;
        let config = crate::config::SafetyConfig {
            connector_governor: ConnectorGovernorConfig {
                global_rate_limit: crate::connector_governor::TokenBucketConfig {
                    capacity: 500,
                    refill_rate: 50,
                    refill_interval_ms: 1000,
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let mut engine = PolicyEngine::from_safety_config(&config);
        let snap = engine.connector_governor_mut().snapshot(1000);
        assert_eq!(snap.telemetry.evaluations, 0);
    }

    #[test]
    fn connector_governor_mut_accessible() {
        use crate::connector_outbound_bridge::{ConnectorAction, ConnectorActionKind};
        let mut engine = PolicyEngine::permissive();
        let action = ConnectorAction {
            target_connector: "slack".to_string(),
            action_kind: ConnectorActionKind::Notify,
            correlation_id: "corr-test".to_string(),
            params: serde_json::json!({"test": true}),
            created_at_ms: 1000,
        };
        let decision = engine.connector_governor_mut().evaluate(&action, 1000);
        assert!(decision.is_allowed());
        let snap = engine.connector_governor_mut().snapshot(2000);
        assert_eq!(snap.telemetry.evaluations, 1);
        assert_eq!(snap.telemetry.allows, 1);
    }

    #[test]
    fn metrics_dashboard_reflects_governor() {
        use crate::connector_outbound_bridge::{ConnectorAction, ConnectorActionKind};
        let mut engine = PolicyEngine::permissive();
        let action = ConnectorAction {
            target_connector: "slack".to_string(),
            action_kind: ConnectorActionKind::Notify,
            correlation_id: "corr-test".to_string(),
            params: serde_json::json!({"test": true}),
            created_at_ms: 1000,
        };
        engine.connector_governor_mut().evaluate(&action, 1000);
        let dash = engine.metrics_dashboard(2000);
        let gov = &dash.subsystem_metrics["connector_governor"];
        assert_eq!(gov.evaluations, 1);
        assert_eq!(gov.denials, 0);
    }

    // ── ConnectorRegistry integration tests ──────────────────────────

    #[test]
    fn connector_registry_default_in_policy_engine() {
        let engine = PolicyEngine::new(30, 100, true);
        assert_eq!(engine.connector_registry().package_count(), 0);
    }

    #[test]
    fn connector_registry_from_safety_config() {
        use crate::connector_registry::ConnectorRegistryConfig;
        let config = crate::config::SafetyConfig {
            connector_registry: ConnectorRegistryConfig {
                max_packages: 512,
                ..Default::default()
            },
            ..Default::default()
        };
        let engine = PolicyEngine::from_safety_config(&config);
        assert_eq!(engine.connector_registry().package_count(), 0);
    }

    #[test]
    fn connector_registry_config_accessible() {
        use crate::connector_registry::ConnectorRegistryConfig;
        let config = crate::config::SafetyConfig {
            connector_registry: ConnectorRegistryConfig {
                max_packages: 128,
                enforce_transparency: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let engine = PolicyEngine::from_safety_config(&config);
        let reg_config = engine.connector_registry().config();
        assert_eq!(reg_config.max_packages, 128);
        assert!(reg_config.enforce_transparency);
    }

    // ── ConnectorHostRuntime integration tests ───────────────────────

    #[test]
    fn connector_host_runtime_default_in_policy_engine() {
        let engine = PolicyEngine::new(30, 100, true);
        assert_eq!(
            *engine.connector_host_runtime().state(),
            crate::connector_host_runtime::ConnectorLifecycleState::Stopped,
        );
    }

    #[test]
    fn connector_host_runtime_from_safety_config() {
        let config = crate::config::SafetyConfig {
            connector_host_runtime: crate::connector_host_runtime::ConnectorHostConfig {
                heartbeat_interval_ms: 5000,
                ..Default::default()
            },
            ..Default::default()
        };
        let engine = PolicyEngine::from_safety_config(&config);
        assert!(
            engine
                .connector_host_runtime()
                .transition_history()
                .is_empty()
        );
    }

    // ── ReliabilityRegistry integration tests ────────────────────────

    #[test]
    fn reliability_registry_default_in_policy_engine() {
        let engine = PolicyEngine::new(30, 100, true);
        // No controllers exist initially — get() returns None
        assert!(engine.reliability_registry().get("slack").is_none());
    }

    #[test]
    fn reliability_registry_mut_creates_controller() {
        let mut engine = PolicyEngine::permissive();
        // get_or_create should create a new controller
        let ctrl = engine.reliability_registry_mut().get_or_create("slack");
        assert_eq!(
            ctrl.circuit_status().state,
            crate::circuit_breaker::CircuitStateKind::Closed,
        );
        // Now the registry has one controller
        assert!(engine.reliability_registry().get("slack").is_some());
    }

    // =========================================================================
    // metrics_dashboard integration tests
    // =========================================================================

    #[test]
    fn metrics_dashboard_empty_engine_is_healthy() {
        let mut engine = PolicyEngine::permissive();
        let dash = engine.metrics_dashboard(1000);
        assert_eq!(
            dash.overall_health,
            crate::policy_metrics::HealthStatus::Healthy
        );
        assert_eq!(dash.counters.total_evaluations, 0);
        assert_eq!(dash.counters.total_denials, 0);
        assert_eq!(dash.counters.kill_switch_active, false);
        assert_eq!(dash.counters.audit_chain_valid, true);
    }

    #[test]
    fn metrics_dashboard_reflects_decision_log() {
        let mut engine = PolicyEngine::permissive();
        // Record some allow/deny decisions through the decision log
        engine.decision_log_mut().record(
            1000,
            ActionKind::SendText,
            ActorKind::Human,
            PolicySurface::Robot,
            Some(0),
            DecisionOutcome::Allow,
            None,
            Some("allowed action".to_string()),
            1,
        );
        engine.decision_log_mut().record(
            1001,
            ActionKind::SendText,
            ActorKind::Robot,
            PolicySurface::Robot,
            Some(0),
            DecisionOutcome::Deny,
            None,
            Some("denied action".to_string()),
            1,
        );
        let dash = engine.metrics_dashboard(2000);
        // Decision log subsystem should reflect the records
        let dl = &dash.subsystem_metrics["decision_log"];
        assert_eq!(dl.evaluations, 2);
        assert_eq!(dl.denials, 1);
    }

    #[test]
    fn metrics_dashboard_reflects_quarantine() {
        use crate::policy_quarantine::{ComponentKind, QuarantineReason, QuarantineSeverity};
        let mut engine = PolicyEngine::permissive();
        engine
            .quarantine_component(
                "bad-connector",
                ComponentKind::Connector,
                QuarantineSeverity::Isolated,
                QuarantineReason::OperatorDirected {
                    operator: "admin".to_string(),
                    note: "test quarantine".to_string(),
                },
                "admin",
                1000,
                u64::MAX,
            )
            .unwrap();
        let dash = engine.metrics_dashboard(2000);
        let q = &dash.subsystem_metrics["quarantine"];
        assert_eq!(q.active_quarantines, 1);
    }

    #[test]
    fn metrics_dashboard_reflects_kill_switch() {
        use crate::policy_quarantine::KillSwitchLevel;
        let mut engine = PolicyEngine::permissive();
        engine.quarantine_registry_mut().trip_kill_switch(
            KillSwitchLevel::HardStop,
            "admin",
            "emergency",
            1000,
        );
        let dash = engine.metrics_dashboard(2000);
        assert!(dash.counters.kill_switch_active);
        assert!(
            dash.overall_health >= crate::policy_metrics::HealthStatus::Critical,
            "expected Critical+ but got {:?}",
            dash.overall_health,
        );
    }

    #[test]
    fn metrics_dashboard_reflects_audit_chain() {
        let mut engine = PolicyEngine::permissive();
        // Append some audit entries
        engine.audit_chain_mut().append(
            crate::policy_audit_chain::AuditEntryKind::PolicyDecision,
            "test-component",
            "test decision",
            "test-actor",
            1000,
        );
        engine.audit_chain_mut().append(
            crate::policy_audit_chain::AuditEntryKind::PolicyDecision,
            "test-component",
            "test decision 2",
            "test-actor",
            2000,
        );
        let dash = engine.metrics_dashboard(3000);
        assert_eq!(dash.counters.audit_chain_length, 2);
        assert!(dash.counters.audit_chain_valid);
    }

    #[test]
    fn metrics_dashboard_with_custom_thresholds() {
        use crate::policy_metrics::PolicyMetricsThresholds;
        let mut engine = PolicyEngine::permissive();
        // Record 3 denials out of 10 = 30% denial rate
        for _ in 0..7 {
            engine.decision_log_mut().record(
                1000,
                ActionKind::SendText,
                ActorKind::Human,
                PolicySurface::Robot,
                Some(0),
                DecisionOutcome::Allow,
                None,
                Some("allowed".to_string()),
                1,
            );
        }
        for _ in 0..3 {
            engine.decision_log_mut().record(
                1000,
                ActionKind::SendText,
                ActorKind::Human,
                PolicySurface::Robot,
                Some(0),
                DecisionOutcome::Deny,
                None,
                Some("denied".to_string()),
                1,
            );
        }
        // With default thresholds (warning=10, critical=25), 30% should be critical
        let dash = engine.metrics_dashboard(2000);
        let dl = &dash.subsystem_metrics["decision_log"];
        assert_eq!(dl.denial_rate_pct, 30);

        // With raised thresholds, 30% should be just warning
        let custom = PolicyMetricsThresholds {
            denial_rate_warning_pct: 20,
            denial_rate_critical_pct: 50,
            ..PolicyMetricsThresholds::default()
        };
        let dash2 = engine.metrics_dashboard_with_thresholds(3000, custom);
        let denial_ind = dash2
            .indicators
            .iter()
            .find(|i| i.name == "denial_rate")
            .unwrap();
        assert_eq!(
            denial_ind.status,
            crate::policy_metrics::HealthStatus::Warning
        );
    }

    // =========================================================================
    // BundleRegistry integration tests
    // =========================================================================

    #[test]
    fn bundle_registry_accessible_from_engine() {
        let engine = PolicyEngine::permissive();
        assert!(engine.bundle_registry().is_empty());
        assert_eq!(engine.bundle_registry().len(), 0);
    }

    #[test]
    fn register_bundle_records_audit_chain() {
        use crate::connector_bundles::{
            BundleCategory, BundleConnectorEntry, BundleTier, ConnectorBundle,
        };

        let mut engine = PolicyEngine::permissive();
        let mut bundle = ConnectorBundle::new(
            "test-devtools",
            "Test DevTools",
            BundleTier::Tier1,
            BundleCategory::SourceControl,
            500,
        );
        bundle
            .connectors
            .push(BundleConnectorEntry::required("github", "GitHub"));

        let chain_before = engine.audit_chain().len();
        engine
            .register_bundle(bundle, "policy-admin", 1000)
            .unwrap();
        assert_eq!(engine.bundle_registry().len(), 1);
        assert!(engine.bundle_registry().get("test-devtools").is_some());
        assert!(engine.audit_chain().len() > chain_before);
    }

    #[test]
    fn remove_bundle_records_audit_chain() {
        use crate::connector_bundles::{
            BundleCategory, BundleConnectorEntry, BundleTier, ConnectorBundle,
        };

        let mut engine = PolicyEngine::permissive();
        let mut bundle = ConnectorBundle::new(
            "test-rm",
            "Remove Me",
            BundleTier::Tier2,
            BundleCategory::Messaging,
            500,
        );
        bundle
            .connectors
            .push(BundleConnectorEntry::required("slack", "Slack"));

        engine.register_bundle(bundle, "admin", 1000).unwrap();
        let chain_after_register = engine.audit_chain().len();
        let removed = engine.remove_bundle("test-rm", "admin", 2000).unwrap();
        assert_eq!(removed.bundle_id, "test-rm");
        assert!(engine.bundle_registry().is_empty());
        assert!(engine.audit_chain().len() > chain_after_register);
    }

    #[test]
    fn register_bundle_feeds_compliance() {
        use crate::connector_bundles::{
            BundleCategory, BundleConnectorEntry, BundleTier, ConnectorBundle,
        };

        let mut engine = PolicyEngine::permissive();
        let mut bundle = ConnectorBundle::new(
            "compliance-test",
            "Compliance Test",
            BundleTier::Tier1,
            BundleCategory::Monitoring,
            500,
        );
        bundle
            .connectors
            .push(BundleConnectorEntry::required("datadog", "Datadog"));

        let snap_before = engine.compliance_engine_mut().snapshot(1000);
        engine.register_bundle(bundle, "admin", 1000).unwrap();
        let snap_after = engine.compliance_engine_mut().snapshot(2000);
        assert!(snap_after.counters.total_evaluations > snap_before.counters.total_evaluations);
    }

    #[test]
    fn metrics_dashboard_reflects_bundle_registry() {
        use crate::connector_bundles::{
            BundleCategory, BundleConnectorEntry, BundleTier, ConnectorBundle,
        };

        let mut engine = PolicyEngine::permissive();
        let mut bundle = ConnectorBundle::new(
            "dash-test",
            "Dashboard Test",
            BundleTier::Tier1,
            BundleCategory::SourceControl,
            500,
        );
        bundle
            .connectors
            .push(BundleConnectorEntry::required("github", "GitHub"));

        engine.register_bundle(bundle, "admin", 1000).unwrap();
        let dash = engine.metrics_dashboard(2000);
        let br = &dash.subsystem_metrics["bundle_registry"];
        assert!(br.evaluations >= 1);
    }

    #[test]
    fn bundle_registry_from_safety_config() {
        use crate::connector_bundles::BundleRegistryConfig;

        let config = crate::config::SafetyConfig {
            bundle_registry: BundleRegistryConfig {
                max_bundles: 64,
                max_audit_entries: 128,
            },
            ..Default::default()
        };
        let engine = PolicyEngine::from_safety_config(&config);
        assert!(engine.bundle_registry().is_empty());
    }

    // =========================================================================
    // ConnectorMesh integration tests
    // =========================================================================

    #[test]
    fn connector_mesh_accessible_from_engine() {
        let engine = PolicyEngine::permissive();
        assert_eq!(
            engine
                .connector_mesh()
                .telemetry()
                .snapshot()
                .routing_requests,
            0
        );
    }

    #[test]
    fn connector_mesh_zone_management_through_engine() {
        use crate::connector_mesh::MeshZone;

        let mut engine = PolicyEngine::permissive();
        engine
            .connector_mesh_mut()
            .register_zone(MeshZone {
                zone_id: "us-east-1".to_string(),
                label: "US East".to_string(),
                priority: 1,
                active: true,
                metadata: std::collections::BTreeMap::new(),
            })
            .unwrap();
        let zones = engine.connector_mesh().zones();
        assert!(zones.iter().any(|z| z.zone_id == "us-east-1"));
    }

    #[test]
    fn connector_mesh_from_safety_config() {
        use crate::connector_mesh::ConnectorMeshConfig;

        let config = crate::config::SafetyConfig {
            connector_mesh: ConnectorMeshConfig {
                heartbeat_timeout_ms: 10_000,
                allow_cross_zone_fallback: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let engine = PolicyEngine::from_safety_config(&config);
        assert_eq!(
            engine
                .connector_mesh()
                .telemetry()
                .snapshot()
                .routing_requests,
            0
        );
    }

    #[test]
    fn metrics_dashboard_reflects_connector_mesh() {
        let mut engine = PolicyEngine::permissive();
        let dash = engine.metrics_dashboard(1000);
        let mesh = &dash.subsystem_metrics["connector_mesh"];
        assert_eq!(mesh.evaluations, 0);
        assert_eq!(mesh.denials, 0);
    }

    // =========================================================================
    // IngestionPipeline integration tests
    // =========================================================================

    #[test]
    fn ingestion_pipeline_accessible_from_engine() {
        let engine = PolicyEngine::permissive();
        assert_eq!(engine.ingestion_pipeline().telemetry().events_received, 0);
    }

    #[test]
    fn ingestion_pipeline_from_safety_config() {
        use crate::connector_bundles::IngestionPipelineConfig;

        let config = crate::config::SafetyConfig {
            ingestion_pipeline: IngestionPipelineConfig {
                max_ingest_per_sec: 100,
                max_audit_entries: 2048,
                ..Default::default()
            },
            ..Default::default()
        };
        let engine = PolicyEngine::from_safety_config(&config);
        assert_eq!(engine.ingestion_pipeline().telemetry().events_received, 0);
    }

    #[test]
    fn metrics_dashboard_reflects_ingestion_pipeline() {
        let mut engine = PolicyEngine::permissive();
        let dash = engine.metrics_dashboard(1000);
        let ip = &dash.subsystem_metrics["ingestion_pipeline"];
        assert_eq!(ip.evaluations, 0);
        assert_eq!(ip.denials, 0);
    }

    // =========================================================================
    // NamespaceIsolation integration tests
    // =========================================================================

    #[test]
    fn namespace_registry_accessible_from_engine() {
        let engine = PolicyEngine::permissive();
        assert_eq!(engine.namespace_registry().binding_count(), 0);
        // Namespace isolation is opt-in (disabled by default)
        assert!(!engine.namespace_isolation_enabled());
    }

    #[test]
    fn namespace_registry_from_safety_config() {
        use crate::namespace_isolation::{CrossTenantPolicy, NamespaceIsolationConfig};

        let config = crate::config::SafetyConfig {
            namespace_isolation: NamespaceIsolationConfig {
                enabled: true,
                cross_tenant_policy: CrossTenantPolicy::strict(),
                max_audit_entries: 512,
            },
            ..Default::default()
        };
        let engine = PolicyEngine::from_safety_config(&config);
        assert!(engine.namespace_isolation_enabled());
        assert_eq!(engine.namespace_registry().binding_count(), 0);
    }

    #[test]
    fn namespace_isolation_disabled_allows_all() {
        use crate::namespace_isolation::{
            NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let config = crate::config::SafetyConfig {
            namespace_isolation: NamespaceIsolationConfig::disabled(),
            ..Default::default()
        };
        let mut engine = PolicyEngine::from_safety_config(&config);
        assert!(!engine.namespace_isolation_enabled());

        // Bind pane to a specific namespace
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Pane,
            "0",
            TenantNamespace::new("org-a").unwrap(),
        );

        // Even with different domain, should be allowed because isolation is disabled
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(0)
            .with_domain("org-b")
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn namespace_isolation_denies_cross_tenant_access() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let config = crate::config::SafetyConfig {
            namespace_isolation: NamespaceIsolationConfig {
                enabled: true,
                cross_tenant_policy: CrossTenantPolicy::strict(),
                max_audit_entries: 1024,
            },
            ..Default::default()
        };
        let mut engine = PolicyEngine::from_safety_config(&config);

        // Bind pane 5 to org-a
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Pane,
            "5",
            TenantNamespace::new("org-a").unwrap(),
        );

        // Actor in org-b tries to read pane 5 in org-a → denied
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(5)
            .with_domain("org-b")
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        assert!(!decision.is_allowed());
        assert!(
            decision
                .context()
                .and_then(|c| c.determining_rule.as_deref())
                == Some("policy.namespace_isolation")
        );
    }

    #[test]
    fn namespace_isolation_allows_same_tenant_access() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let config = crate::config::SafetyConfig {
            namespace_isolation: NamespaceIsolationConfig {
                enabled: true,
                cross_tenant_policy: CrossTenantPolicy::strict(),
                max_audit_entries: 1024,
            },
            ..Default::default()
        };
        let mut engine = PolicyEngine::from_safety_config(&config);

        // Bind pane 3 to org-a
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Pane,
            "3",
            TenantNamespace::new("org-a").unwrap(),
        );

        // Actor in org-a reads pane 3 in org-a → allowed
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(3)
            .with_domain("org-a")
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn bind_resource_to_namespace_records_audit() {
        use crate::namespace_isolation::{NamespacedResourceKind, TenantNamespace};

        let mut engine = PolicyEngine::permissive();
        let prev = engine.bind_resource_to_namespace(
            NamespacedResourceKind::Pane,
            "42",
            TenantNamespace::new("org-x").unwrap(),
            "test-agent",
            1000,
        );
        assert!(prev.is_none());
        assert_eq!(engine.namespace_registry().binding_count(), 1);
        assert!(!engine.audit_chain().is_empty());
    }

    #[test]
    fn check_cross_tenant_access_records_audit() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let config = crate::config::SafetyConfig {
            namespace_isolation: NamespaceIsolationConfig {
                enabled: true,
                cross_tenant_policy: CrossTenantPolicy::strict(),
                max_audit_entries: 1024,
            },
            ..Default::default()
        };
        let mut engine = PolicyEngine::from_safety_config(&config);

        let src = TenantNamespace::new("team-a").unwrap();
        let tgt = TenantNamespace::new("team-b").unwrap();
        let result = engine.check_cross_tenant_access(
            &src,
            &tgt,
            NamespacedResourceKind::Connector,
            "slack-bot",
            "test-agent",
            2000,
        );

        // Strict policy → deny cross-tenant
        assert!(!result.is_allowed());
        assert!(result.crosses_boundary);
        // Audit chain should have recorded the denial
        assert!(!engine.audit_chain().is_empty());
    }

    #[test]
    fn metrics_dashboard_reflects_namespace_isolation() {
        use crate::namespace_isolation::{NamespacedResourceKind, TenantNamespace};

        let mut engine = PolicyEngine::permissive();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Pane,
            "1",
            TenantNamespace::default(),
        );
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Session,
            "s1",
            TenantNamespace::new("team-a").unwrap(),
        );

        let dash = engine.metrics_dashboard(1000);
        let ns = &dash.subsystem_metrics["namespace_isolation"];
        assert_eq!(ns.evaluations, 2); // 2 bindings
    }

    // ========================================================================
    // Namespace isolation — authorize() integration tests
    // ========================================================================

    #[test]
    fn authorize_allows_same_namespace_access() {
        use crate::namespace_isolation::{NamespacedResourceKind, TenantNamespace};

        let mut engine = PolicyEngine::permissive();
        let ns = TenantNamespace::new("team-a").unwrap();
        engine
            .namespace_registry_mut()
            .bind(NamespacedResourceKind::Pane, "pane-42", ns.clone());

        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(42)
            .with_namespace(ns);
        let decision = engine.authorize(&input);
        assert!(
            decision.is_allowed(),
            "same-namespace read access should be allowed: {:?}",
            decision.reason()
        );
    }

    #[test]
    fn authorize_denies_cross_namespace_with_strict_policy() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let actor_ns = TenantNamespace::new("team-a").unwrap();
        let target_ns = TenantNamespace::new("team-b").unwrap();
        engine
            .namespace_registry_mut()
            .bind(NamespacedResourceKind::Pane, "pane-10", target_ns);

        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(10)
            .with_namespace(actor_ns);
        let decision = engine.authorize(&input);
        assert!(
            decision.is_denied(),
            "cross-namespace access should be denied with strict policy"
        );
        assert!(
            decision
                .reason()
                .unwrap_or("")
                .contains("Cross-tenant access denied"),
            "denial reason should mention cross-tenant"
        );
    }

    #[test]
    fn authorize_allows_cross_namespace_with_permissive_policy() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::permissive(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let actor_ns = TenantNamespace::new("team-a").unwrap();
        let target_ns = TenantNamespace::new("team-b").unwrap();
        engine
            .namespace_registry_mut()
            .bind(NamespacedResourceKind::Pane, "pane-20", target_ns);

        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(20)
            .with_namespace(actor_ns);
        let decision = engine.authorize(&input);
        assert!(
            decision.is_allowed(),
            "cross-namespace read should be allowed with permissive policy: {:?}",
            decision.reason()
        );
    }

    #[test]
    fn authorize_skips_namespace_check_when_disabled() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: false,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let actor_ns = TenantNamespace::new("team-a").unwrap();
        let target_ns = TenantNamespace::new("team-b").unwrap();
        engine
            .namespace_registry_mut()
            .bind(NamespacedResourceKind::Pane, "pane-30", target_ns);

        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(30)
            .with_namespace(actor_ns);
        let decision = engine.authorize(&input);
        assert!(
            decision.is_allowed(),
            "namespace check should be skipped when disabled: {:?}",
            decision.reason()
        );
    }

    #[test]
    fn authorize_skips_namespace_check_without_actor_namespace() {
        use crate::namespace_isolation::{NamespacedResourceKind, TenantNamespace};

        let mut engine = PolicyEngine::permissive();
        let target_ns = TenantNamespace::new("team-b").unwrap();
        engine
            .namespace_registry_mut()
            .bind(NamespacedResourceKind::Pane, "5", target_ns);

        // No actor_namespace set — namespace check should be skipped
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot).with_pane(5);
        let decision = engine.authorize(&input);
        assert!(
            decision.is_allowed(),
            "namespace check should be skipped when actor has no namespace: {:?}",
            decision.reason()
        );
    }

    #[test]
    fn authorize_namespace_connector_action_denied_cross_tenant() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let actor_ns = TenantNamespace::new("org-a").unwrap();
        let connector_ns = TenantNamespace::new("org-b").unwrap();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Connector,
            "slack-webhook",
            connector_ns,
        );

        let input = PolicyInput::new(ActionKind::ConnectorNotify, ActorKind::Robot)
            .with_domain("slack-webhook")
            .with_namespace(actor_ns);
        let decision = engine.authorize(&input);
        assert!(
            decision.is_denied(),
            "cross-tenant connector access should be denied"
        );
    }

    #[test]
    fn authorize_namespace_audit_chain_records_denial() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let actor_ns = TenantNamespace::new("ns-x").unwrap();
        let target_ns = TenantNamespace::new("ns-y").unwrap();
        engine
            .namespace_registry_mut()
            .bind(NamespacedResourceKind::Pane, "pane-99", target_ns);

        let chain_len_before = engine.audit_chain().len();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(99)
            .with_namespace(actor_ns);
        let _decision = engine.authorize(&input);
        assert!(
            engine.audit_chain().len() > chain_len_before,
            "audit chain should record namespace denial"
        );
    }

    #[test]
    fn authorize_namespace_compliance_records_violation() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let actor_ns = TenantNamespace::new("tenant-1").unwrap();
        let target_ns = TenantNamespace::new("tenant-2").unwrap();
        engine
            .namespace_registry_mut()
            .bind(NamespacedResourceKind::Pane, "pane-77", target_ns);

        let violations_before = engine.compliance_engine().counters().total_denials;
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(77)
            .with_namespace(actor_ns);
        let _decision = engine.authorize(&input);
        assert!(
            engine.compliance_engine().counters().total_denials > violations_before,
            "compliance engine should record namespace violation"
        );
    }

    // ---- Namespace isolation: connector execution context tests ----

    #[test]
    fn authorize_namespace_same_tenant_connector_allowed() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let ns = TenantNamespace::new("org-alpha").unwrap();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Connector,
            "jira-webhook",
            ns.clone(),
        );

        let input = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
            .with_domain("jira-webhook")
            .with_namespace(ns);
        let decision = engine.authorize(&input);
        assert!(
            decision.is_allowed(),
            "same-tenant connector invoke should be allowed: {:?}",
            decision.reason()
        );
    }

    #[test]
    fn authorize_namespace_workflow_denied_cross_tenant() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let actor_ns = TenantNamespace::new("dept-eng").unwrap();
        let wf_ns = TenantNamespace::new("dept-sales").unwrap();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Workflow,
            "deploy-prod",
            wf_ns,
        );

        let input = PolicyInput::new(ActionKind::WorkflowRun, ActorKind::Robot)
            .with_workflow("deploy-prod")
            .with_namespace(actor_ns);
        let decision = engine.authorize(&input);
        assert!(
            decision.is_denied(),
            "cross-tenant workflow access should be denied with strict policy"
        );
    }

    #[test]
    fn authorize_namespace_workflow_same_tenant_allowed() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let ns = TenantNamespace::new("dept-eng").unwrap();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Workflow,
            "deploy-staging",
            ns.clone(),
        );

        let input = PolicyInput::new(ActionKind::WorkflowRun, ActorKind::Robot)
            .with_workflow("deploy-staging")
            .with_namespace(ns);
        let decision = engine.authorize(&input);
        assert!(
            decision.is_allowed(),
            "same-tenant workflow access should be allowed: {:?}",
            decision.reason()
        );
    }

    #[test]
    fn authorize_namespace_hierarchical_parent_to_child() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        // Permissive policy allows hierarchical access
        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::permissive(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let parent_ns = TenantNamespace::new("org").unwrap();
        let child_ns = TenantNamespace::new("org.team-alpha").unwrap();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Pane,
            "pane-200",
            child_ns,
        );

        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(200)
            .with_namespace(parent_ns);
        let decision = engine.authorize(&input);
        assert!(
            decision.is_allowed(),
            "parent namespace should access child resources with permissive policy: {:?}",
            decision.reason()
        );
    }

    #[test]
    fn authorize_namespace_connector_credential_cross_tenant_denied() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let actor_ns = TenantNamespace::new("team-red").unwrap();
        let cred_ns = TenantNamespace::new("team-blue").unwrap();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Connector,
            "github-api",
            cred_ns,
        );

        // ConnectorCredentialAction should be denied cross-tenant
        let input = PolicyInput::new(ActionKind::ConnectorCredentialAction, ActorKind::Robot)
            .with_domain("github-api")
            .with_namespace(actor_ns);
        let decision = engine.authorize(&input);
        assert!(
            decision.is_denied(),
            "cross-tenant credential action should be denied"
        );
    }

    #[test]
    fn authorize_namespace_all_connector_action_kinds_isolated() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let connector_actions = [
            ActionKind::ConnectorNotify,
            ActionKind::ConnectorTicket,
            ActionKind::ConnectorTriggerWorkflow,
            ActionKind::ConnectorAuditLog,
            ActionKind::ConnectorInvoke,
            ActionKind::ConnectorCredentialAction,
        ];

        for action in &connector_actions {
            let ns_config = NamespaceIsolationConfig {
                enabled: true,
                cross_tenant_policy: CrossTenantPolicy::strict(),
                ..Default::default()
            };
            let mut safety = crate::config::SafetyConfig::default();
            safety.namespace_isolation = ns_config;
            let mut engine = PolicyEngine::from_safety_config(&safety);

            let actor_ns = TenantNamespace::new("isolated-a").unwrap();
            let connector_ns = TenantNamespace::new("isolated-b").unwrap();
            engine.namespace_registry_mut().bind(
                NamespacedResourceKind::Connector,
                "test-connector",
                connector_ns,
            );

            let input = PolicyInput::new(*action, ActorKind::Robot)
                .with_domain("test-connector")
                .with_namespace(actor_ns);
            let decision = engine.authorize(&input);
            assert!(
                decision.is_denied(),
                "cross-tenant {action:?} should be denied with strict policy",
            );
        }
    }

    #[test]
    fn check_credential_namespace_denies_cross_tenant() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let actor_ns = TenantNamespace::new("team-x").unwrap();
        let cred_ns = TenantNamespace::new("team-y").unwrap();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Credential,
            "slack-bot-token",
            cred_ns,
        );

        let allowed = engine.check_credential_namespace(
            "slack-bot-token",
            &actor_ns,
            "agent-007",
            1000,
        );
        assert!(!allowed, "cross-tenant credential access should be denied");

        // Verify audit chain recorded the check
        assert!(
            !engine.audit_chain().is_empty(),
            "audit chain should record credential namespace check"
        );
    }

    #[test]
    fn check_credential_namespace_allows_same_tenant() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let ns = TenantNamespace::new("team-x").unwrap();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Credential,
            "my-token",
            ns.clone(),
        );

        let allowed = engine.check_credential_namespace(
            "my-token",
            &ns,
            "agent-007",
            1000,
        );
        assert!(allowed, "same-tenant credential access should be allowed");
    }

    #[test]
    fn register_bundle_in_namespace_binds_to_tenant() {
        use crate::connector_bundles::{
            BundleCategory, BundleConnectorEntry, BundleTier, ConnectorBundle,
        };
        use crate::namespace_isolation::{NamespacedResourceKind, TenantNamespace};

        let mut engine = PolicyEngine::permissive();

        let ns = TenantNamespace::new("acme-corp").unwrap();
        let bundle = ConnectorBundle::new(
            "acme-slack",
            "Acme Slack Connector",
            BundleTier::Tier1,
            BundleCategory::Messaging,
            5000,
        )
        .with_connector(BundleConnectorEntry::required("slack-hook", "Slack Webhook"));

        engine
            .register_bundle_in_namespace(bundle, &ns, "admin", 5000)
            .expect("registration should succeed");

        // Verify the bundle's connector is bound to the namespace
        let bound_ns = engine.namespace_registry().lookup(
            NamespacedResourceKind::Connector,
            "acme-slack",
        );
        assert_eq!(
            bound_ns.as_str(),
            "acme-corp",
            "bundle should be bound to the registering actor's namespace"
        );
    }

    #[test]
    fn namespace_isolation_e2e_multi_tenant_scenario() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        // E2e scenario: Two tenants with strict isolation, each with their own
        // panes, connectors, and workflows. Verify complete isolation.
        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let tenant_a = TenantNamespace::new("tenant-alpha").unwrap();
        let tenant_b = TenantNamespace::new("tenant-beta").unwrap();

        // Bind resources to tenant-alpha
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Pane,
            "pane-301",
            tenant_a.clone(),
        );
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Connector,
            "alpha-slack",
            tenant_a.clone(),
        );
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Workflow,
            "alpha-deploy",
            tenant_a.clone(),
        );
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Credential,
            "alpha-token",
            tenant_a.clone(),
        );

        // Bind resources to tenant-beta
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Pane,
            "pane-302",
            tenant_b.clone(),
        );
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Connector,
            "beta-jira",
            tenant_b.clone(),
        );

        // --- Tenant A accessing own resources: all allowed ---
        let input_a_pane = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(301)
            .with_namespace(tenant_a.clone());
        let decision_a_pane = engine.authorize(&input_a_pane);
        assert!(
            decision_a_pane.is_allowed(),
            "tenant-alpha should access own pane: {:?}",
            decision_a_pane.reason()
        );

        let input_a_conn = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
            .with_domain("alpha-slack")
            .with_namespace(tenant_a.clone());
        assert!(
            engine.authorize(&input_a_conn).is_allowed(),
            "tenant-alpha should access own connector"
        );

        let input_a_wf = PolicyInput::new(ActionKind::WorkflowRun, ActorKind::Robot)
            .with_workflow("alpha-deploy")
            .with_namespace(tenant_a.clone());
        assert!(
            engine.authorize(&input_a_wf).is_allowed(),
            "tenant-alpha should access own workflow"
        );

        // --- Tenant B accessing tenant A's resources: all denied ---
        let cross_pane = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(301)
            .with_namespace(tenant_b.clone());
        assert!(
            engine.authorize(&cross_pane).is_denied(),
            "tenant-beta must not access tenant-alpha's pane"
        );

        let cross_conn = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
            .with_domain("alpha-slack")
            .with_namespace(tenant_b.clone());
        assert!(
            engine.authorize(&cross_conn).is_denied(),
            "tenant-beta must not access tenant-alpha's connector"
        );

        let cross_wf = PolicyInput::new(ActionKind::WorkflowRun, ActorKind::Robot)
            .with_workflow("alpha-deploy")
            .with_namespace(tenant_b.clone());
        assert!(
            engine.authorize(&cross_wf).is_denied(),
            "tenant-beta must not access tenant-alpha's workflow"
        );

        // --- Tenant A accessing tenant B's resources: denied ---
        let input_a_b_conn = PolicyInput::new(ActionKind::ConnectorNotify, ActorKind::Robot)
            .with_domain("beta-jira")
            .with_namespace(tenant_a.clone());
        assert!(
            engine.authorize(&input_a_b_conn).is_denied(),
            "tenant-alpha must not access tenant-beta's connector"
        );

        // --- Verify audit chain recorded all denials ---
        let chain_len = engine.audit_chain().len();
        assert!(
            chain_len >= 4,
            "audit chain should record at least 4 cross-tenant denials, got {chain_len}"
        );

        // --- Verify compliance engine tracked violations ---
        let denials = engine.compliance_engine().counters().total_denials;
        assert!(
            denials >= 4,
            "compliance engine should have at least 4 denials, got {denials}"
        );

        // --- Verify namespace registry has correct binding count ---
        let snap = engine.namespace_registry().snapshot();
        assert_eq!(snap.total_bindings, 6, "should have 6 resource bindings");
        assert_eq!(
            snap.active_namespaces, 2,
            "should have 2 active namespaces"
        );
    }

    #[test]
    fn namespace_isolation_e2e_failure_recovery_scenario() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        // Failure injection scenario: Bind resource to wrong namespace,
        // detect violation, rebind to correct namespace, verify recovery.
        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let correct_ns = TenantNamespace::new("team-correct").unwrap();
        let wrong_ns = TenantNamespace::new("team-wrong").unwrap();

        // Step 1: Accidentally bind connector to wrong namespace
        engine.bind_resource_to_namespace(
            NamespacedResourceKind::Connector,
            "critical-api",
            wrong_ns,
            "admin",
            1000,
        );

        // Step 2: Actor from correct namespace tries to access — denied
        let input = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
            .with_domain("critical-api")
            .with_namespace(correct_ns.clone());
        let decision = engine.authorize(&input);
        assert!(
            decision.is_denied(),
            "access should be denied when connector bound to wrong namespace"
        );

        // Step 3: Detect the issue via audit chain
        let chain_len_before_fix = engine.audit_chain().len();
        assert!(
            chain_len_before_fix > 0,
            "audit chain should have recorded the denial"
        );

        // Step 4: Rebind connector to correct namespace (recovery)
        let prev = engine.bind_resource_to_namespace(
            NamespacedResourceKind::Connector,
            "critical-api",
            correct_ns.clone(),
            "admin",
            2000,
        );
        assert_eq!(
            prev.map(|ns| ns.as_str().to_string()),
            Some("team-wrong".to_string()),
            "should return previous namespace binding"
        );

        // Step 5: Access now succeeds
        let input2 = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
            .with_domain("critical-api")
            .with_namespace(correct_ns);
        let decision2 = engine.authorize(&input2);
        assert!(
            decision2.is_allowed(),
            "access should be allowed after rebinding: {:?}",
            decision2.reason()
        );

        // Step 6: Verify audit chain recorded both the denial and the rebinding
        assert!(
            engine.audit_chain().len() > chain_len_before_fix,
            "audit chain should have recorded the rebinding"
        );
    }

    // ── Approval Tracker unit tests ───────────────────────────────

    #[test]
    fn approval_tracker_submit_and_lookup() {
        let mut tracker = ApprovalTracker::default();
        let id = tracker.submit(
            "ConnectorInvoke",
            "agent-1",
            "slack-webhook",
            "sensitive action",
            "rule.sensitive",
            1000,
            2000,
        );
        assert!(id.starts_with("appr-"));
        assert_eq!(tracker.len(), 1);
        assert_eq!(tracker.pending().len(), 1);

        let entry = tracker.get(&id).unwrap();
        assert_eq!(entry.action, "ConnectorInvoke");
        assert_eq!(entry.actor, "agent-1");
        assert_eq!(entry.resource, "slack-webhook");
        assert_eq!(entry.status, ApprovalStatus::Pending);
    }

    #[test]
    fn approval_tracker_approve_reject_revoke() {
        let mut tracker = ApprovalTracker::default();
        let id1 = tracker.submit("a", "x", "r1", "reason", "rule", 100, 0);
        let id2 = tracker.submit("a", "x", "r2", "reason", "rule", 100, 0);
        let id3 = tracker.submit("a", "x", "r3", "reason", "rule", 100, 0);

        assert!(tracker.approve(&id1, "admin", 200));
        assert_eq!(tracker.get(&id1).unwrap().status, ApprovalStatus::Approved);
        assert_eq!(tracker.get(&id1).unwrap().decided_by, "admin");

        assert!(tracker.reject(&id2, "admin", 200));
        assert_eq!(tracker.get(&id2).unwrap().status, ApprovalStatus::Rejected);

        assert!(tracker.revoke(&id1, "admin", 300));
        assert_eq!(tracker.get(&id1).unwrap().status, ApprovalStatus::Revoked);

        assert!(!tracker.approve(&id2, "admin", 300));

        assert_eq!(tracker.get(&id3).unwrap().status, ApprovalStatus::Pending);
        assert_eq!(tracker.count_by_status(&ApprovalStatus::Pending), 1);
    }

    #[test]
    fn approval_tracker_expire_stale() {
        let mut tracker = ApprovalTracker::default();
        tracker.submit("a", "x", "r1", "reason", "rule", 100, 500);
        tracker.submit("a", "x", "r2", "reason", "rule", 100, 1000);
        tracker.submit("a", "x", "r3", "reason", "rule", 100, 0);

        let expired = tracker.expire_stale(600);
        assert_eq!(expired, 1);
        assert_eq!(tracker.count_by_status(&ApprovalStatus::Expired), 1);
        assert_eq!(tracker.count_by_status(&ApprovalStatus::Pending), 2);

        let expired2 = tracker.expire_stale(1100);
        assert_eq!(expired2, 1);
        assert_eq!(tracker.count_by_status(&ApprovalStatus::Pending), 1);
    }

    #[test]
    fn approval_tracker_eviction() {
        let mut tracker = ApprovalTracker::new(3);
        tracker.submit("a", "x", "r1", "r", "rule", 100, 0);
        tracker.submit("a", "x", "r2", "r", "rule", 200, 0);
        tracker.submit("a", "x", "r3", "r", "rule", 300, 0);
        assert_eq!(tracker.len(), 3);

        tracker.submit("a", "x", "r4", "r", "rule", 400, 0);
        assert_eq!(tracker.len(), 3);
        assert!(tracker.get("appr-1").is_none());
        assert!(tracker.get("appr-4").is_some());
    }

    #[test]
    fn approval_tracker_snapshot() {
        let mut tracker = ApprovalTracker::new(100);
        let id1 = tracker.submit("a", "x", "r1", "r", "rule", 100, 0);
        let id2 = tracker.submit("a", "x", "r2", "r", "rule", 100, 500);
        tracker.submit("a", "x", "r3", "r", "rule", 100, 0);

        tracker.approve(&id1, "admin", 200);
        tracker.expire_stale(600);

        let snap = tracker.snapshot();
        assert_eq!(snap.total, 3);
        assert_eq!(snap.approved, 1);
        assert_eq!(snap.expired, 1);
        assert_eq!(snap.pending, 1);

        assert!(!tracker.reject(&id2, "admin", 700));
    }

    // ── Revocation Registry unit tests ────────────────────────────

    #[test]
    fn revocation_registry_revoke_and_check() {
        let mut registry = RevocationRegistry::default();
        let rev_id = registry.revoke("connector", "slack-api", "compromised", "admin", 1000);
        assert!(rev_id.starts_with("rev-"));
        assert!(registry.is_revoked("connector", "slack-api"));
        assert!(!registry.is_revoked("connector", "jira-api"));
        assert_eq!(registry.active_count(), 1);
    }

    #[test]
    fn revocation_registry_reinstate() {
        let mut registry = RevocationRegistry::default();
        let rev_id = registry.revoke("credential", "my-token", "leaked", "admin", 1000);
        assert!(registry.is_revoked("credential", "my-token"));

        assert!(registry.reinstate(&rev_id));
        assert!(!registry.is_revoked("credential", "my-token"));
        assert_eq!(registry.active_count(), 0);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn revocation_registry_active_revocation() {
        let mut registry = RevocationRegistry::default();
        registry.revoke("session", "sess-42", "suspicious", "admin", 1000);

        let active = registry.active_revocation("session", "sess-42");
        assert!(active.is_some());
        assert_eq!(active.unwrap().reason, "suspicious");
        assert!(registry.active_revocation("session", "sess-99").is_none());
    }

    #[test]
    fn revocation_registry_eviction() {
        let mut registry = RevocationRegistry::new(2);
        registry.revoke("c", "r1", "r", "admin", 100);
        registry.revoke("c", "r2", "r", "admin", 200);
        assert_eq!(registry.len(), 2);

        registry.revoke("c", "r3", "r", "admin", 300);
        assert_eq!(registry.len(), 2);
        assert!(!registry.is_revoked("c", "r1"));
        assert!(registry.is_revoked("c", "r3"));
    }

    #[test]
    fn revocation_registry_snapshot() {
        let mut registry = RevocationRegistry::default();
        let rev1 = registry.revoke("connector", "c1", "r", "a", 100);
        registry.revoke("connector", "c2", "r", "a", 200);
        registry.reinstate(&rev1);

        let snap = registry.snapshot();
        assert_eq!(snap.total_records, 2);
        assert_eq!(snap.active_revocations, 1);
    }

    // ── PolicyEngine: approval workflow integration tests ─────────

    #[test]
    fn policy_engine_approval_workflow_submit_grant_revoke() {
        let mut engine = PolicyEngine::permissive();

        let approval_id = engine.submit_approval(
            "ConnectorInvoke",
            "agent-007",
            "slack-webhook",
            "high-risk connector",
            "policy.sensitive_action",
            1000,
            5000,
        );
        assert!(approval_id.starts_with("appr-"));
        assert_eq!(engine.approval_tracker().pending().len(), 1);

        let chain_before = engine.audit_chain().len();
        assert!(engine.grant_approval(&approval_id, "operator", 2000));
        assert!(engine.audit_chain().len() > chain_before);
        assert!(
            engine
                .approval_tracker()
                .get(&approval_id)
                .unwrap()
                .status
                .grants_access()
        );

        assert!(engine.revoke_approval(&approval_id, "operator", 3000));
        assert!(
            !engine
                .approval_tracker()
                .get(&approval_id)
                .unwrap()
                .status
                .grants_access()
        );
    }

    #[test]
    fn policy_engine_approval_reject_records_compliance() {
        let mut engine = PolicyEngine::permissive();

        let approval_id = engine.submit_approval(
            "DeleteFile",
            "agent-rogue",
            "/etc/shadow",
            "destructive action",
            "policy.destructive",
            1000,
            0,
        );

        let denials_before = engine.compliance_engine().counters().total_denials;
        engine.reject_approval(&approval_id, "operator", 2000);
        assert!(
            engine.compliance_engine().counters().total_denials > denials_before,
            "rejecting an approval should count as a compliance denial"
        );
    }

    // ── PolicyEngine: revocation integration tests ────────────────

    #[test]
    fn policy_engine_revoke_connector_blocks_authorize() {
        let mut engine = PolicyEngine::permissive();

        let input = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
            .with_domain("slack-api");
        assert!(
            engine.authorize(&input).is_allowed(),
            "connector should be allowed before revocation"
        );

        engine.revoke_resource("connector", "slack-api", "security incident", "admin", 1000);

        let decision2 = engine.authorize(&input);
        assert!(decision2.is_denied(), "should be denied after revocation");
        assert!(decision2.reason().unwrap_or("").contains("revoked"));
    }

    #[test]
    fn policy_engine_reinstate_connector_restores_access() {
        let mut engine = PolicyEngine::permissive();

        let rev_id =
            engine.revoke_resource("connector", "jira-api", "precautionary", "admin", 1000);

        let input = PolicyInput::new(ActionKind::ConnectorNotify, ActorKind::Robot)
            .with_domain("jira-api");
        assert!(engine.authorize(&input).is_denied());

        assert!(engine.reinstate_resource(&rev_id, "admin", 2000));
        assert!(
            engine.authorize(&input).is_allowed(),
            "should be allowed after reinstatement"
        );
    }

    #[test]
    fn policy_engine_revocation_audit_chain_records() {
        let mut engine = PolicyEngine::permissive();

        let chain_before = engine.audit_chain().len();
        let rev_id =
            engine.revoke_resource("credential", "api-key-12", "leaked in logs", "sec-bot", 1000);
        assert!(engine.audit_chain().len() > chain_before);

        let chain_before2 = engine.audit_chain().len();
        engine.reinstate_resource(&rev_id, "admin", 2000);
        assert!(engine.audit_chain().len() > chain_before2);
    }

    #[test]
    fn policy_engine_revocation_compliance_tracks_denial() {
        let mut engine = PolicyEngine::permissive();

        let denials_before = engine.compliance_engine().counters().total_denials;
        engine.revoke_resource("connector", "compromised-api", "incident", "admin", 1000);
        assert!(engine.compliance_engine().counters().total_denials > denials_before);
    }

    // ── E2e: Approval + Revocation combined scenario ──────────────

    #[test]
    fn e2e_approval_revocation_full_lifecycle() {
        let mut engine = PolicyEngine::permissive();

        // Phase 1: Connector working normally
        let input = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
            .with_domain("payment-gateway");
        assert!(engine.authorize(&input).is_allowed());

        // Phase 2: Security incident — revoke the connector
        let rev_id = engine.revoke_resource(
            "connector",
            "payment-gateway",
            "suspected credential compromise",
            "incident-commander",
            1000,
        );
        assert!(engine.authorize(&input).is_denied());

        // Phase 3: Submit approval for emergency access
        let approval_id = engine.submit_approval(
            "ConnectorInvoke",
            "oncall-engineer",
            "payment-gateway",
            "emergency refund processing",
            "policy.emergency_override",
            2000,
            3_600_000,
        );
        assert_eq!(engine.approval_tracker().pending().len(), 1);

        // Phase 4: Operator approves but connector still revoked
        engine.grant_approval(&approval_id, "incident-commander", 2500);
        assert!(
            engine.authorize(&input).is_denied(),
            "revoked connector should stay denied even with approval"
        );

        // Phase 5: Reinstate connector after credential rotation
        engine.reinstate_resource(&rev_id, "incident-commander", 3000);
        assert!(engine.authorize(&input).is_allowed());

        // Phase 6: Verify complete audit trail
        let chain_len = engine.audit_chain().len();
        assert!(
            chain_len >= 4,
            "audit chain should have revocation+approval+grant+reinstatement: got {chain_len}"
        );

        // Phase 7: Verify approval tracker state
        let snap = engine.approval_tracker().snapshot();
        assert_eq!(snap.total, 1);
        assert_eq!(snap.approved, 1);

        // Phase 8: Verify revocation registry state
        let rev_snap = engine.revocation_registry().snapshot();
        assert_eq!(rev_snap.total_records, 1);
        assert_eq!(rev_snap.active_revocations, 0);
    }

    // ── Forensic Report tests ─────────────────────────────────────

    #[test]
    fn forensic_report_empty_engine() {
        let mut engine = PolicyEngine::permissive();
        let query = ForensicQuery::default();
        let report = engine.generate_forensic_report(&query, 10000);

        assert_eq!(report.generated_at_ms, 10000);
        assert!(report.decisions.is_empty());
        assert!(report.revocations.is_empty());
        assert!(report.approvals.is_empty());
        assert!(report.namespace_violations.is_empty());
        assert!(report.quarantine_active.is_empty());
        assert!(!report.kill_switch_active);
        assert_eq!(report.compliance_summary.total_evaluations, 0);
    }

    #[test]
    fn forensic_report_captures_decisions() {
        let mut engine = PolicyEngine::permissive();

        // Generate some decisions
        let input1 = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot).with_pane(1);
        engine.authorize(&input1);

        let input2 = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Human).with_pane(2);
        engine.authorize(&input2);

        let query = ForensicQuery::default();
        let report = engine.generate_forensic_report(&query, 10000);

        assert!(
            !report.decisions.is_empty(),
            "should capture decisions from authorize calls"
        );
    }

    #[test]
    fn forensic_report_captures_revocations() {
        let mut engine = PolicyEngine::permissive();

        engine.revoke_resource("connector", "slack-api", "compromised", "admin", 1000);
        engine.revoke_resource("credential", "api-key", "leaked", "sec-bot", 2000);

        let query = ForensicQuery::default();
        let report = engine.generate_forensic_report(&query, 10000);

        assert_eq!(report.revocations.len(), 2);
        assert!(report.audit_trail.len() >= 2);
    }

    #[test]
    fn forensic_report_captures_approvals() {
        let mut engine = PolicyEngine::permissive();

        engine.submit_approval("ConnectorInvoke", "agent-1", "slack", "sensitive", "rule.1", 1000, 0);
        engine.submit_approval("DeleteFile", "agent-2", "/tmp/data", "destructive", "rule.2", 2000, 0);

        let query = ForensicQuery {
            start_ms: 0,
            end_ms: 5000,
            ..Default::default()
        };
        let report = engine.generate_forensic_report(&query, 5000);

        assert_eq!(report.approvals.len(), 2);
    }

    #[test]
    fn forensic_report_filters_by_time_range() {
        let mut engine = PolicyEngine::permissive();

        engine.submit_approval("a", "x", "r1", "r", "rule", 1000, 0);
        engine.submit_approval("a", "x", "r2", "r", "rule", 3000, 0);
        engine.submit_approval("a", "x", "r3", "r", "rule", 5000, 0);

        let query = ForensicQuery {
            start_ms: 2000,
            end_ms: 4000,
            ..Default::default()
        };
        let report = engine.generate_forensic_report(&query, 10000);

        assert_eq!(
            report.approvals.len(),
            1,
            "should only include approvals in time range"
        );
    }

    #[test]
    fn forensic_report_filters_denials_only() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        // Allow: same namespace
        let ns_a = TenantNamespace::new("team-a").unwrap();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Pane,
            "pane-500",
            ns_a.clone(),
        );
        let input_allow = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(500)
            .with_namespace(ns_a);
        engine.authorize(&input_allow);

        // Deny: cross namespace
        let ns_b = TenantNamespace::new("team-b").unwrap();
        let input_deny = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(500)
            .with_namespace(ns_b);
        engine.authorize(&input_deny);

        let query = ForensicQuery {
            denials_only: true,
            ..Default::default()
        };
        let report = engine.generate_forensic_report(&query, 10000);

        assert!(
            !report.decisions.is_empty(),
            "should have denial records"
        );
        for d in &report.decisions {
            assert_eq!(
                d.decision,
                crate::policy_decision_log::DecisionOutcome::Deny,
                "denials_only filter should exclude non-deny decisions"
            );
        }
    }

    #[test]
    fn forensic_report_captures_namespace_violations() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let ns_a = TenantNamespace::new("org-a").unwrap();
        let ns_b = TenantNamespace::new("org-b").unwrap();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Connector,
            "shared-connector",
            ns_b,
        );

        // This will trigger a cross-namespace check via authorize
        let input = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
            .with_domain("shared-connector")
            .with_namespace(ns_a);
        engine.authorize(&input);

        let query = ForensicQuery::default();
        let report = engine.generate_forensic_report(&query, 10000);

        assert!(
            !report.namespace_violations.is_empty(),
            "should capture namespace boundary violations"
        );
    }

    #[test]
    fn forensic_report_export_json() {
        let mut engine = PolicyEngine::permissive();
        engine.revoke_resource("connector", "test-api", "test reason", "admin", 1000);

        let query = ForensicQuery::default();
        let json = engine
            .export_forensic_report_json(&query, 5000)
            .expect("JSON export should succeed");

        assert!(json.contains("test-api"));
        assert!(json.contains("generated_at_ms"));
        // Verify it's valid JSON
        let _parsed: serde_json::Value =
            serde_json::from_str(&json).expect("should be valid JSON");
    }

    #[test]
    fn forensic_report_export_jsonl() {
        let mut engine = PolicyEngine::permissive();
        engine.submit_approval("a", "x", "r", "reason", "rule", 1000, 0);
        engine.revoke_resource("connector", "c", "r", "admin", 2000);

        let query = ForensicQuery::default();
        let jsonl = engine
            .export_forensic_report_jsonl(&query, 5000)
            .expect("JSONL export should succeed");

        let lines: Vec<&str> = jsonl.lines().collect();
        assert!(
            lines.len() >= 2,
            "should have at least approval + revocation + compliance summary lines"
        );
        // Each line should be valid JSON
        for line in &lines {
            let _: serde_json::Value =
                serde_json::from_str(line).expect("each JSONL line should be valid JSON");
        }
    }

    #[test]
    fn forensic_report_e2e_incident_reconstruction() {
        use crate::namespace_isolation::{
            CrossTenantPolicy, NamespaceIsolationConfig, NamespacedResourceKind, TenantNamespace,
        };

        // E2e scenario: Simulate a security incident and reconstruct it from
        // the forensic report.
        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = crate::config::SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let ns_legit = TenantNamespace::new("prod-team").unwrap();
        let ns_attacker = TenantNamespace::new("rogue-actor").unwrap();

        // Step 1: Normal operation — legit team accesses their connector
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Connector,
            "payment-api",
            ns_legit.clone(),
        );
        let legit_input = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
            .with_domain("payment-api")
            .with_namespace(ns_legit.clone());
        engine.authorize(&legit_input);

        // Step 2: Attack attempt — rogue actor tries cross-tenant access
        let attack_input = PolicyInput::new(ActionKind::ConnectorInvoke, ActorKind::Robot)
            .with_domain("payment-api")
            .with_namespace(ns_attacker);
        engine.authorize(&attack_input);

        // Step 3: Incident response — revoke the connector
        engine.revoke_resource(
            "connector",
            "payment-api",
            "unauthorized access detected",
            "incident-commander",
            3000,
        );

        // Step 4: Generate forensic report
        let query = ForensicQuery::default();
        let report = engine.generate_forensic_report(&query, 10000);

        // Verify the report captures the full incident chain
        assert!(
            !report.decisions.is_empty(),
            "should have decision records"
        );
        assert!(
            !report.audit_trail.is_empty(),
            "should have audit trail entries"
        );
        assert_eq!(report.revocations.len(), 1, "should have the revocation");
        assert!(
            !report.namespace_violations.is_empty(),
            "should have the cross-tenant violation"
        );
        assert!(
            report.compliance_summary.total_denials > 0,
            "should have compliance denials"
        );

        // Verify JSON export works
        let json = engine
            .export_forensic_report_json(&query, 10000)
            .expect("should export");
        assert!(json.contains("payment-api"));
        assert!(json.contains("unauthorized access detected"));
    }

    #[test]
    fn telemetry_snapshot_captures_all_subsystems() {
        let mut engine = PolicyEngine::new(10, 100, true);
        let now_ms = 1_700_000_000_000;
        let snap = engine.telemetry_snapshot(now_ms);

        assert_eq!(snap.captured_at_ms, now_ms);
        assert_eq!(snap.decision_log.current_entries, 0);
        assert_eq!(snap.quarantine.active_quarantines, 0);
        assert_eq!(snap.compliance.active_violations.len(), 0);
        assert!(snap.connector_reliability.is_empty());
        assert_eq!(snap.approval_tracker.total, 0);
        assert_eq!(snap.revocation_registry.total_records, 0);
        assert!(!snap.namespace_isolation_enabled);
    }

    #[test]
    fn telemetry_snapshot_serializes_to_json() {
        let mut engine = PolicyEngine::new(10, 100, true);
        let snap = engine.telemetry_snapshot(1_700_000_000_000);
        let json = serde_json::to_string(&snap).expect("should serialize");
        assert!(!json.is_empty());
        let val: serde_json::Value =
            serde_json::from_str(&json).expect("should be valid JSON");
        let obj = val.as_object().expect("should be object");
        assert!(obj.contains_key("decision_log"));
        assert!(obj.contains_key("quarantine"));
        assert!(obj.contains_key("audit_chain"));
        assert!(obj.contains_key("compliance"));
        assert!(obj.contains_key("connector_governor"));
        assert!(obj.contains_key("namespace_isolation_enabled"));
    }

    #[test]
    fn telemetry_snapshot_reflects_engine_state() {
        let mut engine = PolicyEngine::new(10, 100, true);
        let now_ms = 1_700_000_000_000;

        // Quarantine a component
        let _ = engine.quarantine_registry_mut().quarantine(
            "snap-test",
            crate::policy_quarantine::ComponentKind::Connector,
            crate::policy_quarantine::QuarantineSeverity::Restricted,
            crate::policy_quarantine::QuarantineReason::OperatorDirected {
                operator: "test".to_string(),
                note: "snapshot test".to_string(),
            },
            "test-op",
            now_ms,
            now_ms + 60_000,
        );

        let snap = engine.telemetry_snapshot(now_ms);
        assert_eq!(snap.quarantine.active_quarantines, 1);
    }
}
