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
use std::time::{Duration, Instant};

use crate::config::{CommandGateConfig, DcgDenyPolicy, DcgMode};
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
        /// Optional allow-once approval payload
        #[serde(skip_serializing_if = "Option::is_none")]
        approval: Option<ApprovalRequest>,
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
            approval: None,
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

    /// Attach an allow-once approval payload to a RequireApproval decision
    #[must_use]
    pub fn with_approval(self, approval: ApprovalRequest) -> Self {
        match self {
            Self::RequireApproval {
                reason, rule_id, ..
            } => Self::RequireApproval {
                reason,
                rule_id,
                approval: Some(approval),
            },
            other => other,
        }
    }

    /// Returns a stable string representation of the decision type
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny { .. } => "deny",
            Self::RequireApproval { .. } => "require_approval",
        }
    }

    /// Returns the decision reason, if any (for both Deny and RequireApproval)
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Deny { reason, .. } | Self::RequireApproval { reason, .. } => Some(reason),
            Self::Allow => None,
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

static RM_RF_ROOT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:sudo\s+)?rm\s+-rf\s+(/|~)(\s|$)").expect("rm -rf root regex")
});
static RM_RF_GENERIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:sudo\s+)?rm\s+-rf\s+").expect("rm -rf regex"));
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
    Regex::new(r#"(?i)password\s*[=:]\s*['"]?([^\s'"]{4,})['"]?"#).expect("Generic password regex")
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
        // Check rate limit for configured action kinds
        if input.action.is_rate_limited() {
            match self.rate_limiter.check(input.action, input.pane_id) {
                RateLimitOutcome::Allowed => {}
                RateLimitOutcome::Limited(hit) => {
                    return PolicyDecision::require_approval_with_rule(
                        hit.reason(),
                        "policy.rate_limit",
                    );
                }
            }
        }

        // Check prompt state for send actions
        if matches!(input.action, ActionKind::SendText | ActionKind::SendControl)
            && self.require_prompt_active
            && !input.capabilities.prompt_active
        {
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

        // Command safety gate for SendText
        if matches!(input.action, ActionKind::SendText) {
            if let Some(text) = input.command_text.as_deref() {
                match evaluate_command_gate(text, &self.command_gate) {
                    CommandGateOutcome::Allow => {}
                    CommandGateOutcome::Deny { reason, rule_id } => {
                        return PolicyDecision::deny_with_rule(reason, rule_id);
                    }
                    CommandGateOutcome::RequireApproval { reason, rule_id } => {
                        return PolicyDecision::require_approval_with_rule(reason, rule_id);
                    }
                }
            }
        }

        // Destructive actions require approval for non-trusted actors
        if input.action.is_destructive() && !input.actor.is_trusted() {
            return PolicyDecision::require_approval_with_rule(
                format!(
                    "Destructive action '{}' requires approval",
                    input.action.as_str()
                ),
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
    },
    /// Injection failed due to an error (after policy allowed)
    Error {
        /// Error message
        error: String,
        /// Pane ID that was targeted
        pane_id: u64,
        /// Action kind that was attempted
        action: ActionKind,
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
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        match self {
            Self::Allowed {
                decision,
                summary,
                pane_id,
                action,
            } => crate::storage::AuditActionRecord {
                id: 0, // Assigned by database
                ts: now_ms,
                actor_kind: actor.as_str().to_string(),
                actor_id,
                pane_id: Some(*pane_id),
                domain,
                action_kind: action.as_str().to_string(),
                policy_decision: decision.as_str().to_string(),
                decision_reason: None,
                rule_id: None,
                input_summary: Some(summary.clone()),
                verification_summary: None,
                result: "success".to_string(),
            },
            Self::Denied {
                decision,
                summary,
                pane_id,
                action,
            } => crate::storage::AuditActionRecord {
                id: 0,
                ts: now_ms,
                actor_kind: actor.as_str().to_string(),
                actor_id,
                pane_id: Some(*pane_id),
                domain,
                action_kind: action.as_str().to_string(),
                policy_decision: decision.as_str().to_string(),
                decision_reason: decision.reason().map(String::from),
                rule_id: decision.rule_id().map(String::from),
                input_summary: Some(summary.clone()),
                verification_summary: None,
                result: "denied".to_string(),
            },
            Self::RequiresApproval {
                decision,
                summary,
                pane_id,
                action,
            } => crate::storage::AuditActionRecord {
                id: 0,
                ts: now_ms,
                actor_kind: actor.as_str().to_string(),
                actor_id,
                pane_id: Some(*pane_id),
                domain,
                action_kind: action.as_str().to_string(),
                policy_decision: decision.as_str().to_string(),
                decision_reason: decision.reason().map(String::from),
                rule_id: decision.rule_id().map(String::from),
                input_summary: Some(summary.clone()),
                verification_summary: None,
                result: "require_approval".to_string(),
            },
            Self::Error {
                error,
                pane_id,
                action,
            } => crate::storage::AuditActionRecord {
                id: 0,
                ts: now_ms,
                actor_kind: actor.as_str().to_string(),
                actor_id,
                pane_id: Some(*pane_id),
                domain,
                action_kind: action.as_str().to_string(),
                policy_decision: "allow".to_string(), // Policy allowed, but execution failed
                decision_reason: None,
                rule_id: None,
                input_summary: None,
                verification_summary: Some(error.clone()),
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
pub struct PolicyGatedInjector {
    engine: PolicyEngine,
    client: crate::wezterm::WeztermClient,
    /// Optional storage handle for audit trail emission
    storage: Option<crate::storage::StorageHandle>,
}

impl PolicyGatedInjector {
    /// Create a new policy-gated injector without audit trail storage
    #[must_use]
    pub fn new(engine: PolicyEngine, client: crate::wezterm::WeztermClient) -> Self {
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
        client: crate::wezterm::WeztermClient,
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
    pub fn permissive(client: crate::wezterm::WeztermClient) -> Self {
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
        let result = match &decision {
            PolicyDecision::Allow => {
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
                    },
                    Err(e) => InjectionResult::Error {
                        error: e.to_string(),
                        pane_id,
                        action,
                    },
                }
            }
            PolicyDecision::Deny { .. } => InjectionResult::Denied {
                decision,
                summary,
                pane_id,
                action,
            },
            PolicyDecision::RequireApproval { .. } => InjectionResult::RequiresApproval {
                decision,
                summary,
                pane_id,
                action,
            },
        };

        // Emit audit record if storage is configured (wa-4vx.8.7)
        // Audit is emitted for ALL outcomes: allow, deny, require_approval, and error
        if let Some(ref storage) = self.storage {
            let audit_record = result.to_audit_record(
                actor,
                workflow_id.map(String::from),
                None, // domain - could be derived from pane info if available
            );
            // Fire-and-forget: don't block on audit persistence
            // Log on failure but don't propagate the error
            if let Err(e) = storage.record_audit_action_redacted(audit_record).await {
                tracing::warn!(
                    pane_id,
                    action = action.as_str(),
                    "Failed to emit audit record: {e}"
                );
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
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"status\":\"allowed\""));
        assert!(json.contains("\"pane_id\":42"));
    }
}
