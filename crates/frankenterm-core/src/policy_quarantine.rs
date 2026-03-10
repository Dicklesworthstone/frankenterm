//! Policy quarantine and kill-switch controls.
//!
//! Provides quarantine state management and emergency kill-switch semantics
//! for the policy engine. Quarantined components are isolated from performing
//! actions until the quarantine is explicitly lifted. Kill switches provide
//! graduated emergency stops with clear severity tiers.
//!
//! Part of ft-3681t.6.3 precursor work.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

// =============================================================================
// Quarantine reason and severity
// =============================================================================

/// Why a component was quarantined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineReason {
    /// Policy violation detected.
    PolicyViolation { rule_id: String, detail: String },
    /// Credential compromise suspected.
    CredentialCompromise { credential_id: String },
    /// Anomalous behavior detected (e.g., excessive rate, unexpected patterns).
    AnomalousBehavior { metric: String, observed: String },
    /// Manual quarantine by operator.
    OperatorDirected { operator: String, note: String },
    /// Automated circuit-breaker trip.
    CircuitBreakerTrip { circuit_id: String },
    /// Dependency on a quarantined component.
    CascadeFromParent { parent_component_id: String },
}

impl std::fmt::Display for QuarantineReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PolicyViolation { rule_id, detail } => {
                write!(f, "policy_violation({rule_id}): {detail}")
            }
            Self::CredentialCompromise { credential_id } => {
                write!(f, "credential_compromise({credential_id})")
            }
            Self::AnomalousBehavior { metric, observed } => {
                write!(f, "anomalous_behavior({metric}={observed})")
            }
            Self::OperatorDirected { operator, note } => {
                write!(f, "operator_directed({operator}): {note}")
            }
            Self::CircuitBreakerTrip { circuit_id } => {
                write!(f, "circuit_breaker_trip({circuit_id})")
            }
            Self::CascadeFromParent {
                parent_component_id,
            } => {
                write!(f, "cascade_from({parent_component_id})")
            }
        }
    }
}

/// Severity of a quarantine action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineSeverity {
    /// Advisory: component flagged but still operational.
    Advisory,
    /// Restricted: component can perform reads but not writes.
    Restricted,
    /// Isolated: component cannot perform any actions.
    Isolated,
    /// Terminated: component is being shut down.
    Terminated,
}

impl std::fmt::Display for QuarantineSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Advisory => f.write_str("advisory"),
            Self::Restricted => f.write_str("restricted"),
            Self::Isolated => f.write_str("isolated"),
            Self::Terminated => f.write_str("terminated"),
        }
    }
}

// =============================================================================
// Quarantine state
// =============================================================================

/// Current quarantine state of a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineState {
    /// Normal operation.
    Clear,
    /// Under quarantine.
    Quarantined,
    /// Quarantine lifted, under observation period.
    ProbationaryRelease,
}

impl std::fmt::Display for QuarantineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clear => f.write_str("clear"),
            Self::Quarantined => f.write_str("quarantined"),
            Self::ProbationaryRelease => f.write_str("probationary_release"),
        }
    }
}

/// Type of component that can be quarantined.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    Connector,
    Pane,
    Workflow,
    Agent,
    Session,
}

impl std::fmt::Display for ComponentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connector => f.write_str("connector"),
            Self::Pane => f.write_str("pane"),
            Self::Workflow => f.write_str("workflow"),
            Self::Agent => f.write_str("agent"),
            Self::Session => f.write_str("session"),
        }
    }
}

/// A quarantined component record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantinedComponent {
    /// Unique component identifier.
    pub component_id: String,
    /// Type of component.
    pub component_kind: ComponentKind,
    /// Current quarantine state.
    pub state: QuarantineState,
    /// Severity of the quarantine.
    pub severity: QuarantineSeverity,
    /// Why this component was quarantined.
    pub reason: QuarantineReason,
    /// When the quarantine was imposed.
    pub quarantined_at_ms: u64,
    /// When the quarantine expires (0 = indefinite until manual release).
    pub expires_at_ms: u64,
    /// When the quarantine was last reviewed/updated.
    pub last_reviewed_at_ms: u64,
    /// Who or what imposed the quarantine.
    pub imposed_by: String,
    /// How many times this component has been quarantined historically.
    pub quarantine_count: u32,
}

impl QuarantinedComponent {
    /// Check if this quarantine has expired at the given time.
    #[must_use]
    pub fn is_expired_at(&self, now_ms: u64) -> bool {
        self.expires_at_ms > 0 && now_ms >= self.expires_at_ms
    }

    /// Check if this quarantine blocks writes.
    #[must_use]
    pub fn blocks_writes(&self) -> bool {
        self.state == QuarantineState::Quarantined
            && self.severity >= QuarantineSeverity::Restricted
    }

    /// Check if this quarantine blocks all actions (reads and writes).
    #[must_use]
    pub fn blocks_all(&self) -> bool {
        self.state == QuarantineState::Quarantined && self.severity >= QuarantineSeverity::Isolated
    }
}

// =============================================================================
// Kill switch
// =============================================================================

/// Graduated emergency stop levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KillSwitchLevel {
    /// Normal operation — no kill switch engaged.
    Disarmed,
    /// Soft stop: pause new workflow launches, allow in-flight to complete.
    SoftStop,
    /// Hard stop: cancel all in-flight workflows, block new actions.
    HardStop,
    /// Emergency halt: immediate termination of all agent activities.
    EmergencyHalt,
}

impl std::fmt::Display for KillSwitchLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disarmed => f.write_str("disarmed"),
            Self::SoftStop => f.write_str("soft_stop"),
            Self::HardStop => f.write_str("hard_stop"),
            Self::EmergencyHalt => f.write_str("emergency_halt"),
        }
    }
}

/// State of the kill switch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSwitch {
    /// Current level.
    pub level: KillSwitchLevel,
    /// When the kill switch was last changed.
    pub changed_at_ms: u64,
    /// Who armed/tripped the kill switch.
    pub changed_by: String,
    /// Reason for the current state.
    pub reason: String,
    /// Whether the kill switch auto-disarms after a timeout.
    pub auto_disarm_at_ms: u64,
}

impl KillSwitch {
    /// Create a disarmed kill switch.
    #[must_use]
    pub fn disarmed() -> Self {
        Self {
            level: KillSwitchLevel::Disarmed,
            changed_at_ms: 0,
            changed_by: String::new(),
            reason: String::new(),
            auto_disarm_at_ms: 0,
        }
    }

    /// Arm the kill switch at a specific level.
    pub fn trip(&mut self, level: KillSwitchLevel, by: &str, reason: &str, now_ms: u64) {
        self.level = level;
        self.changed_at_ms = now_ms;
        self.changed_by = by.to_string();
        self.reason = reason.to_string();
    }

    /// Arm with auto-disarm timeout.
    pub fn trip_with_timeout(
        &mut self,
        level: KillSwitchLevel,
        by: &str,
        reason: &str,
        now_ms: u64,
        timeout_ms: u64,
    ) {
        self.trip(level, by, reason, now_ms);
        self.auto_disarm_at_ms = now_ms.saturating_add(timeout_ms);
    }

    /// Reset the kill switch to disarmed.
    pub fn reset(&mut self, by: &str, now_ms: u64) {
        self.level = KillSwitchLevel::Disarmed;
        self.changed_at_ms = now_ms;
        self.changed_by = by.to_string();
        self.reason = "manually reset".to_string();
        self.auto_disarm_at_ms = 0;
    }

    /// Check if auto-disarm should occur at the given time.
    #[must_use]
    pub fn should_auto_disarm(&self, now_ms: u64) -> bool {
        self.auto_disarm_at_ms > 0
            && now_ms >= self.auto_disarm_at_ms
            && self.level != KillSwitchLevel::Disarmed
    }

    /// Process auto-disarm if applicable.
    pub fn tick(&mut self, now_ms: u64) -> bool {
        if self.should_auto_disarm(now_ms) {
            self.level = KillSwitchLevel::Disarmed;
            self.changed_at_ms = now_ms;
            self.changed_by = "auto_disarm".to_string();
            self.reason = "timeout elapsed".to_string();
            self.auto_disarm_at_ms = 0;
            true
        } else {
            false
        }
    }

    /// Check if new workflow launches are allowed.
    #[must_use]
    pub fn allows_new_workflows(&self) -> bool {
        self.level < KillSwitchLevel::SoftStop
    }

    /// Check if in-flight workflows should continue.
    #[must_use]
    pub fn allows_inflight(&self) -> bool {
        self.level < KillSwitchLevel::HardStop
    }

    /// Check if the system is in emergency halt.
    #[must_use]
    pub fn is_emergency(&self) -> bool {
        self.level >= KillSwitchLevel::EmergencyHalt
    }
}

impl Default for KillSwitch {
    fn default() -> Self {
        Self::disarmed()
    }
}

// =============================================================================
// Quarantine audit events
// =============================================================================

/// Types of quarantine audit events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineAuditType {
    Imposed,
    SeverityEscalated,
    SeverityDeescalated,
    Released,
    ProbationStarted,
    ProbationCompleted,
    ProbationRevoked,
    Expired,
    KillSwitchTripped,
    KillSwitchReset,
    KillSwitchAutoDisarmed,
}

impl std::fmt::Display for QuarantineAuditType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Imposed => f.write_str("imposed"),
            Self::SeverityEscalated => f.write_str("severity_escalated"),
            Self::SeverityDeescalated => f.write_str("severity_deescalated"),
            Self::Released => f.write_str("released"),
            Self::ProbationStarted => f.write_str("probation_started"),
            Self::ProbationCompleted => f.write_str("probation_completed"),
            Self::ProbationRevoked => f.write_str("probation_revoked"),
            Self::Expired => f.write_str("expired"),
            Self::KillSwitchTripped => f.write_str("kill_switch_tripped"),
            Self::KillSwitchReset => f.write_str("kill_switch_reset"),
            Self::KillSwitchAutoDisarmed => f.write_str("kill_switch_auto_disarmed"),
        }
    }
}

/// Audit event for quarantine operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuarantineAuditEvent {
    pub timestamp_ms: u64,
    pub event_type: QuarantineAuditType,
    pub component_id: String,
    pub component_kind: Option<ComponentKind>,
    pub actor: String,
    pub detail: String,
}

// =============================================================================
// Telemetry
// =============================================================================

/// Telemetry counters for quarantine operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuarantineTelemetry {
    pub quarantines_imposed: u64,
    pub quarantines_released: u64,
    pub quarantines_expired: u64,
    pub probations_started: u64,
    pub probations_completed: u64,
    pub probations_revoked: u64,
    pub severity_escalations: u64,
    pub severity_deescalations: u64,
    pub kill_switch_trips: u64,
    pub kill_switch_resets: u64,
}

/// Snapshot of quarantine telemetry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuarantineTelemetrySnapshot {
    pub captured_at_ms: u64,
    pub counters: QuarantineTelemetry,
    pub active_quarantines: u32,
    pub kill_switch_level: KillSwitchLevel,
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the quarantine subsystem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct QuarantineConfig {
    /// Maximum number of audit events to retain (oldest evicted first).
    pub max_audit_events: usize,
    /// Whether to auto-expire quarantines on each authorize() tick.
    pub auto_expire: bool,
    /// Default quarantine severity when not explicitly specified.
    pub default_severity: QuarantineSeverity,
}

impl Default for QuarantineConfig {
    fn default() -> Self {
        Self {
            max_audit_events: 512,
            auto_expire: true,
            default_severity: QuarantineSeverity::Restricted,
        }
    }
}

// =============================================================================
// Quarantine registry
// =============================================================================

const DEFAULT_MAX_AUDIT_EVENTS: usize = 512;

/// Error types for quarantine operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuarantineError {
    #[error("component not found: {component_id}")]
    ComponentNotFound { component_id: String },

    #[error("component not quarantined: {component_id}")]
    NotQuarantined { component_id: String },

    #[error("component already quarantined: {component_id}")]
    AlreadyQuarantined { component_id: String },

    #[error("cannot escalate: already at maximum severity")]
    AlreadyMaxSeverity,

    #[error("cannot deescalate: already at minimum severity")]
    AlreadyMinSeverity,

    #[error("kill switch blocks operation: level={level}")]
    KillSwitchActive { level: KillSwitchLevel },
}

/// Registry of quarantined components and kill switch state.
#[derive(Debug)]
pub struct QuarantineRegistry {
    components: BTreeMap<String, QuarantinedComponent>,
    kill_switch: KillSwitch,
    audit_log: VecDeque<QuarantineAuditEvent>,
    telemetry: QuarantineTelemetry,
    max_audit_events: usize,
}

impl QuarantineRegistry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            components: BTreeMap::new(),
            kill_switch: KillSwitch::disarmed(),
            audit_log: VecDeque::new(),
            telemetry: QuarantineTelemetry::default(),
            max_audit_events: DEFAULT_MAX_AUDIT_EVENTS,
        }
    }

    /// Create a registry from configuration.
    #[must_use]
    pub fn from_config(config: &QuarantineConfig) -> Self {
        Self {
            components: BTreeMap::new(),
            kill_switch: KillSwitch::disarmed(),
            audit_log: VecDeque::new(),
            telemetry: QuarantineTelemetry::default(),
            max_audit_events: config.max_audit_events,
        }
    }

    // ---- Quarantine operations ----

    /// Quarantine a component.
    pub fn quarantine(
        &mut self,
        component_id: &str,
        component_kind: ComponentKind,
        severity: QuarantineSeverity,
        reason: QuarantineReason,
        imposed_by: &str,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<(), QuarantineError> {
        if self.components.contains_key(component_id) {
            let existing = &self.components[component_id];
            if existing.state == QuarantineState::Quarantined {
                return Err(QuarantineError::AlreadyQuarantined {
                    component_id: component_id.to_string(),
                });
            }
        }

        let quarantine_count = self
            .components
            .get(component_id)
            .map_or(0, |c| c.quarantine_count);

        let component = QuarantinedComponent {
            component_id: component_id.to_string(),
            component_kind: component_kind.clone(),
            state: QuarantineState::Quarantined,
            severity,
            reason,
            quarantined_at_ms: now_ms,
            expires_at_ms,
            last_reviewed_at_ms: now_ms,
            imposed_by: imposed_by.to_string(),
            quarantine_count: quarantine_count + 1,
        };

        self.components
            .insert(component_id.to_string(), component);
        self.telemetry.quarantines_imposed += 1;

        self.emit_audit(QuarantineAuditEvent {
            timestamp_ms: now_ms,
            event_type: QuarantineAuditType::Imposed,
            component_id: component_id.to_string(),
            component_kind: Some(component_kind),
            actor: imposed_by.to_string(),
            detail: format!("quarantined at severity {severity}"),
        });

        Ok(())
    }

    /// Release a component from quarantine (optionally into probation).
    pub fn release(
        &mut self,
        component_id: &str,
        released_by: &str,
        probation: bool,
        now_ms: u64,
    ) -> Result<(), QuarantineError> {
        let (component_kind, audit_type) = {
            let component =
                self.components
                    .get_mut(component_id)
                    .ok_or(QuarantineError::ComponentNotFound {
                        component_id: component_id.to_string(),
                    })?;

            if component.state != QuarantineState::Quarantined {
                return Err(QuarantineError::NotQuarantined {
                    component_id: component_id.to_string(),
                });
            }

            let kind = component.component_kind.clone();
            if probation {
                component.state = QuarantineState::ProbationaryRelease;
                component.last_reviewed_at_ms = now_ms;
                (kind, QuarantineAuditType::ProbationStarted)
            } else {
                component.state = QuarantineState::Clear;
                component.last_reviewed_at_ms = now_ms;
                (kind, QuarantineAuditType::Released)
            }
        };

        if probation {
            self.telemetry.probations_started += 1;
            self.emit_audit(QuarantineAuditEvent {
                timestamp_ms: now_ms,
                event_type: audit_type,
                component_id: component_id.to_string(),
                component_kind: Some(component_kind),
                actor: released_by.to_string(),
                detail: "released to probation".to_string(),
            });
        } else {
            self.telemetry.quarantines_released += 1;
            self.emit_audit(QuarantineAuditEvent {
                timestamp_ms: now_ms,
                event_type: audit_type,
                component_id: component_id.to_string(),
                component_kind: Some(component_kind),
                actor: released_by.to_string(),
                detail: "released from quarantine".to_string(),
            });
        }

        Ok(())
    }

    /// Complete probation — clear the component.
    pub fn complete_probation(
        &mut self,
        component_id: &str,
        now_ms: u64,
    ) -> Result<(), QuarantineError> {
        let component_kind = {
            let component =
                self.components
                    .get_mut(component_id)
                    .ok_or(QuarantineError::ComponentNotFound {
                        component_id: component_id.to_string(),
                    })?;

            if component.state != QuarantineState::ProbationaryRelease {
                return Err(QuarantineError::NotQuarantined {
                    component_id: component_id.to_string(),
                });
            }

            component.state = QuarantineState::Clear;
            component.last_reviewed_at_ms = now_ms;
            component.component_kind.clone()
        };

        self.telemetry.probations_completed += 1;
        self.emit_audit(QuarantineAuditEvent {
            timestamp_ms: now_ms,
            event_type: QuarantineAuditType::ProbationCompleted,
            component_id: component_id.to_string(),
            component_kind: Some(component_kind),
            actor: "system".to_string(),
            detail: "probation completed successfully".to_string(),
        });

        Ok(())
    }

    /// Revoke probation — re-quarantine the component.
    pub fn revoke_probation(
        &mut self,
        component_id: &str,
        reason: QuarantineReason,
        now_ms: u64,
    ) -> Result<(), QuarantineError> {
        let component_kind = {
            let component =
                self.components
                    .get_mut(component_id)
                    .ok_or(QuarantineError::ComponentNotFound {
                        component_id: component_id.to_string(),
                    })?;

            if component.state != QuarantineState::ProbationaryRelease {
                return Err(QuarantineError::NotQuarantined {
                    component_id: component_id.to_string(),
                });
            }

            component.state = QuarantineState::Quarantined;
            component.reason = reason;
            component.quarantine_count += 1;
            component.last_reviewed_at_ms = now_ms;
            component.component_kind.clone()
        };

        self.telemetry.probations_revoked += 1;
        self.telemetry.quarantines_imposed += 1;
        self.emit_audit(QuarantineAuditEvent {
            timestamp_ms: now_ms,
            event_type: QuarantineAuditType::ProbationRevoked,
            component_id: component_id.to_string(),
            component_kind: Some(component_kind),
            actor: "system".to_string(),
            detail: "probation revoked, re-quarantined".to_string(),
        });

        Ok(())
    }

    /// Escalate quarantine severity.
    pub fn escalate(
        &mut self,
        component_id: &str,
        actor: &str,
        now_ms: u64,
    ) -> Result<QuarantineSeverity, QuarantineError> {
        let (component_kind, new_severity) = {
            let component =
                self.components
                    .get_mut(component_id)
                    .ok_or(QuarantineError::ComponentNotFound {
                        component_id: component_id.to_string(),
                    })?;

            let new_severity = match component.severity {
                QuarantineSeverity::Advisory => QuarantineSeverity::Restricted,
                QuarantineSeverity::Restricted => QuarantineSeverity::Isolated,
                QuarantineSeverity::Isolated => QuarantineSeverity::Terminated,
                QuarantineSeverity::Terminated => return Err(QuarantineError::AlreadyMaxSeverity),
            };

            component.severity = new_severity;
            component.last_reviewed_at_ms = now_ms;
            (component.component_kind.clone(), new_severity)
        };

        self.telemetry.severity_escalations += 1;
        self.emit_audit(QuarantineAuditEvent {
            timestamp_ms: now_ms,
            event_type: QuarantineAuditType::SeverityEscalated,
            component_id: component_id.to_string(),
            component_kind: Some(component_kind),
            actor: actor.to_string(),
            detail: format!("escalated to {new_severity}"),
        });

        Ok(new_severity)
    }

    /// Deescalate quarantine severity.
    pub fn deescalate(
        &mut self,
        component_id: &str,
        actor: &str,
        now_ms: u64,
    ) -> Result<QuarantineSeverity, QuarantineError> {
        let (component_kind, new_severity) = {
            let component =
                self.components
                    .get_mut(component_id)
                    .ok_or(QuarantineError::ComponentNotFound {
                        component_id: component_id.to_string(),
                    })?;

            let new_severity = match component.severity {
                QuarantineSeverity::Advisory => return Err(QuarantineError::AlreadyMinSeverity),
                QuarantineSeverity::Restricted => QuarantineSeverity::Advisory,
                QuarantineSeverity::Isolated => QuarantineSeverity::Restricted,
                QuarantineSeverity::Terminated => QuarantineSeverity::Isolated,
            };

            component.severity = new_severity;
            component.last_reviewed_at_ms = now_ms;
            (component.component_kind.clone(), new_severity)
        };

        self.telemetry.severity_deescalations += 1;
        self.emit_audit(QuarantineAuditEvent {
            timestamp_ms: now_ms,
            event_type: QuarantineAuditType::SeverityDeescalated,
            component_id: component_id.to_string(),
            component_kind: Some(component_kind),
            actor: actor.to_string(),
            detail: format!("deescalated to {new_severity}"),
        });

        Ok(new_severity)
    }

    /// Expire quarantines that have passed their TTL.
    pub fn expire_quarantines(&mut self, now_ms: u64) -> Vec<String> {
        let mut expired = Vec::new();
        for (id, comp) in &mut self.components {
            if comp.state == QuarantineState::Quarantined && comp.is_expired_at(now_ms) {
                comp.state = QuarantineState::Clear;
                comp.last_reviewed_at_ms = now_ms;
                expired.push(id.clone());
            }
        }
        for id in &expired {
            self.telemetry.quarantines_expired += 1;
            let kind = self.components[id].component_kind.clone();
            self.emit_audit(QuarantineAuditEvent {
                timestamp_ms: now_ms,
                event_type: QuarantineAuditType::Expired,
                component_id: id.clone(),
                component_kind: Some(kind),
                actor: "system".to_string(),
                detail: "quarantine expired".to_string(),
            });
        }
        expired
    }

    // ---- Kill switch operations ----

    /// Trip the kill switch.
    pub fn trip_kill_switch(
        &mut self,
        level: KillSwitchLevel,
        by: &str,
        reason: &str,
        now_ms: u64,
    ) {
        self.kill_switch.trip(level, by, reason, now_ms);
        self.telemetry.kill_switch_trips += 1;
        self.emit_audit(QuarantineAuditEvent {
            timestamp_ms: now_ms,
            event_type: QuarantineAuditType::KillSwitchTripped,
            component_id: String::new(),
            component_kind: None,
            actor: by.to_string(),
            detail: format!("kill switch tripped to {level}: {reason}"),
        });
    }

    /// Trip the kill switch with auto-disarm timeout.
    pub fn trip_kill_switch_with_timeout(
        &mut self,
        level: KillSwitchLevel,
        by: &str,
        reason: &str,
        now_ms: u64,
        timeout_ms: u64,
    ) {
        self.kill_switch
            .trip_with_timeout(level, by, reason, now_ms, timeout_ms);
        self.telemetry.kill_switch_trips += 1;
        self.emit_audit(QuarantineAuditEvent {
            timestamp_ms: now_ms,
            event_type: QuarantineAuditType::KillSwitchTripped,
            component_id: String::new(),
            component_kind: None,
            actor: by.to_string(),
            detail: format!(
                "kill switch tripped to {level} (auto-disarm in {timeout_ms}ms): {reason}"
            ),
        });
    }

    /// Reset the kill switch.
    pub fn reset_kill_switch(&mut self, by: &str, now_ms: u64) {
        self.kill_switch.reset(by, now_ms);
        self.telemetry.kill_switch_resets += 1;
        self.emit_audit(QuarantineAuditEvent {
            timestamp_ms: now_ms,
            event_type: QuarantineAuditType::KillSwitchReset,
            component_id: String::new(),
            component_kind: None,
            actor: by.to_string(),
            detail: "kill switch reset".to_string(),
        });
    }

    /// Tick the kill switch for auto-disarm.
    pub fn tick_kill_switch(&mut self, now_ms: u64) -> bool {
        if self.kill_switch.tick(now_ms) {
            self.emit_audit(QuarantineAuditEvent {
                timestamp_ms: now_ms,
                event_type: QuarantineAuditType::KillSwitchAutoDisarmed,
                component_id: String::new(),
                component_kind: None,
                actor: "auto_disarm".to_string(),
                detail: "kill switch auto-disarmed after timeout".to_string(),
            });
            true
        } else {
            false
        }
    }

    // ---- Query helpers ----

    /// Get a quarantined component by ID.
    #[must_use]
    pub fn get(&self, component_id: &str) -> Option<&QuarantinedComponent> {
        self.components.get(component_id)
    }

    /// Check if a component is currently quarantined.
    #[must_use]
    pub fn is_quarantined(&self, component_id: &str) -> bool {
        self.components
            .get(component_id)
            .is_some_and(|c| c.state == QuarantineState::Quarantined)
    }

    /// Check if a component is blocked from performing writes.
    #[must_use]
    pub fn is_blocked_for_writes(&self, component_id: &str) -> bool {
        if !self.kill_switch.allows_new_workflows() {
            return true;
        }
        self.components
            .get(component_id)
            .is_some_and(|c| c.blocks_writes())
    }

    /// Check if a component is blocked from all actions.
    #[must_use]
    pub fn is_blocked_for_all(&self, component_id: &str) -> bool {
        if self.kill_switch.is_emergency() {
            return true;
        }
        self.components
            .get(component_id)
            .is_some_and(|c| c.blocks_all())
    }

    /// List all actively quarantined component IDs.
    #[must_use]
    pub fn active_quarantines(&self) -> Vec<String> {
        self.components
            .iter()
            .filter(|(_, c)| c.state == QuarantineState::Quarantined)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// List all components in probation.
    #[must_use]
    pub fn probationary_components(&self) -> Vec<String> {
        self.components
            .iter()
            .filter(|(_, c)| c.state == QuarantineState::ProbationaryRelease)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Get the current kill switch state.
    #[must_use]
    pub fn kill_switch(&self) -> &KillSwitch {
        &self.kill_switch
    }

    /// Get the audit log.
    #[must_use]
    pub fn audit_log(&self) -> &VecDeque<QuarantineAuditEvent> {
        &self.audit_log
    }

    /// Get telemetry snapshot.
    #[must_use]
    pub fn telemetry_snapshot(&self, now_ms: u64) -> QuarantineTelemetrySnapshot {
        QuarantineTelemetrySnapshot {
            captured_at_ms: now_ms,
            counters: self.telemetry.clone(),
            active_quarantines: self
                .components
                .values()
                .filter(|c| c.state == QuarantineState::Quarantined)
                .count() as u32,
            kill_switch_level: self.kill_switch.level,
        }
    }

    // ---- Internal helpers ----

    fn emit_audit(&mut self, event: QuarantineAuditEvent) {
        if self.audit_log.len() >= self.max_audit_events {
            self.audit_log.pop_front();
        }
        self.audit_log.push_back(event);
    }
}

impl Default for QuarantineRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_reason() -> QuarantineReason {
        QuarantineReason::PolicyViolation {
            rule_id: "rule-1".to_string(),
            detail: "exceeded rate limit".to_string(),
        }
    }

    // ---- Quarantine lifecycle ----

    #[test]
    fn quarantine_and_release() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "conn-1",
            ComponentKind::Connector,
            QuarantineSeverity::Restricted,
            test_reason(),
            "operator",
            1000,
            0,
        )
        .unwrap();

        assert!(reg.is_quarantined("conn-1"));
        assert!(reg.is_blocked_for_writes("conn-1"));
        assert!(!reg.is_blocked_for_all("conn-1"));

        reg.release("conn-1", "operator", false, 2000).unwrap();
        assert!(!reg.is_quarantined("conn-1"));
        assert!(!reg.is_blocked_for_writes("conn-1"));
    }

    #[test]
    fn quarantine_already_quarantined_fails() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "conn-1",
            ComponentKind::Connector,
            QuarantineSeverity::Isolated,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();

        let err = reg
            .quarantine(
                "conn-1",
                ComponentKind::Connector,
                QuarantineSeverity::Isolated,
                test_reason(),
                "op",
                2000,
                0,
            )
            .unwrap_err();
        assert!(matches!(err, QuarantineError::AlreadyQuarantined { .. }));
    }

    #[test]
    fn release_nonexistent_fails() {
        let mut reg = QuarantineRegistry::new();
        let err = reg.release("ghost", "op", false, 1000).unwrap_err();
        assert!(matches!(err, QuarantineError::ComponentNotFound { .. }));
    }

    #[test]
    fn release_not_quarantined_fails() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1",
            ComponentKind::Pane,
            QuarantineSeverity::Isolated,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();
        reg.release("c1", "op", false, 2000).unwrap();

        let err = reg.release("c1", "op", false, 3000).unwrap_err();
        assert!(matches!(err, QuarantineError::NotQuarantined { .. }));
    }

    // ---- Probation ----

    #[test]
    fn probation_workflow() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "conn-1",
            ComponentKind::Connector,
            QuarantineSeverity::Isolated,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();

        // Release to probation
        reg.release("conn-1", "op", true, 2000).unwrap();
        assert!(!reg.is_quarantined("conn-1"));
        assert_eq!(reg.probationary_components(), vec!["conn-1"]);

        // Complete probation
        reg.complete_probation("conn-1", 3000).unwrap();
        assert!(reg.probationary_components().is_empty());
        assert_eq!(reg.get("conn-1").unwrap().state, QuarantineState::Clear);
    }

    #[test]
    fn revoke_probation() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1",
            ComponentKind::Agent,
            QuarantineSeverity::Restricted,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();
        reg.release("c1", "op", true, 2000).unwrap();

        let revoke_reason = QuarantineReason::AnomalousBehavior {
            metric: "error_rate".to_string(),
            observed: "25%".to_string(),
        };
        reg.revoke_probation("c1", revoke_reason, 3000).unwrap();
        assert!(reg.is_quarantined("c1"));
        assert_eq!(reg.get("c1").unwrap().quarantine_count, 2);
    }

    // ---- Severity escalation ----

    #[test]
    fn escalate_severity() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1",
            ComponentKind::Workflow,
            QuarantineSeverity::Advisory,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();

        assert!(!reg.is_blocked_for_writes("c1")); // Advisory doesn't block

        let new = reg.escalate("c1", "op", 2000).unwrap();
        assert_eq!(new, QuarantineSeverity::Restricted);
        assert!(reg.is_blocked_for_writes("c1"));

        let new = reg.escalate("c1", "op", 3000).unwrap();
        assert_eq!(new, QuarantineSeverity::Isolated);
        assert!(reg.is_blocked_for_all("c1"));

        let new = reg.escalate("c1", "op", 4000).unwrap();
        assert_eq!(new, QuarantineSeverity::Terminated);

        let err = reg.escalate("c1", "op", 5000).unwrap_err();
        assert!(matches!(err, QuarantineError::AlreadyMaxSeverity));
    }

    #[test]
    fn deescalate_severity() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1",
            ComponentKind::Session,
            QuarantineSeverity::Terminated,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();

        let new = reg.deescalate("c1", "op", 2000).unwrap();
        assert_eq!(new, QuarantineSeverity::Isolated);

        let new = reg.deescalate("c1", "op", 3000).unwrap();
        assert_eq!(new, QuarantineSeverity::Restricted);

        let new = reg.deescalate("c1", "op", 4000).unwrap();
        assert_eq!(new, QuarantineSeverity::Advisory);

        let err = reg.deescalate("c1", "op", 5000).unwrap_err();
        assert!(matches!(err, QuarantineError::AlreadyMinSeverity));
    }

    // ---- Quarantine expiry ----

    #[test]
    fn expire_quarantine() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1",
            ComponentKind::Connector,
            QuarantineSeverity::Isolated,
            test_reason(),
            "op",
            1000,
            5000,
        )
        .unwrap();

        let expired = reg.expire_quarantines(4999);
        assert!(expired.is_empty());

        let expired = reg.expire_quarantines(5000);
        assert_eq!(expired, vec!["c1"]);
        assert!(!reg.is_quarantined("c1"));
        assert_eq!(reg.telemetry.quarantines_expired, 1);
    }

    #[test]
    fn indefinite_quarantine_never_expires() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1",
            ComponentKind::Connector,
            QuarantineSeverity::Isolated,
            test_reason(),
            "op",
            1000,
            0, // indefinite
        )
        .unwrap();

        let expired = reg.expire_quarantines(u64::MAX);
        assert!(expired.is_empty());
        assert!(reg.is_quarantined("c1"));
    }

    // ---- Kill switch ----

    #[test]
    fn kill_switch_blocks_workflows() {
        let mut reg = QuarantineRegistry::new();
        assert!(reg.kill_switch().allows_new_workflows());
        assert!(reg.kill_switch().allows_inflight());

        reg.trip_kill_switch(KillSwitchLevel::SoftStop, "op", "incident", 1000);
        assert!(!reg.kill_switch().allows_new_workflows());
        assert!(reg.kill_switch().allows_inflight());

        reg.trip_kill_switch(KillSwitchLevel::HardStop, "op", "escalation", 2000);
        assert!(!reg.kill_switch().allows_new_workflows());
        assert!(!reg.kill_switch().allows_inflight());
        assert!(!reg.kill_switch().is_emergency());

        reg.trip_kill_switch(KillSwitchLevel::EmergencyHalt, "op", "critical", 3000);
        assert!(reg.kill_switch().is_emergency());
    }

    #[test]
    fn kill_switch_reset() {
        let mut reg = QuarantineRegistry::new();
        reg.trip_kill_switch(KillSwitchLevel::HardStop, "op", "test", 1000);
        reg.reset_kill_switch("op", 2000);
        assert!(reg.kill_switch().allows_new_workflows());
        assert_eq!(reg.kill_switch().level, KillSwitchLevel::Disarmed);
    }

    #[test]
    fn kill_switch_auto_disarm() {
        let mut reg = QuarantineRegistry::new();
        reg.trip_kill_switch_with_timeout(
            KillSwitchLevel::SoftStop,
            "automation",
            "temporary pause",
            1000,
            5000,
        );

        assert!(!reg.tick_kill_switch(5999));
        assert!(!reg.kill_switch().allows_new_workflows());

        assert!(reg.tick_kill_switch(6000));
        assert!(reg.kill_switch().allows_new_workflows());
    }

    #[test]
    fn kill_switch_blocks_write_for_any_component() {
        let mut reg = QuarantineRegistry::new();
        reg.trip_kill_switch(KillSwitchLevel::SoftStop, "op", "test", 1000);
        // Any component is blocked for writes when kill switch is active
        assert!(reg.is_blocked_for_writes("unknown-component"));
    }

    #[test]
    fn emergency_halt_blocks_all_for_any_component() {
        let mut reg = QuarantineRegistry::new();
        reg.trip_kill_switch(KillSwitchLevel::EmergencyHalt, "op", "test", 1000);
        assert!(reg.is_blocked_for_all("unknown-component"));
    }

    // ---- Telemetry ----

    #[test]
    fn telemetry_snapshot() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1",
            ComponentKind::Connector,
            QuarantineSeverity::Isolated,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();
        reg.quarantine(
            "c2",
            ComponentKind::Agent,
            QuarantineSeverity::Advisory,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();

        let snap = reg.telemetry_snapshot(2000);
        assert_eq!(snap.active_quarantines, 2);
        assert_eq!(snap.counters.quarantines_imposed, 2);
        assert_eq!(snap.kill_switch_level, KillSwitchLevel::Disarmed);
    }

    #[test]
    fn telemetry_serde_roundtrip() {
        let snap = QuarantineTelemetrySnapshot {
            captured_at_ms: 1000,
            counters: QuarantineTelemetry {
                quarantines_imposed: 5,
                quarantines_released: 3,
                quarantines_expired: 1,
                probations_started: 2,
                probations_completed: 1,
                probations_revoked: 1,
                severity_escalations: 2,
                severity_deescalations: 1,
                kill_switch_trips: 1,
                kill_switch_resets: 1,
            },
            active_quarantines: 1,
            kill_switch_level: KillSwitchLevel::Disarmed,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: QuarantineTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // ---- Audit log ----

    #[test]
    fn audit_log_records_events() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1",
            ComponentKind::Connector,
            QuarantineSeverity::Isolated,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();
        reg.release("c1", "op", false, 2000).unwrap();
        assert_eq!(reg.audit_log().len(), 2); // imposed + released
    }

    #[test]
    fn audit_log_bounded() {
        let mut reg = QuarantineRegistry::new();
        for i in 0..DEFAULT_MAX_AUDIT_EVENTS + 100 {
            reg.quarantine(
                &format!("c{i}"),
                ComponentKind::Pane,
                QuarantineSeverity::Advisory,
                test_reason(),
                "op",
                i as u64,
                0,
            )
            .unwrap();
        }
        assert!(reg.audit_log().len() <= DEFAULT_MAX_AUDIT_EVENTS);
    }

    // ---- Display impls ----

    #[test]
    fn quarantine_reason_display() {
        let r = QuarantineReason::PolicyViolation {
            rule_id: "r1".into(),
            detail: "bad".into(),
        };
        assert_eq!(r.to_string(), "policy_violation(r1): bad");

        let r = QuarantineReason::CascadeFromParent {
            parent_component_id: "parent".into(),
        };
        assert_eq!(r.to_string(), "cascade_from(parent)");
    }

    #[test]
    fn severity_ordering() {
        assert!(QuarantineSeverity::Advisory < QuarantineSeverity::Restricted);
        assert!(QuarantineSeverity::Restricted < QuarantineSeverity::Isolated);
        assert!(QuarantineSeverity::Isolated < QuarantineSeverity::Terminated);
    }

    #[test]
    fn kill_switch_level_ordering() {
        assert!(KillSwitchLevel::Disarmed < KillSwitchLevel::SoftStop);
        assert!(KillSwitchLevel::SoftStop < KillSwitchLevel::HardStop);
        assert!(KillSwitchLevel::HardStop < KillSwitchLevel::EmergencyHalt);
    }

    #[test]
    fn component_kind_display() {
        assert_eq!(ComponentKind::Connector.to_string(), "connector");
        assert_eq!(ComponentKind::Agent.to_string(), "agent");
        assert_eq!(ComponentKind::Session.to_string(), "session");
    }

    #[test]
    fn quarantine_state_display() {
        assert_eq!(QuarantineState::Clear.to_string(), "clear");
        assert_eq!(QuarantineState::Quarantined.to_string(), "quarantined");
        assert_eq!(
            QuarantineState::ProbationaryRelease.to_string(),
            "probationary_release"
        );
    }

    #[test]
    fn audit_type_display() {
        assert_eq!(QuarantineAuditType::Imposed.to_string(), "imposed");
        assert_eq!(
            QuarantineAuditType::KillSwitchTripped.to_string(),
            "kill_switch_tripped"
        );
        assert_eq!(
            QuarantineAuditType::ProbationCompleted.to_string(),
            "probation_completed"
        );
    }

    // ---- Error display ----

    #[test]
    fn error_display() {
        let e = QuarantineError::ComponentNotFound {
            component_id: "x".into(),
        };
        assert_eq!(e.to_string(), "component not found: x");

        let e = QuarantineError::KillSwitchActive {
            level: KillSwitchLevel::HardStop,
        };
        assert_eq!(
            e.to_string(),
            "kill switch blocks operation: level=hard_stop"
        );
    }

    // ---- Default impls ----

    #[test]
    fn default_registry() {
        let reg = QuarantineRegistry::default();
        assert!(reg.active_quarantines().is_empty());
        assert!(reg.kill_switch().allows_new_workflows());
    }

    #[test]
    fn default_kill_switch() {
        let ks = KillSwitch::default();
        assert_eq!(ks.level, KillSwitchLevel::Disarmed);
        assert!(ks.allows_new_workflows());
        assert!(ks.allows_inflight());
        assert!(!ks.is_emergency());
    }

    // ---- Re-quarantine after release ----

    #[test]
    fn re_quarantine_after_release() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1",
            ComponentKind::Connector,
            QuarantineSeverity::Isolated,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();
        reg.release("c1", "op", false, 2000).unwrap();

        // Can quarantine again after release
        reg.quarantine(
            "c1",
            ComponentKind::Connector,
            QuarantineSeverity::Restricted,
            QuarantineReason::OperatorDirected {
                operator: "admin".to_string(),
                note: "second incident".to_string(),
            },
            "admin",
            3000,
            0,
        )
        .unwrap();

        assert!(reg.is_quarantined("c1"));
        assert_eq!(reg.get("c1").unwrap().quarantine_count, 2);
    }

    // ---- Multiple components ----

    #[test]
    fn multiple_components_independent() {
        let mut reg = QuarantineRegistry::new();
        reg.quarantine(
            "c1",
            ComponentKind::Connector,
            QuarantineSeverity::Isolated,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();
        reg.quarantine(
            "c2",
            ComponentKind::Agent,
            QuarantineSeverity::Advisory,
            test_reason(),
            "op",
            1000,
            0,
        )
        .unwrap();

        assert_eq!(reg.active_quarantines().len(), 2);

        reg.release("c1", "op", false, 2000).unwrap();
        assert_eq!(reg.active_quarantines(), vec!["c2"]);
        assert!(reg.is_quarantined("c2"));
        assert!(!reg.is_quarantined("c1"));
    }
}
