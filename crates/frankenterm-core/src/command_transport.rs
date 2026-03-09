// =============================================================================
// Native mux command transport primitives (ft-3681t.2.3)
//
// Low-latency, policy-aware command transport for pane/session/fleet scopes.
// Builds on the lifecycle model (ft-3681t.2.1) to route commands only to
// entities in valid states.
// =============================================================================

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::policy::{ActionKind, ActorKind, PolicyDecision, PolicySurface};
use crate::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleRegistry, LifecycleState,
    MuxPaneLifecycleState,
};

// =============================================================================
// Command scope
// =============================================================================

/// Scope of a command: which entities it targets.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum CommandScope {
    /// Target a single pane by identity.
    Pane { identity: LifecycleIdentity },
    /// Target all panes in a window.
    Window { identity: LifecycleIdentity },
    /// Target all panes in a session.
    Session { identity: LifecycleIdentity },
    /// Target all panes in the fleet (all sessions in the registry).
    Fleet,
}

impl CommandScope {
    /// Build a pane-scoped command target.
    #[must_use]
    pub fn pane(identity: LifecycleIdentity) -> Self {
        Self::Pane { identity }
    }

    /// Build a window-scoped command target.
    #[must_use]
    pub fn window(identity: LifecycleIdentity) -> Self {
        Self::Window { identity }
    }

    /// Build a session-scoped command target.
    #[must_use]
    pub fn session(identity: LifecycleIdentity) -> Self {
        Self::Session { identity }
    }

    /// Build a fleet-wide command target.
    #[must_use]
    pub fn fleet() -> Self {
        Self::Fleet
    }

    /// Human-readable label for diagnostics.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Pane { identity } => format!("pane:{}", identity.stable_key()),
            Self::Window { identity } => format!("window:{}", identity.stable_key()),
            Self::Session { identity } => format!("session:{}", identity.stable_key()),
            Self::Fleet => "fleet:*".to_string(),
        }
    }
}

// =============================================================================
// Command kind
// =============================================================================

/// The kind of command to transport.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CommandKind {
    /// Send text input to the target pane(s).
    SendInput {
        text: String,
        /// If true, use bracketed paste mode for multi-line input.
        #[serde(default)]
        paste_mode: bool,
        /// If true, append a newline after the text.
        #[serde(default = "default_true")]
        append_newline: bool,
    },
    /// Send a control signal to the target pane(s).
    Interrupt { signal: InterruptSignal },
    /// Capture current output from the target pane(s).
    Capture {
        /// Number of lines from the tail to capture (0 = all available).
        #[serde(default)]
        tail_lines: u32,
        /// If true, include ANSI escape sequences.
        #[serde(default)]
        include_escapes: bool,
    },
    /// Broadcast a message to all targeted panes.
    Broadcast {
        text: String,
        /// If true, use bracketed paste mode.
        #[serde(default)]
        paste_mode: bool,
    },
    /// Acknowledge a previous command delivery.
    Acknowledge {
        /// The command ID being acknowledged.
        command_id: String,
        /// Outcome of the acknowledged command.
        outcome: AckOutcome,
    },
}

fn default_true() -> bool {
    true
}

/// Interrupt signal types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptSignal {
    /// Ctrl+C (SIGINT)
    CtrlC,
    /// Ctrl+D (EOF)
    CtrlD,
    /// Ctrl+Z (SIGTSTP)
    CtrlZ,
    /// Ctrl+\ (SIGQUIT)
    CtrlBackslash,
}

impl InterruptSignal {
    /// The actual byte(s) to send for this signal.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        match self {
            Self::CtrlC => b"\x03",
            Self::CtrlD => b"\x04",
            Self::CtrlZ => b"\x1a",
            Self::CtrlBackslash => b"\x1c",
        }
    }

    /// Human-readable label.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::CtrlC => "ctrl-c",
            Self::CtrlD => "ctrl-d",
            Self::CtrlZ => "ctrl-z",
            Self::CtrlBackslash => "ctrl-backslash",
        }
    }
}

/// Outcome of an acknowledged command.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckOutcome {
    Delivered,
    Failed { reason: String },
    Timeout,
}

// =============================================================================
// Command request / result
// =============================================================================

/// A command transport request with metadata for policy, tracing, and replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRequest {
    /// Unique command ID for correlation and deduplication.
    pub command_id: String,
    /// Which entities to target.
    pub scope: CommandScope,
    /// What to do.
    pub command: CommandKind,
    /// Caller context for policy/audit.
    pub context: CommandContext,
    /// If true, don't execute — just validate routing and policy.
    #[serde(default)]
    pub dry_run: bool,
}

/// Caller context attached to every command for policy evaluation and auditing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandContext {
    pub timestamp_ms: u64,
    pub component: String,
    pub correlation_id: String,
    /// Identity of the caller (agent/operator/robot).
    pub caller_identity: String,
    /// Optional reason for the command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional upstream policy trace metadata to carry into routing audit logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_trace: Option<CommandPolicyTrace>,
}

impl CommandContext {
    #[must_use]
    pub fn new(
        component: impl Into<String>,
        correlation_id: impl Into<String>,
        caller_identity: impl Into<String>,
    ) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            timestamp_ms,
            component: component.into(),
            correlation_id: correlation_id.into(),
            caller_identity: caller_identity.into(),
            reason: None,
            policy_trace: None,
        }
    }

    /// Set a reason for the command.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Create context with an explicit timestamp (for testing/replay).
    #[must_use]
    pub fn with_timestamp(mut self, timestamp_ms: u64) -> Self {
        self.timestamp_ms = timestamp_ms;
        self
    }

    /// Attach upstream policy trace metadata for audit preservation.
    #[must_use]
    pub fn with_policy_trace(mut self, policy_trace: CommandPolicyTrace) -> Self {
        self.policy_trace = Some(policy_trace);
        self
    }

    /// Build and attach policy trace metadata from a canonical policy decision.
    #[must_use]
    pub fn with_policy_decision(
        mut self,
        surface: PolicySurface,
        decision: &PolicyDecision,
    ) -> Self {
        self.policy_trace = Some(CommandPolicyTrace::from_surface_and_decision(
            surface, decision,
        ));
        self
    }
}

/// Structured policy metadata preserved alongside command-transport audit events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandPolicyTrace {
    pub decision: String,
    pub surface: PolicySurface,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<ActorKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ActionKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub determining_rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
}

impl CommandPolicyTrace {
    /// Preserve the explicit surface unless a non-unknown decision context surface exists.
    #[must_use]
    pub fn from_surface_and_decision(surface: PolicySurface, decision: &PolicyDecision) -> Self {
        let context = decision.context();
        let context_surface = context.map(|ctx| ctx.surface);
        let determining_rule = context.and_then(|ctx| ctx.determining_rule.clone());
        let surface = match context_surface {
            Some(PolicySurface::Unknown) | None => surface,
            Some(explicit) => explicit,
        };

        Self {
            decision: decision.as_str().to_string(),
            surface,
            actor: context.map(|ctx| ctx.actor),
            action: context.map(|ctx| ctx.action),
            reason: decision.reason().map(ToString::to_string),
            rule_id: decision.rule_id().map(ToString::to_string),
            determining_rule,
            pane_id: context.and_then(|ctx| ctx.pane_id),
            domain: context.and_then(|ctx| ctx.domain.clone()),
            workflow_id: context.and_then(|ctx| ctx.workflow_id.clone()),
        }
    }
}

/// Result of routing a command to one target pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDelivery {
    /// The target pane identity.
    pub target: LifecycleIdentity,
    /// Delivery status.
    pub status: DeliveryStatus,
    /// For capture commands, the captured text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_text: Option<String>,
}

/// Delivery status for a single target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum DeliveryStatus {
    /// Command was delivered (or would be in dry_run).
    Delivered,
    /// Command was skipped: target is not in a valid state.
    Skipped { reason: String },
    /// Command was rejected by policy.
    PolicyDenied { reason: String },
    /// Command routing failed.
    RoutingError { reason: String },
}

impl DeliveryStatus {
    #[must_use]
    pub fn is_delivered(&self) -> bool {
        matches!(self, Self::Delivered)
    }

    #[must_use]
    pub fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }

    #[must_use]
    pub fn is_policy_denied(&self) -> bool {
        matches!(self, Self::PolicyDenied { .. })
    }

    #[must_use]
    pub fn is_routing_error(&self) -> bool {
        matches!(self, Self::RoutingError { .. })
    }
}

/// Aggregate result for a command request (may target multiple panes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    /// The command ID echoed back for correlation.
    pub command_id: String,
    /// Per-target delivery outcomes.
    pub deliveries: Vec<CommandDelivery>,
    /// True if this was a dry run.
    pub dry_run: bool,
    /// Elapsed time in microseconds.
    pub elapsed_us: u64,
}

impl CommandResult {
    /// Count how many deliveries succeeded.
    #[must_use]
    pub fn delivered_count(&self) -> usize {
        self.deliveries
            .iter()
            .filter(|d| d.status.is_delivered())
            .count()
    }

    /// Count how many deliveries were skipped.
    #[must_use]
    pub fn skipped_count(&self) -> usize {
        self.deliveries
            .iter()
            .filter(|d| d.status.is_skipped())
            .count()
    }

    /// Count how many deliveries were rejected by policy.
    #[must_use]
    pub fn policy_denied_count(&self) -> usize {
        self.deliveries
            .iter()
            .filter(|d| d.status.is_policy_denied())
            .count()
    }

    /// Count how many deliveries failed due to routing errors.
    #[must_use]
    pub fn routing_error_count(&self) -> usize {
        self.deliveries
            .iter()
            .filter(|d| d.status.is_routing_error())
            .count()
    }

    /// True if all targets were delivered successfully.
    #[must_use]
    pub fn all_delivered(&self) -> bool {
        !self.deliveries.is_empty() && self.deliveries.iter().all(|d| d.status.is_delivered())
    }
}

// =============================================================================
// Transport error
// =============================================================================

/// Errors from the command transport layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandTransportError {
    /// The scope referenced an entity not in the registry.
    TargetNotFound { scope_label: String },
    /// The scope referenced a non-pane entity where a pane was expected.
    InvalidScopeKind {
        expected: LifecycleEntityKind,
        actual: LifecycleEntityKind,
    },
    /// Required context field is empty.
    InvalidContext { field: &'static str },
    /// No panes matched the scope.
    EmptyScope { scope_label: String },
}

impl std::fmt::Display for CommandTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetNotFound { scope_label } => {
                write!(f, "command transport target not found: {scope_label}")
            }
            Self::InvalidScopeKind { expected, actual } => {
                write!(
                    f,
                    "command transport scope kind mismatch: expected {} got {}",
                    expected.as_str(),
                    actual.as_str()
                )
            }
            Self::InvalidContext { field } => {
                write!(f, "command transport context missing field: {field}")
            }
            Self::EmptyScope { scope_label } => {
                write!(
                    f,
                    "command transport scope resolved to zero panes: {scope_label}"
                )
            }
        }
    }
}

impl std::error::Error for CommandTransportError {}

// =============================================================================
// Command router
// =============================================================================

/// Audit log entry for command transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAuditEntry {
    pub timestamp_ms: u64,
    pub command_id: String,
    pub scope_label: String,
    pub command_kind: String,
    pub caller_identity: String,
    pub component: String,
    pub correlation_id: String,
    pub target_count: usize,
    pub delivered_count: usize,
    pub skipped_count: usize,
    pub policy_denied_count: usize,
    pub routing_error_count: usize,
    pub dry_run: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_trace: Option<CommandPolicyTrace>,
}

/// Routes commands to lifecycle-managed panes with state validation and audit.
///
/// The router is stateless w.r.t. the transport itself — it reads lifecycle
/// state from the registry and produces delivery instructions. Actual I/O is
/// the caller's responsibility (pass deliveries to the backend adapter).
#[derive(Debug, Clone, Default)]
pub struct CommandRouter {
    audit_log: Vec<CommandAuditEntry>,
}

impl CommandRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The audit trail of routed commands.
    #[must_use]
    pub fn audit_log(&self) -> &[CommandAuditEntry] {
        &self.audit_log
    }

    /// Serialize the audit log for evidence capture.
    ///
    /// # Errors
    /// Returns an error if serialization fails.
    pub fn audit_log_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.audit_log)
    }

    /// Route a command request through the lifecycle registry.
    ///
    /// This resolves the scope to target panes, validates each target's
    /// lifecycle state, and produces delivery instructions. It does NOT
    /// execute I/O — the caller must dispatch the deliveries to the backend.
    ///
    /// # Errors
    /// Returns an error if the scope cannot be resolved or the context is
    /// invalid.
    pub fn route(
        &mut self,
        request: &CommandRequest,
        registry: &LifecycleRegistry,
    ) -> Result<CommandResult, CommandTransportError> {
        validate_command_context(&request.context)?;

        let start = SystemTime::now();

        // Resolve scope to target pane identities
        let targets = self.resolve_scope(&request.scope, registry)?;

        // Validate each target and build deliveries
        let mut deliveries = Vec::with_capacity(targets.len());
        for target in targets {
            let delivery = self.evaluate_target(&target, &request.command, registry);
            deliveries.push(delivery);
        }

        let elapsed_us = start.elapsed().unwrap_or_default().as_micros() as u64;

        let result = CommandResult {
            command_id: request.command_id.clone(),
            deliveries,
            dry_run: request.dry_run,
            elapsed_us,
        };

        // Audit log
        let command_kind = match &request.command {
            CommandKind::SendInput { .. } => "send_input",
            CommandKind::Interrupt { .. } => "interrupt",
            CommandKind::Capture { .. } => "capture",
            CommandKind::Broadcast { .. } => "broadcast",
            CommandKind::Acknowledge { .. } => "acknowledge",
        };

        self.audit_log.push(CommandAuditEntry {
            timestamp_ms: request.context.timestamp_ms,
            command_id: request.command_id.clone(),
            scope_label: request.scope.label(),
            command_kind: command_kind.to_string(),
            caller_identity: request.context.caller_identity.clone(),
            component: request.context.component.clone(),
            correlation_id: request.context.correlation_id.clone(),
            target_count: result.deliveries.len(),
            delivered_count: result.delivered_count(),
            skipped_count: result.skipped_count(),
            policy_denied_count: result.policy_denied_count(),
            routing_error_count: result.routing_error_count(),
            dry_run: request.dry_run,
            reason: request.context.reason.clone(),
            error: None,
            policy_trace: request.context.policy_trace.clone(),
        });

        tracing::info!(
            timestamp_ms = request.context.timestamp_ms,
            subsystem = "native_mux.command_transport",
            command_id = %request.command_id,
            scope = %request.scope.label(),
            command_kind,
            caller = %request.context.caller_identity,
            component = %request.context.component,
            correlation_id = %request.context.correlation_id,
            target_count = result.deliveries.len(),
            delivered = result.delivered_count(),
            skipped = result.skipped_count(),
            policy_denied = result.policy_denied_count(),
            routing_errors = result.routing_error_count(),
            policy_surface = request.context.policy_trace.as_ref().map(|trace| trace.surface.as_str()),
            policy_actor = request.context.policy_trace.as_ref().and_then(|trace| trace.actor.map(|actor| actor.as_str())),
            policy_action = request.context.policy_trace.as_ref().and_then(|trace| trace.action.map(|action| action.as_str())),
            policy_decision = request.context.policy_trace.as_ref().map(|trace| trace.decision.as_str()),
            policy_rule_id = request.context.policy_trace.as_ref().and_then(|trace| trace.rule_id.as_deref()),
            policy_determining_rule = request.context.policy_trace.as_ref().and_then(|trace| trace.determining_rule.as_deref()),
            policy_pane_id = request.context.policy_trace.as_ref().and_then(|trace| trace.pane_id),
            policy_domain = request.context.policy_trace.as_ref().and_then(|trace| trace.domain.as_deref()),
            policy_workflow_id = request.context.policy_trace.as_ref().and_then(|trace| trace.workflow_id.as_deref()),
            dry_run = request.dry_run,
            elapsed_us,
            "command routed"
        );

        Ok(result)
    }

    /// Resolve a command scope to a list of target pane identities.
    fn resolve_scope(
        &self,
        scope: &CommandScope,
        registry: &LifecycleRegistry,
    ) -> Result<Vec<LifecycleIdentity>, CommandTransportError> {
        let scope_label = scope.label();
        match scope {
            CommandScope::Pane { identity } => {
                if identity.kind != LifecycleEntityKind::Pane {
                    return Err(CommandTransportError::InvalidScopeKind {
                        expected: LifecycleEntityKind::Pane,
                        actual: identity.kind,
                    });
                }
                if registry.get(identity).is_none() {
                    return Err(CommandTransportError::TargetNotFound { scope_label });
                }
                Ok(vec![identity.clone()])
            }
            CommandScope::Window { identity } => {
                if identity.kind != LifecycleEntityKind::Window {
                    return Err(CommandTransportError::InvalidScopeKind {
                        expected: LifecycleEntityKind::Window,
                        actual: identity.kind,
                    });
                }
                if registry.get(identity).is_none() {
                    return Err(CommandTransportError::TargetNotFound { scope_label });
                }
                let panes = self.panes_in_container(identity, registry);
                if panes.is_empty() {
                    return Err(CommandTransportError::EmptyScope { scope_label });
                }
                Ok(panes)
            }
            CommandScope::Session { identity } => {
                if identity.kind != LifecycleEntityKind::Session {
                    return Err(CommandTransportError::InvalidScopeKind {
                        expected: LifecycleEntityKind::Session,
                        actual: identity.kind,
                    });
                }
                if registry.get(identity).is_none() {
                    return Err(CommandTransportError::TargetNotFound { scope_label });
                }
                let panes = self.panes_in_container(identity, registry);
                if panes.is_empty() {
                    return Err(CommandTransportError::EmptyScope { scope_label });
                }
                Ok(panes)
            }
            CommandScope::Fleet => {
                let panes = self.all_panes(registry);
                if panes.is_empty() {
                    return Err(CommandTransportError::EmptyScope { scope_label });
                }
                Ok(panes)
            }
        }
    }

    /// Find all pane identities belonging to a window or session container.
    ///
    /// Match by workspace_id + domain + generation (panes share these with
    /// their parent window/session when bootstrapped together).
    #[allow(clippy::unused_self)]
    fn panes_in_container(
        &self,
        container: &LifecycleIdentity,
        registry: &LifecycleRegistry,
    ) -> Vec<LifecycleIdentity> {
        let snapshot = registry.snapshot();
        snapshot
            .into_iter()
            .filter(|record| {
                record.identity.kind == LifecycleEntityKind::Pane
                    && record.identity.workspace_id == container.workspace_id
                    && record.identity.domain == container.domain
                    && record.identity.generation == container.generation
            })
            .map(|record| record.identity)
            .collect()
    }

    /// Collect all pane identities in the registry.
    #[allow(clippy::unused_self)]
    fn all_panes(&self, registry: &LifecycleRegistry) -> Vec<LifecycleIdentity> {
        let snapshot = registry.snapshot();
        snapshot
            .into_iter()
            .filter(|record| record.identity.kind == LifecycleEntityKind::Pane)
            .map(|record| record.identity)
            .collect()
    }

    /// Evaluate whether a command can be delivered to a specific pane.
    #[allow(clippy::unused_self)]
    fn evaluate_target(
        &self,
        target: &LifecycleIdentity,
        command: &CommandKind,
        registry: &LifecycleRegistry,
    ) -> CommandDelivery {
        let Some(record) = registry.get(target) else {
            return CommandDelivery {
                target: target.clone(),
                status: DeliveryStatus::RoutingError {
                    reason: format!("pane not found: {}", target.stable_key()),
                },
                captured_text: None,
            };
        };

        // Check lifecycle state allows the command
        match &record.state {
            LifecycleState::Pane(pane_state) => {
                if !is_pane_commandable(*pane_state, command) {
                    let state_label = LifecycleState::Pane(*pane_state).label();
                    return CommandDelivery {
                        target: target.clone(),
                        status: DeliveryStatus::Skipped {
                            reason: format!(
                                "pane in {state_label} state does not accept {:?}",
                                command_kind_label(command)
                            ),
                        },
                        captured_text: None,
                    };
                }
            }
            other => {
                return CommandDelivery {
                    target: target.clone(),
                    status: DeliveryStatus::RoutingError {
                        reason: format!("target is {} not pane", other.kind().as_str()),
                    },
                    captured_text: None,
                };
            }
        }

        CommandDelivery {
            target: target.clone(),
            status: DeliveryStatus::Delivered,
            captured_text: None,
        }
    }
}

// =============================================================================
// State validation helpers
// =============================================================================

/// Determine if a pane in the given lifecycle state can receive a command.
///
/// Rules:
/// - `Running` panes accept all commands (send, interrupt, capture, broadcast)
/// - `Ready` panes accept capture and acknowledge only
/// - `Draining` panes accept capture and interrupt only
/// - `Orphaned` panes accept capture only
/// - `Provisioning` and `Closed` panes reject everything
fn is_pane_commandable(state: MuxPaneLifecycleState, command: &CommandKind) -> bool {
    match state {
        MuxPaneLifecycleState::Running => true,
        MuxPaneLifecycleState::Ready => matches!(
            command,
            CommandKind::Capture { .. } | CommandKind::Acknowledge { .. }
        ),
        MuxPaneLifecycleState::Draining => matches!(
            command,
            CommandKind::Capture { .. } | CommandKind::Interrupt { .. }
        ),
        MuxPaneLifecycleState::Orphaned => matches!(command, CommandKind::Capture { .. }),
        MuxPaneLifecycleState::Provisioning | MuxPaneLifecycleState::Closed => false,
    }
}

fn command_kind_label(command: &CommandKind) -> &'static str {
    match command {
        CommandKind::SendInput { .. } => "send_input",
        CommandKind::Interrupt { .. } => "interrupt",
        CommandKind::Capture { .. } => "capture",
        CommandKind::Broadcast { .. } => "broadcast",
        CommandKind::Acknowledge { .. } => "acknowledge",
    }
}

fn validate_command_context(context: &CommandContext) -> Result<(), CommandTransportError> {
    if context.component.trim().is_empty() {
        return Err(CommandTransportError::InvalidContext { field: "component" });
    }
    if context.correlation_id.trim().is_empty() {
        return Err(CommandTransportError::InvalidContext {
            field: "correlation_id",
        });
    }
    if context.caller_identity.trim().is_empty() {
        return Err(CommandTransportError::InvalidContext {
            field: "caller_identity",
        });
    }
    Ok(())
}

// =============================================================================
// Deduplication tracker
// =============================================================================

/// Simple in-memory deduplication tracker for command IDs.
///
/// Prevents re-execution of commands with the same ID within a TTL window.
#[derive(Debug, Clone)]
pub struct CommandDeduplicator {
    seen: HashMap<String, u64>,
    ttl_ms: u64,
}

impl CommandDeduplicator {
    /// Create a deduplicator with the given TTL in milliseconds.
    #[must_use]
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            seen: HashMap::new(),
            ttl_ms,
        }
    }

    /// Check if a command ID has been seen within the TTL window.
    ///
    /// Returns `true` if this is a duplicate.
    pub fn is_duplicate(&mut self, command_id: &str, now_ms: u64) -> bool {
        // Evict expired entries
        self.seen
            .retain(|_, ts| now_ms.saturating_sub(*ts) < self.ttl_ms);

        if self.seen.contains_key(command_id) {
            true
        } else {
            self.seen.insert(command_id.to_string(), now_ms);
            false
        }
    }

    /// Number of tracked command IDs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// True if the tracker is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{
        ActionKind, ActorKind, DecisionContext, PolicyDecision, PolicySurface,
    };
    use crate::session_topology::{
        LifecycleEntityKind, LifecycleIdentity, LifecycleRegistry, LifecycleState,
        MuxPaneLifecycleState, SessionLifecycleState, WindowLifecycleState,
    };

    fn test_context(ts: u64) -> CommandContext {
        CommandContext {
            timestamp_ms: ts,
            component: "test".to_string(),
            correlation_id: "corr-1".to_string(),
            caller_identity: "agent-test".to_string(),
            reason: None,
            policy_trace: None,
        }
    }

    fn make_pane_identity(pane_id: u64) -> LifecycleIdentity {
        LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws1", "local", pane_id, 1)
    }

    fn make_window_identity(window_id: u64) -> LifecycleIdentity {
        LifecycleIdentity::new(LifecycleEntityKind::Window, "ws1", "local", window_id, 1)
    }

    fn make_session_identity(session_id: u64) -> LifecycleIdentity {
        LifecycleIdentity::new(LifecycleEntityKind::Session, "ws1", "local", session_id, 1)
    }

    fn seed_registry() -> LifecycleRegistry {
        let mut registry = LifecycleRegistry::new();
        // Session
        let _ = registry.register_entity(
            make_session_identity(0),
            LifecycleState::Session(SessionLifecycleState::Active),
            100,
        );
        // Window
        let _ = registry.register_entity(
            make_window_identity(0),
            LifecycleState::Window(WindowLifecycleState::Active),
            100,
        );
        // Running pane
        let _ = registry.register_entity(
            make_pane_identity(1),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            100,
        );
        // Ready pane
        let _ = registry.register_entity(
            make_pane_identity(2),
            LifecycleState::Pane(MuxPaneLifecycleState::Ready),
            100,
        );
        // Draining pane
        let _ = registry.register_entity(
            make_pane_identity(3),
            LifecycleState::Pane(MuxPaneLifecycleState::Draining),
            100,
        );
        // Orphaned pane
        let _ = registry.register_entity(
            make_pane_identity(4),
            LifecycleState::Pane(MuxPaneLifecycleState::Orphaned),
            100,
        );
        // Closed pane
        let _ = registry.register_entity(
            make_pane_identity(5),
            LifecycleState::Pane(MuxPaneLifecycleState::Closed),
            100,
        );
        // Provisioning pane
        let _ = registry.register_entity(
            make_pane_identity(6),
            LifecycleState::Pane(MuxPaneLifecycleState::Provisioning),
            100,
        );
        registry
    }

    fn send_input_cmd() -> CommandKind {
        CommandKind::SendInput {
            text: "hello".to_string(),
            paste_mode: false,
            append_newline: true,
        }
    }

    fn capture_cmd() -> CommandKind {
        CommandKind::Capture {
            tail_lines: 50,
            include_escapes: false,
        }
    }

    fn interrupt_cmd() -> CommandKind {
        CommandKind::Interrupt {
            signal: InterruptSignal::CtrlC,
        }
    }

    fn ack_cmd() -> CommandKind {
        CommandKind::Acknowledge {
            command_id: "prev-1".to_string(),
            outcome: AckOutcome::Delivered,
        }
    }

    // =========================================================================
    // Scope resolution tests
    // =========================================================================

    #[test]
    fn route_to_single_running_pane_delivers() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let request = CommandRequest {
            command_id: "cmd-1".to_string(),
            scope: CommandScope::pane(make_pane_identity(1)),
            command: send_input_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert_eq!(result.delivered_count(), 1);
        assert!(result.all_delivered());
        assert_eq!(result.command_id, "cmd-1");
    }

    #[test]
    fn route_to_nonexistent_pane_errors() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let request = CommandRequest {
            command_id: "cmd-2".to_string(),
            scope: CommandScope::pane(make_pane_identity(99)),
            command: send_input_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let err = router.route(&request, &registry).unwrap_err();
        assert!(matches!(err, CommandTransportError::TargetNotFound { .. }));
    }

    #[test]
    fn route_pane_scope_with_wrong_kind_errors() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let request = CommandRequest {
            command_id: "cmd-3".to_string(),
            scope: CommandScope::pane(make_window_identity(0)),
            command: send_input_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let err = router.route(&request, &registry).unwrap_err();
        assert!(matches!(
            err,
            CommandTransportError::InvalidScopeKind { .. }
        ));
    }

    #[test]
    fn route_to_window_resolves_panes() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let request = CommandRequest {
            command_id: "cmd-4".to_string(),
            scope: CommandScope::window(make_window_identity(0)),
            command: capture_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        // Should find all panes in the window (ws1, local, gen 1)
        assert!(!result.deliveries.is_empty());
    }

    #[test]
    fn route_to_session_resolves_panes() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let request = CommandRequest {
            command_id: "cmd-5".to_string(),
            scope: CommandScope::session(make_session_identity(0)),
            command: capture_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert!(!result.deliveries.is_empty());
    }

    #[test]
    fn route_fleet_targets_all_panes() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let request = CommandRequest {
            command_id: "cmd-6".to_string(),
            scope: CommandScope::fleet(),
            command: capture_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        // 6 panes registered (IDs 1-6)
        assert_eq!(result.deliveries.len(), 6);
    }

    // =========================================================================
    // Lifecycle state validation tests
    // =========================================================================

    #[test]
    fn running_pane_accepts_all_commands() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();

        for (i, cmd) in [send_input_cmd(), capture_cmd(), interrupt_cmd(), ack_cmd()]
            .into_iter()
            .enumerate()
        {
            let request = CommandRequest {
                command_id: format!("run-{i}"),
                scope: CommandScope::pane(make_pane_identity(1)),
                command: cmd,
                context: test_context(200),
                dry_run: false,
            };
            let result = router.route(&request, &registry).unwrap();
            assert!(
                result.all_delivered(),
                "Running pane should accept all commands"
            );
        }
    }

    #[test]
    fn ready_pane_accepts_only_capture_and_ack() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();

        // Capture: accepted
        let request = CommandRequest {
            command_id: "ready-cap".to_string(),
            scope: CommandScope::pane(make_pane_identity(2)),
            command: capture_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert!(result.all_delivered());

        // Ack: accepted
        let request = CommandRequest {
            command_id: "ready-ack".to_string(),
            scope: CommandScope::pane(make_pane_identity(2)),
            command: ack_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert!(result.all_delivered());

        // SendInput: skipped
        let request = CommandRequest {
            command_id: "ready-send".to_string(),
            scope: CommandScope::pane(make_pane_identity(2)),
            command: send_input_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert_eq!(result.skipped_count(), 1);

        // Interrupt: skipped
        let request = CommandRequest {
            command_id: "ready-int".to_string(),
            scope: CommandScope::pane(make_pane_identity(2)),
            command: interrupt_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert_eq!(result.skipped_count(), 1);
    }

    #[test]
    fn draining_pane_accepts_capture_and_interrupt() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();

        // Capture: accepted
        let request = CommandRequest {
            command_id: "drain-cap".to_string(),
            scope: CommandScope::pane(make_pane_identity(3)),
            command: capture_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert!(result.all_delivered());

        // Interrupt: accepted
        let request = CommandRequest {
            command_id: "drain-int".to_string(),
            scope: CommandScope::pane(make_pane_identity(3)),
            command: interrupt_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert!(result.all_delivered());

        // SendInput: skipped
        let request = CommandRequest {
            command_id: "drain-send".to_string(),
            scope: CommandScope::pane(make_pane_identity(3)),
            command: send_input_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert_eq!(result.skipped_count(), 1);
    }

    #[test]
    fn orphaned_pane_accepts_only_capture() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();

        // Capture: accepted
        let request = CommandRequest {
            command_id: "orphan-cap".to_string(),
            scope: CommandScope::pane(make_pane_identity(4)),
            command: capture_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert!(result.all_delivered());

        // SendInput: skipped
        let request = CommandRequest {
            command_id: "orphan-send".to_string(),
            scope: CommandScope::pane(make_pane_identity(4)),
            command: send_input_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert_eq!(result.skipped_count(), 1);

        // Interrupt: skipped
        let request = CommandRequest {
            command_id: "orphan-int".to_string(),
            scope: CommandScope::pane(make_pane_identity(4)),
            command: interrupt_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        assert_eq!(result.skipped_count(), 1);
    }

    #[test]
    fn closed_pane_rejects_all_commands() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();

        for (i, cmd) in [send_input_cmd(), capture_cmd(), interrupt_cmd(), ack_cmd()]
            .into_iter()
            .enumerate()
        {
            let request = CommandRequest {
                command_id: format!("closed-{i}"),
                scope: CommandScope::pane(make_pane_identity(5)),
                command: cmd,
                context: test_context(200),
                dry_run: false,
            };
            let result = router.route(&request, &registry).unwrap();
            assert_eq!(
                result.skipped_count(),
                1,
                "Closed pane should reject all commands"
            );
        }
    }

    #[test]
    fn provisioning_pane_rejects_all_commands() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();

        for (i, cmd) in [send_input_cmd(), capture_cmd(), interrupt_cmd()]
            .into_iter()
            .enumerate()
        {
            let request = CommandRequest {
                command_id: format!("prov-{i}"),
                scope: CommandScope::pane(make_pane_identity(6)),
                command: cmd,
                context: test_context(200),
                dry_run: false,
            };
            let result = router.route(&request, &registry).unwrap();
            assert_eq!(
                result.skipped_count(),
                1,
                "Provisioning pane should reject all commands"
            );
        }
    }

    // =========================================================================
    // Context validation tests
    // =========================================================================

    #[test]
    fn empty_component_rejected() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let mut ctx = test_context(200);
        ctx.component = "  ".to_string();
        let request = CommandRequest {
            command_id: "ctx-1".to_string(),
            scope: CommandScope::pane(make_pane_identity(1)),
            command: send_input_cmd(),
            context: ctx,
            dry_run: false,
        };
        let err = router.route(&request, &registry).unwrap_err();
        assert!(matches!(
            err,
            CommandTransportError::InvalidContext { field: "component" }
        ));
    }

    #[test]
    fn empty_correlation_id_rejected() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let mut ctx = test_context(200);
        ctx.correlation_id = String::new();
        let request = CommandRequest {
            command_id: "ctx-2".to_string(),
            scope: CommandScope::pane(make_pane_identity(1)),
            command: send_input_cmd(),
            context: ctx,
            dry_run: false,
        };
        let err = router.route(&request, &registry).unwrap_err();
        assert!(matches!(
            err,
            CommandTransportError::InvalidContext {
                field: "correlation_id"
            }
        ));
    }

    #[test]
    fn empty_caller_identity_rejected() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let mut ctx = test_context(200);
        ctx.caller_identity = String::new();
        let request = CommandRequest {
            command_id: "ctx-3".to_string(),
            scope: CommandScope::pane(make_pane_identity(1)),
            command: send_input_cmd(),
            context: ctx,
            dry_run: false,
        };
        let err = router.route(&request, &registry).unwrap_err();
        assert!(matches!(
            err,
            CommandTransportError::InvalidContext {
                field: "caller_identity"
            }
        ));
    }

    // =========================================================================
    // Dry run tests
    // =========================================================================

    #[test]
    fn dry_run_produces_deliveries_without_side_effects() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let request = CommandRequest {
            command_id: "dry-1".to_string(),
            scope: CommandScope::pane(make_pane_identity(1)),
            command: send_input_cmd(),
            context: test_context(200),
            dry_run: true,
        };
        let result = router.route(&request, &registry).unwrap();
        assert!(result.dry_run);
        assert!(result.all_delivered());
        // Audit log still records the dry run
        assert_eq!(router.audit_log().len(), 1);
        assert!(router.audit_log()[0].dry_run);
    }

    // =========================================================================
    // Audit log tests
    // =========================================================================

    #[test]
    fn audit_log_records_all_routes() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();

        for i in 0..3 {
            let request = CommandRequest {
                command_id: format!("audit-{i}"),
                scope: CommandScope::pane(make_pane_identity(1)),
                command: send_input_cmd(),
                context: test_context(200 + i),
                dry_run: false,
            };
            let _ = router.route(&request, &registry).unwrap();
        }

        assert_eq!(router.audit_log().len(), 3);
        assert_eq!(router.audit_log()[0].command_id, "audit-0");
        assert_eq!(router.audit_log()[1].command_id, "audit-1");
        assert_eq!(router.audit_log()[2].command_id, "audit-2");
    }

    #[test]
    fn audit_log_json_serializes() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let request = CommandRequest {
            command_id: "json-1".to_string(),
            scope: CommandScope::pane(make_pane_identity(1)),
            command: send_input_cmd(),
            context: test_context(200),
            dry_run: false,
        };
        let _ = router.route(&request, &registry).unwrap();
        let json = router.audit_log_json().unwrap();
        assert!(json.contains("json-1"));
        assert!(json.contains("send_input"));
    }

    #[test]
    fn command_policy_trace_prefers_non_unknown_decision_context_surface() {
        let mut ctx = DecisionContext::new_audit(
            42,
            ActionKind::SendText,
            ActorKind::Robot,
            PolicySurface::Swarm,
            Some(1),
            None,
            Some("route test".to_string()),
            None,
        );
        ctx.set_determining_rule("policy.route.test");
        let decision =
            PolicyDecision::deny_with_rule("blocked", "policy.route.test").with_context(ctx);

        let trace = CommandPolicyTrace::from_surface_and_decision(PolicySurface::Mux, &decision);

        assert_eq!(trace.decision, "deny");
        assert_eq!(trace.surface, PolicySurface::Swarm);
        assert_eq!(trace.actor, Some(ActorKind::Robot));
        assert_eq!(trace.action, Some(ActionKind::SendText));
        assert_eq!(trace.reason.as_deref(), Some("blocked"));
        assert_eq!(trace.rule_id.as_deref(), Some("policy.route.test"));
        assert_eq!(trace.determining_rule.as_deref(), Some("policy.route.test"));
        assert_eq!(trace.pane_id, Some(1));
        assert_eq!(trace.domain, None);
        assert_eq!(trace.workflow_id, None);
    }

    #[test]
    fn context_builder_attaches_policy_trace_without_decision_context() {
        let ctx = test_context(250).with_policy_decision(
            PolicySurface::Mux,
            &PolicyDecision::allow_with_rule("policy.route.allow"),
        );

        let trace = ctx.policy_trace.expect("policy trace must be attached");
        assert_eq!(trace.decision, "allow");
        assert_eq!(trace.surface, PolicySurface::Mux);
        assert_eq!(trace.rule_id.as_deref(), Some("policy.route.allow"));
        assert!(trace.actor.is_none());
        assert!(trace.action.is_none());
        assert!(trace.reason.is_none());
        assert!(trace.determining_rule.is_none());
        assert!(trace.pane_id.is_none());
        assert!(trace.domain.is_none());
        assert!(trace.workflow_id.is_none());
    }

    #[test]
    fn audit_log_preserves_policy_trace_and_full_delivery_breakdown() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let mut ctx = DecisionContext::new_audit(
            260,
            ActionKind::SendText,
            ActorKind::Workflow,
            PolicySurface::Swarm,
            Some(7),
            Some("local".to_string()),
            Some("attention all agents".to_string()),
            Some("wf-broadcast".to_string()),
        );
        ctx.set_determining_rule("policy.route.broadcast");
        let decision =
            PolicyDecision::allow_with_rule("policy.route.broadcast").with_context(ctx);
        let request = CommandRequest {
            command_id: "trace-1".to_string(),
            scope: CommandScope::fleet(),
            command: CommandKind::Broadcast {
                text: "attention all agents".to_string(),
                paste_mode: false,
            },
            context: test_context(260).with_policy_decision(PolicySurface::Swarm, &decision),
            dry_run: false,
        };

        let result = router.route(&request, &registry).unwrap();
        let audit = &router.audit_log()[0];

        assert_eq!(result.delivered_count(), 1);
        assert_eq!(result.skipped_count(), 5);
        assert_eq!(result.policy_denied_count(), 0);
        assert_eq!(result.routing_error_count(), 0);
        assert_eq!(audit.delivered_count, 1);
        assert_eq!(audit.skipped_count, 5);
        assert_eq!(audit.policy_denied_count, 0);
        assert_eq!(audit.routing_error_count, 0);
        let trace = audit
            .policy_trace
            .as_ref()
            .expect("audit trace must persist");
        assert_eq!(trace.surface, PolicySurface::Swarm);
        assert_eq!(trace.actor, Some(ActorKind::Workflow));
        assert_eq!(trace.action, Some(ActionKind::SendText));
        assert_eq!(trace.decision, "allow");
        assert_eq!(trace.rule_id.as_deref(), Some("policy.route.broadcast"));
        assert_eq!(
            trace.determining_rule.as_deref(),
            Some("policy.route.broadcast")
        );
        assert_eq!(trace.pane_id, Some(7));
        assert_eq!(trace.domain.as_deref(), Some("local"));
        assert_eq!(trace.workflow_id.as_deref(), Some("wf-broadcast"));
    }

    // =========================================================================
    // Fleet broadcast test
    // =========================================================================

    #[test]
    fn fleet_broadcast_delivers_to_running_skips_closed() {
        let registry = seed_registry();
        let mut router = CommandRouter::new();
        let request = CommandRequest {
            command_id: "fleet-1".to_string(),
            scope: CommandScope::fleet(),
            command: CommandKind::Broadcast {
                text: "attention all agents".to_string(),
                paste_mode: false,
            },
            context: test_context(200),
            dry_run: false,
        };
        let result = router.route(&request, &registry).unwrap();
        // 6 panes total; Running(1) accepts broadcast, others should skip/reject
        let delivered = result.delivered_count();
        let skipped = result.skipped_count();
        assert_eq!(delivered, 1, "Only Running pane should accept broadcast");
        assert_eq!(skipped, 5, "Non-running panes should be skipped");
    }

    // =========================================================================
    // Deduplicator tests
    // =========================================================================

    #[test]
    fn deduplicator_detects_duplicate_within_ttl() {
        let mut dedup = CommandDeduplicator::new(5000);
        assert!(!dedup.is_duplicate("cmd-1", 1000));
        assert!(dedup.is_duplicate("cmd-1", 2000));
        assert!(dedup.is_duplicate("cmd-1", 5999));
    }

    #[test]
    fn deduplicator_allows_after_ttl_expires() {
        let mut dedup = CommandDeduplicator::new(5000);
        assert!(!dedup.is_duplicate("cmd-1", 1000));
        // After TTL expires
        assert!(!dedup.is_duplicate("cmd-1", 6001));
    }

    #[test]
    fn deduplicator_tracks_independent_commands() {
        let mut dedup = CommandDeduplicator::new(5000);
        assert!(!dedup.is_duplicate("cmd-a", 1000));
        assert!(!dedup.is_duplicate("cmd-b", 1000));
        assert!(dedup.is_duplicate("cmd-a", 2000));
        assert!(dedup.is_duplicate("cmd-b", 2000));
        assert_eq!(dedup.len(), 2);
    }

    #[test]
    fn deduplicator_evicts_expired_entries() {
        let mut dedup = CommandDeduplicator::new(1000);
        assert!(!dedup.is_duplicate("old", 100));
        assert_eq!(dedup.len(), 1);
        // Check with a much later timestamp — triggers eviction
        assert!(!dedup.is_duplicate("new", 5000));
        // "old" should have been evicted
        assert_eq!(dedup.len(), 1);
    }

    // =========================================================================
    // InterruptSignal tests
    // =========================================================================

    #[test]
    fn interrupt_signal_bytes() {
        assert_eq!(InterruptSignal::CtrlC.as_bytes(), b"\x03");
        assert_eq!(InterruptSignal::CtrlD.as_bytes(), b"\x04");
        assert_eq!(InterruptSignal::CtrlZ.as_bytes(), b"\x1a");
        assert_eq!(InterruptSignal::CtrlBackslash.as_bytes(), b"\x1c");
    }

    #[test]
    fn interrupt_signal_labels() {
        assert_eq!(InterruptSignal::CtrlC.label(), "ctrl-c");
        assert_eq!(InterruptSignal::CtrlD.label(), "ctrl-d");
        assert_eq!(InterruptSignal::CtrlZ.label(), "ctrl-z");
        assert_eq!(InterruptSignal::CtrlBackslash.label(), "ctrl-backslash");
    }

    // =========================================================================
    // CommandResult helpers tests
    // =========================================================================

    #[test]
    fn command_result_counts() {
        let result = CommandResult {
            command_id: "test".to_string(),
            deliveries: vec![
                CommandDelivery {
                    target: make_pane_identity(1),
                    status: DeliveryStatus::Delivered,
                    captured_text: None,
                },
                CommandDelivery {
                    target: make_pane_identity(2),
                    status: DeliveryStatus::Skipped {
                        reason: "not running".to_string(),
                    },
                    captured_text: None,
                },
                CommandDelivery {
                    target: make_pane_identity(3),
                    status: DeliveryStatus::Delivered,
                    captured_text: None,
                },
            ],
            dry_run: false,
            elapsed_us: 100,
        };
        assert_eq!(result.delivered_count(), 2);
        assert_eq!(result.skipped_count(), 1);
        assert!(!result.all_delivered());
    }

    #[test]
    fn all_delivered_true_when_all_succeed() {
        let result = CommandResult {
            command_id: "test".to_string(),
            deliveries: vec![CommandDelivery {
                target: make_pane_identity(1),
                status: DeliveryStatus::Delivered,
                captured_text: None,
            }],
            dry_run: false,
            elapsed_us: 50,
        };
        assert!(result.all_delivered());
    }

    // =========================================================================
    // Serde roundtrip tests
    // =========================================================================

    #[test]
    fn command_request_serde_roundtrip() {
        let request = CommandRequest {
            command_id: "serde-1".to_string(),
            scope: CommandScope::pane(make_pane_identity(42)),
            command: CommandKind::SendInput {
                text: "ls -la".to_string(),
                paste_mode: true,
                append_newline: false,
            },
            context: test_context(999),
            dry_run: true,
        };
        let json = serde_json::to_string(&request).unwrap();
        let deserialized: CommandRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.command_id, "serde-1");
        assert!(deserialized.dry_run);
    }

    #[test]
    fn command_result_serde_roundtrip() {
        let result = CommandResult {
            command_id: "serde-2".to_string(),
            deliveries: vec![CommandDelivery {
                target: make_pane_identity(1),
                status: DeliveryStatus::Delivered,
                captured_text: Some("output here".to_string()),
            }],
            dry_run: false,
            elapsed_us: 42,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: CommandResult = serde_json::from_str(&json).unwrap();
        assert_eq!(
            deserialized.deliveries[0].captured_text.as_deref(),
            Some("output here")
        );
    }

    // =========================================================================
    // Scope label tests
    // =========================================================================

    #[test]
    fn scope_labels_are_descriptive() {
        let pane_scope = CommandScope::pane(make_pane_identity(42));
        assert!(pane_scope.label().contains("pane:"));
        assert!(pane_scope.label().contains("42"));

        let window_scope = CommandScope::window(make_window_identity(7));
        assert!(window_scope.label().contains("window:"));

        let fleet_scope = CommandScope::fleet();
        assert_eq!(fleet_scope.label(), "fleet:*");
    }

    // =========================================================================
    // Error display tests
    // =========================================================================

    #[test]
    fn error_display_messages() {
        let err = CommandTransportError::TargetNotFound {
            scope_label: "pane:ws1:local:pane:99:1".to_string(),
        };
        assert!(err.to_string().contains("target not found"));

        let err = CommandTransportError::InvalidScopeKind {
            expected: LifecycleEntityKind::Pane,
            actual: LifecycleEntityKind::Window,
        };
        assert!(err.to_string().contains("scope kind mismatch"));

        let err = CommandTransportError::InvalidContext { field: "component" };
        assert!(err.to_string().contains("component"));

        let err = CommandTransportError::EmptyScope {
            scope_label: "session:ws1:local:session:0:1".to_string(),
        };
        assert!(err.to_string().contains("zero panes"));
    }

    // =========================================================================
    // CommandContext builder tests
    // =========================================================================

    #[test]
    fn command_context_builder() {
        let ctx = CommandContext::new("robot", "corr-42", "agent-foo")
            .with_reason("test reason")
            .with_timestamp(12345);
        assert_eq!(ctx.component, "robot");
        assert_eq!(ctx.correlation_id, "corr-42");
        assert_eq!(ctx.caller_identity, "agent-foo");
        assert_eq!(ctx.reason.as_deref(), Some("test reason"));
        assert_eq!(ctx.timestamp_ms, 12345);
    }
}
