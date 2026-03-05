//! Outbound connector bridge: FrankenTerm events → connector actions.
//!
//! Routes mux/orchestration/policy events into connector-triggered actions
//! (notifications, ticketing, incident workflows, etc.) with policy-governed
//! deduplication, correlation, and sandbox enforcement.
//!
//! Key concerns:
//! - Event → action routing via configurable rules (pattern matching on event type/source)
//! - Deduplication via correlation IDs (bounded LRU cache, same as inbound bridge)
//! - Policy gate before action dispatch (fail-closed)
//! - Sandbox capability check before connector invocation
//! - Structured telemetry for end-to-end correlation
//!
//! Part of ft-3681t.5.6.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::connector_host_runtime::{ConnectorCapability, ConnectorSandboxZone};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the outbound connector bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorOutboundBridgeConfig {
    /// Maximum dedup cache entries before oldest are evicted.
    #[serde(default = "default_dedup_capacity")]
    pub dedup_capacity: usize,
    /// Time-to-live for dedup entries (seconds).
    #[serde(default = "default_dedup_ttl_secs")]
    pub dedup_ttl_secs: u64,
    /// Maximum pending actions in the dispatch queue before backpressure.
    #[serde(default = "default_dispatch_queue_capacity")]
    pub dispatch_queue_capacity: usize,
    /// Maximum dispatch history entries retained for audit.
    #[serde(default = "default_dispatch_history_capacity")]
    pub dispatch_history_capacity: usize,
    /// Whether to reject events with no matching routing rules (fail-closed).
    #[serde(default)]
    pub reject_unmatched_events: bool,
    /// Whether to enforce sandbox capability checks before dispatch.
    #[serde(default = "default_enforce_sandbox")]
    pub enforce_sandbox: bool,
}

fn default_dedup_capacity() -> usize {
    4096
}

fn default_dedup_ttl_secs() -> u64 {
    300
}

fn default_dispatch_queue_capacity() -> usize {
    1024
}

fn default_dispatch_history_capacity() -> usize {
    256
}

fn default_enforce_sandbox() -> bool {
    true
}

impl Default for ConnectorOutboundBridgeConfig {
    fn default() -> Self {
        Self {
            dedup_capacity: default_dedup_capacity(),
            dedup_ttl_secs: default_dedup_ttl_secs(),
            dispatch_queue_capacity: default_dispatch_queue_capacity(),
            dispatch_history_capacity: default_dispatch_history_capacity(),
            reject_unmatched_events: false,
            enforce_sandbox: default_enforce_sandbox(),
        }
    }
}

// =============================================================================
// Event source types
// =============================================================================

/// Source classification for events entering the outbound bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundEventSource {
    /// Pattern detection (trigger rule matched in a pane).
    PatternDetected,
    /// Pane lifecycle (discovered, disappeared).
    PaneLifecycle,
    /// Workflow lifecycle (started, step, completed).
    WorkflowLifecycle,
    /// User-initiated event (e.g., ft robot command).
    UserAction,
    /// Policy decision (approval/denial escalation).
    PolicyDecision,
    /// Health/metrics threshold crossed.
    HealthAlert,
    /// Custom/plugin-defined event source.
    Custom,
}

impl OutboundEventSource {
    /// Stable string label for rule matching and correlation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PatternDetected => "pattern_detected",
            Self::PaneLifecycle => "pane_lifecycle",
            Self::WorkflowLifecycle => "workflow_lifecycle",
            Self::UserAction => "user_action",
            Self::PolicyDecision => "policy_decision",
            Self::HealthAlert => "health_alert",
            Self::Custom => "custom",
        }
    }
}

impl std::fmt::Display for OutboundEventSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// =============================================================================
// Outbound action types
// =============================================================================

/// Kind of action to dispatch to a connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorActionKind {
    /// Send a notification (Slack message, email, webhook POST).
    Notify,
    /// Create or update an external ticket (Jira, Linear, GitHub issue).
    Ticket,
    /// Trigger an external workflow/pipeline (CI/CD, runbook).
    TriggerWorkflow,
    /// Log/audit event to external sink (Datadog, Splunk).
    AuditLog,
    /// Invoke a generic connector action (custom RPC).
    Invoke,
    /// Revoke/rotate a credential via secret provider.
    CredentialAction,
}

impl ConnectorActionKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Notify => "notify",
            Self::Ticket => "ticket",
            Self::TriggerWorkflow => "trigger_workflow",
            Self::AuditLog => "audit_log",
            Self::Invoke => "invoke",
            Self::CredentialAction => "credential_action",
        }
    }

    /// Map action kind to the required connector capability.
    #[must_use]
    pub const fn required_capability(self) -> ConnectorCapability {
        match self {
            Self::Notify | Self::AuditLog => ConnectorCapability::NetworkEgress,
            Self::Ticket | Self::TriggerWorkflow => ConnectorCapability::Invoke,
            Self::Invoke => ConnectorCapability::Invoke,
            Self::CredentialAction => ConnectorCapability::SecretBroker,
        }
    }
}

impl std::fmt::Display for ConnectorActionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// =============================================================================
// Outbound event (bridge input)
// =============================================================================

/// An event entering the outbound bridge for potential connector dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundEvent {
    /// Source classification.
    pub source: OutboundEventSource,
    /// Stable event type key (e.g., "pattern.ci_failure", "pane.disappeared").
    pub event_type: String,
    /// Opaque correlation ID for deduplication and tracing.
    pub correlation_id: Option<String>,
    /// Timestamp at source (millis since epoch).
    pub timestamp_ms: u64,
    /// Pane ID (if event is pane-scoped).
    pub pane_id: Option<u64>,
    /// Workflow ID (if event is workflow-scoped).
    pub workflow_id: Option<String>,
    /// Structured payload from the event.
    pub payload: serde_json::Value,
    /// Severity hint for routing priority.
    pub severity: OutboundSeverity,
}

/// Severity levels for outbound events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundSeverity {
    Info,
    Warning,
    Critical,
}

impl Default for OutboundSeverity {
    fn default() -> Self {
        Self::Info
    }
}

impl OutboundEvent {
    /// Create a new outbound event.
    #[must_use]
    pub fn new(
        source: OutboundEventSource,
        event_type: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            source,
            event_type: event_type.into(),
            correlation_id: None,
            timestamp_ms: now_ms,
            pane_id: None,
            workflow_id: None,
            payload,
            severity: OutboundSeverity::default(),
        }
    }

    /// Set the correlation ID.
    #[must_use]
    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    /// Set the pane ID.
    #[must_use]
    pub fn with_pane_id(mut self, pane_id: u64) -> Self {
        self.pane_id = Some(pane_id);
        self
    }

    /// Set the workflow ID.
    #[must_use]
    pub fn with_workflow_id(mut self, workflow_id: impl Into<String>) -> Self {
        self.workflow_id = Some(workflow_id.into());
        self
    }

    /// Set the timestamp.
    #[must_use]
    pub fn with_timestamp_ms(mut self, ts: u64) -> Self {
        self.timestamp_ms = ts;
        self
    }

    /// Set the severity.
    #[must_use]
    pub fn with_severity(mut self, severity: OutboundSeverity) -> Self {
        self.severity = severity;
        self
    }
}

// =============================================================================
// Connector action (bridge output)
// =============================================================================

/// A concrete action to dispatch to a target connector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorAction {
    /// Target connector identifier (e.g., "slack", "github", "datadog").
    pub target_connector: String,
    /// Kind of action.
    pub action_kind: ConnectorActionKind,
    /// Correlation ID linking back to the source event.
    pub correlation_id: String,
    /// Action parameters (connector-specific).
    pub params: serde_json::Value,
    /// Timestamp when the action was created.
    pub created_at_ms: u64,
}

// =============================================================================
// Routing rules
// =============================================================================

/// A routing rule that maps events to connector actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundRoutingRule {
    /// Unique rule identifier.
    pub rule_id: String,
    /// Event source filter (matches all if None).
    pub source_filter: Option<OutboundEventSource>,
    /// Event type prefix filter (matches all if None).
    /// Supports prefix matching: "pattern." matches "pattern.ci_failure".
    pub event_type_prefix: Option<String>,
    /// Minimum severity required to trigger this rule.
    pub min_severity: Option<OutboundSeverity>,
    /// Target connector for the action.
    pub target_connector: String,
    /// Kind of action to dispatch.
    pub action_kind: ConnectorActionKind,
    /// Whether this rule is enabled.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Priority (lower = higher priority). Used for ordering when multiple rules match.
    #[serde(default)]
    pub priority: u32,
}

fn default_enabled() -> bool {
    true
}

impl OutboundRoutingRule {
    /// Check if this rule matches the given event.
    #[must_use]
    pub fn matches(&self, event: &OutboundEvent) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(source) = self.source_filter {
            if event.source != source {
                return false;
            }
        }
        if let Some(ref prefix) = self.event_type_prefix {
            if !event.event_type.starts_with(prefix) {
                return false;
            }
        }
        if let Some(min_sev) = self.min_severity {
            if severity_rank(event.severity) < severity_rank(min_sev) {
                return false;
            }
        }
        true
    }
}

/// Numeric rank for severity comparison.
fn severity_rank(s: OutboundSeverity) -> u8 {
    match s {
        OutboundSeverity::Info => 0,
        OutboundSeverity::Warning => 1,
        OutboundSeverity::Critical => 2,
    }
}

// =============================================================================
// Dispatch result
// =============================================================================

/// Outcome of processing a single event through the outbound bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundDispatchResult {
    /// Correlation ID for the event.
    pub correlation_id: String,
    /// Whether the event was deduplicated (skipped).
    pub deduplicated: bool,
    /// Actions that were dispatched (or queued for dispatch).
    pub actions_dispatched: Vec<DispatchedAction>,
    /// Actions that were blocked by policy or sandbox.
    pub actions_blocked: Vec<BlockedAction>,
}

/// Record of a successfully dispatched action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchedAction {
    pub rule_id: String,
    pub target_connector: String,
    pub action_kind: ConnectorActionKind,
    pub correlation_id: String,
}

/// Record of a blocked action with reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockedAction {
    pub rule_id: String,
    pub target_connector: String,
    pub action_kind: ConnectorActionKind,
    pub reason: String,
}

// =============================================================================
// Errors
// =============================================================================

/// Errors from the outbound connector bridge.
#[derive(Debug, Error)]
pub enum OutboundBridgeError {
    #[error("duplicate event (correlation_id={0})")]
    DuplicateEvent(String),
    #[error("no matching routing rules for event type: {0}")]
    NoMatchingRules(String),
    #[error("dispatch queue full (capacity={0})")]
    DispatchQueueFull(usize),
    #[error("sandbox violation: {capability} denied for connector {connector} (reason: {reason})")]
    SandboxViolation {
        connector: String,
        capability: String,
        reason: String,
    },
    #[error("all matched rules blocked by policy")]
    AllRulesBlocked,
}

// =============================================================================
// Deduplicator (reuses pattern from inbound bridge)
// =============================================================================

/// Entry in the dedup cache.
#[derive(Debug, Clone)]
struct DedupEntry {
    correlation_id: String,
    inserted_at_ms: u64,
}

/// Bounded, TTL-aware deduplicator for outbound events.
#[derive(Debug)]
pub struct OutboundDeduplicator {
    entries: VecDeque<DedupEntry>,
    capacity: usize,
    ttl_ms: u64,
}

impl OutboundDeduplicator {
    /// Create a new deduplicator.
    #[must_use]
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(4096)),
            capacity,
            ttl_ms: ttl.as_millis() as u64,
        }
    }

    /// Check if a correlation_id is new (not seen within TTL).
    /// If new, records it and returns `true`.
    /// If duplicate, returns `false`.
    pub fn check_and_record(&mut self, correlation_id: &str, now_ms: u64) -> bool {
        self.evict_expired(now_ms);

        if self
            .entries
            .iter()
            .any(|e| e.correlation_id == correlation_id)
        {
            return false;
        }

        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }

        self.entries.push_back(DedupEntry {
            correlation_id: correlation_id.to_string(),
            inserted_at_ms: now_ms,
        });
        true
    }

    fn evict_expired(&mut self, now_ms: u64) {
        while let Some(front) = self.entries.front() {
            if now_ms.saturating_sub(front.inserted_at_ms) > self.ttl_ms {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }

    /// Number of entries currently tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the dedup cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// =============================================================================
// Telemetry
// =============================================================================

/// Telemetry counters for the outbound bridge.
#[derive(Debug, Default)]
pub struct OutboundBridgeTelemetry {
    pub events_received: u64,
    pub events_routed: u64,
    pub events_deduplicated: u64,
    pub events_unmatched: u64,
    pub actions_dispatched: u64,
    pub actions_blocked_policy: u64,
    pub actions_blocked_sandbox: u64,
    pub dispatch_queue_overflows: u64,
}

/// Serializable telemetry snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutboundBridgeTelemetrySnapshot {
    pub events_received: u64,
    pub events_routed: u64,
    pub events_deduplicated: u64,
    pub events_unmatched: u64,
    pub actions_dispatched: u64,
    pub actions_blocked_policy: u64,
    pub actions_blocked_sandbox: u64,
    pub dispatch_queue_overflows: u64,
}

impl OutboundBridgeTelemetry {
    /// Take a snapshot.
    #[must_use]
    pub fn snapshot(&self) -> OutboundBridgeTelemetrySnapshot {
        OutboundBridgeTelemetrySnapshot {
            events_received: self.events_received,
            events_routed: self.events_routed,
            events_deduplicated: self.events_deduplicated,
            events_unmatched: self.events_unmatched,
            actions_dispatched: self.actions_dispatched,
            actions_blocked_policy: self.actions_blocked_policy,
            actions_blocked_sandbox: self.actions_blocked_sandbox,
            dispatch_queue_overflows: self.dispatch_queue_overflows,
        }
    }
}

// =============================================================================
// Dispatch history
// =============================================================================

/// Record of a dispatch outcome for audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchHistoryEntry {
    pub correlation_id: String,
    pub event_type: String,
    pub timestamp_ms: u64,
    pub actions: Vec<DispatchedAction>,
    pub blocked: Vec<BlockedAction>,
}

// =============================================================================
// Sandbox checker
// =============================================================================

/// Lightweight sandbox capability checker for outbound actions.
///
/// Validates that the target connector has the required capability
/// before allowing an action to be dispatched.
pub struct OutboundSandboxChecker {
    /// Per-connector sandbox zones. Key: connector name.
    zones: HashMap<String, ConnectorSandboxZone>,
    /// Default zone for connectors without explicit configuration.
    default_zone: ConnectorSandboxZone,
}

impl OutboundSandboxChecker {
    /// Create a new sandbox checker with the default zone.
    #[must_use]
    pub fn new() -> Self {
        Self {
            zones: HashMap::new(),
            default_zone: ConnectorSandboxZone::default(),
        }
    }

    /// Register a sandbox zone for a specific connector.
    pub fn register_zone(&mut self, connector: impl Into<String>, zone: ConnectorSandboxZone) {
        self.zones.insert(connector.into(), zone);
    }

    /// Set the default zone for unregistered connectors.
    pub fn set_default_zone(&mut self, zone: ConnectorSandboxZone) {
        self.default_zone = zone;
    }

    /// Check if a connector has the required capability.
    #[must_use]
    pub fn check_capability(
        &self,
        connector: &str,
        capability: ConnectorCapability,
    ) -> SandboxCheckResult {
        let zone = self.zones.get(connector).unwrap_or(&self.default_zone);
        if zone
            .capability_envelope
            .allowed_capabilities
            .contains(&capability)
        {
            SandboxCheckResult::Allowed
        } else if zone.fail_closed {
            SandboxCheckResult::Denied {
                zone_id: zone.zone_id.clone(),
                reason: format!("sandbox.denied.capability.{}", capability.as_str()),
            }
        } else {
            SandboxCheckResult::Allowed
        }
    }
}

impl Default for OutboundSandboxChecker {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a sandbox capability check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxCheckResult {
    Allowed,
    Denied { zone_id: String, reason: String },
}

// =============================================================================
// Bridge
// =============================================================================

/// Outbound connector bridge.
///
/// Routes FrankenTerm events into connector actions via configurable
/// routing rules, with deduplication, policy gates, and sandbox enforcement.
pub struct ConnectorOutboundBridge {
    config: ConnectorOutboundBridgeConfig,
    rules: Vec<OutboundRoutingRule>,
    deduplicator: OutboundDeduplicator,
    sandbox: OutboundSandboxChecker,
    dispatch_queue: VecDeque<ConnectorAction>,
    dispatch_history: VecDeque<DispatchHistoryEntry>,
    telemetry: OutboundBridgeTelemetry,
}

impl ConnectorOutboundBridge {
    /// Create a new outbound bridge.
    #[must_use]
    pub fn new(config: ConnectorOutboundBridgeConfig) -> Self {
        let dedup_ttl = Duration::from_secs(config.dedup_ttl_secs);
        Self {
            deduplicator: OutboundDeduplicator::new(config.dedup_capacity, dedup_ttl),
            dispatch_queue: VecDeque::with_capacity(config.dispatch_queue_capacity.min(4096)),
            dispatch_history: VecDeque::with_capacity(config.dispatch_history_capacity.min(1024)),
            config,
            rules: Vec::new(),
            sandbox: OutboundSandboxChecker::new(),
            telemetry: OutboundBridgeTelemetry::default(),
        }
    }

    /// Add a routing rule.
    pub fn add_rule(&mut self, rule: OutboundRoutingRule) {
        self.rules.push(rule);
        self.rules.sort_by_key(|r| r.priority);
    }

    /// Register a sandbox zone for a connector.
    pub fn register_sandbox_zone(
        &mut self,
        connector: impl Into<String>,
        zone: ConnectorSandboxZone,
    ) {
        self.sandbox.register_zone(connector, zone);
    }

    /// Process an event through the outbound bridge.
    ///
    /// Steps:
    /// 1. Deduplication check (if correlation_id present)
    /// 2. Match event against routing rules
    /// 3. For each matched rule:
    ///    a. Sandbox capability check
    ///    b. Build connector action
    ///    c. Enqueue for dispatch
    /// 4. Record history and telemetry
    pub fn process_event(
        &mut self,
        event: &OutboundEvent,
    ) -> Result<OutboundDispatchResult, OutboundBridgeError> {
        self.telemetry.events_received += 1;

        // Generate or use existing correlation ID
        let correlation_id = event.correlation_id.clone().unwrap_or_else(|| {
            format!(
                "out-{}-{}-{}",
                event.source.as_str(),
                event.timestamp_ms,
                event.pane_id.unwrap_or(0)
            )
        });

        // 1. Deduplication
        if event.correlation_id.is_some()
            && !self
                .deduplicator
                .check_and_record(&correlation_id, event.timestamp_ms)
        {
            self.telemetry.events_deduplicated += 1;
            debug!(
                correlation_id = %correlation_id,
                source = %event.source,
                event_type = %event.event_type,
                "outbound event deduplicated"
            );
            return Ok(OutboundDispatchResult {
                correlation_id,
                deduplicated: true,
                actions_dispatched: vec![],
                actions_blocked: vec![],
            });
        }

        // 2. Match routing rules
        let matched_rules: Vec<&OutboundRoutingRule> =
            self.rules.iter().filter(|r| r.matches(event)).collect();

        if matched_rules.is_empty() {
            self.telemetry.events_unmatched += 1;
            if self.config.reject_unmatched_events {
                warn!(
                    event_type = %event.event_type,
                    source = %event.source,
                    "outbound event has no matching rules (rejected)"
                );
                return Err(OutboundBridgeError::NoMatchingRules(
                    event.event_type.clone(),
                ));
            }
            debug!(
                event_type = %event.event_type,
                source = %event.source,
                "outbound event has no matching rules (ignored)"
            );
            return Ok(OutboundDispatchResult {
                correlation_id,
                deduplicated: false,
                actions_dispatched: vec![],
                actions_blocked: vec![],
            });
        }

        // 3. Process each matched rule
        let mut dispatched = Vec::new();
        let mut blocked = Vec::new();
        let now_ms = event.timestamp_ms;

        for rule in &matched_rules {
            let required_cap = rule.action_kind.required_capability();

            // 3a. Sandbox check
            if self.config.enforce_sandbox {
                match self
                    .sandbox
                    .check_capability(&rule.target_connector, required_cap)
                {
                    SandboxCheckResult::Allowed => {}
                    SandboxCheckResult::Denied { zone_id, reason } => {
                        self.telemetry.actions_blocked_sandbox += 1;
                        warn!(
                            rule_id = %rule.rule_id,
                            connector = %rule.target_connector,
                            capability = %required_cap.as_str(),
                            zone_id = %zone_id,
                            "outbound action blocked by sandbox"
                        );
                        blocked.push(BlockedAction {
                            rule_id: rule.rule_id.clone(),
                            target_connector: rule.target_connector.clone(),
                            action_kind: rule.action_kind,
                            reason: format!("sandbox: {reason}"),
                        });
                        continue;
                    }
                }
            }

            // 3b. Build action
            let action = ConnectorAction {
                target_connector: rule.target_connector.clone(),
                action_kind: rule.action_kind,
                correlation_id: correlation_id.clone(),
                params: event.payload.clone(),
                created_at_ms: now_ms,
            };

            // 3c. Enqueue
            if self.dispatch_queue.len() >= self.config.dispatch_queue_capacity {
                self.telemetry.dispatch_queue_overflows += 1;
                warn!(
                    capacity = self.config.dispatch_queue_capacity,
                    "outbound dispatch queue full, dropping oldest"
                );
                self.dispatch_queue.pop_front();
            }
            self.dispatch_queue.push_back(action);

            self.telemetry.actions_dispatched += 1;
            dispatched.push(DispatchedAction {
                rule_id: rule.rule_id.clone(),
                target_connector: rule.target_connector.clone(),
                action_kind: rule.action_kind,
                correlation_id: correlation_id.clone(),
            });

            info!(
                rule_id = %rule.rule_id,
                connector = %rule.target_connector,
                action = %rule.action_kind,
                correlation_id = %correlation_id,
                "outbound action dispatched"
            );
        }

        self.telemetry.events_routed += 1;

        // 4. Record history
        let entry = DispatchHistoryEntry {
            correlation_id: correlation_id.clone(),
            event_type: event.event_type.clone(),
            timestamp_ms: now_ms,
            actions: dispatched.clone(),
            blocked: blocked.clone(),
        };
        if self.dispatch_history.len() >= self.config.dispatch_history_capacity {
            self.dispatch_history.pop_front();
        }
        self.dispatch_history.push_back(entry);

        Ok(OutboundDispatchResult {
            correlation_id,
            deduplicated: false,
            actions_dispatched: dispatched,
            actions_blocked: blocked,
        })
    }

    /// Drain pending actions from the dispatch queue.
    pub fn drain_actions(&mut self) -> Vec<ConnectorAction> {
        self.dispatch_queue.drain(..).collect()
    }

    /// Peek at the next pending action without removing it.
    #[must_use]
    pub fn peek_action(&self) -> Option<&ConnectorAction> {
        self.dispatch_queue.front()
    }

    /// Number of pending actions in the dispatch queue.
    #[must_use]
    pub fn pending_action_count(&self) -> usize {
        self.dispatch_queue.len()
    }

    /// Get a slice of dispatch history entries.
    #[must_use]
    pub fn dispatch_history(&self) -> &VecDeque<DispatchHistoryEntry> {
        &self.dispatch_history
    }

    /// Take a telemetry snapshot.
    #[must_use]
    pub fn telemetry(&self) -> OutboundBridgeTelemetrySnapshot {
        self.telemetry.snapshot()
    }

    /// Number of routing rules registered.
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Get the current config.
    #[must_use]
    pub fn config(&self) -> &ConnectorOutboundBridgeConfig {
        &self.config
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector_host_runtime::ConnectorCapabilityEnvelope;

    fn make_event(event_type: &str, source: OutboundEventSource) -> OutboundEvent {
        OutboundEvent::new(source, event_type, serde_json::json!({"key": "value"}))
            .with_timestamp_ms(1000)
    }

    fn make_rule(
        rule_id: &str,
        source: Option<OutboundEventSource>,
        prefix: Option<&str>,
        connector: &str,
        kind: ConnectorActionKind,
    ) -> OutboundRoutingRule {
        OutboundRoutingRule {
            rule_id: rule_id.to_string(),
            source_filter: source,
            event_type_prefix: prefix.map(String::from),
            min_severity: None,
            target_connector: connector.to_string(),
            action_kind: kind,
            enabled: true,
            priority: 0,
        }
    }

    fn permissive_zone() -> ConnectorSandboxZone {
        ConnectorSandboxZone {
            zone_id: "zone.permissive".to_string(),
            fail_closed: true,
            capability_envelope: ConnectorCapabilityEnvelope {
                allowed_capabilities: vec![
                    ConnectorCapability::Invoke,
                    ConnectorCapability::NetworkEgress,
                    ConnectorCapability::SecretBroker,
                    ConnectorCapability::ReadState,
                    ConnectorCapability::StreamEvents,
                ],
                filesystem_read_prefixes: vec![],
                filesystem_write_prefixes: vec![],
                network_allow_hosts: vec![],
                allowed_exec_commands: vec![],
            },
        }
    }

    fn restrictive_zone() -> ConnectorSandboxZone {
        ConnectorSandboxZone {
            zone_id: "zone.restrictive".to_string(),
            fail_closed: true,
            capability_envelope: ConnectorCapabilityEnvelope {
                allowed_capabilities: vec![ConnectorCapability::ReadState],
                filesystem_read_prefixes: vec![],
                filesystem_write_prefixes: vec![],
                network_allow_hosts: vec![],
                allowed_exec_commands: vec![],
            },
        }
    }

    // ---- Config defaults ----

    #[test]
    fn connector_outbound_bridge_config_defaults() {
        let config = ConnectorOutboundBridgeConfig::default();
        assert_eq!(config.dedup_capacity, 4096);
        assert_eq!(config.dedup_ttl_secs, 300);
        assert_eq!(config.dispatch_queue_capacity, 1024);
        assert_eq!(config.dispatch_history_capacity, 256);
        assert!(!config.reject_unmatched_events);
        assert!(config.enforce_sandbox);
    }

    #[test]
    fn connector_outbound_bridge_config_serde_roundtrip() {
        let config = ConnectorOutboundBridgeConfig {
            dedup_capacity: 2048,
            dedup_ttl_secs: 600,
            dispatch_queue_capacity: 512,
            dispatch_history_capacity: 128,
            reject_unmatched_events: true,
            enforce_sandbox: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ConnectorOutboundBridgeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.dedup_capacity, 2048);
        assert_eq!(deserialized.dedup_ttl_secs, 600);
        assert!(deserialized.reject_unmatched_events);
        assert!(!deserialized.enforce_sandbox);
    }

    // ---- Event source ----

    #[test]
    fn connector_outbound_bridge_event_source_labels() {
        assert_eq!(
            OutboundEventSource::PatternDetected.as_str(),
            "pattern_detected"
        );
        assert_eq!(
            OutboundEventSource::PaneLifecycle.as_str(),
            "pane_lifecycle"
        );
        assert_eq!(
            OutboundEventSource::WorkflowLifecycle.as_str(),
            "workflow_lifecycle"
        );
        assert_eq!(OutboundEventSource::UserAction.as_str(), "user_action");
        assert_eq!(
            OutboundEventSource::PolicyDecision.as_str(),
            "policy_decision"
        );
        assert_eq!(OutboundEventSource::HealthAlert.as_str(), "health_alert");
        assert_eq!(OutboundEventSource::Custom.as_str(), "custom");
    }

    #[test]
    fn connector_outbound_bridge_event_source_display() {
        assert_eq!(
            format!("{}", OutboundEventSource::PatternDetected),
            "pattern_detected"
        );
    }

    // ---- Action kind ----

    #[test]
    fn connector_outbound_bridge_action_kind_labels() {
        assert_eq!(ConnectorActionKind::Notify.as_str(), "notify");
        assert_eq!(ConnectorActionKind::Ticket.as_str(), "ticket");
        assert_eq!(
            ConnectorActionKind::TriggerWorkflow.as_str(),
            "trigger_workflow"
        );
        assert_eq!(ConnectorActionKind::AuditLog.as_str(), "audit_log");
        assert_eq!(ConnectorActionKind::Invoke.as_str(), "invoke");
        assert_eq!(
            ConnectorActionKind::CredentialAction.as_str(),
            "credential_action"
        );
    }

    #[test]
    fn connector_outbound_bridge_action_capability_mapping() {
        assert_eq!(
            ConnectorActionKind::Notify.required_capability(),
            ConnectorCapability::NetworkEgress
        );
        assert_eq!(
            ConnectorActionKind::Ticket.required_capability(),
            ConnectorCapability::Invoke
        );
        assert_eq!(
            ConnectorActionKind::CredentialAction.required_capability(),
            ConnectorCapability::SecretBroker
        );
    }

    // ---- Severity ranking ----

    #[test]
    fn connector_outbound_bridge_severity_ranking() {
        assert!(severity_rank(OutboundSeverity::Info) < severity_rank(OutboundSeverity::Warning));
        assert!(
            severity_rank(OutboundSeverity::Warning) < severity_rank(OutboundSeverity::Critical)
        );
    }

    // ---- Deduplicator ----

    #[test]
    fn connector_outbound_bridge_dedup_new_returns_true() {
        let mut dedup = OutboundDeduplicator::new(100, Duration::from_secs(300));
        assert!(dedup.check_and_record("abc", 1000));
        assert_eq!(dedup.len(), 1);
    }

    #[test]
    fn connector_outbound_bridge_dedup_duplicate_returns_false() {
        let mut dedup = OutboundDeduplicator::new(100, Duration::from_secs(300));
        assert!(dedup.check_and_record("abc", 1000));
        assert!(!dedup.check_and_record("abc", 1500));
        assert_eq!(dedup.len(), 1);
    }

    #[test]
    fn connector_outbound_bridge_dedup_ttl_expiry() {
        let mut dedup = OutboundDeduplicator::new(100, Duration::from_secs(10));
        assert!(dedup.check_and_record("abc", 1000));
        // After TTL, same ID should be accepted again
        assert!(dedup.check_and_record("abc", 1000 + 11_000));
        assert_eq!(dedup.len(), 1);
    }

    #[test]
    fn connector_outbound_bridge_dedup_capacity_eviction() {
        let mut dedup = OutboundDeduplicator::new(3, Duration::from_secs(300));
        assert!(dedup.check_and_record("a", 100));
        assert!(dedup.check_and_record("b", 200));
        assert!(dedup.check_and_record("c", 300));
        // At capacity, oldest is evicted
        assert!(dedup.check_and_record("d", 400));
        assert_eq!(dedup.len(), 3);
        // "a" was evicted, should be accepted again
        assert!(dedup.check_and_record("a", 500));
    }

    // ---- Routing rule matching ----

    #[test]
    fn connector_outbound_bridge_rule_matches_all_when_no_filters() {
        let rule = make_rule("r1", None, None, "slack", ConnectorActionKind::Notify);
        let event = make_event("pattern.ci_failure", OutboundEventSource::PatternDetected);
        assert!(rule.matches(&event));
    }

    #[test]
    fn connector_outbound_bridge_rule_filters_by_source() {
        let rule = make_rule(
            "r1",
            Some(OutboundEventSource::WorkflowLifecycle),
            None,
            "slack",
            ConnectorActionKind::Notify,
        );
        let event = make_event("pattern.ci_failure", OutboundEventSource::PatternDetected);
        assert!(!rule.matches(&event));

        let workflow_event =
            make_event("workflow.completed", OutboundEventSource::WorkflowLifecycle);
        assert!(rule.matches(&workflow_event));
    }

    #[test]
    fn connector_outbound_bridge_rule_filters_by_event_type_prefix() {
        let rule = make_rule(
            "r1",
            None,
            Some("pattern."),
            "slack",
            ConnectorActionKind::Notify,
        );
        let matching = make_event("pattern.ci_failure", OutboundEventSource::PatternDetected);
        assert!(rule.matches(&matching));

        let non_matching = make_event("workflow.completed", OutboundEventSource::WorkflowLifecycle);
        assert!(!rule.matches(&non_matching));
    }

    #[test]
    fn connector_outbound_bridge_rule_filters_by_severity() {
        let mut rule = make_rule("r1", None, None, "pagerduty", ConnectorActionKind::Notify);
        rule.min_severity = Some(OutboundSeverity::Critical);

        let info_event = make_event("test", OutboundEventSource::Custom);
        assert!(!rule.matches(&info_event));

        let critical_event = make_event("test", OutboundEventSource::Custom)
            .with_severity(OutboundSeverity::Critical);
        assert!(rule.matches(&critical_event));
    }

    #[test]
    fn connector_outbound_bridge_disabled_rule_never_matches() {
        let mut rule = make_rule("r1", None, None, "slack", ConnectorActionKind::Notify);
        rule.enabled = false;
        let event = make_event("anything", OutboundEventSource::Custom);
        assert!(!rule.matches(&event));
    }

    // ---- Sandbox checker ----

    #[test]
    fn connector_outbound_bridge_sandbox_allows_registered_capability() {
        let mut checker = OutboundSandboxChecker::new();
        checker.register_zone("slack", permissive_zone());
        let result = checker.check_capability("slack", ConnectorCapability::NetworkEgress);
        assert_eq!(result, SandboxCheckResult::Allowed);
    }

    #[test]
    fn connector_outbound_bridge_sandbox_denies_missing_capability() {
        let mut checker = OutboundSandboxChecker::new();
        checker.register_zone("restricted", restrictive_zone());
        let result = checker.check_capability("restricted", ConnectorCapability::Invoke);
        assert!(matches!(result, SandboxCheckResult::Denied { .. }));
    }

    #[test]
    fn connector_outbound_bridge_sandbox_uses_default_zone_for_unknown() {
        let checker = OutboundSandboxChecker::new();
        // Default zone allows Invoke, ReadState, StreamEvents
        let result = checker.check_capability("unknown", ConnectorCapability::Invoke);
        assert_eq!(result, SandboxCheckResult::Allowed);
        // But denies capabilities not in default set
        let result = checker.check_capability("unknown", ConnectorCapability::SecretBroker);
        assert!(matches!(result, SandboxCheckResult::Denied { .. }));
    }

    // ---- Bridge integration ----

    #[test]
    fn connector_outbound_bridge_routes_event_to_matching_rule() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        bridge.register_sandbox_zone("slack", permissive_zone());
        bridge.add_rule(make_rule(
            "r1",
            Some(OutboundEventSource::PatternDetected),
            Some("pattern."),
            "slack",
            ConnectorActionKind::Notify,
        ));

        let event = make_event("pattern.ci_failure", OutboundEventSource::PatternDetected);
        let result = bridge.process_event(&event).unwrap();
        assert!(!result.deduplicated);
        assert_eq!(result.actions_dispatched.len(), 1);
        assert_eq!(result.actions_dispatched[0].target_connector, "slack");
        assert_eq!(result.actions_blocked.len(), 0);
        assert_eq!(bridge.pending_action_count(), 1);
    }

    #[test]
    fn connector_outbound_bridge_deduplicates_same_correlation_id() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        bridge.register_sandbox_zone("slack", permissive_zone());
        bridge.add_rule(make_rule(
            "r1",
            None,
            None,
            "slack",
            ConnectorActionKind::Notify,
        ));

        let event = make_event("test", OutboundEventSource::Custom).with_correlation_id("dedup-1");
        let r1 = bridge.process_event(&event).unwrap();
        assert!(!r1.deduplicated);
        assert_eq!(r1.actions_dispatched.len(), 1);

        let r2 = bridge.process_event(&event).unwrap();
        assert!(r2.deduplicated);
        assert_eq!(r2.actions_dispatched.len(), 0);
    }

    #[test]
    fn connector_outbound_bridge_no_dedup_without_correlation_id() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        bridge.register_sandbox_zone("slack", permissive_zone());
        bridge.add_rule(make_rule(
            "r1",
            None,
            None,
            "slack",
            ConnectorActionKind::Notify,
        ));

        let event = make_event("test", OutboundEventSource::Custom);
        // Without correlation_id, dedup is skipped — both should dispatch
        let r1 = bridge.process_event(&event).unwrap();
        assert!(!r1.deduplicated);
        let r2 = bridge.process_event(&event).unwrap();
        assert!(!r2.deduplicated);
        assert_eq!(bridge.pending_action_count(), 2);
    }

    #[test]
    fn connector_outbound_bridge_blocks_sandbox_violation() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        bridge.register_sandbox_zone("locked_down", restrictive_zone());
        bridge.add_rule(make_rule(
            "r1",
            None,
            None,
            "locked_down",
            ConnectorActionKind::Notify, // requires NetworkEgress
        ));

        let event = make_event("test", OutboundEventSource::Custom);
        let result = bridge.process_event(&event).unwrap();
        assert_eq!(result.actions_dispatched.len(), 0);
        assert_eq!(result.actions_blocked.len(), 1);
        assert!(result.actions_blocked[0].reason.contains("sandbox"));
    }

    #[test]
    fn connector_outbound_bridge_unmatched_event_ignored_by_default() {
        let bridge_config = ConnectorOutboundBridgeConfig::default();
        let mut bridge = ConnectorOutboundBridge::new(bridge_config);
        // No rules added

        let event = make_event("unknown.event", OutboundEventSource::Custom);
        let result = bridge.process_event(&event).unwrap();
        assert!(!result.deduplicated);
        assert_eq!(result.actions_dispatched.len(), 0);
        assert_eq!(result.actions_blocked.len(), 0);
    }

    #[test]
    fn connector_outbound_bridge_unmatched_event_rejected_when_configured() {
        let config = ConnectorOutboundBridgeConfig {
            reject_unmatched_events: true,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);

        let event = make_event("unknown.event", OutboundEventSource::Custom);
        let err = bridge.process_event(&event).unwrap_err();
        assert!(matches!(err, OutboundBridgeError::NoMatchingRules(_)));
    }

    #[test]
    fn connector_outbound_bridge_multiple_rules_match() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        bridge.register_sandbox_zone("slack", permissive_zone());
        bridge.register_sandbox_zone("datadog", permissive_zone());

        bridge.add_rule(make_rule(
            "r1",
            None,
            None,
            "slack",
            ConnectorActionKind::Notify,
        ));
        bridge.add_rule(make_rule(
            "r2",
            None,
            None,
            "datadog",
            ConnectorActionKind::AuditLog,
        ));

        let event = make_event("test", OutboundEventSource::Custom);
        let result = bridge.process_event(&event).unwrap();
        assert_eq!(result.actions_dispatched.len(), 2);
        assert_eq!(bridge.pending_action_count(), 2);
    }

    #[test]
    fn connector_outbound_bridge_drain_actions() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        bridge.register_sandbox_zone("slack", permissive_zone());
        bridge.add_rule(make_rule(
            "r1",
            None,
            None,
            "slack",
            ConnectorActionKind::Notify,
        ));

        let event = make_event("test", OutboundEventSource::Custom);
        bridge.process_event(&event).unwrap();
        assert_eq!(bridge.pending_action_count(), 1);

        let actions = bridge.drain_actions();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].target_connector, "slack");
        assert_eq!(bridge.pending_action_count(), 0);
    }

    #[test]
    fn connector_outbound_bridge_dispatch_queue_overflow_drops_oldest() {
        let config = ConnectorOutboundBridgeConfig {
            dispatch_queue_capacity: 2,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);
        bridge.register_sandbox_zone("slack", permissive_zone());
        bridge.add_rule(make_rule(
            "r1",
            None,
            None,
            "slack",
            ConnectorActionKind::Notify,
        ));

        for i in 0..4 {
            let event = make_event(&format!("event.{i}"), OutboundEventSource::Custom)
                .with_timestamp_ms(1000 + i * 100);
            bridge.process_event(&event).unwrap();
        }
        // Queue capacity is 2, so only latest 2 should remain
        assert_eq!(bridge.pending_action_count(), 2);

        let tel = bridge.telemetry();
        assert_eq!(tel.dispatch_queue_overflows, 2);
    }

    #[test]
    fn connector_outbound_bridge_dispatch_history_bounded() {
        let config = ConnectorOutboundBridgeConfig {
            dispatch_history_capacity: 3,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);
        bridge.register_sandbox_zone("slack", permissive_zone());
        bridge.add_rule(make_rule(
            "r1",
            None,
            None,
            "slack",
            ConnectorActionKind::Notify,
        ));

        for i in 0..5 {
            let event = make_event(&format!("event.{i}"), OutboundEventSource::Custom)
                .with_timestamp_ms(1000 + i * 100);
            bridge.process_event(&event).unwrap();
        }
        assert_eq!(bridge.dispatch_history().len(), 3);
    }

    #[test]
    fn connector_outbound_bridge_telemetry_counters() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        bridge.register_sandbox_zone("slack", permissive_zone());
        bridge.register_sandbox_zone("locked", restrictive_zone());

        bridge.add_rule(make_rule(
            "r1",
            Some(OutboundEventSource::PatternDetected),
            None,
            "slack",
            ConnectorActionKind::Notify,
        ));
        bridge.add_rule(make_rule(
            "r2",
            Some(OutboundEventSource::PatternDetected),
            None,
            "locked",
            ConnectorActionKind::Invoke, // requires Invoke cap, locked doesn't have it
        ));

        // First event: matches both rules, one dispatched, one blocked
        let event1 = make_event("test", OutboundEventSource::PatternDetected);
        bridge.process_event(&event1).unwrap();

        // Unmatched event
        let event2 = make_event("test", OutboundEventSource::WorkflowLifecycle);
        bridge.process_event(&event2).unwrap();

        // Duplicate
        let event3 =
            make_event("test", OutboundEventSource::PatternDetected).with_correlation_id("dup-1");
        bridge.process_event(&event3).unwrap();
        bridge.process_event(&event3).unwrap();

        let tel = bridge.telemetry();
        assert_eq!(tel.events_received, 4);
        assert_eq!(tel.events_routed, 2); // event1 + first event3
        assert_eq!(tel.events_unmatched, 1); // event2
        assert_eq!(tel.events_deduplicated, 1); // second event3
        assert_eq!(tel.actions_dispatched, 2); // slack from event1 + event3
        assert_eq!(tel.actions_blocked_sandbox, 2); // locked from event1 + event3
    }

    #[test]
    fn connector_outbound_bridge_telemetry_snapshot_serde_roundtrip() {
        let snapshot = OutboundBridgeTelemetrySnapshot {
            events_received: 10,
            events_routed: 8,
            events_deduplicated: 1,
            events_unmatched: 1,
            actions_dispatched: 7,
            actions_blocked_policy: 0,
            actions_blocked_sandbox: 1,
            dispatch_queue_overflows: 0,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let deserialized: OutboundBridgeTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snapshot, deserialized);
    }

    // ---- Event builder ----

    #[test]
    fn connector_outbound_bridge_event_builder() {
        let event = OutboundEvent::new(
            OutboundEventSource::PatternDetected,
            "pattern.ci_failure",
            serde_json::json!({"build_id": 123}),
        )
        .with_correlation_id("corr-123")
        .with_pane_id(42)
        .with_workflow_id("wf-1")
        .with_timestamp_ms(5000)
        .with_severity(OutboundSeverity::Critical);

        assert_eq!(event.source, OutboundEventSource::PatternDetected);
        assert_eq!(event.event_type, "pattern.ci_failure");
        assert_eq!(event.correlation_id.as_deref(), Some("corr-123"));
        assert_eq!(event.pane_id, Some(42));
        assert_eq!(event.workflow_id.as_deref(), Some("wf-1"));
        assert_eq!(event.timestamp_ms, 5000);
        assert_eq!(event.severity, OutboundSeverity::Critical);
    }

    // ---- Rule priority ordering ----

    #[test]
    fn connector_outbound_bridge_rules_sorted_by_priority() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        bridge.register_sandbox_zone("a", permissive_zone());
        bridge.register_sandbox_zone("b", permissive_zone());
        bridge.register_sandbox_zone("c", permissive_zone());

        let mut r1 = make_rule("r1", None, None, "a", ConnectorActionKind::Notify);
        r1.priority = 10;
        let mut r2 = make_rule("r2", None, None, "b", ConnectorActionKind::Notify);
        r2.priority = 1;
        let mut r3 = make_rule("r3", None, None, "c", ConnectorActionKind::Notify);
        r3.priority = 5;

        bridge.add_rule(r1);
        bridge.add_rule(r2);
        bridge.add_rule(r3);

        let event = make_event("test", OutboundEventSource::Custom);
        let result = bridge.process_event(&event).unwrap();
        assert_eq!(result.actions_dispatched.len(), 3);
        // Verify priority order
        assert_eq!(result.actions_dispatched[0].target_connector, "b"); // priority 1
        assert_eq!(result.actions_dispatched[1].target_connector, "c"); // priority 5
        assert_eq!(result.actions_dispatched[2].target_connector, "a"); // priority 10
    }

    // ---- Sandbox bypass when enforcement disabled ----

    #[test]
    fn connector_outbound_bridge_sandbox_bypass_when_disabled() {
        let config = ConnectorOutboundBridgeConfig {
            enforce_sandbox: false,
            ..Default::default()
        };
        let mut bridge = ConnectorOutboundBridge::new(config);
        // No sandbox zones registered, but enforcement disabled
        bridge.add_rule(make_rule(
            "r1",
            None,
            None,
            "unknown_connector",
            ConnectorActionKind::Notify,
        ));

        let event = make_event("test", OutboundEventSource::Custom);
        let result = bridge.process_event(&event).unwrap();
        assert_eq!(result.actions_dispatched.len(), 1);
        assert_eq!(result.actions_blocked.len(), 0);
    }

    // ---- Action kind display ----

    #[test]
    fn connector_outbound_bridge_action_kind_display() {
        assert_eq!(format!("{}", ConnectorActionKind::Notify), "notify");
        assert_eq!(format!("{}", ConnectorActionKind::Ticket), "ticket");
    }

    // ---- Outbound severity default ----

    #[test]
    fn connector_outbound_bridge_severity_default_is_info() {
        assert_eq!(OutboundSeverity::default(), OutboundSeverity::Info);
    }

    // ---- Mixed dispatched and blocked ----

    #[test]
    fn connector_outbound_bridge_mixed_dispatch_and_block() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        bridge.register_sandbox_zone("allowed", permissive_zone());
        bridge.register_sandbox_zone("blocked", restrictive_zone());

        bridge.add_rule(make_rule(
            "r-ok",
            None,
            None,
            "allowed",
            ConnectorActionKind::Notify,
        ));
        bridge.add_rule(make_rule(
            "r-blocked",
            None,
            None,
            "blocked",
            ConnectorActionKind::TriggerWorkflow, // needs Invoke
        ));

        let event = make_event("test", OutboundEventSource::Custom);
        let result = bridge.process_event(&event).unwrap();
        assert_eq!(result.actions_dispatched.len(), 1);
        assert_eq!(result.actions_dispatched[0].rule_id, "r-ok");
        assert_eq!(result.actions_blocked.len(), 1);
        assert_eq!(result.actions_blocked[0].rule_id, "r-blocked");
    }

    // ---- Peek action ----

    #[test]
    fn connector_outbound_bridge_peek_action() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        assert!(bridge.peek_action().is_none());

        bridge.register_sandbox_zone("slack", permissive_zone());
        bridge.add_rule(make_rule(
            "r1",
            None,
            None,
            "slack",
            ConnectorActionKind::Notify,
        ));

        let event = make_event("test", OutboundEventSource::Custom);
        bridge.process_event(&event).unwrap();
        assert!(bridge.peek_action().is_some());
        assert_eq!(bridge.peek_action().unwrap().target_connector, "slack");
        // Peek doesn't consume
        assert_eq!(bridge.pending_action_count(), 1);
    }

    // ---- Rule count ----

    #[test]
    fn connector_outbound_bridge_rule_count() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        assert_eq!(bridge.rule_count(), 0);
        bridge.add_rule(make_rule(
            "r1",
            None,
            None,
            "a",
            ConnectorActionKind::Notify,
        ));
        bridge.add_rule(make_rule(
            "r2",
            None,
            None,
            "b",
            ConnectorActionKind::Ticket,
        ));
        assert_eq!(bridge.rule_count(), 2);
    }

    // ---- Auto-generated correlation ID ----

    #[test]
    fn connector_outbound_bridge_auto_correlation_id() {
        let mut bridge = ConnectorOutboundBridge::new(ConnectorOutboundBridgeConfig::default());
        bridge.register_sandbox_zone("slack", permissive_zone());
        bridge.add_rule(make_rule(
            "r1",
            None,
            None,
            "slack",
            ConnectorActionKind::Notify,
        ));

        let event = make_event("test", OutboundEventSource::PatternDetected).with_pane_id(42);
        let result = bridge.process_event(&event).unwrap();
        assert!(result.correlation_id.starts_with("out-pattern_detected-"));
        assert!(result.correlation_id.contains("-42"));
    }

    // ---- OutboundEvent serde roundtrip ----

    #[test]
    fn connector_outbound_bridge_event_serde_roundtrip() {
        let event = OutboundEvent::new(
            OutboundEventSource::HealthAlert,
            "health.cpu_high",
            serde_json::json!({"cpu_percent": 95}),
        )
        .with_correlation_id("health-1")
        .with_severity(OutboundSeverity::Warning);

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: OutboundEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.source, OutboundEventSource::HealthAlert);
        assert_eq!(deserialized.event_type, "health.cpu_high");
        assert_eq!(deserialized.severity, OutboundSeverity::Warning);
    }

    // ---- DispatchHistoryEntry serde roundtrip ----

    #[test]
    fn connector_outbound_bridge_history_entry_serde() {
        let entry = DispatchHistoryEntry {
            correlation_id: "corr-1".to_string(),
            event_type: "test".to_string(),
            timestamp_ms: 1000,
            actions: vec![DispatchedAction {
                rule_id: "r1".to_string(),
                target_connector: "slack".to_string(),
                action_kind: ConnectorActionKind::Notify,
                correlation_id: "corr-1".to_string(),
            }],
            blocked: vec![],
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: DispatchHistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.correlation_id, "corr-1");
        assert_eq!(deserialized.actions.len(), 1);
    }

    // ---- ConnectorAction serde roundtrip ----

    #[test]
    fn connector_outbound_bridge_action_serde() {
        let action = ConnectorAction {
            target_connector: "github".to_string(),
            action_kind: ConnectorActionKind::Ticket,
            correlation_id: "corr-abc".to_string(),
            params: serde_json::json!({"title": "Bug fix needed"}),
            created_at_ms: 1000,
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: ConnectorAction = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.target_connector, "github");
        assert_eq!(deserialized.action_kind, ConnectorActionKind::Ticket);
    }

    // ---- Routing rule serde roundtrip ----

    #[test]
    fn connector_outbound_bridge_routing_rule_serde() {
        let rule = OutboundRoutingRule {
            rule_id: "r1".to_string(),
            source_filter: Some(OutboundEventSource::PatternDetected),
            event_type_prefix: Some("pattern.".to_string()),
            min_severity: Some(OutboundSeverity::Warning),
            target_connector: "slack".to_string(),
            action_kind: ConnectorActionKind::Notify,
            enabled: true,
            priority: 5,
        };
        let json = serde_json::to_string(&rule).unwrap();
        let deserialized: OutboundRoutingRule = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.rule_id, "r1");
        assert_eq!(deserialized.priority, 5);
        assert_eq!(
            deserialized.source_filter,
            Some(OutboundEventSource::PatternDetected)
        );
    }
}
