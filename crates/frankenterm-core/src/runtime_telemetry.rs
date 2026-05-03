//! Unified runtime telemetry schema and logging harmonization (ft-e34d9.10.7.1).
//!
//! Standardizes runtime telemetry fields for scopes, cancellation states,
//! queue/backpressure, and failure classes across all FrankenTerm subsystems.
//!
//! # Motivation
//!
//! Before this module, each subsystem defined its own event structures:
//! - `MissionEventKind` / `MissionEvent` (mission_events.rs)
//! - `TxEventKind` / `TxObservabilityEvent` (tx_observability.rs)
//! - `AegisLogEvent` (aegis_diagnostics.rs)
//! - `StorageHealthTier` (storage_telemetry.rs)
//! - `NetworkPressureTier` (network_observer.rs)
//!
//! This module defines a **common envelope** that all subsystems can emit into,
//! enabling unified filtering, routing, aggregation, and forensic queries.
//!
//! # Schema
//!
//! Every `RuntimeTelemetryEvent` carries these fields:
//!
//! | Field              | Type                    | Purpose                            |
//! |--------------------|-------------------------|------------------------------------|
//! | `timestamp_ms`     | `u64`                   | Epoch millis (monotonic-safe)      |
//! | `component`        | `String`                | Emitting subsystem (dot-separated) |
//! | `scope_id`         | `Option<String>`        | Scope tree node (e.g. `daemon:capture`) |
//! | `event_kind`       | `RuntimeTelemetryKind`  | What happened                      |
//! | `health_tier`      | `HealthTier`            | 4-tier severity classification     |
//! | `phase`            | `RuntimePhase`          | Lifecycle phase                    |
//! | `reason_code`      | `String`                | Stable `subsystem.phase.detail`    |
//! | `correlation_id`   | `String`                | Cross-event correlation            |
//! | `details`          | `HashMap<String, Value>`| Type-erased payload                |
//!
//! # Health tier unification
//!
//! The project uses a consistent 4-tier pattern across backpressure, storage,
//! network, and aegis subsystems. `HealthTier` is the canonical representation:
//!
//! ```text
//! Green  → nominal operation
//! Yellow → elevated load / early warning
//! Red    → high pressure / degraded
//! Black  → critical overload / failure
//! ```

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::LazyLock;
use std::time::SystemTime;

use crate::connector_credential_broker::{CredentialAuditEvent, CredentialAuditType};
use crate::connector_data_classification::{DataSensitivity, RedactionStrategy};
use crate::connector_event_model::{CanonicalConnectorEvent, CanonicalSeverity};
use crate::connector_host_runtime::{
    ConnectorCapability, ConnectorFailureClass, ConnectorLifecyclePhase,
};
use crate::diagnostic_redaction::DiagnosticFieldPolicy;
use crate::recorder_audit::{AccessTier, AuditEventType, AuthzDecision, RecorderAuditEntry};
use crate::swarm_scheduler::{ScaleEventType, SchedulerSnapshot};
use crate::swarm_work_queue::QueueStats;
use crate::telemetry::{Histogram, HistogramSummary};
use frankenterm_core_connector_types::CredentialBrokerTelemetrySnapshot;

// =============================================================================
// Health tier (unified 4-tier pattern)
// =============================================================================

/// Unified health tier classification used across all subsystems.
///
/// Maps to the project-wide Green/Yellow/Red/Black pattern used in:
/// - `BackpressureTier` (backpressure.rs)
/// - `StorageHealthTier` (storage_telemetry.rs)
/// - `NetworkPressureTier` (network_observer.rs)
/// - Aegis severity thresholds (aegis_diagnostics.rs)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthTier {
    /// Nominal operation. All metrics within budget.
    Green = 0,
    /// Elevated load. Early warning thresholds breached.
    Yellow = 1,
    /// High pressure. Degraded operation, throttling active.
    Red = 2,
    /// Critical overload. Shedding load, potential data loss.
    Black = 3,
}

impl HealthTier {
    /// Numeric severity value (0–3).
    #[must_use]
    pub fn severity(self) -> u8 {
        self as u8
    }

    /// Whether this tier requires operator attention.
    #[must_use]
    pub fn requires_attention(self) -> bool {
        matches!(self, Self::Red | Self::Black)
    }

    /// Whether this tier is in a degraded state (Yellow or worse).
    #[must_use]
    pub fn is_degraded(self) -> bool {
        self != Self::Green
    }

    /// Convert from a ratio (0.0–1.0) using standard thresholds.
    ///
    /// The thresholds match the project-wide convention:
    /// - `< 0.50` → Green
    /// - `< 0.80` → Yellow
    /// - `< 0.95` → Red
    /// - `>= 0.95` → Black
    #[must_use]
    pub fn from_ratio(ratio: f64) -> Self {
        if ratio < 0.50 {
            Self::Green
        } else if ratio < 0.80 {
            Self::Yellow
        } else if ratio < 0.95 {
            Self::Red
        } else {
            Self::Black
        }
    }
}

impl fmt::Display for HealthTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Green => f.write_str("green"),
            Self::Yellow => f.write_str("yellow"),
            Self::Red => f.write_str("red"),
            Self::Black => f.write_str("black"),
        }
    }
}

// =============================================================================
// Runtime phase (lifecycle)
// =============================================================================

/// Lifecycle phase for runtime telemetry events.
///
/// Covers the full lifecycle from startup through steady-state to shutdown,
/// plus transient phases for cancellation and recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePhase {
    /// System initialization and configuration loading.
    Init,
    /// Scope tree construction and daemon startup.
    Startup,
    /// Normal steady-state operation.
    Running,
    /// Graceful shutdown phase 1: draining in-flight work.
    Draining,
    /// Graceful shutdown phase 2: running finalizers.
    Finalizing,
    /// Shutdown complete, all scopes closed.
    Shutdown,
    /// Active cancellation propagation.
    Cancelling,
    /// Recovery from error or degraded state.
    Recovering,
    /// Maintenance operations (GC, compaction, checkpoint).
    Maintenance,
}

impl RuntimePhase {
    /// Whether this phase is terminal (no further transitions expected).
    #[must_use]
    pub fn is_terminal(self) -> bool {
        self == Self::Shutdown
    }

    /// Whether this phase represents active shutdown processing.
    #[must_use]
    pub fn is_shutting_down(self) -> bool {
        matches!(self, Self::Draining | Self::Finalizing | Self::Cancelling)
    }
}

impl fmt::Display for RuntimePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Init => "init",
            Self::Startup => "startup",
            Self::Running => "running",
            Self::Draining => "draining",
            Self::Finalizing => "finalizing",
            Self::Shutdown => "shutdown",
            Self::Cancelling => "cancelling",
            Self::Recovering => "recovering",
            Self::Maintenance => "maintenance",
        };
        f.write_str(s)
    }
}

// =============================================================================
// Event kind taxonomy
// =============================================================================

/// Unified runtime telemetry event taxonomy.
///
/// Covers all subsystem event classes. Each variant maps to a broad operational
/// category; specific details are carried in `reason_code` and `details`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTelemetryKind {
    // ── Scope lifecycle ──
    /// A scope was created in the scope tree.
    ScopeCreated,
    /// A scope transitioned to Running state.
    ScopeStarted,
    /// A scope entered draining (shutdown phase 1).
    ScopeDraining,
    /// A scope is finalizing (shutdown phase 2).
    ScopeFinalizing,
    /// A scope was fully closed.
    ScopeClosed,

    // ── Cancellation ──
    /// Cancellation was requested for a scope.
    CancellationRequested,
    /// Cancellation propagated to child scopes.
    CancellationPropagated,
    /// Grace period expired during drain.
    GracePeriodExpired,
    /// A finalizer completed (success or failure).
    FinalizerCompleted,

    // ── Backpressure ──
    /// Health tier transitioned (e.g. Green → Yellow).
    TierTransition,
    /// Backpressure throttle action applied.
    ThrottleApplied,
    /// Backpressure recovered (tier decreased).
    ThrottleReleased,
    /// Load shedding activated (Black tier).
    LoadShedding,

    // ── Queue/Channel ──
    /// Queue depth observation recorded.
    QueueDepthObserved,
    /// Channel closed (expected or unexpected).
    ChannelClosed,
    /// Permit exhaustion detected (semaphore/resource pool).
    PermitExhausted,

    // ── Error/Failure ──
    /// A transient error occurred (retryable).
    TransientError,
    /// A permanent error occurred (not retryable).
    PermanentError,
    /// A panic was caught and contained.
    PanicCaptured,
    /// An invariant violation was detected.
    InvariantViolation,
    /// A safety policy was triggered.
    SafetyPolicyTriggered,

    // ── Resource ──
    /// Resource usage observation (memory, FDs, connections).
    ResourceObserved,
    /// Resource budget exhausted.
    ResourceExhausted,

    // ── Operational ──
    /// SLO measurement recorded.
    SloMeasurement,
    /// Configuration change applied.
    ConfigApplied,
    /// Diagnostic dump exported.
    DiagnosticExported,
    /// Heartbeat from a running scope.
    Heartbeat,
}

impl RuntimeTelemetryKind {
    /// The subsystem category for this event kind.
    #[must_use]
    pub fn category(self) -> &'static str {
        match self {
            Self::ScopeCreated
            | Self::ScopeStarted
            | Self::ScopeDraining
            | Self::ScopeFinalizing
            | Self::ScopeClosed => "scope",

            Self::CancellationRequested
            | Self::CancellationPropagated
            | Self::GracePeriodExpired
            | Self::FinalizerCompleted => "cancellation",

            Self::TierTransition
            | Self::ThrottleApplied
            | Self::ThrottleReleased
            | Self::LoadShedding => "backpressure",

            Self::QueueDepthObserved | Self::ChannelClosed | Self::PermitExhausted => "queue",

            Self::TransientError
            | Self::PermanentError
            | Self::PanicCaptured
            | Self::InvariantViolation
            | Self::SafetyPolicyTriggered => "error",

            Self::ResourceObserved | Self::ResourceExhausted => "resource",

            Self::SloMeasurement
            | Self::ConfigApplied
            | Self::DiagnosticExported
            | Self::Heartbeat => "operational",
        }
    }
}

impl fmt::Display for RuntimeTelemetryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Use serde's snake_case representation for Display
        let json = serde_json::to_value(self).unwrap_or_default();
        if let Some(s) = json.as_str() {
            f.write_str(s)
        } else {
            write!(f, "{self:?}")
        }
    }
}

// =============================================================================
// Failure class taxonomy
// =============================================================================

/// Classification of failures for structured alerting and routing.
///
/// Harmonizes error classes from:
/// - `NetworkErrorKind` (network_reliability.rs): transient/permanent/degraded
/// - `RecorderStorageErrorClass` (recorder_storage.rs): overload/retryable/terminal/corruption
/// - `ErrorCode` (test harness reason_codes.rs): assertion/timeout/panic/deadlock/etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Transient error — retryable after backoff.
    Transient,
    /// Permanent error — retry will not help.
    Permanent,
    /// Degraded operation — reduced capacity but functional.
    Degraded,
    /// Resource overload — load shedding or circuit breaking needed.
    Overload,
    /// Data corruption — integrity check failed.
    Corruption,
    /// Timeout — operation exceeded deadline.
    Timeout,
    /// Panic — caught unwind from unexpected code path.
    Panic,
    /// Deadlock — detected or suspected livelock/deadlock.
    Deadlock,
    /// Safety — policy or invariant violation.
    Safety,
    /// Configuration — invalid or incompatible configuration.
    Configuration,
}

impl FailureClass {
    /// Whether a retry is potentially useful for this failure class.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Transient | Self::Degraded | Self::Timeout)
    }

    /// Suggested health tier escalation for this failure class.
    #[must_use]
    pub fn suggested_tier(self) -> HealthTier {
        match self {
            Self::Transient | Self::Timeout => HealthTier::Yellow,
            Self::Degraded | Self::Overload => HealthTier::Red,
            Self::Corruption | Self::Panic | Self::Deadlock | Self::Safety => HealthTier::Black,
            Self::Permanent | Self::Configuration => HealthTier::Red,
        }
    }
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
            Self::Degraded => "degraded",
            Self::Overload => "overload",
            Self::Corruption => "corruption",
            Self::Timeout => "timeout",
            Self::Panic => "panic",
            Self::Deadlock => "deadlock",
            Self::Safety => "safety",
            Self::Configuration => "configuration",
        };
        f.write_str(s)
    }
}

// =============================================================================
// Runtime telemetry event (canonical envelope)
// =============================================================================

/// A single runtime telemetry event in the unified envelope format.
///
/// This is the canonical wire format for all runtime telemetry events.
/// Subsystem-specific events should be convertible to this envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeTelemetryEvent {
    /// Epoch milliseconds (wall-clock, monotonic-safe within a session).
    pub timestamp_ms: u64,

    /// Emitting subsystem identifier (dot-separated hierarchy).
    ///
    /// Convention: `rt.<subsystem>[.<detail>]`
    /// Examples: `rt.scope`, `rt.backpressure.capture`, `rt.cancellation`,
    ///           `rt.storage.flush`, `rt.network.mux`
    pub component: String,

    /// Optional scope tree node identifier (e.g. `"daemon:capture"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,

    /// What happened — the event discriminant.
    pub event_kind: RuntimeTelemetryKind,

    /// Current health tier at the time of the event.
    pub health_tier: HealthTier,

    /// Lifecycle phase at the time of the event.
    pub phase: RuntimePhase,

    /// Stable reason code string following `subsystem.phase.detail` convention.
    ///
    /// Examples: `scope.startup.created`, `backpressure.running.tier_yellow`,
    ///           `cancellation.draining.grace_expired`
    pub reason_code: String,

    /// Cross-event correlation identifier.
    ///
    /// Used to link events within the same operation, cycle, or shutdown sequence.
    pub correlation_id: String,

    /// Optional failure classification (present for error events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,

    /// Type-erased detail payload.
    ///
    /// Keys should be stable snake_case identifiers.
    /// Common keys: `queue_depth`, `queue_capacity`, `tier_from`, `tier_to`,
    ///              `error_message`, `scope_tier`, `grace_period_ms`
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub details: HashMap<String, serde_json::Value>,
}

impl RuntimeTelemetryEvent {
    /// Current wall-clock epoch milliseconds.
    #[must_use]
    pub fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

// =============================================================================
// Cross-subsystem telemetry normalization
// =============================================================================

/// Schema version for the cross-subsystem unified telemetry contract.
pub const UNIFIED_TELEMETRY_SCHEMA_VERSION: &str = "ft.telemetry.unified.v1";

static UNIFIED_TELEMETRY_FIELD_POLICY: LazyLock<DiagnosticFieldPolicy> =
    LazyLock::new(DiagnosticFieldPolicy::default);

/// Source subsystem family for a normalized telemetry record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnifiedTelemetrySource {
    /// Runtime / mux / swarm operational telemetry.
    Runtime,
    /// Connector lifecycle, inbound, and outbound events.
    Connector,
    /// Policy and recorder audit trail events.
    Policy,
}

/// Safe cross-subsystem telemetry record for storage, alerting, and dashboards.
///
/// This record is intentionally narrower than each source schema. It keeps the
/// fields needed for correlation, triage, and severity routing while projecting
/// source-specific payloads into a redacted attribute bag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedTelemetryRecord {
    /// Unified schema version for this normalized contract.
    pub schema_version: String,
    /// Original source family.
    pub source: UnifiedTelemetrySource,
    /// Stable identifier for this normalized record.
    pub record_id: String,
    /// Original source schema version, when the source schema carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_schema_version: Option<String>,
    /// Event timestamp in epoch milliseconds.
    pub timestamp_ms: u64,
    /// Normalized emitting component identifier.
    pub component: String,
    /// Stable reason/event code used for correlation, filtering, and alerting.
    pub reason_code: String,
    /// Shared correlation identifier when the source provides one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Primary normalized scope identifier, when one can be inferred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    /// Normalized health/severity tier.
    pub health_tier: HealthTier,
    /// Normalized failure class when present or inferable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    /// Highest sensitivity classification still implied by this record.
    pub sensitivity: DataSensitivity,
    /// Dominant redaction strategy used to make this record safe to emit.
    pub redaction_strategy: RedactionStrategy,
    /// Safe structural attributes preserved from the source event.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, serde_json::Value>,
}

impl UnifiedTelemetryRecord {
    /// Normalize a runtime telemetry event into the shared record shape.
    #[must_use]
    pub fn from_runtime_event(event: &RuntimeTelemetryEvent) -> Self {
        let mut attributes = BTreeMap::new();
        let (sanitized_details, sensitivity, redaction_strategy, redacted_detail_count) =
            sanitize_runtime_details(&event.details);

        attributes.insert(
            "event_kind".to_string(),
            serde_json::json!(enum_string(&event.event_kind)),
        );
        attributes.insert(
            "category".to_string(),
            serde_json::json!(event.event_kind.category()),
        );
        attributes.insert(
            "phase".to_string(),
            serde_json::json!(event.phase.to_string()),
        );
        if redacted_detail_count > 0 {
            attributes.insert(
                "redacted_detail_count".to_string(),
                serde_json::json!(redacted_detail_count),
            );
        }
        attributes.extend(sanitized_details);

        Self {
            schema_version: UNIFIED_TELEMETRY_SCHEMA_VERSION.to_string(),
            source: UnifiedTelemetrySource::Runtime,
            record_id: runtime_record_id(event),
            source_schema_version: None,
            timestamp_ms: event.timestamp_ms,
            component: event.component.clone(),
            reason_code: runtime_reason_code(event),
            correlation_id: non_empty_string(&event.correlation_id),
            scope_id: event.scope_id.clone(),
            health_tier: event.health_tier,
            failure_class: event.failure_class,
            sensitivity,
            redaction_strategy,
            attributes,
        }
    }

    /// Normalize a canonical connector event into the shared record shape.
    #[must_use]
    pub fn from_connector_event(event: &CanonicalConnectorEvent) -> Self {
        let mut attributes = BTreeMap::new();
        let mut sensitivity = DataSensitivity::Internal;
        let mut redaction_strategy = RedactionStrategy::Passthrough;
        let failure_class = event.failure_class.map(|class| match class {
            ConnectorFailureClass::Auth => FailureClass::Permanent,
            ConnectorFailureClass::Quota => FailureClass::Overload,
            ConnectorFailureClass::Network => FailureClass::Transient,
            ConnectorFailureClass::Policy => FailureClass::Safety,
            ConnectorFailureClass::Validation => FailureClass::Configuration,
            ConnectorFailureClass::Timeout => FailureClass::Timeout,
            ConnectorFailureClass::Unknown => FailureClass::Permanent,
        });

        attributes.insert(
            "direction".to_string(),
            serde_json::json!(event.direction.as_str()),
        );
        attributes.insert(
            "severity".to_string(),
            serde_json::json!(event.severity.as_str()),
        );
        if let Some(name) = &event.connector_name {
            attributes.insert("connector_name".to_string(), serde_json::json!(name));
        }
        if let Some(kind) = &event.signal_kind {
            attributes.insert(
                "signal_kind".to_string(),
                serde_json::json!(enum_string(kind)),
            );
        }
        if let Some(sub_type) = &event.signal_sub_type {
            attributes.insert("signal_sub_type".to_string(), serde_json::json!(sub_type));
        }
        if let Some(source) = &event.event_source {
            attributes.insert(
                "event_source".to_string(),
                serde_json::json!(enum_string(source)),
            );
        }
        if let Some(kind) = &event.action_kind {
            attributes.insert(
                "action_kind".to_string(),
                serde_json::json!(enum_string(kind)),
            );
        }
        if let Some(phase) = event.lifecycle_phase {
            attributes.insert(
                "lifecycle_phase".to_string(),
                serde_json::json!(phase.as_str()),
            );
        }
        if let Some(class) = event.failure_class {
            attributes.insert(
                "connector_failure_class".to_string(),
                serde_json::json!(class.as_str()),
            );
        }
        if let Some(pane_id) = event.pane_id {
            attributes.insert("pane_id".to_string(), serde_json::json!(pane_id));
        }
        if let Some(workflow_id) = &event.workflow_id {
            attributes.insert("workflow_id".to_string(), serde_json::json!(workflow_id));
        }
        if let Some(zone_id) = &event.zone_id {
            attributes.insert("zone_id".to_string(), serde_json::json!(zone_id));
        }
        if let Some(capability) = event.capability {
            attributes.insert(
                "capability".to_string(),
                serde_json::json!(capability.as_str()),
            );
        }

        if !event.metadata.is_empty() {
            let metadata_keys = event.metadata.keys().cloned().collect::<Vec<_>>();
            attributes.insert(
                "metadata_keys".to_string(),
                serde_json::json!(metadata_keys),
            );
            attributes.insert(
                "metadata_count".to_string(),
                serde_json::json!(event.metadata.len()),
            );
            redaction_strategy = RedactionStrategy::Remove;
        }

        if !event.payload.is_null() {
            attributes.insert("payload_redacted".to_string(), serde_json::json!(true));
            summarize_value_shape("payload", &event.payload, &mut attributes);
            sensitivity = sensitivity.max(DataSensitivity::Confidential);
            redaction_strategy = RedactionStrategy::Remove;
        }

        if matches!(event.capability, Some(ConnectorCapability::SecretBroker))
            || matches!(event.failure_class, Some(ConnectorFailureClass::Auth))
        {
            sensitivity = sensitivity.max(DataSensitivity::Restricted);
            redaction_strategy = RedactionStrategy::Remove;
        }

        Self {
            schema_version: UNIFIED_TELEMETRY_SCHEMA_VERSION.to_string(),
            source: UnifiedTelemetrySource::Connector,
            record_id: event.event_id.clone(),
            source_schema_version: Some(event.schema_version.to_string()),
            timestamp_ms: event.timestamp_ms,
            component: format!("connector.{}", event.connector_id),
            reason_code: event.event_type.clone(),
            correlation_id: non_empty_string(&event.correlation_id),
            scope_id: connector_scope_id(event),
            health_tier: connector_health_tier(event, failure_class),
            failure_class,
            sensitivity,
            redaction_strategy,
            attributes,
        }
    }

    /// Normalize a connector credential-broker audit event into the shared record shape.
    #[must_use]
    pub fn from_credential_audit(event: &CredentialAuditEvent) -> Self {
        let mut attributes = BTreeMap::new();
        let mut redaction_strategy = RedactionStrategy::Passthrough;
        let failure_class = credential_audit_failure_class(event);

        attributes.insert(
            "event_type".to_string(),
            serde_json::json!(enum_string(&event.event_type)),
        );

        if !event.credential_id.is_empty() {
            attributes.insert(
                "credential_id_hash".to_string(),
                serde_json::json!(stable_hash(&event.credential_id)),
            );
            redaction_strategy = RedactionStrategy::Hash;
        }

        if let Some(connector_id) = &event.connector_id {
            attributes.insert(
                "connector_id_hash".to_string(),
                serde_json::json!(stable_hash(connector_id)),
            );
            redaction_strategy = RedactionStrategy::Hash;
        }

        if let Some(lease_id) = &event.lease_id {
            attributes.insert(
                "lease_id_hash".to_string(),
                serde_json::json!(stable_hash(lease_id)),
            );
            redaction_strategy = RedactionStrategy::Hash;
        }

        if let Some(provider_status) = credential_provider_status_label(&event.detail) {
            attributes.insert(
                "provider_status".to_string(),
                serde_json::json!(provider_status),
            );
        }

        if !event.detail.is_empty() {
            attributes.insert("detail_redacted".to_string(), serde_json::json!(true));
            attributes.insert(
                "detail_length".to_string(),
                serde_json::json!(event.detail.chars().count()),
            );
            redaction_strategy = RedactionStrategy::Remove;
        }

        Self {
            schema_version: UNIFIED_TELEMETRY_SCHEMA_VERSION.to_string(),
            source: UnifiedTelemetrySource::Connector,
            record_id: credential_audit_record_id(event),
            source_schema_version: None,
            timestamp_ms: event.timestamp_ms,
            component: "connector.credential_broker".to_string(),
            reason_code: format!(
                "connector.credential_broker.{}",
                enum_string(&event.event_type)
            ),
            correlation_id: credential_audit_correlation_id(event),
            scope_id: credential_audit_scope_id(event),
            health_tier: credential_audit_health_tier(event, failure_class),
            failure_class,
            sensitivity: DataSensitivity::Restricted,
            redaction_strategy,
            attributes,
        }
    }

    /// Normalize a credential-broker telemetry snapshot into the shared record shape.
    #[must_use]
    pub fn from_credential_broker_snapshot(snapshot: &CredentialBrokerTelemetrySnapshot) -> Self {
        let failure_class = credential_broker_snapshot_failure_class(snapshot);
        let mut attributes = BTreeMap::new();

        attributes.insert(
            "leases_issued".to_string(),
            serde_json::json!(snapshot.counters.leases_issued),
        );
        attributes.insert(
            "leases_expired".to_string(),
            serde_json::json!(snapshot.counters.leases_expired),
        );
        attributes.insert(
            "leases_revoked".to_string(),
            serde_json::json!(snapshot.counters.leases_revoked),
        );
        attributes.insert(
            "access_denied".to_string(),
            serde_json::json!(snapshot.counters.access_denied),
        );
        attributes.insert(
            "rotations_completed".to_string(),
            serde_json::json!(snapshot.counters.rotations_completed),
        );
        attributes.insert(
            "rotations_failed".to_string(),
            serde_json::json!(snapshot.counters.rotations_failed),
        );
        attributes.insert(
            "credentials_registered".to_string(),
            serde_json::json!(snapshot.counters.credentials_registered),
        );
        attributes.insert(
            "credentials_revoked".to_string(),
            serde_json::json!(snapshot.counters.credentials_revoked),
        );
        attributes.insert(
            "providers_registered".to_string(),
            serde_json::json!(snapshot.counters.providers_registered),
        );
        attributes.insert(
            "active_leases".to_string(),
            serde_json::json!(snapshot.active_leases),
        );
        attributes.insert(
            "active_credentials".to_string(),
            serde_json::json!(snapshot.active_credentials),
        );
        attributes.insert(
            "active_providers".to_string(),
            serde_json::json!(snapshot.active_providers),
        );

        Self {
            schema_version: UNIFIED_TELEMETRY_SCHEMA_VERSION.to_string(),
            source: UnifiedTelemetrySource::Connector,
            record_id: format!("broker-snapshot:{}", snapshot.captured_at_ms),
            source_schema_version: None,
            timestamp_ms: snapshot.captured_at_ms,
            component: "connector.credential_broker".to_string(),
            reason_code: "connector.credential_broker.snapshot".to_string(),
            correlation_id: None,
            scope_id: Some("connector:credential_broker".to_string()),
            health_tier: credential_broker_snapshot_health_tier(snapshot, failure_class),
            failure_class,
            sensitivity: DataSensitivity::Confidential,
            redaction_strategy: RedactionStrategy::Passthrough,
            attributes,
        }
    }

    /// Normalize a recorder audit entry into the shared record shape.
    #[must_use]
    pub fn from_recorder_audit(entry: &RecorderAuditEntry) -> Self {
        let mut attributes = BTreeMap::new();
        let access_tier = audit_access_tier(entry.event_type);
        let mut sensitivity = sensitivity_for_access_tier(access_tier);
        let mut redaction_strategy = RedactionStrategy::Passthrough;

        attributes.insert(
            "event_type".to_string(),
            serde_json::json!(enum_string(&entry.event_type)),
        );
        attributes.insert(
            "actor_kind".to_string(),
            serde_json::json!(entry.actor.kind.as_str()),
        );
        attributes.insert(
            "actor_identity_hash".to_string(),
            serde_json::json!(stable_hash(&entry.actor.identity)),
        );
        attributes.insert(
            "decision".to_string(),
            serde_json::json!(enum_string(&entry.decision)),
        );
        attributes.insert(
            "access_tier".to_string(),
            serde_json::json!(access_tier.level()),
        );
        attributes.insert(
            "policy_version".to_string(),
            serde_json::json!(entry.policy_version),
        );

        if !entry.scope.pane_ids.is_empty() {
            attributes.insert(
                "pane_count".to_string(),
                serde_json::json!(entry.scope.pane_ids.len()),
            );
            attributes.insert(
                "pane_ids".to_string(),
                serde_json::json!(entry.scope.pane_ids),
            );
        }
        if let Some((start_ms, end_ms)) = entry.scope.time_range {
            attributes.insert(
                "time_range_start_ms".to_string(),
                serde_json::json!(start_ms),
            );
            attributes.insert("time_range_end_ms".to_string(), serde_json::json!(end_ms));
        }
        if !entry.scope.segment_ids.is_empty() {
            attributes.insert(
                "segment_count".to_string(),
                serde_json::json!(entry.scope.segment_ids.len()),
            );
        }
        if let Some(result_count) = entry.scope.result_count {
            attributes.insert("result_count".to_string(), serde_json::json!(result_count));
        }
        if let Some(query) = &entry.scope.query {
            attributes.insert("query_redacted".to_string(), serde_json::json!(true));
            attributes.insert(
                "query_length".to_string(),
                serde_json::json!(query.chars().count()),
            );
            sensitivity = sensitivity.max(DataSensitivity::Confidential);
            redaction_strategy = RedactionStrategy::Remove;
        }
        if let Some(justification) = &entry.justification {
            attributes.insert(
                "justification_redacted".to_string(),
                serde_json::json!(true),
            );
            attributes.insert(
                "justification_length".to_string(),
                serde_json::json!(justification.chars().count()),
            );
            sensitivity = sensitivity.max(DataSensitivity::Restricted);
            redaction_strategy = RedactionStrategy::Remove;
        }
        if let Some(details) = &entry.details {
            attributes.insert("details_redacted".to_string(), serde_json::json!(true));
            summarize_value_shape("details", details, &mut attributes);
            sensitivity = sensitivity.max(DataSensitivity::Restricted);
            redaction_strategy = RedactionStrategy::Remove;
        }

        Self {
            schema_version: UNIFIED_TELEMETRY_SCHEMA_VERSION.to_string(),
            source: UnifiedTelemetrySource::Policy,
            record_id: format!("audit-{}", entry.ordinal),
            source_schema_version: Some(entry.audit_version.clone()),
            timestamp_ms: entry.timestamp_ms,
            component: "policy.recorder_audit".to_string(),
            reason_code: format!("policy.audit.{}", enum_string(&entry.event_type)),
            correlation_id: audit_correlation_id(entry),
            scope_id: audit_scope_id(entry),
            health_tier: audit_health_tier(entry.decision.clone(), access_tier),
            failure_class: audit_failure_class(entry.decision.clone()),
            sensitivity,
            redaction_strategy,
            attributes,
        }
    }

    /// Normalize a policy metrics dashboard into the shared record shape.
    #[must_use]
    pub fn from_policy_metrics_dashboard(
        dashboard: &crate::policy_metrics::PolicyMetricsDashboard,
    ) -> Self {
        let mut attributes = BTreeMap::new();

        attributes.insert(
            "total_evaluations".to_string(),
            serde_json::json!(dashboard.counters.total_evaluations),
        );
        attributes.insert(
            "total_denials".to_string(),
            serde_json::json!(dashboard.counters.total_denials),
        );
        attributes.insert(
            "total_quarantines_active".to_string(),
            serde_json::json!(dashboard.counters.total_quarantines_active),
        );
        attributes.insert(
            "total_violations_active".to_string(),
            serde_json::json!(dashboard.counters.total_violations_active),
        );
        attributes.insert(
            "audit_chain_length".to_string(),
            serde_json::json!(dashboard.counters.audit_chain_length),
        );
        attributes.insert(
            "audit_chain_valid".to_string(),
            serde_json::json!(dashboard.counters.audit_chain_valid),
        );
        attributes.insert(
            "kill_switch_active".to_string(),
            serde_json::json!(dashboard.counters.kill_switch_active),
        );
        attributes.insert(
            "forensic_records_count".to_string(),
            serde_json::json!(dashboard.counters.forensic_records_count),
        );
        attributes.insert(
            "snapshots_generated".to_string(),
            serde_json::json!(dashboard.counters.snapshots_generated),
        );
        attributes.insert(
            "overall_health".to_string(),
            serde_json::json!(dashboard.overall_health.to_string()),
        );
        attributes.insert(
            "indicator_count".to_string(),
            serde_json::json!(dashboard.indicators.len()),
        );
        attributes.insert(
            "subsystem_count".to_string(),
            serde_json::json!(dashboard.subsystem_metrics.len()),
        );

        let health_tier = match dashboard.overall_health {
            crate::policy_metrics::HealthStatus::Healthy => HealthTier::Green,
            crate::policy_metrics::HealthStatus::Warning => HealthTier::Yellow,
            crate::policy_metrics::HealthStatus::Critical => HealthTier::Red,
            crate::policy_metrics::HealthStatus::Unknown => HealthTier::Black,
        };

        let failure_class = if dashboard.counters.kill_switch_active {
            Some(FailureClass::Safety)
        } else if !dashboard.counters.audit_chain_valid {
            Some(FailureClass::Corruption)
        } else if dashboard.overall_health >= crate::policy_metrics::HealthStatus::Critical {
            Some(FailureClass::Overload)
        } else {
            None
        };

        Self {
            schema_version: UNIFIED_TELEMETRY_SCHEMA_VERSION.to_string(),
            source: UnifiedTelemetrySource::Policy,
            record_id: format!("policy-dashboard:{}", dashboard.captured_at_ms),
            source_schema_version: None,
            timestamp_ms: dashboard.captured_at_ms,
            component: "policy.metrics_dashboard".to_string(),
            reason_code: "policy.metrics.dashboard_snapshot".to_string(),
            correlation_id: None,
            scope_id: Some("policy:all_subsystems".to_string()),
            health_tier,
            failure_class,
            sensitivity: DataSensitivity::Internal,
            redaction_strategy: RedactionStrategy::Passthrough,
            attributes,
        }
    }

    /// Normalize a compliance snapshot into the shared record shape.
    #[must_use]
    pub fn from_compliance_snapshot(
        snapshot: &crate::policy_compliance::ComplianceSnapshot,
    ) -> Self {
        let mut attributes = BTreeMap::new();

        attributes.insert(
            "overall_status".to_string(),
            serde_json::json!(format!("{:?}", snapshot.overall_status)),
        );
        attributes.insert(
            "total_evaluations".to_string(),
            serde_json::json!(snapshot.counters.total_evaluations),
        );
        attributes.insert(
            "total_denials".to_string(),
            serde_json::json!(snapshot.counters.total_denials),
        );
        attributes.insert(
            "total_violations_detected".to_string(),
            serde_json::json!(snapshot.counters.total_violations_detected),
        );
        attributes.insert(
            "total_violations_remediated".to_string(),
            serde_json::json!(snapshot.counters.total_violations_remediated),
        );
        attributes.insert(
            "active_violations".to_string(),
            serde_json::json!(snapshot.active_violations.len()),
        );
        attributes.insert(
            "total_quarantines".to_string(),
            serde_json::json!(snapshot.counters.total_quarantines),
        );
        attributes.insert(
            "total_kill_switch_trips".to_string(),
            serde_json::json!(snapshot.counters.total_kill_switch_trips),
        );
        attributes.insert(
            "subsystem_count".to_string(),
            serde_json::json!(snapshot.subsystem_status.len()),
        );

        let health_tier = match snapshot.overall_status {
            crate::policy_compliance::ComplianceStatus::Compliant => HealthTier::Green,
            crate::policy_compliance::ComplianceStatus::Advisory => HealthTier::Yellow,
            crate::policy_compliance::ComplianceStatus::NonCompliant => HealthTier::Red,
            crate::policy_compliance::ComplianceStatus::Critical => HealthTier::Black,
        };

        let failure_class = if snapshot.counters.total_kill_switch_trips > 0
            || !snapshot.active_violations.is_empty()
        {
            Some(FailureClass::Safety)
        } else {
            None
        };

        Self {
            schema_version: UNIFIED_TELEMETRY_SCHEMA_VERSION.to_string(),
            source: UnifiedTelemetrySource::Policy,
            record_id: format!("compliance-snapshot:{}", snapshot.captured_at_ms),
            source_schema_version: None,
            timestamp_ms: snapshot.captured_at_ms,
            component: "policy.compliance_engine".to_string(),
            reason_code: "policy.compliance.snapshot".to_string(),
            correlation_id: None,
            scope_id: Some("policy:compliance".to_string()),
            health_tier,
            failure_class,
            sensitivity: DataSensitivity::Confidential,
            redaction_strategy: RedactionStrategy::Passthrough,
            attributes,
        }
    }

    /// Normalize a [`MuxPoolStats`] snapshot into the shared record shape.
    ///
    /// Health tier is derived from connection failure ratios and pool saturation.
    #[cfg(all(feature = "vendored", unix))]
    #[must_use]
    pub fn from_mux_pool_stats(stats: &crate::vendored::MuxPoolStats, now_ms: u64) -> Self {
        let mut attributes = BTreeMap::new();
        attributes.insert(
            "max_size".to_string(),
            serde_json::json!(stats.pool.max_size),
        );
        attributes.insert(
            "idle_count".to_string(),
            serde_json::json!(stats.pool.idle_count),
        );
        attributes.insert(
            "active_count".to_string(),
            serde_json::json!(stats.pool.active_count),
        );
        attributes.insert(
            "connections_created".to_string(),
            serde_json::json!(stats.connections_created),
        );
        attributes.insert(
            "connections_failed".to_string(),
            serde_json::json!(stats.connections_failed),
        );
        attributes.insert(
            "health_checks".to_string(),
            serde_json::json!(stats.health_checks),
        );
        attributes.insert(
            "health_check_failures".to_string(),
            serde_json::json!(stats.health_check_failures),
        );
        attributes.insert(
            "recovery_attempts".to_string(),
            serde_json::json!(stats.recovery_attempts),
        );
        attributes.insert(
            "recovery_successes".to_string(),
            serde_json::json!(stats.recovery_successes),
        );
        attributes.insert(
            "permanent_failures".to_string(),
            serde_json::json!(stats.permanent_failures),
        );
        attributes.insert(
            "total_acquired".to_string(),
            serde_json::json!(stats.pool.total_acquired),
        );
        attributes.insert(
            "total_timeouts".to_string(),
            serde_json::json!(stats.pool.total_timeouts),
        );

        // Derive health tier from connection reliability.
        let total_attempts = stats.connections_created + stats.connections_failed;
        let failure_ratio = if total_attempts > 0 {
            stats.connections_failed as f64 / total_attempts as f64
        } else {
            0.0
        };
        let saturation = if stats.pool.max_size > 0 {
            stats.pool.active_count as f64 / stats.pool.max_size as f64
        } else {
            0.0
        };
        let health_tier = if stats.permanent_failures > 0 || failure_ratio >= 0.5 {
            HealthTier::Black
        } else if failure_ratio >= 0.2 || saturation >= 0.95 {
            HealthTier::Red
        } else if failure_ratio >= 0.05 || saturation >= 0.80 {
            HealthTier::Yellow
        } else {
            HealthTier::Green
        };

        let failure_class = if stats.permanent_failures > 0 {
            Some(FailureClass::Permanent)
        } else if stats.connections_failed > 0 {
            Some(FailureClass::Transient)
        } else {
            None
        };

        Self {
            schema_version: UNIFIED_TELEMETRY_SCHEMA_VERSION.to_string(),
            source: UnifiedTelemetrySource::Runtime,
            record_id: format!("mux-pool-stats:{now_ms}"),
            source_schema_version: None,
            timestamp_ms: now_ms,
            component: "mux.pool".to_string(),
            reason_code: "mux.pool.snapshot".to_string(),
            correlation_id: None,
            scope_id: Some("mux:pool".to_string()),
            health_tier,
            failure_class,
            sensitivity: DataSensitivity::Internal,
            redaction_strategy: RedactionStrategy::Passthrough,
            attributes,
        }
    }

    /// Normalize a [`SchedulerSnapshot`] into the shared record shape.
    ///
    /// Health tier is derived from circuit breaker state and recent scale history.
    #[must_use]
    pub fn from_scheduler_snapshot(snapshot: &SchedulerSnapshot, now_ms: u64) -> Self {
        let mut attributes = BTreeMap::new();
        attributes.insert(
            "fleet_agents".to_string(),
            serde_json::json!(snapshot.agent_first_seen.len()),
        );
        attributes.insert(
            "consecutive_scale_ops".to_string(),
            serde_json::json!(snapshot.consecutive_scale_ops),
        );
        attributes.insert(
            "circuit_breaker_tripped".to_string(),
            serde_json::json!(snapshot.circuit_breaker_tripped_at.is_some()),
        );
        attributes.insert(
            "scale_history_len".to_string(),
            serde_json::json!(snapshot.scale_history.len()),
        );
        attributes.insert("sequence".to_string(), serde_json::json!(snapshot.sequence));
        attributes.insert(
            "last_evaluation_ms".to_string(),
            serde_json::json!(snapshot.last_evaluation_ms),
        );
        attributes.insert(
            "last_scale_up_ms".to_string(),
            serde_json::json!(snapshot.last_scale_up_ms),
        );
        attributes.insert(
            "last_scale_down_ms".to_string(),
            serde_json::json!(snapshot.last_scale_down_ms),
        );

        // Aggregate per-agent failure rates for fleet-wide health assessment.
        let total_completed: u32 = snapshot.agent_completed.values().sum();
        let total_failed: u32 = snapshot.agent_failed.values().sum();
        let total_work = total_completed + total_failed;
        let fleet_failure_rate = if total_work > 0 {
            total_failed as f64 / total_work as f64
        } else {
            0.0
        };
        attributes.insert(
            "total_completed".to_string(),
            serde_json::json!(total_completed),
        );
        attributes.insert("total_failed".to_string(), serde_json::json!(total_failed));
        attributes.insert(
            "fleet_failure_rate".to_string(),
            serde_json::json!(fleet_failure_rate),
        );

        // Count recent scale event types for anomaly detection.
        let scale_up_count = snapshot
            .scale_history
            .iter()
            .filter(|e| matches!(e.event_type, ScaleEventType::ScaleUp))
            .count();
        let scale_down_count = snapshot
            .scale_history
            .iter()
            .filter(|e| matches!(e.event_type, ScaleEventType::ScaleDown))
            .count();
        attributes.insert("scale_ups".to_string(), serde_json::json!(scale_up_count));
        attributes.insert(
            "scale_downs".to_string(),
            serde_json::json!(scale_down_count),
        );

        // Health tier: circuit breaker tripped is Black; high failure rate is Red;
        // high consecutive ops is Yellow; otherwise Green.
        let health_tier = if snapshot.circuit_breaker_tripped_at.is_some() {
            HealthTier::Black
        } else if fleet_failure_rate >= 0.5 {
            HealthTier::Red
        } else if snapshot.consecutive_scale_ops >= snapshot.config.max_consecutive_scale_ops / 2 {
            HealthTier::Yellow
        } else {
            HealthTier::Green
        };

        let failure_class = if snapshot.circuit_breaker_tripped_at.is_some() {
            Some(FailureClass::Overload)
        } else if fleet_failure_rate >= 0.2 {
            Some(FailureClass::Degraded)
        } else {
            None
        };

        Self {
            schema_version: UNIFIED_TELEMETRY_SCHEMA_VERSION.to_string(),
            source: UnifiedTelemetrySource::Runtime,
            record_id: format!("scheduler-snapshot:{}:{}", snapshot.sequence, now_ms),
            source_schema_version: None,
            timestamp_ms: now_ms,
            component: "swarm.scheduler".to_string(),
            reason_code: "swarm.scheduler.snapshot".to_string(),
            correlation_id: None,
            scope_id: Some("swarm:scheduler".to_string()),
            health_tier,
            failure_class,
            sensitivity: DataSensitivity::Internal,
            redaction_strategy: RedactionStrategy::Passthrough,
            attributes,
        }
    }

    /// Normalize a [`QueueStats`] snapshot into the shared record shape.
    ///
    /// Health tier is derived from failure rate, blocked ratio, and queue depth.
    #[must_use]
    pub fn from_queue_stats(stats: &QueueStats, now_ms: u64) -> Self {
        let mut attributes = BTreeMap::new();
        attributes.insert(
            "total_items".to_string(),
            serde_json::json!(stats.total_items),
        );
        attributes.insert("blocked".to_string(), serde_json::json!(stats.blocked));
        attributes.insert("ready".to_string(), serde_json::json!(stats.ready));
        attributes.insert(
            "in_progress".to_string(),
            serde_json::json!(stats.in_progress),
        );
        attributes.insert("completed".to_string(), serde_json::json!(stats.completed));
        attributes.insert("failed".to_string(), serde_json::json!(stats.failed));
        attributes.insert("cancelled".to_string(), serde_json::json!(stats.cancelled));
        attributes.insert(
            "active_agents".to_string(),
            serde_json::json!(stats.active_agents),
        );

        let terminal = stats.completed + stats.failed + stats.cancelled;
        let failure_rate = if terminal > 0 {
            stats.failed as f64 / terminal as f64
        } else {
            0.0
        };
        let non_terminal = stats.total_items.saturating_sub(terminal);
        let blocked_ratio = if non_terminal > 0 {
            stats.blocked as f64 / non_terminal as f64
        } else {
            0.0
        };
        attributes.insert("failure_rate".to_string(), serde_json::json!(failure_rate));
        attributes.insert(
            "blocked_ratio".to_string(),
            serde_json::json!(blocked_ratio),
        );

        let health_tier = if failure_rate >= 0.5 {
            HealthTier::Black
        } else if failure_rate >= 0.2 || blocked_ratio >= 0.8 {
            HealthTier::Red
        } else if failure_rate >= 0.05 || blocked_ratio >= 0.5 {
            HealthTier::Yellow
        } else {
            HealthTier::Green
        };

        let failure_class = if failure_rate >= 0.5 {
            Some(FailureClass::Permanent)
        } else if failure_rate >= 0.1 {
            Some(FailureClass::Degraded)
        } else if blocked_ratio >= 0.8 {
            Some(FailureClass::Overload)
        } else {
            None
        };

        Self {
            schema_version: UNIFIED_TELEMETRY_SCHEMA_VERSION.to_string(),
            source: UnifiedTelemetrySource::Runtime,
            record_id: format!("queue-stats:{now_ms}"),
            source_schema_version: None,
            timestamp_ms: now_ms,
            component: "swarm.work_queue".to_string(),
            reason_code: "swarm.queue.snapshot".to_string(),
            correlation_id: None,
            scope_id: Some("swarm:work_queue".to_string()),
            health_tier,
            failure_class,
            sensitivity: DataSensitivity::Internal,
            redaction_strategy: RedactionStrategy::Passthrough,
            attributes,
        }
    }
}

impl From<&QueueStats> for UnifiedTelemetryRecord {
    fn from(stats: &QueueStats) -> Self {
        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self::from_queue_stats(stats, now_ms)
    }
}

impl From<&RuntimeTelemetryEvent> for UnifiedTelemetryRecord {
    fn from(event: &RuntimeTelemetryEvent) -> Self {
        Self::from_runtime_event(event)
    }
}

impl From<&CanonicalConnectorEvent> for UnifiedTelemetryRecord {
    fn from(event: &CanonicalConnectorEvent) -> Self {
        Self::from_connector_event(event)
    }
}

impl From<&CredentialAuditEvent> for UnifiedTelemetryRecord {
    fn from(event: &CredentialAuditEvent) -> Self {
        Self::from_credential_audit(event)
    }
}

impl From<&CredentialBrokerTelemetrySnapshot> for UnifiedTelemetryRecord {
    fn from(snapshot: &CredentialBrokerTelemetrySnapshot) -> Self {
        Self::from_credential_broker_snapshot(snapshot)
    }
}

impl From<&RecorderAuditEntry> for UnifiedTelemetryRecord {
    fn from(entry: &RecorderAuditEntry) -> Self {
        Self::from_recorder_audit(entry)
    }
}

impl From<&crate::policy_metrics::PolicyMetricsDashboard> for UnifiedTelemetryRecord {
    fn from(dashboard: &crate::policy_metrics::PolicyMetricsDashboard) -> Self {
        Self::from_policy_metrics_dashboard(dashboard)
    }
}

impl From<&crate::policy_compliance::ComplianceSnapshot> for UnifiedTelemetryRecord {
    fn from(snapshot: &crate::policy_compliance::ComplianceSnapshot) -> Self {
        Self::from_compliance_snapshot(snapshot)
    }
}

impl From<&SchedulerSnapshot> for UnifiedTelemetryRecord {
    fn from(snapshot: &SchedulerSnapshot) -> Self {
        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self::from_scheduler_snapshot(snapshot, now_ms)
    }
}

#[cfg(all(feature = "vendored", unix))]
impl From<&crate::vendored::MuxPoolStats> for UnifiedTelemetryRecord {
    fn from(stats: &crate::vendored::MuxPoolStats) -> Self {
        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self::from_mux_pool_stats(stats, now_ms)
    }
}

fn sanitize_runtime_details(
    details: &HashMap<String, serde_json::Value>,
) -> (
    BTreeMap<String, serde_json::Value>,
    DataSensitivity,
    RedactionStrategy,
    usize,
) {
    let mut attributes = BTreeMap::new();
    let mut sensitivity = DataSensitivity::Internal;
    let mut redaction_strategy = RedactionStrategy::Passthrough;
    let mut redacted_fields = 0usize;

    let mut keys = details.keys().cloned().collect::<Vec<_>>();
    keys.sort_unstable();

    for key in keys {
        let Some(value) = details.get(&key) else {
            continue;
        };

        if UNIFIED_TELEMETRY_FIELD_POLICY.always_redact.contains(&key) {
            attributes.insert(
                key,
                serde_json::json!(UNIFIED_TELEMETRY_FIELD_POLICY.redaction_marker),
            );
            sensitivity = sensitivity.max(DataSensitivity::Restricted);
            redaction_strategy = RedactionStrategy::Mask;
            redacted_fields += 1;
            continue;
        }

        let safe_key = UNIFIED_TELEMETRY_FIELD_POLICY.always_safe.contains(&key);
        match value {
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
                attributes.insert(key, value.clone());
            }
            serde_json::Value::String(_) if safe_key => {
                attributes.insert(key, value.clone());
            }
            _ => {
                attributes.insert(
                    key,
                    serde_json::json!(UNIFIED_TELEMETRY_FIELD_POLICY.redaction_marker),
                );
                sensitivity = sensitivity.max(DataSensitivity::Confidential);
                redaction_strategy = RedactionStrategy::Mask;
                redacted_fields += 1;
            }
        }
    }

    (attributes, sensitivity, redaction_strategy, redacted_fields)
}

fn runtime_record_id(event: &RuntimeTelemetryEvent) -> String {
    let reason_code = runtime_reason_code(event);
    match non_empty_string(&event.correlation_id) {
        Some(correlation_id) => format!(
            "rt:{}:{}:{}:{}",
            event.timestamp_ms, event.component, correlation_id, reason_code
        ),
        None => format!(
            "rt:{}:{}:{}",
            event.timestamp_ms, event.component, reason_code
        ),
    }
}

fn runtime_reason_code(event: &RuntimeTelemetryEvent) -> String {
    if event.reason_code.is_empty() {
        enum_string(&event.event_kind)
    } else {
        event.reason_code.clone()
    }
}

fn connector_scope_id(event: &CanonicalConnectorEvent) -> Option<String> {
    event
        .workflow_id
        .as_ref()
        .map(|workflow_id| format!("workflow:{workflow_id}"))
        .or_else(|| event.pane_id.map(|pane_id| format!("pane:{pane_id}")))
        .or_else(|| {
            event
                .zone_id
                .as_ref()
                .map(|zone_id| format!("zone:{zone_id}"))
        })
}

fn connector_health_tier(
    event: &CanonicalConnectorEvent,
    failure_class: Option<FailureClass>,
) -> HealthTier {
    let mut tier = match event.severity {
        CanonicalSeverity::Info => HealthTier::Green,
        CanonicalSeverity::Warning => HealthTier::Yellow,
        CanonicalSeverity::Critical => HealthTier::Red,
    };

    if let Some(phase) = event.lifecycle_phase {
        let phase_tier = match phase {
            ConnectorLifecyclePhase::Stopped
            | ConnectorLifecyclePhase::Starting
            | ConnectorLifecyclePhase::Running => HealthTier::Green,
            ConnectorLifecyclePhase::Degraded => HealthTier::Red,
            ConnectorLifecyclePhase::Failed => HealthTier::Black,
        };
        tier = tier.max(phase_tier);
    }

    if let Some(failure_class) = failure_class {
        tier = tier.max(failure_class.suggested_tier());
    }

    tier
}

fn credential_audit_record_id(event: &CredentialAuditEvent) -> String {
    let discriminator = event
        .lease_id
        .as_ref()
        .map(|lease_id| stable_hash(lease_id))
        .or_else(|| (!event.credential_id.is_empty()).then(|| stable_hash(&event.credential_id)))
        .unwrap_or_else(|| enum_string(&event.event_type));

    format!("broker-audit:{}:{discriminator}", event.timestamp_ms)
}

fn credential_audit_correlation_id(event: &CredentialAuditEvent) -> Option<String> {
    event
        .lease_id
        .as_ref()
        .map(|lease_id| stable_hash(lease_id))
        .or_else(|| {
            event
                .connector_id
                .as_ref()
                .map(|connector_id| stable_hash(connector_id))
        })
        .or_else(|| (!event.credential_id.is_empty()).then(|| stable_hash(&event.credential_id)))
}

fn credential_audit_scope_id(event: &CredentialAuditEvent) -> Option<String> {
    event
        .connector_id
        .as_ref()
        .map(|connector_id| format!("connector:{}", stable_hash(connector_id)))
        .or_else(|| {
            (!event.credential_id.is_empty())
                .then(|| format!("credential:{}", stable_hash(&event.credential_id)))
        })
}

fn credential_provider_status_label(detail: &str) -> Option<&'static str> {
    let detail = detail.to_ascii_lowercase();
    if detail.contains("-> unavailable") {
        Some("unavailable")
    } else if detail.contains("-> degraded") {
        Some("degraded")
    } else if detail.contains("-> available") {
        Some("available")
    } else {
        None
    }
}

fn credential_audit_failure_class(event: &CredentialAuditEvent) -> Option<FailureClass> {
    match event.event_type {
        CredentialAuditType::AccessDenied => Some(FailureClass::Safety),
        CredentialAuditType::CredentialExpired => Some(FailureClass::Permanent),
        CredentialAuditType::ProviderStatusChanged => {
            credential_provider_status_label(&event.detail).and_then(|status| match status {
                "unavailable" => Some(FailureClass::Degraded),
                _ => None,
            })
        }
        CredentialAuditType::CredentialRegistered
        | CredentialAuditType::LeaseIssued
        | CredentialAuditType::LeaseExpired
        | CredentialAuditType::LeaseRevoked
        | CredentialAuditType::CredentialRotated
        | CredentialAuditType::CredentialRevoked
        | CredentialAuditType::ProviderRegistered => None,
    }
}

fn credential_audit_health_tier(
    event: &CredentialAuditEvent,
    failure_class: Option<FailureClass>,
) -> HealthTier {
    let mut tier = match event.event_type {
        CredentialAuditType::CredentialRegistered
        | CredentialAuditType::LeaseIssued
        | CredentialAuditType::CredentialRotated
        | CredentialAuditType::ProviderRegistered => HealthTier::Green,
        CredentialAuditType::LeaseExpired
        | CredentialAuditType::LeaseRevoked
        | CredentialAuditType::CredentialRevoked
        | CredentialAuditType::CredentialExpired => HealthTier::Yellow,
        CredentialAuditType::AccessDenied => HealthTier::Red,
        CredentialAuditType::ProviderStatusChanged => {
            match credential_provider_status_label(&event.detail) {
                Some("unavailable") => HealthTier::Red,
                Some("degraded") => HealthTier::Yellow,
                _ => HealthTier::Green,
            }
        }
    };

    if let Some(failure_class) = failure_class {
        tier = tier.max(failure_class.suggested_tier());
    }

    tier
}

fn credential_broker_snapshot_failure_class(
    snapshot: &CredentialBrokerTelemetrySnapshot,
) -> Option<FailureClass> {
    if snapshot.active_leases > 0 && snapshot.active_providers == 0 {
        Some(FailureClass::Safety)
    } else if snapshot.active_credentials > 0 && snapshot.active_providers == 0 {
        Some(FailureClass::Degraded)
    } else {
        None
    }
}

fn credential_broker_snapshot_health_tier(
    snapshot: &CredentialBrokerTelemetrySnapshot,
    failure_class: Option<FailureClass>,
) -> HealthTier {
    let mut tier = if snapshot.active_providers == 0
        && snapshot.active_credentials == 0
        && snapshot.active_leases == 0
    {
        HealthTier::Yellow
    } else {
        HealthTier::Green
    };

    if let Some(failure_class) = failure_class {
        tier = tier.max(failure_class.suggested_tier());
    }

    tier
}

fn audit_access_tier(event_type: AuditEventType) -> AccessTier {
    match event_type {
        AuditEventType::RecorderQuery => AccessTier::A1RedactedQuery,
        AuditEventType::RecorderQueryPrivileged => AccessTier::A3PrivilegedRaw,
        AuditEventType::RecorderReplay | AuditEventType::RecorderExport => AccessTier::A2FullQuery,
        AuditEventType::AdminRetentionOverride
        | AuditEventType::AdminPurge
        | AuditEventType::AdminPolicyChange => AccessTier::A4Admin,
        AuditEventType::AccessApprovalGranted
        | AuditEventType::AccessApprovalExpired
        | AuditEventType::AccessIncidentMode
        | AuditEventType::AccessDebugMode => AccessTier::A3PrivilegedRaw,
        AuditEventType::RetentionSegmentSealed
        | AuditEventType::RetentionSegmentArchived
        | AuditEventType::RetentionSegmentPurged
        | AuditEventType::RetentionAcceleratedPurge => AccessTier::A2FullQuery,
    }
}

fn sensitivity_for_access_tier(access_tier: AccessTier) -> DataSensitivity {
    match access_tier {
        AccessTier::A0PublicMetadata => DataSensitivity::Public,
        AccessTier::A1RedactedQuery => DataSensitivity::Internal,
        AccessTier::A2FullQuery => DataSensitivity::Confidential,
        AccessTier::A3PrivilegedRaw | AccessTier::A4Admin => DataSensitivity::Restricted,
    }
}

fn audit_health_tier(decision: AuthzDecision, access_tier: AccessTier) -> HealthTier {
    let base_tier = match access_tier {
        AccessTier::A0PublicMetadata | AccessTier::A1RedactedQuery => HealthTier::Green,
        AccessTier::A2FullQuery => HealthTier::Yellow,
        AccessTier::A3PrivilegedRaw | AccessTier::A4Admin => HealthTier::Red,
    };
    let decision_tier = match decision {
        AuthzDecision::Allow => HealthTier::Green,
        AuthzDecision::Elevate => HealthTier::Yellow,
        AuthzDecision::Deny => HealthTier::Red,
    };
    base_tier.max(decision_tier)
}

fn audit_failure_class(decision: AuthzDecision) -> Option<FailureClass> {
    match decision {
        AuthzDecision::Allow => None,
        AuthzDecision::Deny | AuthzDecision::Elevate => Some(FailureClass::Safety),
    }
}

fn audit_scope_id(entry: &RecorderAuditEntry) -> Option<String> {
    match entry.scope.pane_ids.as_slice() {
        [pane_id] => Some(format!("pane:{pane_id}")),
        _ => None,
    }
}

fn audit_correlation_id(entry: &RecorderAuditEntry) -> Option<String> {
    let details = entry.details.as_ref()?;
    extract_json_string(details, "correlation_id")
        .or_else(|| extract_json_string(details, "request_id"))
        .or_else(|| extract_json_string(details, "operation_id"))
}

fn extract_json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .as_object()
        .and_then(|object| object.get(key))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn summarize_value_shape(
    prefix: &str,
    value: &serde_json::Value,
    attributes: &mut BTreeMap<String, serde_json::Value>,
) {
    attributes.insert(
        format!("{prefix}_type"),
        serde_json::json!(json_value_kind(value)),
    );

    match value {
        serde_json::Value::Array(items) => {
            attributes.insert(
                format!("{prefix}_item_count"),
                serde_json::json!(items.len()),
            );
        }
        serde_json::Value::Object(fields) => {
            attributes.insert(
                format!("{prefix}_field_count"),
                serde_json::json!(fields.len()),
            );
        }
        serde_json::Value::String(text) => {
            attributes.insert(
                format!("{prefix}_length"),
                serde_json::json!(text.chars().count()),
            );
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn json_value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn stable_hash(value: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)
}

fn non_empty_string(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn enum_string<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|serialized| serialized.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

// =============================================================================
// Event builder (ergonomic construction)
// =============================================================================

/// Fluent builder for `RuntimeTelemetryEvent`.
///
/// ```ignore
/// let event = RuntimeTelemetryEventBuilder::new("rt.scope", RuntimeTelemetryKind::ScopeStarted)
///     .scope_id("daemon:capture")
///     .phase(RuntimePhase::Startup)
///     .reason("scope.startup.daemon_started")
///     .correlation("cycle-42")
///     .detail_str("scope_tier", "daemon")
///     .build();
/// ```
pub struct RuntimeTelemetryEventBuilder {
    event: RuntimeTelemetryEvent,
}

impl RuntimeTelemetryEventBuilder {
    /// Create a new builder with required fields.
    #[must_use]
    pub fn new(component: &str, kind: RuntimeTelemetryKind) -> Self {
        Self {
            event: RuntimeTelemetryEvent {
                timestamp_ms: RuntimeTelemetryEvent::now_ms(),
                component: component.to_string(),
                scope_id: None,
                event_kind: kind,
                health_tier: HealthTier::Green,
                phase: RuntimePhase::Running,
                reason_code: String::new(),
                correlation_id: String::new(),
                failure_class: None,
                details: HashMap::new(),
            },
        }
    }

    /// Set the scope ID.
    #[must_use]
    pub fn scope_id(mut self, id: &str) -> Self {
        self.event.scope_id = Some(id.to_string());
        self
    }

    /// Set the health tier.
    #[must_use]
    pub fn tier(mut self, tier: HealthTier) -> Self {
        self.event.health_tier = tier;
        self
    }

    /// Set the runtime phase.
    #[must_use]
    pub fn phase(mut self, phase: RuntimePhase) -> Self {
        self.event.phase = phase;
        self
    }

    /// Set the reason code.
    #[must_use]
    pub fn reason(mut self, code: &str) -> Self {
        self.event.reason_code = code.to_string();
        self
    }

    /// Set the correlation ID.
    #[must_use]
    pub fn correlation(mut self, id: &str) -> Self {
        self.event.correlation_id = id.to_string();
        self
    }

    /// Set the failure class.
    #[must_use]
    pub fn failure(mut self, class: FailureClass) -> Self {
        self.event.failure_class = Some(class);
        self
    }

    /// Set the timestamp (for testing / replay).
    #[must_use]
    pub fn timestamp_ms(mut self, ts: u64) -> Self {
        self.event.timestamp_ms = ts;
        self
    }

    /// Add a string detail.
    #[must_use]
    pub fn detail_str(mut self, key: &str, value: &str) -> Self {
        self.event.details.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
        self
    }

    /// Add a u64 detail.
    #[must_use]
    pub fn detail_u64(mut self, key: &str, value: u64) -> Self {
        self.event
            .details
            .insert(key.to_string(), serde_json::json!(value));
        self
    }

    /// Add a f64 detail.
    #[must_use]
    pub fn detail_f64(mut self, key: &str, value: f64) -> Self {
        self.event
            .details
            .insert(key.to_string(), serde_json::json!(value));
        self
    }

    /// Add a boolean detail.
    #[must_use]
    pub fn detail_bool(mut self, key: &str, value: bool) -> Self {
        self.event
            .details
            .insert(key.to_string(), serde_json::json!(value));
        self
    }

    /// Consume the builder and produce the event.
    #[must_use]
    pub fn build(self) -> RuntimeTelemetryEvent {
        self.event
    }
}

// =============================================================================
// Telemetry log (bounded buffer)
// =============================================================================

/// Configuration for the runtime telemetry log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeTelemetryLogConfig {
    /// Maximum events retained in the in-memory buffer.
    pub max_events: usize,
    /// Whether telemetry collection is enabled.
    pub enabled: bool,
}

impl Default for RuntimeTelemetryLogConfig {
    fn default() -> Self {
        Self {
            max_events: 2048,
            enabled: true,
        }
    }
}

/// Bounded in-memory buffer for runtime telemetry events.
///
/// Events are appended in order. When the buffer exceeds `max_events`,
/// the oldest events are evicted (FIFO). This matches the pattern used
/// by `MissionEventLog`.
#[derive(Debug)]
pub struct RuntimeTelemetryLog {
    config: RuntimeTelemetryLogConfig,
    events: Vec<RuntimeTelemetryEvent>,
    sequence: u64,
    total_emitted: u64,
    total_evicted: u64,
}

impl RuntimeTelemetryLog {
    /// Create a new telemetry log with the given configuration.
    #[must_use]
    pub fn new(config: RuntimeTelemetryLogConfig) -> Self {
        Self {
            config,
            events: Vec::new(),
            sequence: 0,
            total_emitted: 0,
            total_evicted: 0,
        }
    }

    /// Create a telemetry log with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(RuntimeTelemetryLogConfig::default())
    }

    /// Append an event to the log.
    ///
    /// If the buffer is full, the oldest event is evicted.
    /// Returns the sequence number assigned to this event.
    pub fn append(&mut self, event: RuntimeTelemetryEvent) -> u64 {
        if !self.config.enabled {
            return 0;
        }

        self.sequence += 1;
        self.total_emitted += 1;

        self.events.push(event);

        // Evict oldest if over capacity.
        while self.events.len() > self.config.max_events {
            self.events.remove(0);
            self.total_evicted += 1;
        }

        self.sequence
    }

    /// Emit an event using the builder pattern.
    ///
    /// Returns the sequence number.
    pub fn emit(&mut self, builder: RuntimeTelemetryEventBuilder) -> u64 {
        self.append(builder.build())
    }

    /// All events currently in the buffer (oldest first).
    #[must_use]
    pub fn events(&self) -> &[RuntimeTelemetryEvent] {
        &self.events
    }

    /// Drain all events from the buffer, returning them.
    pub fn drain(&mut self) -> Vec<RuntimeTelemetryEvent> {
        std::mem::take(&mut self.events)
    }

    /// Number of events currently in the buffer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Total events emitted since creation (including evicted).
    #[must_use]
    pub fn total_emitted(&self) -> u64 {
        self.total_emitted
    }

    /// Total events evicted due to capacity limits.
    #[must_use]
    pub fn total_evicted(&self) -> u64 {
        self.total_evicted
    }

    /// Current sequence number (monotonically increasing).
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Filter events by event kind.
    #[must_use]
    pub fn filter_by_kind(&self, kind: RuntimeTelemetryKind) -> Vec<&RuntimeTelemetryEvent> {
        self.events
            .iter()
            .filter(|e| e.event_kind == kind)
            .collect()
    }

    /// Filter events by health tier.
    #[must_use]
    pub fn filter_by_tier(&self, tier: HealthTier) -> Vec<&RuntimeTelemetryEvent> {
        self.events
            .iter()
            .filter(|e| e.health_tier == tier)
            .collect()
    }

    /// Filter events by component prefix.
    #[must_use]
    pub fn filter_by_component(&self, prefix: &str) -> Vec<&RuntimeTelemetryEvent> {
        self.events
            .iter()
            .filter(|e| e.component.starts_with(prefix))
            .collect()
    }

    /// Filter events by correlation ID.
    #[must_use]
    pub fn filter_by_correlation(&self, id: &str) -> Vec<&RuntimeTelemetryEvent> {
        self.events
            .iter()
            .filter(|e| e.correlation_id == id)
            .collect()
    }

    /// Count events matching a health tier.
    #[must_use]
    pub fn count_tier(&self, tier: HealthTier) -> usize {
        self.events.iter().filter(|e| e.health_tier == tier).count()
    }

    /// Count events matching a category.
    #[must_use]
    pub fn count_category(&self, category: &str) -> usize {
        self.events
            .iter()
            .filter(|e| e.event_kind.category() == category)
            .count()
    }

    /// Export all events as a JSONL string.
    #[must_use]
    pub fn export_jsonl(&self) -> String {
        self.events
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Summary snapshot for diagnostics.
    #[must_use]
    pub fn snapshot(&self) -> TelemetryLogSnapshot {
        let mut kind_counts: HashMap<String, u64> = HashMap::new();
        let mut tier_counts = [0u64; 4];
        let mut category_counts: HashMap<String, u64> = HashMap::new();

        for event in &self.events {
            let kind_str = serde_json::to_value(event.event_kind)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| format!("{:?}", event.event_kind));
            *kind_counts.entry(kind_str).or_default() += 1;
            tier_counts[event.health_tier.severity() as usize] += 1;
            *category_counts
                .entry(event.event_kind.category().to_string())
                .or_default() += 1;
        }

        TelemetryLogSnapshot {
            buffered_events: self.events.len() as u64,
            total_emitted: self.total_emitted,
            total_evicted: self.total_evicted,
            sequence: self.sequence,
            kind_counts,
            tier_counts,
            category_counts,
        }
    }
}

/// Diagnostic snapshot of the telemetry log state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryLogSnapshot {
    /// Events currently in the buffer.
    pub buffered_events: u64,
    /// Total events emitted since creation.
    pub total_emitted: u64,
    /// Total events evicted (FIFO overflow).
    pub total_evicted: u64,
    /// Current sequence number.
    pub sequence: u64,
    /// Event counts by kind.
    pub kind_counts: HashMap<String, u64>,
    /// Event counts by health tier: [green, yellow, red, black].
    pub tier_counts: [u64; 4],
    /// Event counts by category.
    pub category_counts: HashMap<String, u64>,
}

// =============================================================================
// Swarm capacity telemetry
// =============================================================================

/// Default retained sample window per stage histogram.
pub const DEFAULT_SWARM_CAPACITY_SAMPLES_PER_STAGE: usize = 512;

/// Hard cap for retained samples per stage histogram.
///
/// The collector has a fixed stage set and four histograms per stage, so the
/// sample memory cap is:
/// `stage_count * 4 * max_samples_per_stage * size_of::<f64>()`.
pub const MAX_SWARM_CAPACITY_SAMPLES_PER_STAGE: usize = 4096;

/// Schema version for capacity decision evidence records and bundles.
pub const SWARM_CAPACITY_EVIDENCE_SCHEMA_VERSION: u32 = 1;

/// Policy version for deterministic capacity fairness planning.
pub const SWARM_CAPACITY_FAIRNESS_POLICY_VERSION: u32 = 1;

/// Default maximum decision records retained in a capacity evidence ledger.
pub const DEFAULT_SWARM_CAPACITY_LEDGER_RECORDS: usize = 1024;

/// Hard cap for retained decision records in one capacity evidence ledger.
pub const MAX_SWARM_CAPACITY_LEDGER_RECORDS: usize = 8192;

/// Default evidence retention window for capacity decisions.
pub const DEFAULT_SWARM_CAPACITY_LEDGER_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

/// Maximum context fields persisted with a capacity decision record.
pub const MAX_SWARM_CAPACITY_CONTEXT_FIELDS: usize = 32;

/// Maximum UTF-8 bytes retained for each context key.
pub const MAX_SWARM_CAPACITY_CONTEXT_KEY_BYTES: usize = 64;

/// Maximum UTF-8 bytes retained for each redacted context value.
pub const MAX_SWARM_CAPACITY_CONTEXT_VALUE_BYTES: usize = 512;

const SWARM_CAPACITY_HISTOGRAMS_PER_STAGE: usize = 4;

/// Fixed hot-path stages used for swarm capacity modeling.
///
/// These labels are intentionally closed over a static enum. Operator-visible
/// telemetry never includes pane text, cwd, command text, or secret material.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityStage {
    /// Pane capture and delta extraction.
    IngestCapture = 0,
    /// Storage writer queue and write service time.
    StorageWrite = 1,
    /// Storage read-pool wait and hold time.
    StorageReadPool = 2,
    /// Event bus fanout and detection latency.
    EventBusFanout = 3,
    /// Workflow runner queue, lock wait, cancellation, and retry latency.
    WorkflowRunner = 4,
    /// Mux IPC / `DirectMuxClient` request latency.
    MuxIpc = 5,
    /// Robot and MCP admission plus completion latency.
    RobotMcp = 6,
}

impl SwarmCapacityStage {
    /// All stages in stable snapshot order.
    pub const ALL: [Self; 7] = [
        Self::IngestCapture,
        Self::StorageWrite,
        Self::StorageReadPool,
        Self::EventBusFanout,
        Self::WorkflowRunner,
        Self::MuxIpc,
        Self::RobotMcp,
    ];

    /// Number of fixed stages.
    pub const COUNT: usize = Self::ALL.len();

    /// Stable, low-cardinality stage label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IngestCapture => "ingest_capture",
            Self::StorageWrite => "storage_write",
            Self::StorageReadPool => "storage_read_pool",
            Self::EventBusFanout => "event_bus_fanout",
            Self::WorkflowRunner => "workflow_runner",
            Self::MuxIpc => "mux_ipc",
            Self::RobotMcp => "robot_mcp",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

impl fmt::Display for SwarmCapacityStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome class for a capacity-observed operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityOutcome {
    /// Operation completed normally.
    Completed,
    /// Operation was cancelled before completion.
    Cancelled,
    /// Operation exceeded its deadline.
    Timeout,
    /// Operation failed with a fixed failure class.
    Error(FailureClass),
}

/// Fixed failure-class counters for capacity snapshots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityFailureCounts {
    /// Transient errors.
    pub transient: u64,
    /// Permanent errors.
    pub permanent: u64,
    /// Degraded-operation errors.
    pub degraded: u64,
    /// Overload errors.
    pub overload: u64,
    /// Data-corruption errors.
    pub corruption: u64,
    /// Timeout errors.
    pub timeout: u64,
    /// Panic failures.
    #[serde(rename = "panic")]
    pub panic_count: u64,
    /// Deadlock failures.
    pub deadlock: u64,
    /// Safety and policy failures.
    pub safety: u64,
    /// Configuration failures.
    pub configuration: u64,
}

impl SwarmCapacityFailureCounts {
    fn increment(&mut self, class: FailureClass) {
        let slot = match class {
            FailureClass::Transient => &mut self.transient,
            FailureClass::Permanent => &mut self.permanent,
            FailureClass::Degraded => &mut self.degraded,
            FailureClass::Overload => &mut self.overload,
            FailureClass::Corruption => &mut self.corruption,
            FailureClass::Timeout => &mut self.timeout,
            FailureClass::Panic => &mut self.panic_count,
            FailureClass::Deadlock => &mut self.deadlock,
            FailureClass::Safety => &mut self.safety,
            FailureClass::Configuration => &mut self.configuration,
        };
        *slot = slot.saturating_add(1);
    }

    /// Count for one failure class.
    #[must_use]
    pub fn count(self, class: FailureClass) -> u64 {
        match class {
            FailureClass::Transient => self.transient,
            FailureClass::Permanent => self.permanent,
            FailureClass::Degraded => self.degraded,
            FailureClass::Overload => self.overload,
            FailureClass::Corruption => self.corruption,
            FailureClass::Timeout => self.timeout,
            FailureClass::Panic => self.panic_count,
            FailureClass::Deadlock => self.deadlock,
            FailureClass::Safety => self.safety,
            FailureClass::Configuration => self.configuration,
        }
    }

    /// Total failure-class observations.
    #[must_use]
    pub fn total(self) -> u64 {
        [
            self.transient,
            self.permanent,
            self.degraded,
            self.overload,
            self.corruption,
            self.timeout,
            self.panic_count,
            self.deadlock,
            self.safety,
            self.configuration,
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add)
    }
}

/// Configuration for fixed-stage swarm capacity telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SwarmCapacityTelemetryConfig {
    /// Whether collection mutates counters and histograms.
    pub enabled: bool,
    /// Retained samples per stage histogram, clamped to a fixed upper bound.
    pub max_samples_per_stage: usize,
}

impl Default for SwarmCapacityTelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_samples_per_stage: DEFAULT_SWARM_CAPACITY_SAMPLES_PER_STAGE,
        }
    }
}

impl SwarmCapacityTelemetryConfig {
    fn normalized_samples_per_stage(&self) -> usize {
        self.max_samples_per_stage
            .clamp(1, MAX_SWARM_CAPACITY_SAMPLES_PER_STAGE)
    }
}

/// Fixed-stage, bounded capacity telemetry collector.
#[derive(Debug, Clone)]
pub struct SwarmCapacityTelemetry {
    config: SwarmCapacityTelemetryConfig,
    stages: Vec<SwarmCapacityStageMetrics>,
}

impl SwarmCapacityTelemetry {
    /// Create a collector with explicit configuration.
    #[must_use]
    pub fn new(config: SwarmCapacityTelemetryConfig) -> Self {
        let max_samples_per_stage = config.normalized_samples_per_stage();
        let stages = SwarmCapacityStage::ALL
            .into_iter()
            .map(|stage| SwarmCapacityStageMetrics::new(stage, max_samples_per_stage))
            .collect();

        Self { config, stages }
    }

    /// Create a collector with default bounded retention.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(SwarmCapacityTelemetryConfig::default())
    }

    /// Current configuration.
    #[must_use]
    pub fn config(&self) -> &SwarmCapacityTelemetryConfig {
        &self.config
    }

    /// Record arrival/admission and observed queue depth for a stage.
    pub fn record_arrival(&mut self, stage: SwarmCapacityStage, queue_depth: u64) {
        if !self.config.enabled {
            return;
        }
        let metrics = self.stage_mut(stage);
        metrics.arrivals = metrics.arrivals.saturating_add(1);
        metrics.record_queue_depth(queue_depth);
    }

    /// Record a standalone queue-depth observation for a stage.
    pub fn observe_queue_depth(&mut self, stage: SwarmCapacityStage, queue_depth: u64) {
        if !self.config.enabled {
            return;
        }
        self.stage_mut(stage).record_queue_depth(queue_depth);
    }

    /// Record wait/admission/lock wait time for a stage.
    pub fn record_wait_time_ms(&mut self, stage: SwarmCapacityStage, wait_time_ms: f64) {
        if !self.config.enabled {
            return;
        }
        self.stage_mut(stage).record_wait_time_ms(wait_time_ms);
    }

    /// Record retry latency for stages with retry loops.
    pub fn record_retry_latency_ms(&mut self, stage: SwarmCapacityStage, retry_time_ms: f64) {
        if !self.config.enabled {
            return;
        }
        self.stage_mut(stage).record_retry_latency_ms(retry_time_ms);
    }

    /// Record successful completion and service time.
    pub fn record_completion(
        &mut self,
        stage: SwarmCapacityStage,
        service_time_ms: f64,
        queue_depth: u64,
    ) {
        self.record_outcome(
            stage,
            SwarmCapacityOutcome::Completed,
            service_time_ms,
            queue_depth,
        );
    }

    /// Record cancellation and elapsed service time.
    pub fn record_cancellation(
        &mut self,
        stage: SwarmCapacityStage,
        service_time_ms: f64,
        queue_depth: u64,
    ) {
        self.record_outcome(
            stage,
            SwarmCapacityOutcome::Cancelled,
            service_time_ms,
            queue_depth,
        );
    }

    /// Record timeout and elapsed service time.
    pub fn record_timeout(
        &mut self,
        stage: SwarmCapacityStage,
        service_time_ms: f64,
        queue_depth: u64,
    ) {
        self.record_outcome(
            stage,
            SwarmCapacityOutcome::Timeout,
            service_time_ms,
            queue_depth,
        );
    }

    /// Record error class and elapsed service time.
    pub fn record_error(
        &mut self,
        stage: SwarmCapacityStage,
        class: FailureClass,
        service_time_ms: f64,
        queue_depth: u64,
    ) {
        self.record_outcome(
            stage,
            SwarmCapacityOutcome::Error(class),
            service_time_ms,
            queue_depth,
        );
    }

    /// Record a completed, cancelled, timed-out, or failed operation.
    pub fn record_outcome(
        &mut self,
        stage: SwarmCapacityStage,
        outcome: SwarmCapacityOutcome,
        service_time_ms: f64,
        queue_depth: u64,
    ) {
        if !self.config.enabled {
            return;
        }

        let metrics = self.stage_mut(stage);
        metrics.record_queue_depth(queue_depth);
        metrics.record_service_time_ms(service_time_ms);

        match outcome {
            SwarmCapacityOutcome::Completed => {
                metrics.completions = metrics.completions.saturating_add(1);
            }
            SwarmCapacityOutcome::Cancelled => {
                metrics.cancellations = metrics.cancellations.saturating_add(1);
            }
            SwarmCapacityOutcome::Timeout => {
                metrics.timeouts = metrics.timeouts.saturating_add(1);
                metrics.failure_counts.increment(FailureClass::Timeout);
            }
            SwarmCapacityOutcome::Error(class) => {
                metrics.errors = metrics.errors.saturating_add(1);
                if class == FailureClass::Timeout {
                    metrics.timeouts = metrics.timeouts.saturating_add(1);
                }
                metrics.failure_counts.increment(class);
            }
        }
    }

    /// Snapshot all stages for robot, doctor, and capacity-certificate consumers.
    #[must_use]
    pub fn snapshot(&self) -> SwarmCapacityTelemetrySnapshot {
        let max_samples_per_stage = self.config.normalized_samples_per_stage();
        SwarmCapacityTelemetrySnapshot {
            enabled: self.config.enabled,
            stage_count: SwarmCapacityStage::COUNT,
            max_samples_per_stage,
            estimated_memory_cap_bytes: estimated_swarm_capacity_memory_cap_bytes(
                max_samples_per_stage,
            ),
            stages: self
                .stages
                .iter()
                .map(SwarmCapacityStageMetrics::snapshot)
                .collect(),
        }
    }

    fn stage_mut(&mut self, stage: SwarmCapacityStage) -> &mut SwarmCapacityStageMetrics {
        debug_assert_eq!(self.stages.len(), SwarmCapacityStage::COUNT);
        &mut self.stages[stage.index()]
    }
}

/// Serializable snapshot for fixed-stage capacity telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCapacityTelemetrySnapshot {
    /// Whether the collector was enabled.
    pub enabled: bool,
    /// Number of fixed stages in the snapshot.
    pub stage_count: usize,
    /// Retained samples per histogram after clamping.
    pub max_samples_per_stage: usize,
    /// Conservative cap estimate for retained sample memory and counters.
    pub estimated_memory_cap_bytes: u64,
    /// Per-stage metrics in `SwarmCapacityStage::ALL` order.
    pub stages: Vec<SwarmCapacityStageSnapshot>,
}

/// Serializable per-stage capacity telemetry snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCapacityStageSnapshot {
    /// Fixed stage enum.
    pub stage: SwarmCapacityStage,
    /// Stable low-cardinality stage label.
    pub stage_name: String,
    /// Admitted or observed arrivals.
    pub arrivals: u64,
    /// Successful completions.
    pub completions: u64,
    /// Cancelled operations.
    pub cancellations: u64,
    /// Timed-out operations.
    pub timeouts: u64,
    /// Failed operations recorded through `record_error`.
    pub errors: u64,
    /// Fixed failure-class counters.
    pub failure_counts: SwarmCapacityFailureCounts,
    /// Saturating estimate of currently in-flight operations.
    pub in_flight_estimate: u64,
    /// Observed queue depth or backlog distribution.
    pub queue_depth: HistogramSummary,
    /// Observed service-time distribution in milliseconds.
    pub service_time_ms: HistogramSummary,
    /// Observed wait/admission/lock-wait distribution in milliseconds.
    pub wait_time_ms: HistogramSummary,
    /// Observed retry latency distribution in milliseconds.
    pub retry_latency_ms: HistogramSummary,
}

/// Workload class attached to a swarm capacity certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityWorkloadClass {
    /// Idle observation loop with little operator traffic.
    IdleObservation,
    /// Heavy pane capture and delta extraction.
    HeavyCapture,
    /// Workflow queue and lock-storm scenario.
    WorkflowStorm,
    /// Policy-denial/audit-write storm.
    PolicyDenialStorm,
    /// Snapshot and restore scenario.
    SnapshotRestore,
    /// Search-heavy query/index refresh scenario.
    SearchHeavy,
    /// Robot/MCP request burst scenario.
    RobotMcpBurst,
    /// Storage writer saturation scenario.
    StorageWriteSaturation,
    /// Backpressure escalation/recovery scenario.
    BackpressureEscalation,
    /// Capacity-governor decision scenario.
    CapacityGovernor,
}

impl Default for SwarmCapacityWorkloadClass {
    fn default() -> Self {
        Self::IdleObservation
    }
}

impl SwarmCapacityWorkloadClass {
    /// Stable workload label for certificates and robot surfaces.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdleObservation => "idle_observation",
            Self::HeavyCapture => "heavy_capture",
            Self::WorkflowStorm => "workflow_storm",
            Self::PolicyDenialStorm => "policy_denial_storm",
            Self::SnapshotRestore => "snapshot_restore",
            Self::SearchHeavy => "search_heavy",
            Self::RobotMcpBurst => "robot_mcp_burst",
            Self::StorageWriteSaturation => "storage_write_saturation",
            Self::BackpressureEscalation => "backpressure_escalation",
            Self::CapacityGovernor => "capacity_governor",
        }
    }
}

impl fmt::Display for SwarmCapacityWorkloadClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Machine shape for a capacity certificate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityMachineClass {
    /// Logical CPU count, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_count: Option<u32>,
    /// Memory bytes, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
}

/// Capacity certificate status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityCertificateStatus {
    /// All selected stages are inside the conservative safe envelope.
    Safe,
    /// At least one selected stage is outside the safe envelope.
    Unsafe,
    /// Missing or invalid assumptions prevent certification.
    Unknown,
}

impl SwarmCapacityCertificateStatus {
    /// Stable reason-prefix label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Unsafe => "unsafe",
            Self::Unknown => "unknown",
        }
    }
}

/// Assumption flags emitted by capacity certificates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityAssumptionFlag {
    /// Source telemetry collector was disabled.
    TelemetryDisabled,
    /// Observation window was missing, zero, or non-finite.
    InvalidObservationWindow,
    /// Stage does not have enough retained observations.
    InsufficientSamples,
    /// Stage has no completed operations.
    NoCompletions,
    /// Required service quantiles or mean are missing.
    MissingServiceQuantiles,
    /// Terminal observations exceed arrivals, so arrival rate is unreliable.
    TerminalExceedsArrivals,
    /// Service tail is too heavy for the first conservative model.
    HeavyTailedService,
    /// Baseline age exceeds the configured staleness threshold.
    StaleBaseline,
    /// Utilization exceeds the certifiably-safe threshold but is below saturation.
    UtilizationAboveSafeThreshold,
    /// Utilization exceeds the saturated threshold.
    SaturatedUtilization,
    /// Error, timeout, or cancellation rate exceeds the budget.
    ErrorBudgetExceeded,
    /// Modeled p99 underestimates observed p99 beyond tolerance.
    ModelResidualGtTolerance,
    /// Little's Law consistency check failed beyond tolerance.
    LittleLawInconsistent,
}

impl SwarmCapacityAssumptionFlag {
    const fn is_unknown_fatal(self) -> bool {
        matches!(
            self,
            Self::TelemetryDisabled
                | Self::InvalidObservationWindow
                | Self::InsufficientSamples
                | Self::NoCompletions
                | Self::MissingServiceQuantiles
                | Self::TerminalExceedsArrivals
                | Self::HeavyTailedService
                | Self::StaleBaseline
                | Self::UtilizationAboveSafeThreshold
                | Self::ModelResidualGtTolerance
                | Self::LittleLawInconsistent
        )
    }

    const fn is_unsafe(self) -> bool {
        matches!(self, Self::SaturatedUtilization | Self::ErrorBudgetExceeded)
    }

    /// Stable flag label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TelemetryDisabled => "telemetry_disabled",
            Self::InvalidObservationWindow => "invalid_observation_window",
            Self::InsufficientSamples => "insufficient_samples",
            Self::NoCompletions => "no_completions",
            Self::MissingServiceQuantiles => "missing_service_quantiles",
            Self::TerminalExceedsArrivals => "terminal_exceeds_arrivals",
            Self::HeavyTailedService => "heavy_tailed_service",
            Self::StaleBaseline => "stale_baseline",
            Self::UtilizationAboveSafeThreshold => "utilization_above_safe_threshold",
            Self::SaturatedUtilization => "saturated_utilization",
            Self::ErrorBudgetExceeded => "error_budget_exceeded",
            Self::ModelResidualGtTolerance => "model_residual_gt_tolerance",
            Self::LittleLawInconsistent => "little_law_inconsistent",
        }
    }
}

/// Configuration for compiling a capacity certificate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SwarmCapacityCertificateConfig {
    /// Workload class represented by the telemetry.
    pub workload_class: SwarmCapacityWorkloadClass,
    /// Machine shape represented by the telemetry.
    pub machine_class: SwarmCapacityMachineClass,
    /// Pane scale represented by the telemetry.
    pub pane_scale: u32,
    /// Observation window in seconds.
    pub observation_window_secs: f64,
    /// Stages included in this certificate. Empty means all fixed stages.
    pub selected_stages: Vec<SwarmCapacityStage>,
    /// Minimum service samples required per selected stage.
    pub min_samples_per_stage: u64,
    /// Utilization at or below this threshold is considered certifiably safe.
    pub safe_utilization_threshold: f64,
    /// Utilization at or above this threshold is reported as saturated.
    pub unsafe_utilization_threshold: f64,
    /// Allowed model-underestimate residual before fail-closed unknown.
    pub model_residual_tolerance: f64,
    /// p99/mean ratio above which service is treated as heavy-tailed.
    pub heavy_tail_p99_to_mean_ratio: f64,
    /// Maximum terminal error/timeout/cancellation rate.
    pub max_terminal_error_rate: f64,
    /// Optional baseline age in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_age_secs: Option<u64>,
    /// Optional staleness threshold in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_after_secs: Option<u64>,
}

impl Default for SwarmCapacityCertificateConfig {
    fn default() -> Self {
        Self {
            workload_class: SwarmCapacityWorkloadClass::default(),
            machine_class: SwarmCapacityMachineClass::default(),
            pane_scale: 0,
            observation_window_secs: 60.0,
            selected_stages: Vec::new(),
            min_samples_per_stage: 5,
            safe_utilization_threshold: 0.80,
            unsafe_utilization_threshold: 0.95,
            model_residual_tolerance: 0.20,
            heavy_tail_p99_to_mean_ratio: 10.0,
            max_terminal_error_rate: 0.01,
            baseline_age_secs: None,
            stale_after_secs: None,
        }
    }
}

impl SwarmCapacityCertificateConfig {
    fn normalized_selected_stages(&self) -> Vec<SwarmCapacityStage> {
        if self.selected_stages.is_empty() {
            return SwarmCapacityStage::ALL.to_vec();
        }

        SwarmCapacityStage::ALL
            .into_iter()
            .filter(|stage| self.selected_stages.contains(stage))
            .collect()
    }

    fn observation_window_secs(&self) -> Option<f64> {
        (self.observation_window_secs.is_finite() && self.observation_window_secs > 0.0)
            .then_some(self.observation_window_secs)
    }

    fn safe_utilization_threshold(&self) -> f64 {
        finite_or(self.safe_utilization_threshold, 0.80).clamp(0.01, 0.99)
    }

    fn unsafe_utilization_threshold(&self) -> f64 {
        finite_or(self.unsafe_utilization_threshold, 0.95)
            .clamp(self.safe_utilization_threshold().max(0.01), 1.0)
    }

    fn model_residual_tolerance(&self) -> f64 {
        finite_or(self.model_residual_tolerance, 0.20).clamp(0.0, 10.0)
    }

    fn heavy_tail_ratio(&self) -> f64 {
        finite_or(self.heavy_tail_p99_to_mean_ratio, 10.0).max(1.0)
    }

    fn max_terminal_error_rate(&self) -> f64 {
        finite_or(self.max_terminal_error_rate, 0.01).clamp(0.0, 1.0)
    }

    fn baseline_stale(&self) -> bool {
        matches!(
            (self.baseline_age_secs, self.stale_after_secs),
            (Some(age), Some(limit)) if age > limit
        )
    }
}

/// Quantile projection used by the capacity certificate model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SwarmCapacityLatencyModel {
    /// Modeled p50 latency in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<f64>,
    /// Modeled p95 latency in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<f64>,
    /// Modeled p99 latency in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p99_ms: Option<f64>,
    /// Conservative modeled p999 latency in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p999_ms: Option<f64>,
}

/// Per-stage capacity certificate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCapacityStageCertificate {
    /// Fixed stage.
    pub stage: SwarmCapacityStage,
    /// Stable fixed stage label.
    pub stage_name: String,
    /// Stage-local status.
    pub status: SwarmCapacityCertificateStatus,
    /// Stable reason code for the stage status.
    pub reason_code: String,
    /// Arrival rate in operations per second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arrival_rate_per_s: Option<f64>,
    /// Service rate in operations per second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_rate_per_s: Option<f64>,
    /// Arrival/service utilization estimate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    /// Terminal error, timeout, and cancellation rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_error_rate: Option<f64>,
    /// Observed service-time distribution.
    pub service_time_ms: HistogramSummary,
    /// Observed queue-depth distribution.
    pub queue_depth: HistogramSummary,
    /// Observed wait/admission/lock-wait distribution.
    pub wait_time_ms: HistogramSummary,
    /// Observed retry latency distribution.
    pub retry_latency_ms: HistogramSummary,
    /// Modeled latency projection.
    pub modeled_latency: SwarmCapacityLatencyModel,
    /// Selected assumption flags.
    pub assumption_flags: Vec<SwarmCapacityAssumptionFlag>,
    /// Successful completions.
    pub completions: u64,
    /// Cancelled operations.
    pub cancellations: u64,
    /// Timed-out operations.
    pub timeouts: u64,
    /// Failed operations.
    pub errors: u64,
}

/// Deterministic capacity certificate compiled from fixed-stage telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCapacityCertificate {
    /// Workload represented by this certificate.
    pub workload_class: SwarmCapacityWorkloadClass,
    /// Machine represented by this certificate.
    pub machine_class: SwarmCapacityMachineClass,
    /// Pane scale represented by this certificate.
    pub pane_scale: u32,
    /// Overall certificate status.
    pub status: SwarmCapacityCertificateStatus,
    /// Stable overall reason code.
    pub reason_code: String,
    /// Observation window in seconds.
    pub observation_window_secs: f64,
    /// Selected-stage certificates in deterministic stage order.
    pub stages: Vec<SwarmCapacityStageCertificate>,
    /// Bottleneck stage by utilization, tie-broken by fixed stage order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottleneck_stage: Option<SwarmCapacityStage>,
    /// Bottleneck utilization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottleneck_utilization: Option<f64>,
    /// Unique assumption flags across selected stages.
    pub assumption_flags: Vec<SwarmCapacityAssumptionFlag>,
}

/// Tail-risk monitor status for capacity-controller safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmTailRiskStatus {
    /// Live telemetry remains inside the calibrated tail budget.
    Green,
    /// Live telemetry is near or just outside budget and should slow admission.
    Watch,
    /// Live telemetry has exceeded a hard tail-risk budget.
    Violated,
    /// Missing or invalid evidence prevents a safe decision.
    Unknown,
    /// The baseline is too old to use for live control decisions.
    StaleBaseline,
}

impl SwarmTailRiskStatus {
    /// Stable status label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Watch => "watch",
            Self::Violated => "violated",
            Self::Unknown => "unknown",
            Self::StaleBaseline => "stale_baseline",
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Green => 0,
            Self::Watch => 1,
            Self::Violated => 2,
            Self::Unknown => 3,
            Self::StaleBaseline => 4,
        }
    }
}

/// Tail-risk evidence signal emitted by the monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmTailRiskSignal {
    /// No stage-local budget was exceeded.
    WithinBudget,
    /// Baseline certificate is older than the configured freshness window.
    BaselineStale,
    /// Baseline did not certify safe, so it cannot calibrate live decisions.
    BaselineNotSafe,
    /// Baseline is missing a selected stage.
    MissingBaselineStage,
    /// Live certificate is missing a selected stage.
    MissingLiveStage,
    /// Baseline has too few retained samples for finite-sample tail monitoring.
    InsufficientBaselineSamples,
    /// Live telemetry has too few retained samples for finite-sample tail monitoring.
    InsufficientLiveSamples,
    /// Live p99 exceeded the calibrated p99 tail budget.
    P99Residual,
    /// Live modeled p999 exceeded the calibrated p999 tail budget.
    P999Residual,
    /// Live queue p99 exceeded the baseline queue budget.
    QueueP99ExceedsBaseline,
    /// Live utilization reached the watch or violation budget.
    Saturation,
    /// Live cancellation rate exceeded the configured tail budget.
    CancellationSpike,
    /// Live retry p99 exceeded the baseline retry budget.
    RetryStorm,
    /// Live terminal error/timeout/cancellation rate exceeded budget.
    TerminalErrorRate,
}

impl SwarmTailRiskSignal {
    /// Stable signal label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WithinBudget => "within_budget",
            Self::BaselineStale => "baseline_stale",
            Self::BaselineNotSafe => "baseline_not_safe",
            Self::MissingBaselineStage => "missing_baseline_stage",
            Self::MissingLiveStage => "missing_live_stage",
            Self::InsufficientBaselineSamples => "insufficient_baseline_samples",
            Self::InsufficientLiveSamples => "insufficient_live_samples",
            Self::P99Residual => "p99_residual",
            Self::P999Residual => "p999_residual",
            Self::QueueP99ExceedsBaseline => "queue_p99_exceeds_baseline",
            Self::Saturation => "saturation",
            Self::CancellationSpike => "cancellation_spike",
            Self::RetryStorm => "retry_storm",
            Self::TerminalErrorRate => "terminal_error_rate",
        }
    }
}

/// Configuration for comparing live telemetry against a baseline certificate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SwarmTailRiskMonitorConfig {
    /// Stages included in the monitor. Empty means all fixed stages.
    pub selected_stages: Vec<SwarmCapacityStage>,
    /// Miscoverage budget used for finite-sample slack.
    pub alpha: f64,
    /// Minimum baseline service samples per selected stage.
    pub min_baseline_samples_per_stage: u64,
    /// Minimum live service samples per selected stage.
    pub min_live_samples_per_stage: u64,
    /// Optional baseline age in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_age_secs: Option<u64>,
    /// Optional freshness window in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_after_secs: Option<u64>,
    /// Additional live p99 slack before watch.
    pub watch_slack_ratio: f64,
    /// Additional live p99 slack beyond watch before violation.
    pub violation_slack_ratio: f64,
    /// Upper bound on finite-sample slack added to baseline p99.
    pub max_finite_sample_slack_ratio: f64,
    /// Queue p99 multiplier that enters watch.
    pub queue_watch_multiplier: f64,
    /// Queue p99 multiplier that enters violation.
    pub queue_violation_multiplier: f64,
    /// Retry p99 multiplier that enters watch.
    pub retry_watch_multiplier: f64,
    /// Retry p99 multiplier that enters violation.
    pub retry_violation_multiplier: f64,
    /// Minimum retry budget in milliseconds when baseline had no retries.
    pub retry_floor_ms: f64,
    /// Terminal error/cancel/timeout rate that enters watch.
    pub terminal_error_watch_rate: f64,
    /// Terminal error/cancel/timeout rate that enters violation.
    pub terminal_error_violation_rate: f64,
    /// Cancellation-only terminal rate that enters watch.
    pub cancellation_watch_rate: f64,
    /// Cancellation-only terminal rate that enters violation.
    pub cancellation_violation_rate: f64,
    /// Utilization that enters watch.
    pub saturation_watch_utilization: f64,
    /// Utilization that enters violation.
    pub saturation_violation_utilization: f64,
}

impl Default for SwarmTailRiskMonitorConfig {
    fn default() -> Self {
        Self {
            selected_stages: Vec::new(),
            alpha: 0.05,
            min_baseline_samples_per_stage: 20,
            min_live_samples_per_stage: 5,
            baseline_age_secs: None,
            stale_after_secs: None,
            watch_slack_ratio: 0.0,
            violation_slack_ratio: 0.25,
            max_finite_sample_slack_ratio: 0.50,
            queue_watch_multiplier: 1.25,
            queue_violation_multiplier: 2.0,
            retry_watch_multiplier: 2.0,
            retry_violation_multiplier: 5.0,
            retry_floor_ms: 1.0,
            terminal_error_watch_rate: 0.01,
            terminal_error_violation_rate: 0.05,
            cancellation_watch_rate: 0.01,
            cancellation_violation_rate: 0.05,
            saturation_watch_utilization: 0.80,
            saturation_violation_utilization: 0.95,
        }
    }
}

impl SwarmTailRiskMonitorConfig {
    fn normalized_selected_stages(&self) -> Vec<SwarmCapacityStage> {
        if self.selected_stages.is_empty() {
            return SwarmCapacityStage::ALL.to_vec();
        }

        SwarmCapacityStage::ALL
            .into_iter()
            .filter(|stage| self.selected_stages.contains(stage))
            .collect()
    }

    fn alpha(&self) -> f64 {
        finite_or(self.alpha, 0.05).clamp(0.001, 0.50)
    }

    fn watch_slack_ratio(&self) -> f64 {
        finite_or(self.watch_slack_ratio, 0.0).clamp(0.0, 10.0)
    }

    fn violation_slack_ratio(&self) -> f64 {
        finite_or(self.violation_slack_ratio, 0.25).clamp(0.0, 10.0)
    }

    fn max_finite_sample_slack_ratio(&self) -> f64 {
        finite_or(self.max_finite_sample_slack_ratio, 0.50).clamp(0.0, 10.0)
    }

    fn queue_watch_multiplier(&self) -> f64 {
        finite_or(self.queue_watch_multiplier, 1.25).max(1.0)
    }

    fn queue_violation_multiplier(&self) -> f64 {
        finite_or(self.queue_violation_multiplier, 2.0).max(self.queue_watch_multiplier())
    }

    fn retry_watch_multiplier(&self) -> f64 {
        finite_or(self.retry_watch_multiplier, 2.0).max(1.0)
    }

    fn retry_violation_multiplier(&self) -> f64 {
        finite_or(self.retry_violation_multiplier, 5.0).max(self.retry_watch_multiplier())
    }

    fn retry_floor_ms(&self) -> f64 {
        finite_or(self.retry_floor_ms, 1.0).max(0.0)
    }

    fn terminal_error_watch_rate(&self) -> f64 {
        finite_or(self.terminal_error_watch_rate, 0.01).clamp(0.0, 1.0)
    }

    fn terminal_error_violation_rate(&self) -> f64 {
        finite_or(self.terminal_error_violation_rate, 0.05)
            .clamp(self.terminal_error_watch_rate(), 1.0)
    }

    fn cancellation_watch_rate(&self) -> f64 {
        finite_or(self.cancellation_watch_rate, 0.01).clamp(0.0, 1.0)
    }

    fn cancellation_violation_rate(&self) -> f64 {
        finite_or(self.cancellation_violation_rate, 0.05).clamp(self.cancellation_watch_rate(), 1.0)
    }

    fn saturation_watch_utilization(&self) -> f64 {
        finite_or(self.saturation_watch_utilization, 0.80).clamp(0.01, 1.0)
    }

    fn saturation_violation_utilization(&self) -> f64 {
        finite_or(self.saturation_violation_utilization, 0.95)
            .clamp(self.saturation_watch_utilization(), 1.0)
    }

    fn baseline_stale(&self) -> bool {
        matches!(
            (self.baseline_age_secs, self.stale_after_secs),
            (Some(age), Some(limit)) if age > limit
        )
    }
}

/// Stage-local evidence emitted by the tail-risk monitor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmTailRiskEvidence {
    /// Fixed stage.
    pub stage: SwarmCapacityStage,
    /// Stable fixed stage label.
    pub stage_name: String,
    /// Evidence signal.
    pub signal: SwarmTailRiskSignal,
    /// Status implied by this evidence item.
    pub status: SwarmTailRiskStatus,
    /// Observed live value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<f64>,
    /// Budget used for the decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<f64>,
    /// Stable human/robot-readable explanation.
    pub detail: String,
}

/// Stage-local tail-risk monitor result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmTailRiskStageReport {
    /// Fixed stage.
    pub stage: SwarmCapacityStage,
    /// Stable fixed stage label.
    pub stage_name: String,
    /// Stage-local monitor status.
    pub status: SwarmTailRiskStatus,
    /// Stable reason code for the stage status.
    pub reason_code: String,
    /// Stage-local evidence records.
    pub evidence: Vec<SwarmTailRiskEvidence>,
}

/// Tail-risk monitor report for a live-vs-baseline certificate comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmTailRiskReport {
    /// Workload represented by the baseline.
    pub workload_class: SwarmCapacityWorkloadClass,
    /// Pane scale represented by the live certificate.
    pub pane_scale: u32,
    /// Miscoverage budget after sanitization.
    pub alpha: f64,
    /// Realized confidence target.
    pub confidence: f64,
    /// Overall monitor status.
    pub status: SwarmTailRiskStatus,
    /// Stable overall reason code.
    pub reason_code: String,
    /// Stage-local reports in deterministic stage order.
    pub stages: Vec<SwarmTailRiskStageReport>,
    /// Flattened evidence in deterministic stage order.
    pub evidence: Vec<SwarmTailRiskEvidence>,
}

/// Conservative controller action recorded in a capacity evidence ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityDecisionAction {
    /// Certificate and live tail-risk evidence allow normal admission.
    Allow,
    /// Admission should slow down, but existing work may continue.
    ReduceAdmission,
    /// New admission should stop until evidence becomes safe again.
    BlockAdmission,
}

impl SwarmCapacityDecisionAction {
    /// Stable action label used in records and replay output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::ReduceAdmission => "reduce_admission",
            Self::BlockAdmission => "block_admission",
        }
    }
}

/// Deterministic controller decision recomputed during evidence replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityDecision {
    /// Side-effect-free action recommendation.
    pub action: SwarmCapacityDecisionAction,
    /// Stable reason code copied from the controlling certificate/report.
    pub reason_code: String,
}

/// Controller inputs persisted with each capacity decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCapacityControllerInputs {
    /// Workload class represented by the decision.
    pub workload_class: SwarmCapacityWorkloadClass,
    /// Pane scale represented by the live evidence.
    pub pane_scale: u32,
    /// Capacity certificate status.
    pub certificate_status: SwarmCapacityCertificateStatus,
    /// Tail-risk monitor status.
    pub monitor_status: SwarmTailRiskStatus,
    /// Bottleneck stage by utilization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottleneck_stage: Option<SwarmCapacityStage>,
    /// Bottleneck utilization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottleneck_utilization: Option<f64>,
    /// Unique capacity assumption flags in deterministic order.
    pub assumption_flags: Vec<SwarmCapacityAssumptionFlag>,
    /// Unique tail-risk signals in deterministic evidence order.
    pub tail_signals: Vec<SwarmTailRiskSignal>,
}

impl SwarmCapacityControllerInputs {
    fn from_evidence(certificate: &SwarmCapacityCertificate, report: &SwarmTailRiskReport) -> Self {
        let mut tail_signals = Vec::new();
        for signal in report.evidence.iter().map(|entry| entry.signal) {
            if !tail_signals.contains(&signal) {
                tail_signals.push(signal);
            }
        }

        Self {
            workload_class: certificate.workload_class,
            pane_scale: report.pane_scale,
            certificate_status: certificate.status,
            monitor_status: report.status,
            bottleneck_stage: certificate.bottleneck_stage,
            bottleneck_utilization: certificate.bottleneck_utilization,
            assumption_flags: certificate.assumption_flags.clone(),
            tail_signals,
        }
    }
}

/// Operator-visible work class used by the capacity fairness policy.
///
/// The class is supplied by existing pane/workflow metadata. It intentionally
/// does not carry pane text, command text, cwd, prompt content, or secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityWorkClass {
    /// Direct operator interaction and explicit foreground control.
    InteractiveOperator,
    /// Claimed agent task that is already part of a coordinated work graph.
    ClaimedAgentTask,
    /// Replay, search, and forensics work that can usually wait briefly.
    ReplaySearch,
    /// Continuous capture and low-level observation traffic.
    BackgroundCapture,
    /// GC, compaction, checkpoint, and other maintenance work.
    Maintenance,
    /// Extra diagnostics that are useful but safe to shed first.
    OptionalDiagnostics,
}

impl SwarmCapacityWorkClass {
    /// Stable class order used for deterministic tie-breaking.
    pub const ALL: [Self; 6] = [
        Self::InteractiveOperator,
        Self::ClaimedAgentTask,
        Self::ReplaySearch,
        Self::BackgroundCapture,
        Self::Maintenance,
        Self::OptionalDiagnostics,
    ];

    /// Stable class label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InteractiveOperator => "interactive_operator",
            Self::ClaimedAgentTask => "claimed_agent_task",
            Self::ReplaySearch => "replay_search",
            Self::BackgroundCapture => "background_capture",
            Self::Maintenance => "maintenance",
            Self::OptionalDiagnostics => "optional_diagnostics",
        }
    }

    const fn class_rank(self) -> u8 {
        match self {
            Self::InteractiveOperator => 0,
            Self::ClaimedAgentTask => 1,
            Self::ReplaySearch => 2,
            Self::BackgroundCapture => 3,
            Self::Maintenance => 4,
            Self::OptionalDiagnostics => 5,
        }
    }
}

/// Capacity fairness action for one request under pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityFairnessAction {
    /// Admit the request in this planning round.
    Admit,
    /// Defer the request without discarding it.
    Defer,
    /// Shed optional work instead of queueing it indefinitely.
    Shed,
}

impl SwarmCapacityFairnessAction {
    /// Stable action label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admit => "admit",
            Self::Defer => "defer",
            Self::Shed => "shed",
        }
    }
}

/// Deterministic policy row for one capacity work class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityFairnessClassPolicy {
    /// Work class covered by this row.
    pub work_class: SwarmCapacityWorkClass,
    /// Weighted-fair-queueing weight. Higher values receive more service.
    pub weight: u32,
    /// Minimum class service target in per-mille units.
    pub minimum_service_per_1000: u16,
    /// Pressure actions allowed for this class, in deterministic preference order.
    pub eligible_pressure_actions: Vec<SwarmCapacityFairnessAction>,
}

impl SwarmCapacityFairnessClassPolicy {
    /// Default deterministic policy row for a work class.
    #[must_use]
    pub fn default_for(work_class: SwarmCapacityWorkClass) -> Self {
        match work_class {
            SwarmCapacityWorkClass::InteractiveOperator => Self {
                work_class,
                weight: 100,
                minimum_service_per_1000: 250,
                eligible_pressure_actions: vec![SwarmCapacityFairnessAction::Defer],
            },
            SwarmCapacityWorkClass::ClaimedAgentTask => Self {
                work_class,
                weight: 90,
                minimum_service_per_1000: 350,
                eligible_pressure_actions: vec![SwarmCapacityFairnessAction::Defer],
            },
            SwarmCapacityWorkClass::ReplaySearch => Self {
                work_class,
                weight: 45,
                minimum_service_per_1000: 120,
                eligible_pressure_actions: vec![SwarmCapacityFairnessAction::Defer],
            },
            SwarmCapacityWorkClass::BackgroundCapture => Self {
                work_class,
                weight: 30,
                minimum_service_per_1000: 100,
                eligible_pressure_actions: vec![
                    SwarmCapacityFairnessAction::Defer,
                    SwarmCapacityFairnessAction::Shed,
                ],
            },
            SwarmCapacityWorkClass::Maintenance => Self {
                work_class,
                weight: 20,
                minimum_service_per_1000: 50,
                eligible_pressure_actions: vec![
                    SwarmCapacityFairnessAction::Defer,
                    SwarmCapacityFairnessAction::Shed,
                ],
            },
            SwarmCapacityWorkClass::OptionalDiagnostics => Self {
                work_class,
                weight: 5,
                minimum_service_per_1000: 0,
                eligible_pressure_actions: vec![
                    SwarmCapacityFairnessAction::Shed,
                    SwarmCapacityFairnessAction::Defer,
                ],
            },
        }
    }

    fn normalized(&self) -> Self {
        let default = Self::default_for(self.work_class);
        let mut eligible_pressure_actions = Vec::new();
        for action in self
            .eligible_pressure_actions
            .iter()
            .copied()
            .filter(|action| !matches!(action, SwarmCapacityFairnessAction::Admit))
        {
            if !eligible_pressure_actions.contains(&action) {
                eligible_pressure_actions.push(action);
            }
        }
        if eligible_pressure_actions.is_empty() {
            eligible_pressure_actions = default.eligible_pressure_actions;
        }

        Self {
            work_class: self.work_class,
            weight: self.weight.clamp(1, 10_000),
            minimum_service_per_1000: self.minimum_service_per_1000.min(1000),
            eligible_pressure_actions,
        }
    }

    fn pressure_action(&self) -> SwarmCapacityFairnessAction {
        self.eligible_pressure_actions
            .first()
            .copied()
            .unwrap_or(SwarmCapacityFairnessAction::Defer)
    }
}

/// Capacity fairness policy configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SwarmCapacityFairnessPolicyConfig {
    /// Whether this policy can reorder, defer, or shed requests.
    pub enabled: bool,
    /// Admission budget under a normal `Allow` controller decision.
    pub admission_budget_units: u32,
    /// Admission budget under a `ReduceAdmission` controller decision.
    pub reduced_admission_budget_units: u32,
    /// Optional class-policy overrides. Missing classes use the default table.
    pub class_policies: Vec<SwarmCapacityFairnessClassPolicy>,
}

impl Default for SwarmCapacityFairnessPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            admission_budget_units: u32::MAX,
            reduced_admission_budget_units: u32::MAX / 2,
            class_policies: Vec::new(),
        }
    }
}

impl SwarmCapacityFairnessPolicyConfig {
    fn normalized_class_policies(&self) -> Vec<SwarmCapacityFairnessClassPolicy> {
        let mut table = BTreeMap::new();
        for work_class in SwarmCapacityWorkClass::ALL {
            table.insert(
                work_class,
                SwarmCapacityFairnessClassPolicy::default_for(work_class),
            );
        }
        for policy in &self.class_policies {
            table.insert(policy.work_class, policy.normalized());
        }

        SwarmCapacityWorkClass::ALL
            .into_iter()
            .filter_map(|work_class| table.remove(&work_class))
            .collect()
    }

    fn budget_for(&self, controller_action: SwarmCapacityDecisionAction) -> u32 {
        match controller_action {
            SwarmCapacityDecisionAction::Allow => self.admission_budget_units,
            SwarmCapacityDecisionAction::ReduceAdmission => self
                .reduced_admission_budget_units
                .min(self.admission_budget_units),
            SwarmCapacityDecisionAction::BlockAdmission => 0,
        }
    }

    /// Stable hash of the normalized policy table and enabled flag.
    #[must_use]
    pub fn policy_hash(&self) -> String {
        stable_json_hash(&(
            SWARM_CAPACITY_FAIRNESS_POLICY_VERSION,
            self.enabled,
            self.admission_budget_units,
            self.reduced_admission_budget_units,
            self.normalized_class_policies(),
        ))
    }
}

/// Side-effect-free request input to the capacity fairness planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityFairnessRequest {
    /// Stable opaque identifier. Do not derive this from pane content.
    pub stable_id: String,
    /// Operator-visible class used for policy lookup.
    pub work_class: SwarmCapacityWorkClass,
    /// Monotonic arrival sequence used as a deterministic tie-breaker.
    pub arrival_sequence: u64,
    /// Capacity units requested in this planning round.
    pub requested_units: u32,
    /// Prior service units for this request or fair-share entity.
    pub served_units: u64,
    /// Observed class service share in per-mille units.
    pub observed_service_per_1000: u16,
    /// Result from existing policy gates. The fairness planner never bypasses it.
    pub policy_gate_open: bool,
    /// Result from existing workflow lock checks.
    pub workflow_lock_available: bool,
}

impl SwarmCapacityFairnessRequest {
    /// Build a request with safe defaults for a side-effect-free planning round.
    #[must_use]
    pub fn new(
        stable_id: impl Into<String>,
        work_class: SwarmCapacityWorkClass,
        arrival_sequence: u64,
    ) -> Self {
        Self {
            stable_id: stable_id.into(),
            work_class,
            arrival_sequence,
            requested_units: 1,
            served_units: 0,
            observed_service_per_1000: 0,
            policy_gate_open: true,
            workflow_lock_available: true,
        }
    }

    fn requested_units(&self) -> u32 {
        self.requested_units.max(1)
    }
}

/// Fairness decision for one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityFairnessDecision {
    /// Request identifier.
    pub stable_id: String,
    /// Work class used for the policy decision.
    pub work_class: SwarmCapacityWorkClass,
    /// Request-level action.
    pub action: SwarmCapacityFairnessAction,
    /// Stable reason code suitable for robot/operator surfaces.
    pub reason_code: String,
    /// Rank after deterministic fairness ordering.
    pub rank: usize,
    /// Units admitted by this decision.
    pub admitted_units: u32,
}

/// Complete side-effect-free fairness plan for one controller decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityFairnessPlan {
    /// Whether fairness policy was enabled.
    pub enabled: bool,
    /// Fairness policy version used to plan.
    pub policy_version: u32,
    /// Stable hash of the normalized policy table.
    pub policy_hash: String,
    /// Controller action that constrained this plan.
    pub controller_action: SwarmCapacityDecisionAction,
    /// Admission budget used for this plan.
    pub budget_units: u32,
    /// Total admitted units.
    pub admitted_units: u32,
    /// Request decisions in deterministic planning order.
    pub decisions: Vec<SwarmCapacityFairnessDecision>,
}

impl SwarmCapacityFairnessPlan {
    /// Decision for a request id, if present.
    #[must_use]
    pub fn decision_for(&self, stable_id: &str) -> Option<&SwarmCapacityFairnessDecision> {
        self.decisions
            .iter()
            .find(|decision| decision.stable_id == stable_id)
    }
}

/// Deterministic priority-aware fairness policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityFairnessPolicy {
    /// Normalized configuration.
    pub config: SwarmCapacityFairnessPolicyConfig,
    /// Normalized deterministic class table.
    pub class_policies: Vec<SwarmCapacityFairnessClassPolicy>,
}

impl SwarmCapacityFairnessPolicy {
    /// Build a policy from possibly-partial config.
    #[must_use]
    pub fn new(config: SwarmCapacityFairnessPolicyConfig) -> Self {
        let class_policies = config.normalized_class_policies();
        Self {
            config,
            class_policies,
        }
    }

    /// Default disabled policy. Planning preserves request order and admits all.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(SwarmCapacityFairnessPolicyConfig::default())
    }

    /// Return the deterministic policy table.
    #[must_use]
    pub fn policy_table(&self) -> &[SwarmCapacityFairnessClassPolicy] {
        &self.class_policies
    }

    /// Plan request-level fairness without executing any pane or workflow action.
    #[must_use]
    pub fn plan(
        &self,
        requests: &[SwarmCapacityFairnessRequest],
        controller_action: SwarmCapacityDecisionAction,
    ) -> SwarmCapacityFairnessPlan {
        if !self.config.enabled {
            let decisions = requests
                .iter()
                .enumerate()
                .map(|(rank, request)| SwarmCapacityFairnessDecision {
                    stable_id: request.stable_id.clone(),
                    work_class: request.work_class,
                    action: SwarmCapacityFairnessAction::Admit,
                    reason_code: "capacity.fairness.disabled".to_string(),
                    rank,
                    admitted_units: request.requested_units(),
                })
                .collect::<Vec<_>>();
            let admitted_units = decisions
                .iter()
                .map(|decision| decision.admitted_units)
                .fold(0u32, u32::saturating_add);

            return SwarmCapacityFairnessPlan {
                enabled: false,
                policy_version: SWARM_CAPACITY_FAIRNESS_POLICY_VERSION,
                policy_hash: self.config.policy_hash(),
                controller_action,
                budget_units: u32::MAX,
                admitted_units,
                decisions,
            };
        }

        let mut ranked = requests.iter().collect::<Vec<_>>();
        ranked.sort_by_key(|request| self.rank_key(request));

        let budget_units = self.config.budget_for(controller_action);
        let mut admitted_units = 0u32;
        let mut decisions = Vec::with_capacity(ranked.len());
        for (rank, request) in ranked.into_iter().enumerate() {
            let policy = self.class_policy(request.work_class);
            let requested_units = request.requested_units();
            let (action, reason_code, decision_admitted_units) = if !request.policy_gate_open {
                (
                    SwarmCapacityFairnessAction::Defer,
                    fairness_reason_code(request.work_class, "policy_gate_closed"),
                    0,
                )
            } else if !request.workflow_lock_available {
                (
                    SwarmCapacityFairnessAction::Defer,
                    fairness_reason_code(request.work_class, "workflow_lock_unavailable"),
                    0,
                )
            } else if admitted_units.saturating_add(requested_units) <= budget_units {
                let reason = if service_debt(policy, request) > 0 {
                    "minimum_service"
                } else {
                    "weighted_fair_queue"
                };
                admitted_units = admitted_units.saturating_add(requested_units);
                (
                    SwarmCapacityFairnessAction::Admit,
                    fairness_reason_code(request.work_class, reason),
                    requested_units,
                )
            } else {
                let action = policy.pressure_action();
                let reason = match action {
                    SwarmCapacityFairnessAction::Admit => "weighted_fair_queue",
                    SwarmCapacityFairnessAction::Defer => "defer_under_pressure",
                    SwarmCapacityFairnessAction::Shed => "shed_under_pressure",
                };
                (action, fairness_reason_code(request.work_class, reason), 0)
            };

            decisions.push(SwarmCapacityFairnessDecision {
                stable_id: request.stable_id.clone(),
                work_class: request.work_class,
                action,
                reason_code,
                rank,
                admitted_units: decision_admitted_units,
            });
        }

        SwarmCapacityFairnessPlan {
            enabled: true,
            policy_version: SWARM_CAPACITY_FAIRNESS_POLICY_VERSION,
            policy_hash: self.config.policy_hash(),
            controller_action,
            budget_units,
            admitted_units,
            decisions,
        }
    }

    fn class_policy(
        &self,
        work_class: SwarmCapacityWorkClass,
    ) -> &SwarmCapacityFairnessClassPolicy {
        self.class_policies
            .iter()
            .find(|policy| policy.work_class == work_class)
            .expect("normalized policy table covers every work class")
    }

    fn rank_key(&self, request: &SwarmCapacityFairnessRequest) -> SwarmCapacityFairnessRankKey {
        let policy = self.class_policy(request.work_class);
        SwarmCapacityFairnessRankKey {
            service_debt_rank: u16::MAX - service_debt(policy, request),
            normalized_service: normalized_service_units(policy, request),
            weight_rank: u32::MAX - policy.weight,
            class_rank: request.work_class.class_rank(),
            arrival_sequence: request.arrival_sequence,
            stable_id: request.stable_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SwarmCapacityFairnessRankKey {
    service_debt_rank: u16,
    normalized_service: u128,
    weight_rank: u32,
    class_rank: u8,
    arrival_sequence: u64,
    stable_id: String,
}

fn service_debt(
    policy: &SwarmCapacityFairnessClassPolicy,
    request: &SwarmCapacityFairnessRequest,
) -> u16 {
    policy
        .minimum_service_per_1000
        .saturating_sub(request.observed_service_per_1000.min(1000))
}

fn normalized_service_units(
    policy: &SwarmCapacityFairnessClassPolicy,
    request: &SwarmCapacityFairnessRequest,
) -> u128 {
    u128::from(request.served_units).saturating_mul(1_000_000) / u128::from(policy.weight.max(1))
}

fn fairness_reason_code(work_class: SwarmCapacityWorkClass, reason: &str) -> String {
    format!("{}.fairness.{reason}", work_class.as_str())
}

/// Redacted, bounded operator context attached to a capacity decision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityEvidenceContext {
    /// Bounded redacted context fields. Values never contain raw pane content.
    pub fields: BTreeMap<String, String>,
    /// Whether the context was passed through the project redaction model.
    pub redacted: bool,
    /// Number of fields dropped by the configured field cap.
    pub dropped_fields: u32,
    /// Number of keys or values truncated by byte caps.
    pub truncated_fields: u32,
}

impl SwarmCapacityEvidenceContext {
    /// Empty redacted context for decisions without operator metadata.
    #[must_use]
    pub fn empty_redacted() -> Self {
        Self {
            redacted: true,
            ..Self::default()
        }
    }

    /// Build bounded context using the existing project [`crate::policy::Redactor`].
    #[must_use]
    pub fn redacted_from_redactor(
        fields: BTreeMap<String, String>,
        redactor: &crate::policy::Redactor,
        config: &SwarmCapacityEvidenceLedgerConfig,
    ) -> Self {
        Self::redacted_from(fields, |value| redactor.redact(value), config)
    }

    /// Build bounded context with an explicit redaction function.
    #[must_use]
    pub fn redacted_from<F>(
        fields: BTreeMap<String, String>,
        redact: F,
        config: &SwarmCapacityEvidenceLedgerConfig,
    ) -> Self
    where
        F: Fn(&str) -> String,
    {
        let max_fields = config.normalized_context_fields();
        let max_value_bytes = config.normalized_context_value_bytes();
        let mut bounded = BTreeMap::new();
        let mut truncated_fields = 0u32;
        let original_len = fields.len();

        for (key, value) in fields.into_iter().take(max_fields) {
            let (key, key_truncated) = truncate_utf8(&key, MAX_SWARM_CAPACITY_CONTEXT_KEY_BYTES);
            let redacted_value = redact(&value);
            let (value, value_truncated) = truncate_utf8(&redacted_value, max_value_bytes);
            if key_truncated || value_truncated {
                truncated_fields = truncated_fields.saturating_add(1);
            }
            bounded.insert(key, value);
        }

        Self {
            fields: bounded,
            redacted: true,
            dropped_fields: original_len.saturating_sub(max_fields) as u32,
            truncated_fields,
        }
    }
}

/// Retention and bounding policy for a capacity evidence ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SwarmCapacityEvidenceLedgerConfig {
    /// Maximum retained records after pruning.
    pub max_records: usize,
    /// Optional age window used for pruning old records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retention_window_secs: Option<u64>,
    /// Maximum redacted context fields retained per record.
    pub max_context_fields: usize,
    /// Maximum UTF-8 bytes retained per redacted context value.
    pub max_context_value_bytes: usize,
}

impl Default for SwarmCapacityEvidenceLedgerConfig {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_SWARM_CAPACITY_LEDGER_RECORDS,
            retention_window_secs: Some(DEFAULT_SWARM_CAPACITY_LEDGER_RETENTION_SECS),
            max_context_fields: MAX_SWARM_CAPACITY_CONTEXT_FIELDS,
            max_context_value_bytes: MAX_SWARM_CAPACITY_CONTEXT_VALUE_BYTES,
        }
    }
}

impl SwarmCapacityEvidenceLedgerConfig {
    fn normalized_max_records(&self) -> usize {
        self.max_records.clamp(1, MAX_SWARM_CAPACITY_LEDGER_RECORDS)
    }

    fn normalized_context_fields(&self) -> usize {
        self.max_context_fields
            .clamp(1, MAX_SWARM_CAPACITY_CONTEXT_FIELDS)
    }

    fn normalized_context_value_bytes(&self) -> usize {
        self.max_context_value_bytes
            .clamp(1, MAX_SWARM_CAPACITY_CONTEXT_VALUE_BYTES)
    }

    fn retention_window_ms(&self) -> Option<u64> {
        self.retention_window_secs
            .and_then(|secs| secs.checked_mul(1000))
    }
}

/// Versioned snapshot of controller and evidence configuration hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityEvidenceConfigSnapshot {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Controller policy version used for deterministic replay.
    pub controller_version: u32,
    /// Stable hash of the certificate compiler config.
    pub certificate_config_hash: String,
    /// Stable hash of the tail-risk monitor config.
    pub tail_monitor_config_hash: String,
    /// Normalized ledger record cap.
    pub ledger_max_records: usize,
    /// Optional retention window in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger_retention_window_secs: Option<u64>,
    /// Stable hash of the normalized fairness policy table, when enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fairness_policy_hash: Option<String>,
}

impl SwarmCapacityEvidenceConfigSnapshot {
    /// Build a deterministic config snapshot from the active capacity configs.
    #[must_use]
    pub fn from_configs(
        certificate_config: &SwarmCapacityCertificateConfig,
        monitor_config: &SwarmTailRiskMonitorConfig,
        ledger_config: &SwarmCapacityEvidenceLedgerConfig,
    ) -> Self {
        Self {
            schema_version: SWARM_CAPACITY_EVIDENCE_SCHEMA_VERSION,
            controller_version: 1,
            certificate_config_hash: stable_json_hash(certificate_config),
            tail_monitor_config_hash: stable_json_hash(monitor_config),
            ledger_max_records: ledger_config.normalized_max_records(),
            ledger_retention_window_secs: ledger_config.retention_window_secs,
            fairness_policy_hash: None,
        }
    }

    /// Build a deterministic config snapshot that includes fairness policy evidence.
    #[must_use]
    pub fn from_configs_with_fairness(
        certificate_config: &SwarmCapacityCertificateConfig,
        monitor_config: &SwarmTailRiskMonitorConfig,
        ledger_config: &SwarmCapacityEvidenceLedgerConfig,
        fairness_config: &SwarmCapacityFairnessPolicyConfig,
    ) -> Self {
        let mut snapshot = Self::from_configs(certificate_config, monitor_config, ledger_config);
        snapshot.controller_version = if fairness_config.enabled { 2 } else { 1 };
        snapshot.fairness_policy_hash = Some(fairness_config.policy_hash());
        snapshot
    }

    /// Stable hash of this normalized config snapshot.
    #[must_use]
    pub fn hash(&self) -> String {
        stable_json_hash(self)
    }
}

/// Versioned, replayable capacity decision evidence record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCapacityEvidenceRecord {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Stable record identifier derived from the evidence payload.
    pub record_id: String,
    /// Unix epoch timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Workload represented by the decision.
    pub workload_class: SwarmCapacityWorkloadClass,
    /// Pane scale represented by the live decision.
    pub pane_scale: u32,
    /// Stable hash of the embedded capacity certificate.
    pub certificate_hash: String,
    /// Tail-risk monitor status observed by the controller.
    pub monitor_status: SwarmTailRiskStatus,
    /// Controller input summary.
    pub controller_inputs: SwarmCapacityControllerInputs,
    /// Recorded side-effect-free action recommendation.
    pub action: SwarmCapacityDecisionAction,
    /// Recorded reason code for the action.
    pub reason_code: String,
    /// Redacted, bounded operator context.
    pub redacted_context: SwarmCapacityEvidenceContext,
    /// Stable hash of the normalized controller/config snapshot.
    pub config_snapshot_hash: String,
    /// Embedded certificate required for deterministic replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity_certificate: Option<SwarmCapacityCertificate>,
    /// Embedded tail-risk report required for deterministic replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail_risk_report: Option<SwarmTailRiskReport>,
}

impl SwarmCapacityEvidenceRecord {
    /// Compile one replayable capacity decision record.
    #[must_use]
    pub fn from_evidence(
        timestamp_ms: u64,
        certificate: SwarmCapacityCertificate,
        report: SwarmTailRiskReport,
        config_snapshot: &SwarmCapacityEvidenceConfigSnapshot,
        redacted_context: SwarmCapacityEvidenceContext,
    ) -> Self {
        let certificate_hash = stable_json_hash(&certificate);
        let tail_risk_report_hash = stable_json_hash(&report);
        let config_snapshot_hash = config_snapshot.hash();
        let redacted_context_hash = stable_json_hash(&redacted_context);
        let decision = deterministic_capacity_decision(&certificate, &report);
        let controller_inputs = SwarmCapacityControllerInputs::from_evidence(&certificate, &report);
        let record_id = swarm_capacity_evidence_record_id(
            timestamp_ms,
            &certificate_hash,
            &tail_risk_report_hash,
            &config_snapshot_hash,
            &redacted_context_hash,
            decision.action,
            &decision.reason_code,
        );

        Self {
            schema_version: SWARM_CAPACITY_EVIDENCE_SCHEMA_VERSION,
            record_id,
            timestamp_ms,
            workload_class: certificate.workload_class,
            pane_scale: report.pane_scale,
            certificate_hash,
            monitor_status: report.status,
            controller_inputs,
            action: decision.action,
            reason_code: decision.reason_code,
            redacted_context,
            config_snapshot_hash,
            capacity_certificate: Some(certificate),
            tail_risk_report: Some(report),
        }
    }

    /// Replay a record without executing pane actions.
    #[must_use]
    pub fn replay(&self) -> SwarmCapacityReplayResult {
        let mut missing_artifacts = Vec::new();
        if self.capacity_certificate.is_none() {
            missing_artifacts.push(SwarmCapacityEvidenceArtifactKind::CapacityCertificate);
        }
        if self.tail_risk_report.is_none() {
            missing_artifacts.push(SwarmCapacityEvidenceArtifactKind::TailRiskReport);
        }
        if !missing_artifacts.is_empty() {
            return SwarmCapacityReplayResult {
                status: SwarmCapacityReplayStatus::IncompleteEvidence,
                record_id: self.record_id.clone(),
                expected_action: self.action,
                expected_reason_code: self.reason_code.clone(),
                recomputed_action: None,
                recomputed_reason_code: None,
                missing_artifacts,
                side_effects_executed: false,
            };
        }

        let certificate = self
            .capacity_certificate
            .as_ref()
            .expect("checked capacity_certificate");
        let report = self
            .tail_risk_report
            .as_ref()
            .expect("checked tail_risk_report");
        let recomputed = deterministic_capacity_decision(certificate, report);
        let recomputed_hash = stable_json_hash(certificate);
        let report_hash = stable_json_hash(report);
        let context_hash = stable_json_hash(&self.redacted_context);
        let expected_record_id = swarm_capacity_evidence_record_id(
            self.timestamp_ms,
            &self.certificate_hash,
            &report_hash,
            &self.config_snapshot_hash,
            &context_hash,
            self.action,
            &self.reason_code,
        );
        let matched = recomputed.action == self.action
            && recomputed.reason_code == self.reason_code
            && recomputed_hash == self.certificate_hash
            && report.status == self.monitor_status
            && expected_record_id == self.record_id;

        SwarmCapacityReplayResult {
            status: if matched {
                SwarmCapacityReplayStatus::Matched
            } else {
                SwarmCapacityReplayStatus::Diverged
            },
            record_id: self.record_id.clone(),
            expected_action: self.action,
            expected_reason_code: self.reason_code.clone(),
            recomputed_action: Some(recomputed.action),
            recomputed_reason_code: Some(recomputed.reason_code),
            missing_artifacts: Vec::new(),
            side_effects_executed: false,
        }
    }
}

fn swarm_capacity_evidence_record_id(
    timestamp_ms: u64,
    certificate_hash: &str,
    tail_risk_report_hash: &str,
    config_snapshot_hash: &str,
    redacted_context_hash: &str,
    action: SwarmCapacityDecisionAction,
    reason_code: &str,
) -> String {
    stable_json_hash(&(
        SWARM_CAPACITY_EVIDENCE_SCHEMA_VERSION,
        timestamp_ms,
        certificate_hash,
        tail_risk_report_hash,
        config_snapshot_hash,
        redacted_context_hash,
        action.as_str(),
        reason_code,
    ))
}

/// Missing or present artifact kind in a capacity evidence bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityEvidenceArtifactKind {
    /// Redacted environment metadata (`env.json`).
    EnvJson,
    /// Bundle manifest (`manifest.json`).
    ManifestJson,
    /// Reproducibility lock evidence (`repro.lock` or equivalent).
    ReproLock,
    /// Capacity certificate artifact.
    CapacityCertificate,
    /// Tail-risk monitor report artifact.
    TailRiskReport,
    /// Telemetry excerpt artifact.
    TelemetryExcerpt,
    /// Decision ledger artifact.
    DecisionLedger,
}

/// Replay status for one evidence record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmCapacityReplayStatus {
    /// Recomputed decision matches the recorded action and reason.
    Matched,
    /// Required artifacts are present, but recomputation diverged.
    Diverged,
    /// Required artifacts are missing.
    IncompleteEvidence,
}

/// Side-effect-free replay result for one capacity decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityReplayResult {
    /// Replay status.
    pub status: SwarmCapacityReplayStatus,
    /// Record identifier that was replayed.
    pub record_id: String,
    /// Recorded action.
    pub expected_action: SwarmCapacityDecisionAction,
    /// Recorded reason code.
    pub expected_reason_code: String,
    /// Recomputed action, if enough evidence was present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recomputed_action: Option<SwarmCapacityDecisionAction>,
    /// Recomputed reason code, if enough evidence was present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recomputed_reason_code: Option<String>,
    /// Missing artifacts for incomplete records.
    pub missing_artifacts: Vec<SwarmCapacityEvidenceArtifactKind>,
    /// Replay is analysis-only and must never execute pane actions.
    pub side_effects_executed: bool,
}

/// Bounded, serializable capacity decision ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCapacityEvidenceLedger {
    /// Retention and size bounds.
    pub config: SwarmCapacityEvidenceLedgerConfig,
    /// Retained decision records in deterministic timestamp order.
    pub records: Vec<SwarmCapacityEvidenceRecord>,
}

impl SwarmCapacityEvidenceLedger {
    /// Create an empty ledger with explicit bounds.
    #[must_use]
    pub fn new(config: SwarmCapacityEvidenceLedgerConfig) -> Self {
        Self {
            config,
            records: Vec::new(),
        }
    }

    /// Create an empty ledger with default bounds.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(SwarmCapacityEvidenceLedgerConfig::default())
    }

    /// Append a decision and apply retention immediately.
    pub fn record_decision(
        &mut self,
        timestamp_ms: u64,
        certificate: SwarmCapacityCertificate,
        report: SwarmTailRiskReport,
        config_snapshot: &SwarmCapacityEvidenceConfigSnapshot,
        redacted_context: SwarmCapacityEvidenceContext,
    ) -> &SwarmCapacityEvidenceRecord {
        let record = SwarmCapacityEvidenceRecord::from_evidence(
            timestamp_ms,
            certificate,
            report,
            config_snapshot,
            redacted_context,
        );
        let record_id = record.record_id.clone();
        self.records.push(record);
        self.apply_retention(timestamp_ms);
        self.records
            .iter()
            .find(|entry| entry.record_id == record_id)
            .expect("newly inserted record should survive normalized retention")
    }

    /// Replay every retained record without side effects.
    #[must_use]
    pub fn replay_all(&self) -> Vec<SwarmCapacityReplayResult> {
        self.records
            .iter()
            .map(SwarmCapacityEvidenceRecord::replay)
            .collect()
    }

    /// Stable hash of the retained decision ledger.
    #[must_use]
    pub fn ledger_hash(&self) -> String {
        stable_json_hash(&self.records)
    }

    /// Build a redaction-preserving incident/repro bundle manifest.
    #[must_use]
    pub fn export_bundle(
        &self,
        created_at_ms: u64,
        env: SwarmCapacityEvidenceContext,
        repro_lock_hash: Option<String>,
        capacity_certificate: Option<SwarmCapacityCertificate>,
        telemetry_excerpt: Option<SwarmCapacityTelemetrySnapshot>,
    ) -> SwarmCapacityEvidenceBundle {
        let mut incomplete_artifacts = Vec::new();
        if !env.redacted {
            incomplete_artifacts.push(SwarmCapacityEvidenceArtifactKind::EnvJson);
        }
        if repro_lock_hash.is_none() {
            incomplete_artifacts.push(SwarmCapacityEvidenceArtifactKind::ReproLock);
        }
        if capacity_certificate.is_none() {
            incomplete_artifacts.push(SwarmCapacityEvidenceArtifactKind::CapacityCertificate);
        }
        if self.records.is_empty() {
            incomplete_artifacts.push(SwarmCapacityEvidenceArtifactKind::DecisionLedger);
        }
        if telemetry_excerpt.is_none() {
            incomplete_artifacts.push(SwarmCapacityEvidenceArtifactKind::TelemetryExcerpt);
        }

        let manifest = SwarmCapacityEvidenceManifest {
            schema_version: SWARM_CAPACITY_EVIDENCE_SCHEMA_VERSION,
            created_at_ms,
            record_count: self.records.len(),
            ledger_hash: self.ledger_hash(),
            env_hash: stable_json_hash(&env),
            repro_lock_hash: repro_lock_hash.clone(),
            certificate_hash: capacity_certificate.as_ref().map(stable_json_hash),
            telemetry_excerpt_hash: telemetry_excerpt.as_ref().map(stable_json_hash),
        };
        let bundle_hash = stable_json_hash(&(
            &manifest,
            &env,
            &repro_lock_hash,
            &capacity_certificate,
            &telemetry_excerpt,
            &self.records,
            &incomplete_artifacts,
        ));

        SwarmCapacityEvidenceBundle {
            schema_version: SWARM_CAPACITY_EVIDENCE_SCHEMA_VERSION,
            manifest,
            env,
            repro_lock_hash,
            capacity_certificate,
            telemetry_excerpt,
            decision_ledger: self.records.clone(),
            incomplete_artifacts,
            bundle_hash,
        }
    }

    fn apply_retention(&mut self, now_ms: u64) {
        self.records.sort_by(|left, right| {
            left.timestamp_ms
                .cmp(&right.timestamp_ms)
                .then_with(|| left.record_id.cmp(&right.record_id))
        });

        if let Some(window_ms) = self.config.retention_window_ms()
            && let Some(cutoff) = now_ms.checked_sub(window_ms)
        {
            self.records.retain(|entry| entry.timestamp_ms >= cutoff);
        }

        let max_records = self.config.normalized_max_records();
        if self.records.len() > max_records {
            let drain_count = self.records.len() - max_records;
            self.records.drain(0..drain_count);
        }
    }
}

/// Manifest fields for an exported capacity evidence bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmCapacityEvidenceManifest {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Bundle creation timestamp in milliseconds.
    pub created_at_ms: u64,
    /// Retained decision record count.
    pub record_count: usize,
    /// Stable hash of the decision ledger.
    pub ledger_hash: String,
    /// Stable hash of the redacted environment metadata.
    pub env_hash: String,
    /// Stable hash of lock evidence, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repro_lock_hash: Option<String>,
    /// Stable hash of the exported capacity certificate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certificate_hash: Option<String>,
    /// Stable hash of the telemetry excerpt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry_excerpt_hash: Option<String>,
}

/// Exportable capacity incident/repro bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCapacityEvidenceBundle {
    /// Evidence schema version.
    pub schema_version: u32,
    /// Manifest metadata equivalent to `manifest.json`.
    pub manifest: SwarmCapacityEvidenceManifest,
    /// Redacted environment metadata equivalent to `env.json`.
    pub env: SwarmCapacityEvidenceContext,
    /// Reproducibility lock hash when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repro_lock_hash: Option<String>,
    /// Capacity certificate artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity_certificate: Option<SwarmCapacityCertificate>,
    /// Telemetry excerpt artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry_excerpt: Option<SwarmCapacityTelemetrySnapshot>,
    /// Retained decision ledger artifact.
    pub decision_ledger: Vec<SwarmCapacityEvidenceRecord>,
    /// Required artifacts that were missing or incomplete.
    pub incomplete_artifacts: Vec<SwarmCapacityEvidenceArtifactKind>,
    /// Stable hash of the bundle payload excluding this field.
    pub bundle_hash: String,
}

impl SwarmCapacityEvidenceBundle {
    /// Whether every required bundle artifact was present.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.incomplete_artifacts.is_empty()
    }
}

impl SwarmCapacityTelemetrySnapshot {
    /// Compile a deterministic queueing capacity certificate.
    #[must_use]
    pub fn capacity_certificate(
        &self,
        config: SwarmCapacityCertificateConfig,
    ) -> SwarmCapacityCertificate {
        let selected_stages = config.normalized_selected_stages();
        let stages = selected_stages
            .into_iter()
            .filter_map(|stage| self.stage(stage))
            .map(|stage| compile_stage_certificate(stage, self.enabled, &config))
            .collect::<Vec<_>>();

        let status = aggregate_capacity_status(&stages);
        let bottleneck = stages
            .iter()
            .filter_map(|stage| stage.utilization.map(|util| (stage.stage, util)))
            .max_by(|(left_stage, left_util), (right_stage, right_util)| {
                left_util
                    .partial_cmp(right_util)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| right_stage.index().cmp(&left_stage.index()))
            });
        let assumption_flags = unique_capacity_flags(&stages);
        let observation_window_secs = config.observation_window_secs().unwrap_or_default();

        SwarmCapacityCertificate {
            workload_class: config.workload_class,
            machine_class: config.machine_class,
            pane_scale: config.pane_scale,
            status,
            reason_code: capacity_reason_code("capacity", status, &assumption_flags),
            observation_window_secs,
            stages,
            bottleneck_stage: bottleneck.map(|(stage, _)| stage),
            bottleneck_utilization: bottleneck.map(|(_, utilization)| utilization),
            assumption_flags,
        }
    }

    fn stage(&self, stage: SwarmCapacityStage) -> Option<&SwarmCapacityStageSnapshot> {
        self.stages.iter().find(|entry| entry.stage == stage)
    }
}

impl SwarmCapacityCertificate {
    /// Compare this baseline certificate against a live certificate.
    #[must_use]
    pub fn tail_risk_report(
        &self,
        live: &SwarmCapacityCertificate,
        config: SwarmTailRiskMonitorConfig,
    ) -> SwarmTailRiskReport {
        let alpha = config.alpha();
        let stages = config
            .normalized_selected_stages()
            .into_iter()
            .map(|stage| compile_tail_risk_stage(stage, self, live, &config))
            .collect::<Vec<_>>();
        let evidence = stages
            .iter()
            .flat_map(|stage| stage.evidence.iter().cloned())
            .collect::<Vec<_>>();
        let status = aggregate_tail_risk_status(&stages);

        SwarmTailRiskReport {
            workload_class: self.workload_class,
            pane_scale: live.pane_scale,
            alpha,
            confidence: 1.0 - alpha,
            status,
            reason_code: tail_risk_reason_code("capacity", status, &evidence),
            stages,
            evidence,
        }
    }

    fn stage_certificate(
        &self,
        stage: SwarmCapacityStage,
    ) -> Option<&SwarmCapacityStageCertificate> {
        self.stages.iter().find(|entry| entry.stage == stage)
    }
}

fn compile_tail_risk_stage(
    stage: SwarmCapacityStage,
    baseline: &SwarmCapacityCertificate,
    live: &SwarmCapacityCertificate,
    config: &SwarmTailRiskMonitorConfig,
) -> SwarmTailRiskStageReport {
    let stage_name = stage.as_str().to_string();
    let mut evidence = Vec::new();

    if config.baseline_stale() {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::BaselineStale,
            SwarmTailRiskStatus::StaleBaseline,
            config.baseline_age_secs.map(|age| age as f64),
            config.stale_after_secs.map(|limit| limit as f64),
            "baseline age exceeds freshness window",
        ));
        return tail_risk_stage_report(stage, stage_name, evidence);
    }

    let Some(baseline_stage) = baseline.stage_certificate(stage) else {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::MissingBaselineStage,
            SwarmTailRiskStatus::Unknown,
            None,
            None,
            "baseline certificate is missing selected stage",
        ));
        return tail_risk_stage_report(stage, stage_name, evidence);
    };

    let Some(live_stage) = live.stage_certificate(stage) else {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::MissingLiveStage,
            SwarmTailRiskStatus::Unknown,
            None,
            None,
            "live certificate is missing selected stage",
        ));
        return tail_risk_stage_report(stage, stage_name, evidence);
    };

    if baseline_stage.status != SwarmCapacityCertificateStatus::Safe {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::BaselineNotSafe,
            SwarmTailRiskStatus::Unknown,
            None,
            None,
            "baseline certificate did not certify safe",
        ));
    }

    if baseline_stage.service_time_ms.count < config.min_baseline_samples_per_stage {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::InsufficientBaselineSamples,
            SwarmTailRiskStatus::Unknown,
            Some(baseline_stage.service_time_ms.count as f64),
            Some(config.min_baseline_samples_per_stage as f64),
            "baseline retained too few service samples",
        ));
    }

    if live_stage.service_time_ms.count < config.min_live_samples_per_stage {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::InsufficientLiveSamples,
            SwarmTailRiskStatus::Unknown,
            Some(live_stage.service_time_ms.count as f64),
            Some(config.min_live_samples_per_stage as f64),
            "live telemetry retained too few service samples",
        ));
    }

    add_tail_latency_evidence(stage, baseline_stage, live_stage, config, &mut evidence);
    add_tail_queue_evidence(stage, baseline_stage, live_stage, config, &mut evidence);
    add_tail_retry_evidence(stage, baseline_stage, live_stage, config, &mut evidence);
    add_tail_utilization_evidence(stage, live_stage, config, &mut evidence);
    add_tail_terminal_evidence(stage, live_stage, config, &mut evidence);

    if evidence.is_empty() {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::WithinBudget,
            SwarmTailRiskStatus::Green,
            None,
            None,
            "live telemetry remains within calibrated tail budgets",
        ));
    }

    tail_risk_stage_report(stage, stage_name, evidence)
}

fn add_tail_latency_evidence(
    stage: SwarmCapacityStage,
    baseline: &SwarmCapacityStageCertificate,
    live: &SwarmCapacityStageCertificate,
    config: &SwarmTailRiskMonitorConfig,
    evidence: &mut Vec<SwarmTailRiskEvidence>,
) {
    let Some(baseline_p99) = baseline
        .modeled_latency
        .p99_ms
        .or(baseline.service_time_ms.p99)
    else {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::P99Residual,
            SwarmTailRiskStatus::Unknown,
            None,
            None,
            "baseline lacks p99 budget",
        ));
        return;
    };
    let Some(live_p99) = live.service_time_ms.p99 else {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::P99Residual,
            SwarmTailRiskStatus::Unknown,
            None,
            Some(baseline_p99),
            "live telemetry lacks p99 observation",
        ));
        return;
    };

    let slack = finite_sample_tail_slack(
        baseline.service_time_ms.count,
        config.alpha(),
        config.max_finite_sample_slack_ratio(),
    );
    let Some(finite_sample_slack) = slack else {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::InsufficientBaselineSamples,
            SwarmTailRiskStatus::Unknown,
            Some(baseline.service_time_ms.count as f64),
            Some(config.min_baseline_samples_per_stage as f64),
            "baseline sample count cannot support finite-sample p99 budget",
        ));
        return;
    };

    let watch_budget = baseline_p99 * (1.0 + finite_sample_slack + config.watch_slack_ratio());
    let violation_budget = watch_budget * (1.0 + config.violation_slack_ratio());
    if live_p99 > violation_budget {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::P99Residual,
            SwarmTailRiskStatus::Violated,
            Some(live_p99),
            Some(violation_budget),
            "live p99 exceeds violation tail budget",
        ));
    } else if live_p99 > watch_budget {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::P99Residual,
            SwarmTailRiskStatus::Watch,
            Some(live_p99),
            Some(watch_budget),
            "live p99 exceeds watch tail budget",
        ));
    }

    if let (Some(baseline_p999), Some(live_p999)) = (
        baseline.modeled_latency.p999_ms,
        live.modeled_latency.p999_ms,
    ) {
        let p999_watch_budget =
            baseline_p999 * (1.0 + finite_sample_slack + config.watch_slack_ratio());
        let p999_violation_budget = p999_watch_budget * (1.0 + config.violation_slack_ratio());
        if live_p999 > p999_violation_budget {
            evidence.push(tail_risk_evidence(
                stage,
                SwarmTailRiskSignal::P999Residual,
                SwarmTailRiskStatus::Violated,
                Some(live_p999),
                Some(p999_violation_budget),
                "live modeled p999 exceeds violation tail budget",
            ));
        } else if live_p999 > p999_watch_budget {
            evidence.push(tail_risk_evidence(
                stage,
                SwarmTailRiskSignal::P999Residual,
                SwarmTailRiskStatus::Watch,
                Some(live_p999),
                Some(p999_watch_budget),
                "live modeled p999 exceeds watch tail budget",
            ));
        }
    }
}

fn add_tail_queue_evidence(
    stage: SwarmCapacityStage,
    baseline: &SwarmCapacityStageCertificate,
    live: &SwarmCapacityStageCertificate,
    config: &SwarmTailRiskMonitorConfig,
    evidence: &mut Vec<SwarmTailRiskEvidence>,
) {
    let baseline_queue_p99 = baseline.queue_depth.p99.unwrap_or(0.0).max(1.0);
    let Some(live_queue_p99) = live.queue_depth.p99 else {
        return;
    };
    let watch_budget = baseline_queue_p99 * config.queue_watch_multiplier();
    let violation_budget = baseline_queue_p99 * config.queue_violation_multiplier();
    if live_queue_p99 > violation_budget {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::QueueP99ExceedsBaseline,
            SwarmTailRiskStatus::Violated,
            Some(live_queue_p99),
            Some(violation_budget),
            "live queue p99 exceeds violation budget",
        ));
    } else if live_queue_p99 > watch_budget {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::QueueP99ExceedsBaseline,
            SwarmTailRiskStatus::Watch,
            Some(live_queue_p99),
            Some(watch_budget),
            "live queue p99 exceeds watch budget",
        ));
    }
}

fn add_tail_retry_evidence(
    stage: SwarmCapacityStage,
    baseline: &SwarmCapacityStageCertificate,
    live: &SwarmCapacityStageCertificate,
    config: &SwarmTailRiskMonitorConfig,
    evidence: &mut Vec<SwarmTailRiskEvidence>,
) {
    let Some(live_retry_p99) = live.retry_latency_ms.p99 else {
        return;
    };
    let baseline_retry_p99 = baseline
        .retry_latency_ms
        .p99
        .unwrap_or_else(|| config.retry_floor_ms())
        .max(config.retry_floor_ms());
    let watch_budget = baseline_retry_p99 * config.retry_watch_multiplier();
    let violation_budget = baseline_retry_p99 * config.retry_violation_multiplier();
    if live_retry_p99 > violation_budget {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::RetryStorm,
            SwarmTailRiskStatus::Violated,
            Some(live_retry_p99),
            Some(violation_budget),
            "live retry p99 exceeds violation budget",
        ));
    } else if live_retry_p99 > watch_budget {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::RetryStorm,
            SwarmTailRiskStatus::Watch,
            Some(live_retry_p99),
            Some(watch_budget),
            "live retry p99 exceeds watch budget",
        ));
    }
}

fn add_tail_utilization_evidence(
    stage: SwarmCapacityStage,
    live: &SwarmCapacityStageCertificate,
    config: &SwarmTailRiskMonitorConfig,
    evidence: &mut Vec<SwarmTailRiskEvidence>,
) {
    let Some(utilization) = live.utilization else {
        return;
    };
    if utilization >= config.saturation_violation_utilization() {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::Saturation,
            SwarmTailRiskStatus::Violated,
            Some(utilization),
            Some(config.saturation_violation_utilization()),
            "live utilization exceeds violation budget",
        ));
    } else if utilization >= config.saturation_watch_utilization() {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::Saturation,
            SwarmTailRiskStatus::Watch,
            Some(utilization),
            Some(config.saturation_watch_utilization()),
            "live utilization exceeds watch budget",
        ));
    }
}

fn add_tail_terminal_evidence(
    stage: SwarmCapacityStage,
    live: &SwarmCapacityStageCertificate,
    config: &SwarmTailRiskMonitorConfig,
    evidence: &mut Vec<SwarmTailRiskEvidence>,
) {
    if let Some(rate) = live.terminal_error_rate {
        if rate >= config.terminal_error_violation_rate() {
            evidence.push(tail_risk_evidence(
                stage,
                SwarmTailRiskSignal::TerminalErrorRate,
                SwarmTailRiskStatus::Violated,
                Some(rate),
                Some(config.terminal_error_violation_rate()),
                "live terminal error rate exceeds violation budget",
            ));
        } else if rate >= config.terminal_error_watch_rate() {
            evidence.push(tail_risk_evidence(
                stage,
                SwarmTailRiskSignal::TerminalErrorRate,
                SwarmTailRiskStatus::Watch,
                Some(rate),
                Some(config.terminal_error_watch_rate()),
                "live terminal error rate exceeds watch budget",
            ));
        }
    }

    let terminal_count = live
        .completions
        .saturating_add(live.cancellations)
        .saturating_add(live.timeouts)
        .saturating_add(live.errors);
    if terminal_count == 0 {
        return;
    }
    let cancellation_rate = live.cancellations as f64 / terminal_count as f64;
    if cancellation_rate >= config.cancellation_violation_rate() {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::CancellationSpike,
            SwarmTailRiskStatus::Violated,
            Some(cancellation_rate),
            Some(config.cancellation_violation_rate()),
            "live cancellation rate exceeds violation budget",
        ));
    } else if cancellation_rate >= config.cancellation_watch_rate() {
        evidence.push(tail_risk_evidence(
            stage,
            SwarmTailRiskSignal::CancellationSpike,
            SwarmTailRiskStatus::Watch,
            Some(cancellation_rate),
            Some(config.cancellation_watch_rate()),
            "live cancellation rate exceeds watch budget",
        ));
    }
}

fn finite_sample_tail_slack(sample_count: u64, alpha: f64, max_slack_ratio: f64) -> Option<f64> {
    if sample_count == 0 || !alpha.is_finite() || !(0.0..1.0).contains(&alpha) {
        return None;
    }
    let sample_count = sample_count as f64;
    let slack = (1.0 / ((sample_count + 1.0) * alpha)).sqrt();
    slack.is_finite().then_some(slack.min(max_slack_ratio))
}

fn tail_risk_evidence(
    stage: SwarmCapacityStage,
    signal: SwarmTailRiskSignal,
    status: SwarmTailRiskStatus,
    observed: Option<f64>,
    budget: Option<f64>,
    detail: &str,
) -> SwarmTailRiskEvidence {
    SwarmTailRiskEvidence {
        stage,
        stage_name: stage.as_str().to_string(),
        signal,
        status,
        observed,
        budget,
        detail: detail.to_string(),
    }
}

fn tail_risk_stage_report(
    stage: SwarmCapacityStage,
    stage_name: String,
    evidence: Vec<SwarmTailRiskEvidence>,
) -> SwarmTailRiskStageReport {
    let status = evidence
        .iter()
        .map(|entry| entry.status)
        .max_by_key(|status| status.rank())
        .unwrap_or(SwarmTailRiskStatus::Green);
    SwarmTailRiskStageReport {
        stage,
        stage_name,
        status,
        reason_code: tail_risk_reason_code(stage.as_str(), status, &evidence),
        evidence,
    }
}

fn aggregate_tail_risk_status(stages: &[SwarmTailRiskStageReport]) -> SwarmTailRiskStatus {
    stages
        .iter()
        .map(|stage| stage.status)
        .max_by_key(|status| status.rank())
        .unwrap_or(SwarmTailRiskStatus::Unknown)
}

fn tail_risk_reason_code(
    prefix: &str,
    status: SwarmTailRiskStatus,
    evidence: &[SwarmTailRiskEvidence],
) -> String {
    let suffix = evidence
        .iter()
        .filter(|entry| entry.status == status)
        .map(|entry| entry.signal.as_str())
        .next()
        .unwrap_or_else(|| status.as_str());
    format!("{prefix}.tail_risk.{suffix}")
}

fn compile_stage_certificate(
    stage: &SwarmCapacityStageSnapshot,
    telemetry_enabled: bool,
    config: &SwarmCapacityCertificateConfig,
) -> SwarmCapacityStageCertificate {
    let mut flags = Vec::new();
    if !telemetry_enabled {
        flags.push(SwarmCapacityAssumptionFlag::TelemetryDisabled);
    }
    if config.observation_window_secs().is_none() {
        flags.push(SwarmCapacityAssumptionFlag::InvalidObservationWindow);
    }
    if config.baseline_stale() {
        flags.push(SwarmCapacityAssumptionFlag::StaleBaseline);
    }

    let sample_count = stage.service_time_ms.count;
    if sample_count < config.min_samples_per_stage {
        flags.push(SwarmCapacityAssumptionFlag::InsufficientSamples);
    }
    if stage.completions == 0 {
        flags.push(SwarmCapacityAssumptionFlag::NoCompletions);
    }
    if required_service_quantiles_missing(&stage.service_time_ms) {
        flags.push(SwarmCapacityAssumptionFlag::MissingServiceQuantiles);
    }

    let terminal_count = stage_terminal_count(stage);
    if terminal_count > stage.arrivals {
        flags.push(SwarmCapacityAssumptionFlag::TerminalExceedsArrivals);
    }

    let service_mean_ms = stage.service_time_ms.mean;
    let observed_p99_ms = stage.service_time_ms.p99;
    if let (Some(mean), Some(p99)) = (service_mean_ms, observed_p99_ms)
        && mean > 0.0
        && p99 / mean > config.heavy_tail_ratio()
    {
        flags.push(SwarmCapacityAssumptionFlag::HeavyTailedService);
    }

    let arrival_rate_per_s = config
        .observation_window_secs()
        .map(|window| stage.arrivals as f64 / window);
    let service_rate_per_s = service_mean_ms
        .and_then(|mean_ms| (mean_ms.is_finite() && mean_ms > 0.0).then_some(1000.0 / mean_ms));
    let utilization = match (arrival_rate_per_s, service_rate_per_s) {
        (Some(arrival_rate), Some(service_rate)) if service_rate > 0.0 => {
            Some(arrival_rate / service_rate)
        }
        _ => None,
    };

    if let Some(rho) = utilization {
        if rho >= config.unsafe_utilization_threshold() {
            flags.push(SwarmCapacityAssumptionFlag::SaturatedUtilization);
        } else if rho > config.safe_utilization_threshold() {
            flags.push(SwarmCapacityAssumptionFlag::UtilizationAboveSafeThreshold);
        }
    }

    let terminal_error_rate = (terminal_count > 0).then(|| {
        stage
            .cancellations
            .saturating_add(stage.timeouts)
            .saturating_add(stage.errors) as f64
            / terminal_count as f64
    });
    if terminal_error_rate.is_some_and(|rate| rate > config.max_terminal_error_rate()) {
        flags.push(SwarmCapacityAssumptionFlag::ErrorBudgetExceeded);
    }

    if little_law_inconsistent(stage, arrival_rate_per_s, service_mean_ms, config) {
        flags.push(SwarmCapacityAssumptionFlag::LittleLawInconsistent);
    }

    let modeled_latency = modeled_latency(&stage.service_time_ms, utilization);
    if model_underestimates_p99(&modeled_latency, observed_p99_ms, config) {
        flags.push(SwarmCapacityAssumptionFlag::ModelResidualGtTolerance);
    }

    let status = stage_capacity_status(&flags);
    SwarmCapacityStageCertificate {
        stage: stage.stage,
        stage_name: stage.stage_name.clone(),
        status,
        reason_code: capacity_reason_code(stage.stage.as_str(), status, &flags),
        arrival_rate_per_s,
        service_rate_per_s,
        utilization,
        terminal_error_rate,
        service_time_ms: stage.service_time_ms.clone(),
        queue_depth: stage.queue_depth.clone(),
        wait_time_ms: stage.wait_time_ms.clone(),
        retry_latency_ms: stage.retry_latency_ms.clone(),
        modeled_latency,
        assumption_flags: flags,
        completions: stage.completions,
        cancellations: stage.cancellations,
        timeouts: stage.timeouts,
        errors: stage.errors,
    }
}

fn stage_terminal_count(stage: &SwarmCapacityStageSnapshot) -> u64 {
    [
        stage.completions,
        stage.cancellations,
        stage.timeouts,
        stage.errors,
    ]
    .into_iter()
    .fold(0u64, u64::saturating_add)
}

fn required_service_quantiles_missing(summary: &HistogramSummary) -> bool {
    summary.mean.is_none()
        || summary.p50.is_none()
        || summary.p95.is_none()
        || summary.p99.is_none()
}

fn modeled_latency(
    service_time_ms: &HistogramSummary,
    utilization: Option<f64>,
) -> SwarmCapacityLatencyModel {
    let rho = utilization.unwrap_or(0.0).clamp(0.0, 0.98);
    let multiplier = 1.0 / (1.0 - rho).max(0.02);

    SwarmCapacityLatencyModel {
        p50_ms: scale_quantile(service_time_ms.p50, multiplier),
        p95_ms: scale_quantile(service_time_ms.p95, multiplier),
        p99_ms: scale_quantile(service_time_ms.p99, multiplier),
        p999_ms: scale_quantile(service_time_ms.p99, multiplier * 1.5),
    }
}

fn scale_quantile(value: Option<f64>, multiplier: f64) -> Option<f64> {
    value.and_then(|quantile| {
        (quantile.is_finite() && multiplier.is_finite()).then_some(quantile * multiplier)
    })
}

fn model_underestimates_p99(
    modeled_latency: &SwarmCapacityLatencyModel,
    observed_p99_ms: Option<f64>,
    config: &SwarmCapacityCertificateConfig,
) -> bool {
    match (modeled_latency.p99_ms, observed_p99_ms) {
        (Some(modeled), Some(observed)) if observed > 0.0 && modeled < observed => {
            (observed - modeled) / observed > config.model_residual_tolerance()
        }
        _ => false,
    }
}

fn little_law_inconsistent(
    stage: &SwarmCapacityStageSnapshot,
    arrival_rate_per_s: Option<f64>,
    service_mean_ms: Option<f64>,
    config: &SwarmCapacityCertificateConfig,
) -> bool {
    let Some(queue_mean) = stage.queue_depth.mean else {
        return false;
    };
    if queue_mean < 1.0 {
        return false;
    }

    let (Some(arrival_rate), Some(service_mean_ms)) = (arrival_rate_per_s, service_mean_ms) else {
        return false;
    };
    if service_mean_ms <= 0.0 {
        return false;
    }

    let predicted_l = arrival_rate * (service_mean_ms / 1000.0);
    if predicted_l <= 0.0 {
        return false;
    }

    ((queue_mean - predicted_l).abs() / queue_mean.max(predicted_l))
        > config.model_residual_tolerance()
}

fn stage_capacity_status(flags: &[SwarmCapacityAssumptionFlag]) -> SwarmCapacityCertificateStatus {
    if flags.iter().any(|flag| flag.is_unknown_fatal()) {
        SwarmCapacityCertificateStatus::Unknown
    } else if flags.iter().any(|flag| flag.is_unsafe()) {
        SwarmCapacityCertificateStatus::Unsafe
    } else {
        SwarmCapacityCertificateStatus::Safe
    }
}

fn aggregate_capacity_status(
    stages: &[SwarmCapacityStageCertificate],
) -> SwarmCapacityCertificateStatus {
    if stages
        .iter()
        .any(|stage| stage.status == SwarmCapacityCertificateStatus::Unknown)
    {
        SwarmCapacityCertificateStatus::Unknown
    } else if stages
        .iter()
        .any(|stage| stage.status == SwarmCapacityCertificateStatus::Unsafe)
    {
        SwarmCapacityCertificateStatus::Unsafe
    } else {
        SwarmCapacityCertificateStatus::Safe
    }
}

fn capacity_reason_code(
    prefix: &str,
    status: SwarmCapacityCertificateStatus,
    flags: &[SwarmCapacityAssumptionFlag],
) -> String {
    let suffix = flags
        .iter()
        .find(|flag| flag.is_unknown_fatal())
        .or_else(|| flags.iter().find(|flag| flag.is_unsafe()))
        .map_or_else(|| status.as_str(), |flag| flag.as_str());
    format!("{prefix}.capacity.{suffix}")
}

fn unique_capacity_flags(
    stages: &[SwarmCapacityStageCertificate],
) -> Vec<SwarmCapacityAssumptionFlag> {
    let mut flags = Vec::new();
    for flag in stages
        .iter()
        .flat_map(|stage| stage.assumption_flags.iter().copied())
    {
        if !flags.contains(&flag) {
            flags.push(flag);
        }
    }
    flags
}

fn deterministic_capacity_decision(
    certificate: &SwarmCapacityCertificate,
    report: &SwarmTailRiskReport,
) -> SwarmCapacityDecision {
    match certificate.status {
        SwarmCapacityCertificateStatus::Unsafe | SwarmCapacityCertificateStatus::Unknown => {
            return SwarmCapacityDecision {
                action: SwarmCapacityDecisionAction::BlockAdmission,
                reason_code: certificate.reason_code.clone(),
            };
        }
        SwarmCapacityCertificateStatus::Safe => {}
    }

    let action = match report.status {
        SwarmTailRiskStatus::Green => SwarmCapacityDecisionAction::Allow,
        SwarmTailRiskStatus::Watch => SwarmCapacityDecisionAction::ReduceAdmission,
        SwarmTailRiskStatus::Violated
        | SwarmTailRiskStatus::Unknown
        | SwarmTailRiskStatus::StaleBaseline => SwarmCapacityDecisionAction::BlockAdmission,
    };

    SwarmCapacityDecision {
        action,
        reason_code: report.reason_code.clone(),
    }
}

fn stable_json_hash<T: Serialize>(value: &T) -> String {
    let json = serde_json::to_string(value)
        .unwrap_or_else(|err| format!(r#"{{"serialization_error":"{err}"}}"#));
    format!("sha256:{}", stable_hash(&json))
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_string(), false);
    }

    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), true)
}

fn finite_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() { value } else { fallback }
}

#[derive(Debug, Clone)]
struct SwarmCapacityStageMetrics {
    stage: SwarmCapacityStage,
    arrivals: u64,
    completions: u64,
    cancellations: u64,
    timeouts: u64,
    errors: u64,
    failure_counts: SwarmCapacityFailureCounts,
    queue_depth: Histogram,
    service_time_ms: Histogram,
    wait_time_ms: Histogram,
    retry_latency_ms: Histogram,
}

impl SwarmCapacityStageMetrics {
    fn new(stage: SwarmCapacityStage, max_samples: usize) -> Self {
        let prefix = format!("swarm_capacity.{}", stage.as_str());
        Self {
            stage,
            arrivals: 0,
            completions: 0,
            cancellations: 0,
            timeouts: 0,
            errors: 0,
            failure_counts: SwarmCapacityFailureCounts::default(),
            queue_depth: Histogram::new(format!("{prefix}.queue_depth"), max_samples),
            service_time_ms: Histogram::new(format!("{prefix}.service_time_ms"), max_samples),
            wait_time_ms: Histogram::new(format!("{prefix}.wait_time_ms"), max_samples),
            retry_latency_ms: Histogram::new(format!("{prefix}.retry_latency_ms"), max_samples),
        }
    }

    fn record_queue_depth(&mut self, queue_depth: u64) {
        record_u64_histogram_sample(&mut self.queue_depth, queue_depth);
    }

    fn record_service_time_ms(&mut self, service_time_ms: f64) {
        record_nonnegative_histogram_sample(&mut self.service_time_ms, service_time_ms);
    }

    fn record_wait_time_ms(&mut self, wait_time_ms: f64) {
        record_nonnegative_histogram_sample(&mut self.wait_time_ms, wait_time_ms);
    }

    fn record_retry_latency_ms(&mut self, retry_time_ms: f64) {
        record_nonnegative_histogram_sample(&mut self.retry_latency_ms, retry_time_ms);
    }

    fn snapshot(&self) -> SwarmCapacityStageSnapshot {
        SwarmCapacityStageSnapshot {
            stage: self.stage,
            stage_name: self.stage.as_str().to_string(),
            arrivals: self.arrivals,
            completions: self.completions,
            cancellations: self.cancellations,
            timeouts: self.timeouts,
            errors: self.errors,
            failure_counts: self.failure_counts,
            in_flight_estimate: self.in_flight_estimate(),
            queue_depth: self.queue_depth.summary(),
            service_time_ms: self.service_time_ms.summary(),
            wait_time_ms: self.wait_time_ms.summary(),
            retry_latency_ms: self.retry_latency_ms.summary(),
        }
    }

    fn terminal_count(&self) -> u64 {
        [
            self.completions,
            self.cancellations,
            self.timeouts,
            self.errors,
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add)
    }

    fn in_flight_estimate(&self) -> u64 {
        self.arrivals.saturating_sub(self.terminal_count())
    }
}

fn estimated_swarm_capacity_memory_cap_bytes(max_samples_per_stage: usize) -> u64 {
    let sample_bytes = SwarmCapacityStage::COUNT
        .saturating_mul(SWARM_CAPACITY_HISTOGRAMS_PER_STAGE)
        .saturating_mul(max_samples_per_stage)
        .saturating_mul(std::mem::size_of::<f64>());
    let counter_bytes =
        SwarmCapacityStage::COUNT.saturating_mul(std::mem::size_of::<SwarmCapacityStageMetrics>());
    u64::try_from(sample_bytes.saturating_add(counter_bytes)).unwrap_or(u64::MAX)
}

#[allow(clippy::cast_precision_loss)]
fn record_u64_histogram_sample(histogram: &mut Histogram, value: u64) {
    histogram.record(value as f64);
}

fn record_nonnegative_histogram_sample(histogram: &mut Histogram, value: f64) {
    if value >= 0.0 {
        histogram.record(value);
    }
}

// =============================================================================
// Tier transition tracking
// =============================================================================

/// Records a health tier transition with context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierTransitionRecord {
    /// When the transition occurred (epoch ms).
    pub timestamp_ms: u64,
    /// Component that observed the transition.
    pub component: String,
    /// Previous tier.
    pub from: HealthTier,
    /// New tier.
    pub to: HealthTier,
    /// Why the transition occurred.
    pub reason_code: String,
    /// How long the previous tier was held (ms).
    pub duration_in_previous_ms: u64,
}

impl TierTransitionRecord {
    /// Whether this transition is an escalation (tier increased).
    #[must_use]
    pub fn is_escalation(&self) -> bool {
        self.to > self.from
    }

    /// Whether this transition is a recovery (tier decreased).
    #[must_use]
    pub fn is_recovery(&self) -> bool {
        self.to < self.from
    }

    /// Convert to a telemetry event.
    #[must_use]
    pub fn to_event(&self, correlation_id: &str) -> RuntimeTelemetryEvent {
        RuntimeTelemetryEventBuilder::new(&self.component, RuntimeTelemetryKind::TierTransition)
            .tier(self.to)
            .phase(RuntimePhase::Running)
            .reason(&self.reason_code)
            .correlation(correlation_id)
            .timestamp_ms(self.timestamp_ms)
            .detail_str("tier_from", &self.from.to_string())
            .detail_str("tier_to", &self.to.to_string())
            .detail_u64("duration_in_previous_ms", self.duration_in_previous_ms)
            .build()
    }
}

// =============================================================================
// Scope telemetry helpers
// =============================================================================

/// Helper for emitting scope lifecycle telemetry events.
pub struct ScopeTelemetryEmitter {
    component: String,
    scope_id: String,
    correlation_id: String,
}

impl ScopeTelemetryEmitter {
    /// Create a new emitter for a specific scope.
    #[must_use]
    pub fn new(component: &str, scope_id: &str, correlation_id: &str) -> Self {
        Self {
            component: component.to_string(),
            scope_id: scope_id.to_string(),
            correlation_id: correlation_id.to_string(),
        }
    }

    /// Emit a scope created event.
    #[must_use]
    pub fn created(&self, scope_tier: &str) -> RuntimeTelemetryEvent {
        RuntimeTelemetryEventBuilder::new(&self.component, RuntimeTelemetryKind::ScopeCreated)
            .scope_id(&self.scope_id)
            .phase(RuntimePhase::Init)
            .reason("scope.init.created")
            .correlation(&self.correlation_id)
            .detail_str("scope_tier", scope_tier)
            .build()
    }

    /// Emit a scope started event.
    #[must_use]
    pub fn started(&self) -> RuntimeTelemetryEvent {
        RuntimeTelemetryEventBuilder::new(&self.component, RuntimeTelemetryKind::ScopeStarted)
            .scope_id(&self.scope_id)
            .phase(RuntimePhase::Startup)
            .reason("scope.startup.started")
            .correlation(&self.correlation_id)
            .build()
    }

    /// Emit a scope draining event.
    #[must_use]
    pub fn draining(&self, reason: &str) -> RuntimeTelemetryEvent {
        RuntimeTelemetryEventBuilder::new(&self.component, RuntimeTelemetryKind::ScopeDraining)
            .scope_id(&self.scope_id)
            .phase(RuntimePhase::Draining)
            .reason("scope.draining.started")
            .correlation(&self.correlation_id)
            .detail_str("shutdown_reason", reason)
            .build()
    }

    /// Emit a scope finalizing event.
    #[must_use]
    pub fn finalizing(&self, finalizer_count: u64) -> RuntimeTelemetryEvent {
        RuntimeTelemetryEventBuilder::new(&self.component, RuntimeTelemetryKind::ScopeFinalizing)
            .scope_id(&self.scope_id)
            .phase(RuntimePhase::Finalizing)
            .reason("scope.finalizing.started")
            .correlation(&self.correlation_id)
            .detail_u64("finalizer_count", finalizer_count)
            .build()
    }

    /// Emit a scope closed event.
    #[must_use]
    pub fn closed(&self, total_duration_ms: u64) -> RuntimeTelemetryEvent {
        RuntimeTelemetryEventBuilder::new(&self.component, RuntimeTelemetryKind::ScopeClosed)
            .scope_id(&self.scope_id)
            .phase(RuntimePhase::Shutdown)
            .reason("scope.shutdown.closed")
            .correlation(&self.correlation_id)
            .detail_u64("total_duration_ms", total_duration_ms)
            .build()
    }
}

// =============================================================================
// Cancellation telemetry helpers
// =============================================================================

/// Helper for emitting cancellation telemetry events.
pub struct CancellationTelemetryEmitter {
    component: String,
    correlation_id: String,
}

impl CancellationTelemetryEmitter {
    /// Create a new cancellation emitter.
    #[must_use]
    pub fn new(component: &str, correlation_id: &str) -> Self {
        Self {
            component: component.to_string(),
            correlation_id: correlation_id.to_string(),
        }
    }

    /// Emit a cancellation requested event.
    #[must_use]
    pub fn requested(&self, scope_id: &str, reason: &str) -> RuntimeTelemetryEvent {
        RuntimeTelemetryEventBuilder::new(
            &self.component,
            RuntimeTelemetryKind::CancellationRequested,
        )
        .scope_id(scope_id)
        .phase(RuntimePhase::Cancelling)
        .reason("cancellation.requested")
        .correlation(&self.correlation_id)
        .detail_str("shutdown_reason", reason)
        .build()
    }

    /// Emit a cancellation propagated event.
    #[must_use]
    pub fn propagated(&self, parent_id: &str, child_count: u64) -> RuntimeTelemetryEvent {
        RuntimeTelemetryEventBuilder::new(
            &self.component,
            RuntimeTelemetryKind::CancellationPropagated,
        )
        .scope_id(parent_id)
        .phase(RuntimePhase::Cancelling)
        .reason("cancellation.propagated")
        .correlation(&self.correlation_id)
        .detail_u64("child_count", child_count)
        .build()
    }

    /// Emit a grace period expired event.
    #[must_use]
    pub fn grace_expired(&self, scope_id: &str, grace_period_ms: u64) -> RuntimeTelemetryEvent {
        RuntimeTelemetryEventBuilder::new(&self.component, RuntimeTelemetryKind::GracePeriodExpired)
            .scope_id(scope_id)
            .phase(RuntimePhase::Draining)
            .tier(HealthTier::Red)
            .reason("cancellation.draining.grace_expired")
            .correlation(&self.correlation_id)
            .detail_u64("grace_period_ms", grace_period_ms)
            .build()
    }
}

// =============================================================================
// Reason code constants (stable, grep-friendly)
// =============================================================================

/// Stable reason code constants for runtime telemetry.
///
/// Follow the `subsystem.phase.detail` naming convention.
/// These are the canonical reason codes for the unified telemetry schema.
pub mod reason_codes {
    // Scope lifecycle
    pub const SCOPE_INIT_CREATED: &str = "scope.init.created";
    pub const SCOPE_STARTUP_STARTED: &str = "scope.startup.started";
    pub const SCOPE_DRAINING_STARTED: &str = "scope.draining.started";
    pub const SCOPE_FINALIZING_STARTED: &str = "scope.finalizing.started";
    pub const SCOPE_SHUTDOWN_CLOSED: &str = "scope.shutdown.closed";

    // Cancellation
    pub const CANCELLATION_REQUESTED: &str = "cancellation.requested";
    pub const CANCELLATION_PROPAGATED: &str = "cancellation.propagated";
    pub const CANCELLATION_GRACE_EXPIRED: &str = "cancellation.draining.grace_expired";
    pub const CANCELLATION_FINALIZER_OK: &str = "cancellation.finalizing.finalizer_ok";
    pub const CANCELLATION_FINALIZER_FAILED: &str = "cancellation.finalizing.finalizer_failed";

    // Backpressure
    pub const BACKPRESSURE_TIER_GREEN: &str = "backpressure.running.tier_green";
    pub const BACKPRESSURE_TIER_YELLOW: &str = "backpressure.running.tier_yellow";
    pub const BACKPRESSURE_TIER_RED: &str = "backpressure.running.tier_red";
    pub const BACKPRESSURE_TIER_BLACK: &str = "backpressure.running.tier_black";
    pub const BACKPRESSURE_THROTTLE_ON: &str = "backpressure.running.throttle_on";
    pub const BACKPRESSURE_THROTTLE_OFF: &str = "backpressure.running.throttle_off";
    pub const BACKPRESSURE_LOAD_SHEDDING: &str = "backpressure.running.load_shedding";

    // Queue/channel
    pub const QUEUE_DEPTH_OBSERVED: &str = "queue.running.depth_observed";
    pub const QUEUE_CHANNEL_CLOSED: &str = "queue.running.channel_closed";
    pub const QUEUE_PERMIT_EXHAUSTED: &str = "queue.running.permit_exhausted";

    // Error/failure
    pub const ERROR_TRANSIENT: &str = "error.running.transient";
    pub const ERROR_PERMANENT: &str = "error.running.permanent";
    pub const ERROR_PANIC: &str = "error.running.panic_captured";
    pub const ERROR_INVARIANT: &str = "error.running.invariant_violation";
    pub const ERROR_SAFETY: &str = "error.running.safety_policy";

    // Resource
    pub const RESOURCE_OBSERVED: &str = "resource.running.observed";
    pub const RESOURCE_EXHAUSTED: &str = "resource.running.exhausted";

    // Operational
    pub const OPS_SLO_MEASUREMENT: &str = "ops.running.slo_measurement";
    pub const OPS_CONFIG_APPLIED: &str = "ops.running.config_applied";
    pub const OPS_DIAGNOSTIC_EXPORTED: &str = "ops.running.diagnostic_exported";
    pub const OPS_HEARTBEAT: &str = "ops.running.heartbeat";
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector_event_model::EventDirection;
    use crate::policy::ActorKind;
    use crate::recorder_audit::{
        AUDIT_SCHEMA_VERSION, ActorIdentity, AuditEventBuilder, AuditLog, AuditLogConfig,
    };
    use proptest::prelude::*;
    use proptest::string::string_regex;
    use std::collections::HashMap;

    fn arb_health_tier() -> impl Strategy<Value = HealthTier> {
        prop_oneof![
            Just(HealthTier::Green),
            Just(HealthTier::Yellow),
            Just(HealthTier::Red),
            Just(HealthTier::Black),
        ]
    }

    fn arb_runtime_phase() -> impl Strategy<Value = RuntimePhase> {
        prop_oneof![
            Just(RuntimePhase::Init),
            Just(RuntimePhase::Startup),
            Just(RuntimePhase::Running),
            Just(RuntimePhase::Draining),
            Just(RuntimePhase::Finalizing),
            Just(RuntimePhase::Shutdown),
            Just(RuntimePhase::Cancelling),
            Just(RuntimePhase::Recovering),
            Just(RuntimePhase::Maintenance),
        ]
    }

    fn arb_runtime_kind() -> impl Strategy<Value = RuntimeTelemetryKind> {
        prop_oneof![
            Just(RuntimeTelemetryKind::ScopeCreated),
            Just(RuntimeTelemetryKind::ScopeStarted),
            Just(RuntimeTelemetryKind::ScopeDraining),
            Just(RuntimeTelemetryKind::ScopeFinalizing),
            Just(RuntimeTelemetryKind::ScopeClosed),
            Just(RuntimeTelemetryKind::CancellationRequested),
            Just(RuntimeTelemetryKind::CancellationPropagated),
            Just(RuntimeTelemetryKind::GracePeriodExpired),
            Just(RuntimeTelemetryKind::FinalizerCompleted),
            Just(RuntimeTelemetryKind::TierTransition),
            Just(RuntimeTelemetryKind::ThrottleApplied),
            Just(RuntimeTelemetryKind::ThrottleReleased),
            Just(RuntimeTelemetryKind::LoadShedding),
            Just(RuntimeTelemetryKind::QueueDepthObserved),
            Just(RuntimeTelemetryKind::ChannelClosed),
            Just(RuntimeTelemetryKind::PermitExhausted),
            Just(RuntimeTelemetryKind::TransientError),
            Just(RuntimeTelemetryKind::PermanentError),
            Just(RuntimeTelemetryKind::PanicCaptured),
            Just(RuntimeTelemetryKind::InvariantViolation),
            Just(RuntimeTelemetryKind::SafetyPolicyTriggered),
            Just(RuntimeTelemetryKind::ResourceObserved),
            Just(RuntimeTelemetryKind::ResourceExhausted),
            Just(RuntimeTelemetryKind::SloMeasurement),
            Just(RuntimeTelemetryKind::ConfigApplied),
            Just(RuntimeTelemetryKind::DiagnosticExported),
            Just(RuntimeTelemetryKind::Heartbeat),
        ]
    }

    fn arb_failure_class() -> impl Strategy<Value = FailureClass> {
        prop_oneof![
            Just(FailureClass::Transient),
            Just(FailureClass::Permanent),
            Just(FailureClass::Degraded),
            Just(FailureClass::Overload),
            Just(FailureClass::Corruption),
            Just(FailureClass::Timeout),
            Just(FailureClass::Panic),
            Just(FailureClass::Deadlock),
            Just(FailureClass::Safety),
            Just(FailureClass::Configuration),
        ]
    }

    fn ident_string() -> impl Strategy<Value = String> {
        string_regex("[a-z][a-z0-9_]{0,8}").expect("valid identifier regex")
    }

    fn component_string() -> impl Strategy<Value = String> {
        (ident_string(), ident_string()).prop_map(|(a, b)| format!("rt.{a}.{b}"))
    }

    fn reason_code_string() -> impl Strategy<Value = String> {
        (ident_string(), ident_string(), ident_string())
            .prop_map(|(a, b, c)| format!("{a}.{b}.{c}"))
    }

    fn scope_id_string() -> impl Strategy<Value = String> {
        (ident_string(), ident_string()).prop_map(|(a, b)| format!("{a}:{b}"))
    }

    fn nonempty_text_string() -> impl Strategy<Value = String> {
        string_regex("[A-Za-z0-9 _:-]{1,24}").expect("valid text regex")
    }

    fn maybe_text_string() -> impl Strategy<Value = String> {
        string_regex("[A-Za-z0-9 _:-]{0,24}").expect("valid optional text regex")
    }

    fn correlation_string() -> impl Strategy<Value = String> {
        string_regex("[A-Za-z0-9_-]{1,24}").expect("valid correlation regex")
    }

    fn maybe_correlation_string() -> impl Strategy<Value = String> {
        proptest::option::of(correlation_string()).prop_map(|value| value.unwrap_or_default())
    }

    fn arb_sensitive_key() -> impl Strategy<Value = &'static str> {
        prop_oneof![
            Just("error_message"),
            Just("token"),
            Just("secret"),
            Just("password"),
            Just("api_key"),
        ]
    }

    fn arb_unknown_complex_value() -> impl Strategy<Value = serde_json::Value> {
        prop_oneof![
            nonempty_text_string().prop_map(serde_json::Value::String),
            proptest::collection::vec(any::<u16>(), 0..5)
                .prop_map(|values| serde_json::json!(values)),
            (ident_string(), any::<u16>())
                .prop_map(|(key, value)| serde_json::json!({ key: value })),
        ]
    }

    fn capacity_stage_snapshot(
        snapshot: &SwarmCapacityTelemetrySnapshot,
        stage: SwarmCapacityStage,
    ) -> &SwarmCapacityStageSnapshot {
        snapshot
            .stages
            .iter()
            .find(|entry| entry.stage == stage)
            .expect("fixed capacity stage should be present")
    }

    fn capacity_stage_certificate(
        certificate: &SwarmCapacityCertificate,
        stage: SwarmCapacityStage,
    ) -> &SwarmCapacityStageCertificate {
        certificate
            .stages
            .iter()
            .find(|entry| entry.stage == stage)
            .expect("selected capacity stage should be certified")
    }

    fn tail_stage_report(
        report: &SwarmTailRiskReport,
        stage: SwarmCapacityStage,
    ) -> &SwarmTailRiskStageReport {
        report
            .stages
            .iter()
            .find(|entry| entry.stage == stage)
            .expect("selected tail-risk stage should be monitored")
    }

    fn stage_capacity_certificate_for_tail_tests(
        stage: SwarmCapacityStage,
        service_time_ms: f64,
        samples: u64,
        queue_depth: u64,
        observation_window_secs: f64,
        retry_latency_ms: Option<f64>,
        cancellations: u64,
    ) -> SwarmCapacityCertificate {
        let mut telemetry = SwarmCapacityTelemetry::new(SwarmCapacityTelemetryConfig {
            enabled: true,
            max_samples_per_stage: 256,
        });

        for _ in 0..samples {
            telemetry.record_arrival(stage, queue_depth);
            telemetry.record_completion(stage, service_time_ms, queue_depth);
            if let Some(retry_latency_ms) = retry_latency_ms {
                telemetry.record_retry_latency_ms(stage, retry_latency_ms);
            }
        }
        for _ in 0..cancellations {
            telemetry.record_arrival(stage, queue_depth);
            telemetry.record_cancellation(stage, service_time_ms, queue_depth);
        }

        telemetry
            .snapshot()
            .capacity_certificate(SwarmCapacityCertificateConfig {
                workload_class: SwarmCapacityWorkloadClass::BackpressureEscalation,
                pane_scale: 200,
                observation_window_secs,
                selected_stages: vec![stage],
                min_samples_per_stage: 5,
                ..SwarmCapacityCertificateConfig::default()
            })
    }

    fn assert_option_f64_close(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("expected finite f64");
        let scale = actual.abs().max(expected.abs()).max(1.0);
        assert!(
            (actual - expected).abs() <= f64::EPSILON * scale * 8.0,
            "expected {expected:?}, got {actual:?}"
        );
    }

    #[test]
    fn swarm_capacity_telemetry_records_two_stages_and_timeout() {
        let mut telemetry = SwarmCapacityTelemetry::new(SwarmCapacityTelemetryConfig {
            enabled: true,
            max_samples_per_stage: 8,
        });

        telemetry.record_arrival(SwarmCapacityStage::IngestCapture, 3);
        telemetry.record_completion(SwarmCapacityStage::IngestCapture, 12.0, 2);
        telemetry.record_wait_time_ms(SwarmCapacityStage::StorageReadPool, 4.0);
        telemetry.record_arrival(SwarmCapacityStage::RobotMcp, 7);
        telemetry.record_timeout(SwarmCapacityStage::RobotMcp, 250.0, 8);
        telemetry.record_arrival(SwarmCapacityStage::EventBusFanout, 4);
        telemetry.record_error(
            SwarmCapacityStage::EventBusFanout,
            FailureClass::Overload,
            9.0,
            4,
        );

        let snapshot = telemetry.snapshot();
        assert!(snapshot.enabled);
        assert_eq!(snapshot.stage_count, SwarmCapacityStage::COUNT);
        assert_eq!(snapshot.max_samples_per_stage, 8);
        assert_eq!(snapshot.stages.len(), SwarmCapacityStage::COUNT);
        assert!(snapshot.estimated_memory_cap_bytes > 0);

        let ingest = capacity_stage_snapshot(&snapshot, SwarmCapacityStage::IngestCapture);
        assert_eq!(ingest.stage_name, "ingest_capture");
        assert_eq!(ingest.arrivals, 1);
        assert_eq!(ingest.completions, 1);
        assert_eq!(ingest.in_flight_estimate, 0);
        assert_eq!(ingest.service_time_ms.count, 1);
        assert_eq!(ingest.service_time_ms.p50, Some(12.0));
        assert_eq!(ingest.queue_depth.count, 2);

        let robot_mcp = capacity_stage_snapshot(&snapshot, SwarmCapacityStage::RobotMcp);
        assert_eq!(robot_mcp.stage_name, "robot_mcp");
        assert_eq!(robot_mcp.arrivals, 1);
        assert_eq!(robot_mcp.timeouts, 1);
        assert_eq!(robot_mcp.service_time_ms.p50, Some(250.0));
        assert_eq!(robot_mcp.failure_counts.count(FailureClass::Timeout), 1);

        let fanout = capacity_stage_snapshot(&snapshot, SwarmCapacityStage::EventBusFanout);
        assert_eq!(fanout.errors, 1);
        assert_eq!(fanout.failure_counts.count(FailureClass::Overload), 1);
        assert_eq!(fanout.failure_counts.total(), 1);

        let labels = snapshot
            .stages
            .iter()
            .map(|stage| stage.stage_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                "ingest_capture",
                "storage_write",
                "storage_read_pool",
                "event_bus_fanout",
                "workflow_runner",
                "mux_ipc",
                "robot_mcp",
            ]
        );
    }

    #[test]
    fn swarm_capacity_telemetry_caps_samples_and_disabled_noops() {
        let mut telemetry = SwarmCapacityTelemetry::new(SwarmCapacityTelemetryConfig {
            enabled: true,
            max_samples_per_stage: 2,
        });

        telemetry.record_completion(SwarmCapacityStage::StorageWrite, 1.0, 1);
        telemetry.record_completion(SwarmCapacityStage::StorageWrite, 2.0, 2);
        telemetry.record_completion(SwarmCapacityStage::StorageWrite, 3.0, 3);
        telemetry.record_retry_latency_ms(SwarmCapacityStage::WorkflowRunner, 5.0);
        telemetry.record_cancellation(SwarmCapacityStage::WorkflowRunner, 20.0, 1);

        let snapshot = telemetry.snapshot();
        let storage_write = capacity_stage_snapshot(&snapshot, SwarmCapacityStage::StorageWrite);
        assert_eq!(storage_write.completions, 3);
        assert_eq!(storage_write.service_time_ms.count, 3);
        assert_eq!(storage_write.service_time_ms.retained, 2);
        assert_eq!(storage_write.queue_depth.count, 3);
        assert_eq!(storage_write.queue_depth.retained, 2);

        let workflow_runner =
            capacity_stage_snapshot(&snapshot, SwarmCapacityStage::WorkflowRunner);
        assert_eq!(workflow_runner.cancellations, 1);
        assert_eq!(workflow_runner.retry_latency_ms.count, 1);

        let mut disabled = SwarmCapacityTelemetry::new(SwarmCapacityTelemetryConfig {
            enabled: false,
            max_samples_per_stage: 2,
        });
        disabled.record_arrival(SwarmCapacityStage::MuxIpc, 99);
        disabled.record_error(SwarmCapacityStage::MuxIpc, FailureClass::Transient, 1.0, 99);
        disabled.record_wait_time_ms(SwarmCapacityStage::MuxIpc, 3.0);

        let disabled_snapshot = disabled.snapshot();
        assert!(!disabled_snapshot.enabled);
        let mux_ipc = capacity_stage_snapshot(&disabled_snapshot, SwarmCapacityStage::MuxIpc);
        assert_eq!(mux_ipc.arrivals, 0);
        assert_eq!(mux_ipc.errors, 0);
        assert_eq!(mux_ipc.queue_depth.count, 0);
        assert_eq!(mux_ipc.wait_time_ms.count, 0);
    }

    #[test]
    fn swarm_capacity_certificate_marks_stable_stage_safe() {
        let mut telemetry = SwarmCapacityTelemetry::new(SwarmCapacityTelemetryConfig {
            enabled: true,
            max_samples_per_stage: 32,
        });

        for _ in 0..10 {
            telemetry.record_arrival(SwarmCapacityStage::StorageWrite, 0);
            telemetry.record_completion(SwarmCapacityStage::StorageWrite, 50.0, 0);
        }

        let certificate =
            telemetry
                .snapshot()
                .capacity_certificate(SwarmCapacityCertificateConfig {
                    workload_class: SwarmCapacityWorkloadClass::StorageWriteSaturation,
                    pane_scale: 200,
                    observation_window_secs: 100.0,
                    selected_stages: vec![SwarmCapacityStage::StorageWrite],
                    min_samples_per_stage: 5,
                    ..SwarmCapacityCertificateConfig::default()
                });

        assert_eq!(certificate.status, SwarmCapacityCertificateStatus::Safe);
        assert_eq!(certificate.reason_code, "capacity.capacity.safe");
        assert_eq!(
            certificate.bottleneck_stage,
            Some(SwarmCapacityStage::StorageWrite)
        );
        assert!(certificate.assumption_flags.is_empty());

        let stage = capacity_stage_certificate(&certificate, SwarmCapacityStage::StorageWrite);
        assert_eq!(stage.status, SwarmCapacityCertificateStatus::Safe);
        assert!(stage.assumption_flags.is_empty());
        assert_option_f64_close(stage.arrival_rate_per_s, 0.1);
        assert_option_f64_close(stage.service_rate_per_s, 20.0);
        assert!(stage.utilization.is_some_and(|rho| rho < 0.01));
        assert!(
            stage
                .modeled_latency
                .p99_ms
                .is_some_and(|modeled| modeled >= stage.service_time_ms.p99.unwrap())
        );
    }

    #[test]
    fn swarm_capacity_certificate_refuses_gray_zone_above_safe_below_saturated() {
        let mut telemetry = SwarmCapacityTelemetry::new(SwarmCapacityTelemetryConfig {
            enabled: true,
            max_samples_per_stage: 128,
        });

        for _ in 0..85 {
            telemetry.record_arrival(SwarmCapacityStage::StorageWrite, 0);
            telemetry.record_completion(SwarmCapacityStage::StorageWrite, 100.0, 0);
        }

        let certificate =
            telemetry
                .snapshot()
                .capacity_certificate(SwarmCapacityCertificateConfig {
                    workload_class: SwarmCapacityWorkloadClass::StorageWriteSaturation,
                    pane_scale: 200,
                    observation_window_secs: 10.0,
                    selected_stages: vec![SwarmCapacityStage::StorageWrite],
                    min_samples_per_stage: 5,
                    safe_utilization_threshold: 0.80,
                    unsafe_utilization_threshold: 0.95,
                    ..SwarmCapacityCertificateConfig::default()
                });

        assert_eq!(certificate.status, SwarmCapacityCertificateStatus::Unknown);
        assert_eq!(
            certificate.assumption_flags,
            vec![SwarmCapacityAssumptionFlag::UtilizationAboveSafeThreshold]
        );

        let stage = capacity_stage_certificate(&certificate, SwarmCapacityStage::StorageWrite);
        assert_eq!(stage.status, SwarmCapacityCertificateStatus::Unknown);
        assert!(
            stage
                .utilization
                .is_some_and(|rho| rho > 0.80 && rho < 0.95)
        );
        assert!(
            !stage
                .assumption_flags
                .contains(&SwarmCapacityAssumptionFlag::SaturatedUtilization)
        );
    }

    #[test]
    fn swarm_capacity_certificate_marks_saturated_stage_unsafe() {
        let mut telemetry = SwarmCapacityTelemetry::new(SwarmCapacityTelemetryConfig {
            enabled: true,
            max_samples_per_stage: 128,
        });

        for _ in 0..100 {
            telemetry.record_arrival(SwarmCapacityStage::StorageWrite, 2);
            telemetry.record_completion(SwarmCapacityStage::StorageWrite, 100.0, 2);
        }

        let certificate =
            telemetry
                .snapshot()
                .capacity_certificate(SwarmCapacityCertificateConfig {
                    workload_class: SwarmCapacityWorkloadClass::StorageWriteSaturation,
                    pane_scale: 200,
                    observation_window_secs: 5.0,
                    selected_stages: vec![SwarmCapacityStage::StorageWrite],
                    min_samples_per_stage: 5,
                    ..SwarmCapacityCertificateConfig::default()
                });

        assert_eq!(certificate.status, SwarmCapacityCertificateStatus::Unsafe);
        assert_eq!(
            certificate.assumption_flags,
            vec![SwarmCapacityAssumptionFlag::SaturatedUtilization]
        );

        let stage = capacity_stage_certificate(&certificate, SwarmCapacityStage::StorageWrite);
        assert_eq!(stage.status, SwarmCapacityCertificateStatus::Unsafe);
        assert!(stage.utilization.is_some_and(|rho| rho >= 1.0));
        assert!(
            stage
                .assumption_flags
                .contains(&SwarmCapacityAssumptionFlag::SaturatedUtilization)
        );
    }

    #[test]
    fn swarm_capacity_certificate_refuses_insufficient_samples() {
        let mut telemetry = SwarmCapacityTelemetry::new(SwarmCapacityTelemetryConfig {
            enabled: true,
            max_samples_per_stage: 16,
        });

        for _ in 0..2 {
            telemetry.record_arrival(SwarmCapacityStage::RobotMcp, 0);
            telemetry.record_completion(SwarmCapacityStage::RobotMcp, 20.0, 0);
        }

        let certificate =
            telemetry
                .snapshot()
                .capacity_certificate(SwarmCapacityCertificateConfig {
                    workload_class: SwarmCapacityWorkloadClass::RobotMcpBurst,
                    pane_scale: 50,
                    observation_window_secs: 60.0,
                    selected_stages: vec![SwarmCapacityStage::RobotMcp],
                    min_samples_per_stage: 5,
                    ..SwarmCapacityCertificateConfig::default()
                });

        assert_eq!(certificate.status, SwarmCapacityCertificateStatus::Unknown);
        assert_eq!(
            certificate.assumption_flags,
            vec![SwarmCapacityAssumptionFlag::InsufficientSamples]
        );

        let stage = capacity_stage_certificate(&certificate, SwarmCapacityStage::RobotMcp);
        assert_eq!(stage.reason_code, "robot_mcp.capacity.insufficient_samples");
    }

    #[test]
    fn swarm_capacity_certificate_refuses_heavy_tailed_service() {
        let mut telemetry = SwarmCapacityTelemetry::new(SwarmCapacityTelemetryConfig {
            enabled: true,
            max_samples_per_stage: 128,
        });

        for _ in 0..98 {
            telemetry.record_arrival(SwarmCapacityStage::EventBusFanout, 0);
            telemetry.record_completion(SwarmCapacityStage::EventBusFanout, 10.0, 0);
        }
        for _ in 0..2 {
            telemetry.record_arrival(SwarmCapacityStage::EventBusFanout, 0);
            telemetry.record_completion(SwarmCapacityStage::EventBusFanout, 1000.0, 0);
        }

        let certificate =
            telemetry
                .snapshot()
                .capacity_certificate(SwarmCapacityCertificateConfig {
                    workload_class: SwarmCapacityWorkloadClass::BackpressureEscalation,
                    pane_scale: 100,
                    observation_window_secs: 1000.0,
                    selected_stages: vec![SwarmCapacityStage::EventBusFanout],
                    min_samples_per_stage: 5,
                    heavy_tail_p99_to_mean_ratio: 10.0,
                    ..SwarmCapacityCertificateConfig::default()
                });

        assert_eq!(certificate.status, SwarmCapacityCertificateStatus::Unknown);
        assert_eq!(
            certificate.reason_code,
            "capacity.capacity.heavy_tailed_service"
        );
        assert!(
            certificate
                .assumption_flags
                .contains(&SwarmCapacityAssumptionFlag::HeavyTailedService)
        );
    }

    #[test]
    fn swarm_tail_risk_monitor_keeps_replayed_baseline_green() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::StorageWrite,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let live = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::StorageWrite,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );

        let report = baseline.tail_risk_report(
            &live,
            SwarmTailRiskMonitorConfig {
                selected_stages: vec![SwarmCapacityStage::StorageWrite],
                alpha: 0.10,
                min_baseline_samples_per_stage: 20,
                min_live_samples_per_stage: 20,
                max_finite_sample_slack_ratio: 0.10,
                ..SwarmTailRiskMonitorConfig::default()
            },
        );

        assert_eq!(report.status, SwarmTailRiskStatus::Green);
        assert_eq!(report.reason_code, "capacity.tail_risk.within_budget");
        let stage = tail_stage_report(&report, SwarmCapacityStage::StorageWrite);
        assert_eq!(stage.status, SwarmTailRiskStatus::Green);
        assert_eq!(stage.evidence[0].signal, SwarmTailRiskSignal::WithinBudget);
    }

    #[test]
    fn swarm_tail_risk_monitor_violates_on_synthetic_tail_saturation() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::StorageWrite,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let live = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::StorageWrite,
            40.0,
            100,
            4,
            1000.0,
            None,
            0,
        );

        let report = baseline.tail_risk_report(
            &live,
            SwarmTailRiskMonitorConfig {
                selected_stages: vec![SwarmCapacityStage::StorageWrite],
                alpha: 0.10,
                min_baseline_samples_per_stage: 20,
                min_live_samples_per_stage: 20,
                max_finite_sample_slack_ratio: 0.10,
                violation_slack_ratio: 0.25,
                ..SwarmTailRiskMonitorConfig::default()
            },
        );

        assert_eq!(report.status, SwarmTailRiskStatus::Violated);
        assert!(
            report
                .evidence
                .iter()
                .any(|entry| entry.signal == SwarmTailRiskSignal::P99Residual
                    && entry.status == SwarmTailRiskStatus::Violated),
            "tail-risk evidence should include a violated p99 residual: {:?}",
            report.evidence
        );
    }

    #[test]
    fn swarm_tail_risk_monitor_refuses_missing_baseline_stage() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::StorageWrite,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let live = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::RobotMcp,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );

        let report = baseline.tail_risk_report(
            &live,
            SwarmTailRiskMonitorConfig {
                selected_stages: vec![SwarmCapacityStage::RobotMcp],
                min_baseline_samples_per_stage: 20,
                min_live_samples_per_stage: 20,
                ..SwarmTailRiskMonitorConfig::default()
            },
        );

        assert_eq!(report.status, SwarmTailRiskStatus::Unknown);
        let stage = tail_stage_report(&report, SwarmCapacityStage::RobotMcp);
        assert_eq!(
            stage.reason_code,
            "robot_mcp.tail_risk.missing_baseline_stage"
        );
        assert_eq!(
            stage.evidence[0].signal,
            SwarmTailRiskSignal::MissingBaselineStage
        );
    }

    #[test]
    fn swarm_tail_risk_monitor_marks_stale_baseline() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::EventBusFanout,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let live = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::EventBusFanout,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );

        let report = baseline.tail_risk_report(
            &live,
            SwarmTailRiskMonitorConfig {
                selected_stages: vec![SwarmCapacityStage::EventBusFanout],
                baseline_age_secs: Some(3601),
                stale_after_secs: Some(3600),
                min_baseline_samples_per_stage: 20,
                min_live_samples_per_stage: 20,
                ..SwarmTailRiskMonitorConfig::default()
            },
        );

        assert_eq!(report.status, SwarmTailRiskStatus::StaleBaseline);
        assert_eq!(report.reason_code, "capacity.tail_risk.baseline_stale");
    }

    #[test]
    fn swarm_tail_risk_monitor_detects_retry_and_cancellation_spikes() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::WorkflowRunner,
            10.0,
            100,
            0,
            1000.0,
            Some(1.0),
            0,
        );
        let live = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::WorkflowRunner,
            10.0,
            100,
            0,
            1000.0,
            Some(20.0),
            10,
        );

        let report = baseline.tail_risk_report(
            &live,
            SwarmTailRiskMonitorConfig {
                selected_stages: vec![SwarmCapacityStage::WorkflowRunner],
                min_baseline_samples_per_stage: 20,
                min_live_samples_per_stage: 20,
                retry_violation_multiplier: 5.0,
                cancellation_violation_rate: 0.05,
                ..SwarmTailRiskMonitorConfig::default()
            },
        );

        assert_eq!(report.status, SwarmTailRiskStatus::Violated);
        assert!(
            report
                .evidence
                .iter()
                .any(|entry| entry.signal == SwarmTailRiskSignal::RetryStorm),
            "retry storm evidence missing: {:?}",
            report.evidence
        );
        assert!(
            report
                .evidence
                .iter()
                .any(|entry| entry.signal == SwarmTailRiskSignal::CancellationSpike),
            "cancellation evidence missing: {:?}",
            report.evidence
        );
    }

    #[test]
    fn swarm_capacity_evidence_record_redacts_context_and_hashes_artifacts() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::RobotMcp,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let live = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::RobotMcp,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let report = baseline.tail_risk_report(
            &live,
            SwarmTailRiskMonitorConfig {
                selected_stages: vec![SwarmCapacityStage::RobotMcp],
                min_baseline_samples_per_stage: 20,
                min_live_samples_per_stage: 20,
                ..SwarmTailRiskMonitorConfig::default()
            },
        );
        let ledger_config = SwarmCapacityEvidenceLedgerConfig {
            max_context_fields: 2,
            max_context_value_bytes: 24,
            ..SwarmCapacityEvidenceLedgerConfig::default()
        };
        let mut raw_context = BTreeMap::new();
        raw_context.insert(
            "token".to_string(),
            "sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890_abcdef".to_string(),
        );
        raw_context.insert(
            "long-field-name-that-will-be-truncated".to_string(),
            "normal-value-that-is-long-enough-to-truncate".to_string(),
        );
        raw_context.insert("dropped".to_string(), "third".to_string());
        let redactor = crate::policy::Redactor::new();
        let context = SwarmCapacityEvidenceContext::redacted_from_redactor(
            raw_context,
            &redactor,
            &ledger_config,
        );
        let config_snapshot = SwarmCapacityEvidenceConfigSnapshot::from_configs(
            &SwarmCapacityCertificateConfig {
                selected_stages: vec![SwarmCapacityStage::RobotMcp],
                ..SwarmCapacityCertificateConfig::default()
            },
            &SwarmTailRiskMonitorConfig {
                selected_stages: vec![SwarmCapacityStage::RobotMcp],
                ..SwarmTailRiskMonitorConfig::default()
            },
            &ledger_config,
        );

        let record = SwarmCapacityEvidenceRecord::from_evidence(
            1_700_000_000_000,
            live,
            report,
            &config_snapshot,
            context,
        );

        assert!(record.certificate_hash.starts_with("sha256:"));
        assert!(record.config_snapshot_hash.starts_with("sha256:"));
        assert!(record.redacted_context.redacted);
        assert_eq!(record.redacted_context.dropped_fields, 1);
        assert!(record.redacted_context.truncated_fields >= 1);
        let serialized_context =
            serde_json::to_string(&record.redacted_context).expect("serialize context");
        assert!(
            !redactor.contains_secrets(&serialized_context),
            "context still contains a secret: {serialized_context}"
        );
    }

    #[test]
    fn swarm_capacity_evidence_replay_is_deterministic_and_side_effect_free() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::StorageReadPool,
            12.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let live = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::StorageReadPool,
            12.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let report = baseline.tail_risk_report(
            &live,
            SwarmTailRiskMonitorConfig {
                selected_stages: vec![SwarmCapacityStage::StorageReadPool],
                min_baseline_samples_per_stage: 20,
                min_live_samples_per_stage: 20,
                ..SwarmTailRiskMonitorConfig::default()
            },
        );
        let ledger_config = SwarmCapacityEvidenceLedgerConfig::default();
        let config_snapshot = SwarmCapacityEvidenceConfigSnapshot::from_configs(
            &SwarmCapacityCertificateConfig {
                selected_stages: vec![SwarmCapacityStage::StorageReadPool],
                ..SwarmCapacityCertificateConfig::default()
            },
            &SwarmTailRiskMonitorConfig {
                selected_stages: vec![SwarmCapacityStage::StorageReadPool],
                ..SwarmTailRiskMonitorConfig::default()
            },
            &ledger_config,
        );

        let record = SwarmCapacityEvidenceRecord::from_evidence(
            1_700_000_000_001,
            live,
            report,
            &config_snapshot,
            SwarmCapacityEvidenceContext::empty_redacted(),
        );
        let replay = record.replay();

        assert_eq!(record.action, SwarmCapacityDecisionAction::Allow);
        assert_eq!(replay.status, SwarmCapacityReplayStatus::Matched);
        assert_eq!(
            replay.recomputed_action,
            Some(SwarmCapacityDecisionAction::Allow)
        );
        assert!(!replay.side_effects_executed);
    }

    #[test]
    fn swarm_capacity_evidence_record_id_includes_report_and_context() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::RobotMcp,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let live = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::RobotMcp,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let monitor_config = SwarmTailRiskMonitorConfig {
            selected_stages: vec![SwarmCapacityStage::RobotMcp],
            min_baseline_samples_per_stage: 20,
            min_live_samples_per_stage: 20,
            ..SwarmTailRiskMonitorConfig::default()
        };
        let report = baseline.tail_risk_report(&live, monitor_config.clone());
        let ledger_config = SwarmCapacityEvidenceLedgerConfig::default();
        let config_snapshot = SwarmCapacityEvidenceConfigSnapshot::from_configs(
            &SwarmCapacityCertificateConfig {
                selected_stages: vec![SwarmCapacityStage::RobotMcp],
                ..SwarmCapacityCertificateConfig::default()
            },
            &monitor_config,
            &ledger_config,
        );

        let base_record = SwarmCapacityEvidenceRecord::from_evidence(
            1_700_000_000_004,
            live.clone(),
            report.clone(),
            &config_snapshot,
            SwarmCapacityEvidenceContext::empty_redacted(),
        );

        let mut changed_report = report.clone();
        changed_report.confidence = (changed_report.confidence + 0.01).min(1.0);
        let changed_report_record = SwarmCapacityEvidenceRecord::from_evidence(
            1_700_000_000_004,
            live.clone(),
            changed_report,
            &config_snapshot,
            SwarmCapacityEvidenceContext::empty_redacted(),
        );

        let mut fields = BTreeMap::new();
        fields.insert("request".to_string(), "changed-context".to_string());
        let changed_context = SwarmCapacityEvidenceContext::redacted_from(
            fields,
            std::borrow::ToOwned::to_owned,
            &ledger_config,
        );
        let changed_context_record = SwarmCapacityEvidenceRecord::from_evidence(
            1_700_000_000_004,
            live,
            report,
            &config_snapshot,
            changed_context,
        );

        assert_ne!(base_record.record_id, changed_report_record.record_id);
        assert_ne!(base_record.record_id, changed_context_record.record_id);
    }

    #[test]
    fn swarm_capacity_evidence_ledger_returns_new_record_when_context_differs() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::MuxIpc,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let live = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::MuxIpc,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let monitor_config = SwarmTailRiskMonitorConfig {
            selected_stages: vec![SwarmCapacityStage::MuxIpc],
            min_baseline_samples_per_stage: 20,
            min_live_samples_per_stage: 20,
            ..SwarmTailRiskMonitorConfig::default()
        };
        let report = baseline.tail_risk_report(&live, monitor_config.clone());
        let ledger_config = SwarmCapacityEvidenceLedgerConfig {
            max_records: 4,
            retention_window_secs: None,
            ..SwarmCapacityEvidenceLedgerConfig::default()
        };
        let config_snapshot = SwarmCapacityEvidenceConfigSnapshot::from_configs(
            &SwarmCapacityCertificateConfig {
                selected_stages: vec![SwarmCapacityStage::MuxIpc],
                ..SwarmCapacityCertificateConfig::default()
            },
            &monitor_config,
            &ledger_config,
        );
        let mut ledger = SwarmCapacityEvidenceLedger::new(ledger_config.clone());
        let context = |value: &str| {
            let mut fields = BTreeMap::new();
            fields.insert("request".to_string(), value.to_string());
            SwarmCapacityEvidenceContext::redacted_from(
                fields,
                std::borrow::ToOwned::to_owned,
                &ledger_config,
            )
        };

        let first = ledger.record_decision(
            1_700_000_000_005,
            live.clone(),
            report.clone(),
            &config_snapshot,
            context("first"),
        );
        let first_id = first.record_id.clone();
        assert_eq!(
            first
                .redacted_context
                .fields
                .get("request")
                .map(String::as_str),
            Some("first")
        );

        let second = ledger.record_decision(
            1_700_000_000_005,
            live,
            report,
            &config_snapshot,
            context("second"),
        );
        let second_id = second.record_id.clone();

        assert_ne!(first_id, second_id);
        assert_eq!(
            second
                .redacted_context
                .fields
                .get("request")
                .map(String::as_str),
            Some("second")
        );
        assert_eq!(ledger.records.len(), 2);
    }

    #[test]
    fn swarm_capacity_evidence_replay_detects_report_tampering() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::StorageReadPool,
            12.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let live = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::StorageReadPool,
            12.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let monitor_config = SwarmTailRiskMonitorConfig {
            selected_stages: vec![SwarmCapacityStage::StorageReadPool],
            min_baseline_samples_per_stage: 20,
            min_live_samples_per_stage: 20,
            ..SwarmTailRiskMonitorConfig::default()
        };
        let report = baseline.tail_risk_report(&live, monitor_config.clone());
        let ledger_config = SwarmCapacityEvidenceLedgerConfig::default();
        let config_snapshot = SwarmCapacityEvidenceConfigSnapshot::from_configs(
            &SwarmCapacityCertificateConfig {
                selected_stages: vec![SwarmCapacityStage::StorageReadPool],
                ..SwarmCapacityCertificateConfig::default()
            },
            &monitor_config,
            &ledger_config,
        );

        let mut record = SwarmCapacityEvidenceRecord::from_evidence(
            1_700_000_000_006,
            live,
            report,
            &config_snapshot,
            SwarmCapacityEvidenceContext::empty_redacted(),
        );
        record
            .tail_risk_report
            .as_mut()
            .expect("tail risk report should be embedded")
            .confidence = 0.42;

        let replay = record.replay();
        assert_eq!(replay.status, SwarmCapacityReplayStatus::Diverged);
        assert!(!replay.side_effects_executed);
    }

    #[test]
    fn swarm_capacity_evidence_ledger_retention_bounds_records() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::MuxIpc,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let ledger_config = SwarmCapacityEvidenceLedgerConfig {
            max_records: 2,
            retention_window_secs: Some(1),
            ..SwarmCapacityEvidenceLedgerConfig::default()
        };
        let config_snapshot = SwarmCapacityEvidenceConfigSnapshot::from_configs(
            &SwarmCapacityCertificateConfig {
                selected_stages: vec![SwarmCapacityStage::MuxIpc],
                ..SwarmCapacityCertificateConfig::default()
            },
            &SwarmTailRiskMonitorConfig {
                selected_stages: vec![SwarmCapacityStage::MuxIpc],
                min_baseline_samples_per_stage: 20,
                min_live_samples_per_stage: 20,
                ..SwarmTailRiskMonitorConfig::default()
            },
            &ledger_config,
        );
        let mut ledger = SwarmCapacityEvidenceLedger::new(ledger_config);

        for timestamp_ms in [1_000, 2_000, 3_000] {
            let live = stage_capacity_certificate_for_tail_tests(
                SwarmCapacityStage::MuxIpc,
                10.0,
                100,
                0,
                1000.0,
                None,
                0,
            );
            let report = baseline.tail_risk_report(
                &live,
                SwarmTailRiskMonitorConfig {
                    selected_stages: vec![SwarmCapacityStage::MuxIpc],
                    min_baseline_samples_per_stage: 20,
                    min_live_samples_per_stage: 20,
                    ..SwarmTailRiskMonitorConfig::default()
                },
            );
            ledger.record_decision(
                timestamp_ms,
                live,
                report,
                &config_snapshot,
                SwarmCapacityEvidenceContext::empty_redacted(),
            );
        }

        assert_eq!(ledger.records.len(), 2);
        assert_eq!(
            ledger
                .records
                .iter()
                .map(|record| record.timestamp_ms)
                .collect::<Vec<_>>(),
            vec![2_000, 3_000]
        );
        assert!(ledger.ledger_hash().starts_with("sha256:"));
    }

    #[test]
    fn swarm_capacity_evidence_bundle_reports_missing_artifacts() {
        let ledger = SwarmCapacityEvidenceLedger::with_defaults();
        let bundle = ledger.export_bundle(
            1_700_000_000_002,
            SwarmCapacityEvidenceContext::empty_redacted(),
            None,
            None,
            None,
        );

        assert!(!bundle.is_complete());
        assert!(
            bundle
                .incomplete_artifacts
                .contains(&SwarmCapacityEvidenceArtifactKind::ReproLock)
        );
        assert!(
            bundle
                .incomplete_artifacts
                .contains(&SwarmCapacityEvidenceArtifactKind::CapacityCertificate)
        );
        assert!(
            bundle
                .incomplete_artifacts
                .contains(&SwarmCapacityEvidenceArtifactKind::DecisionLedger)
        );
        assert!(
            bundle
                .incomplete_artifacts
                .contains(&SwarmCapacityEvidenceArtifactKind::TelemetryExcerpt)
        );
    }

    #[test]
    fn swarm_capacity_evidence_blocks_stale_certificate() {
        let baseline = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::EventBusFanout,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let live = stage_capacity_certificate_for_tail_tests(
            SwarmCapacityStage::EventBusFanout,
            10.0,
            100,
            0,
            1000.0,
            None,
            0,
        );
        let monitor_config = SwarmTailRiskMonitorConfig {
            selected_stages: vec![SwarmCapacityStage::EventBusFanout],
            baseline_age_secs: Some(120),
            stale_after_secs: Some(60),
            min_baseline_samples_per_stage: 20,
            min_live_samples_per_stage: 20,
            ..SwarmTailRiskMonitorConfig::default()
        };
        let report = baseline.tail_risk_report(&live, monitor_config.clone());
        let ledger_config = SwarmCapacityEvidenceLedgerConfig::default();
        let config_snapshot = SwarmCapacityEvidenceConfigSnapshot::from_configs(
            &SwarmCapacityCertificateConfig {
                selected_stages: vec![SwarmCapacityStage::EventBusFanout],
                ..SwarmCapacityCertificateConfig::default()
            },
            &monitor_config,
            &ledger_config,
        );

        let record = SwarmCapacityEvidenceRecord::from_evidence(
            1_700_000_000_003,
            live,
            report,
            &config_snapshot,
            SwarmCapacityEvidenceContext::empty_redacted(),
        );
        let replay = record.replay();

        assert_eq!(record.monitor_status, SwarmTailRiskStatus::StaleBaseline);
        assert_eq!(record.action, SwarmCapacityDecisionAction::BlockAdmission);
        assert_eq!(replay.status, SwarmCapacityReplayStatus::Matched);
        assert!(!replay.side_effects_executed);
    }

    #[test]
    fn swarm_capacity_fairness_default_disabled_preserves_input_order() {
        let policy = SwarmCapacityFairnessPolicy::with_defaults();
        let requests = vec![
            SwarmCapacityFairnessRequest::new(
                "optional-a",
                SwarmCapacityWorkClass::OptionalDiagnostics,
                10,
            ),
            SwarmCapacityFairnessRequest::new(
                "claimed-a",
                SwarmCapacityWorkClass::ClaimedAgentTask,
                11,
            ),
            SwarmCapacityFairnessRequest::new(
                "operator-a",
                SwarmCapacityWorkClass::InteractiveOperator,
                12,
            ),
        ];

        let plan = policy.plan(&requests, SwarmCapacityDecisionAction::BlockAdmission);

        assert!(!plan.enabled);
        assert_eq!(plan.budget_units, u32::MAX);
        assert_eq!(
            plan.decisions
                .iter()
                .map(|decision| decision.stable_id.as_str())
                .collect::<Vec<_>>(),
            vec!["optional-a", "claimed-a", "operator-a"]
        );
        assert!(
            plan.decisions
                .iter()
                .all(|decision| decision.action == SwarmCapacityFairnessAction::Admit)
        );
        assert!(
            plan.decisions
                .iter()
                .all(|decision| decision.reason_code == "capacity.fairness.disabled")
        );
    }

    #[test]
    fn swarm_capacity_fairness_policy_table_is_explicit_and_deterministic() {
        let policy = SwarmCapacityFairnessPolicy::new(SwarmCapacityFairnessPolicyConfig {
            enabled: true,
            admission_budget_units: 8,
            reduced_admission_budget_units: 4,
            class_policies: Vec::new(),
        });

        let table = policy.policy_table();
        assert_eq!(table.len(), SwarmCapacityWorkClass::ALL.len());
        assert_eq!(
            table[0].work_class,
            SwarmCapacityWorkClass::InteractiveOperator
        );
        assert_eq!(table[0].weight, 100);
        assert_eq!(table[0].minimum_service_per_1000, 250);
        assert_eq!(
            table[0].eligible_pressure_actions,
            vec![SwarmCapacityFairnessAction::Defer]
        );
        assert_eq!(
            table.last().expect("optional diagnostics row").work_class,
            SwarmCapacityWorkClass::OptionalDiagnostics
        );
        assert_eq!(
            table
                .last()
                .expect("optional diagnostics row")
                .eligible_pressure_actions[0],
            SwarmCapacityFairnessAction::Shed
        );
        assert_eq!(policy.config.policy_hash(), policy.config.policy_hash());
    }

    #[test]
    fn swarm_capacity_fairness_admits_mission_critical_before_bulk_optional_work() {
        let policy = SwarmCapacityFairnessPolicy::new(SwarmCapacityFairnessPolicyConfig {
            enabled: true,
            admission_budget_units: 2,
            reduced_admission_budget_units: 1,
            class_policies: Vec::new(),
        });
        let mut requests = vec![
            SwarmCapacityFairnessRequest::new(
                "diagnostic-00",
                SwarmCapacityWorkClass::OptionalDiagnostics,
                0,
            ),
            SwarmCapacityFairnessRequest::new(
                "capture-00",
                SwarmCapacityWorkClass::BackgroundCapture,
                1,
            ),
            SwarmCapacityFairnessRequest::new(
                "claimed-critical",
                SwarmCapacityWorkClass::ClaimedAgentTask,
                99,
            ),
        ];
        for idx in 1..20 {
            requests.push(SwarmCapacityFairnessRequest::new(
                format!("diagnostic-{idx:02}"),
                SwarmCapacityWorkClass::OptionalDiagnostics,
                idx + 100,
            ));
        }

        let plan = policy.plan(&requests, SwarmCapacityDecisionAction::Allow);

        assert_eq!(plan.admitted_units, 2);
        assert_eq!(
            plan.decision_for("claimed-critical")
                .expect("claimed request")
                .action,
            SwarmCapacityFairnessAction::Admit
        );
        assert!(
            plan.decision_for("claimed-critical")
                .expect("claimed request")
                .rank
                < plan
                    .decision_for("diagnostic-00")
                    .expect("optional request")
                    .rank
        );
        assert!(
            plan.decisions.iter().any(|decision| {
                decision.work_class == SwarmCapacityWorkClass::OptionalDiagnostics
                    && decision.action == SwarmCapacityFairnessAction::Shed
                    && decision.reason_code == "optional_diagnostics.fairness.shed_under_pressure"
            }),
            "bulk optional diagnostics should be shed first under pressure: {:?}",
            plan.decisions
        );
    }

    #[test]
    fn swarm_capacity_fairness_equal_priority_service_stays_within_one_unit() {
        let policy = SwarmCapacityFairnessPolicy::new(SwarmCapacityFairnessPolicyConfig {
            enabled: true,
            admission_budget_units: 1,
            reduced_admission_budget_units: 1,
            class_policies: Vec::new(),
        });
        let mut requests = vec![
            SwarmCapacityFairnessRequest::new(
                "agent-a",
                SwarmCapacityWorkClass::ClaimedAgentTask,
                0,
            ),
            SwarmCapacityFairnessRequest::new(
                "agent-b",
                SwarmCapacityWorkClass::ClaimedAgentTask,
                1,
            ),
        ];

        for _ in 0..21 {
            let plan = policy.plan(&requests, SwarmCapacityDecisionAction::Allow);
            let admitted = plan
                .decisions
                .iter()
                .find(|decision| decision.action == SwarmCapacityFairnessAction::Admit)
                .expect("one admitted request");
            let request = requests
                .iter_mut()
                .find(|request| request.stable_id == admitted.stable_id)
                .expect("admitted request should exist");
            request.served_units = request.served_units.saturating_add(1);
        }

        let served = requests
            .iter()
            .map(|request| request.served_units)
            .collect::<Vec<_>>();
        assert!(
            served[0].abs_diff(served[1]) <= 1,
            "equal-priority work should remain fair within one unit: {served:?}"
        );
    }

    #[test]
    fn swarm_capacity_fairness_replay_is_stable_and_respects_gate_inputs() {
        let policy = SwarmCapacityFairnessPolicy::new(SwarmCapacityFairnessPolicyConfig {
            enabled: true,
            admission_budget_units: 4,
            reduced_admission_budget_units: 2,
            class_policies: Vec::new(),
        });
        let mut locked = SwarmCapacityFairnessRequest::new(
            "locked-workflow",
            SwarmCapacityWorkClass::ClaimedAgentTask,
            0,
        );
        locked.workflow_lock_available = false;
        let mut gated = SwarmCapacityFairnessRequest::new(
            "policy-gated",
            SwarmCapacityWorkClass::InteractiveOperator,
            1,
        );
        gated.policy_gate_open = false;
        let requests = vec![
            locked,
            gated,
            SwarmCapacityFairnessRequest::new(
                "operator-ok",
                SwarmCapacityWorkClass::InteractiveOperator,
                2,
            ),
        ];

        let first = policy.plan(&requests, SwarmCapacityDecisionAction::ReduceAdmission);
        let replayed = policy.plan(&requests, SwarmCapacityDecisionAction::ReduceAdmission);

        assert_eq!(first, replayed);
        assert_eq!(
            first
                .decision_for("locked-workflow")
                .expect("locked workflow")
                .reason_code,
            "claimed_agent_task.fairness.workflow_lock_unavailable"
        );
        assert_eq!(
            first
                .decision_for("policy-gated")
                .expect("policy-gated request")
                .reason_code,
            "interactive_operator.fairness.policy_gate_closed"
        );
        assert_eq!(
            first
                .decision_for("operator-ok")
                .expect("operator request")
                .action,
            SwarmCapacityFairnessAction::Admit
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn proptest_health_tier_from_ratio_thresholds(ratio in -1.0f64..2.0) {
            let expected = if ratio < 0.50 {
                HealthTier::Green
            } else if ratio < 0.80 {
                HealthTier::Yellow
            } else if ratio < 0.95 {
                HealthTier::Red
            } else {
                HealthTier::Black
            };
            prop_assert_eq!(HealthTier::from_ratio(ratio), expected);
        }

        #[test]
        fn proptest_health_tier_from_ratio_monotonic(a in -1.0f64..2.0, b in -1.0f64..2.0) {
            let (low, high) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!(
                HealthTier::from_ratio(low).severity() <= HealthTier::from_ratio(high).severity()
            );
        }

        #[test]
        fn proptest_health_tier_serde_roundtrip(tier in arb_health_tier()) {
            let json = serde_json::to_string(&tier).unwrap();
            let roundtrip: HealthTier = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(roundtrip, tier);
        }

        #[test]
        fn proptest_runtime_phase_terminal_and_shutdown_flags(phase in arb_runtime_phase()) {
            prop_assert_eq!(phase.is_terminal(), phase == RuntimePhase::Shutdown);
            prop_assert_eq!(
                phase.is_shutting_down(),
                matches!(
                    phase,
                    RuntimePhase::Draining | RuntimePhase::Finalizing | RuntimePhase::Cancelling
                )
            );
        }

        #[test]
        fn proptest_runtime_phase_serde_roundtrip(phase in arb_runtime_phase()) {
            let json = serde_json::to_string(&phase).unwrap();
            let roundtrip: RuntimePhase = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(roundtrip, phase);
        }

        #[test]
        fn proptest_runtime_telemetry_kind_display_matches_serde(kind in arb_runtime_kind()) {
            let expected = serde_json::to_value(kind)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            prop_assert_eq!(kind.to_string(), expected);
        }

        #[test]
        fn proptest_runtime_telemetry_kind_categories_are_known(kind in arb_runtime_kind()) {
            let category = kind.category();
            prop_assert!(matches!(
                category,
                "scope" | "cancellation" | "backpressure" | "queue" | "error" | "resource" | "operational"
            ));
        }

        #[test]
        fn proptest_failure_class_retryable_matches_expected_set(class in arb_failure_class()) {
            let expected = matches!(
                class,
                FailureClass::Transient | FailureClass::Degraded | FailureClass::Timeout
            );
            prop_assert_eq!(class.is_retryable(), expected);
        }

        #[test]
        fn proptest_failure_class_suggested_tier_matches_expected_mapping(class in arb_failure_class()) {
            let expected = match class {
                FailureClass::Transient | FailureClass::Timeout => HealthTier::Yellow,
                FailureClass::Degraded | FailureClass::Overload => HealthTier::Red,
                FailureClass::Corruption
                | FailureClass::Panic
                | FailureClass::Deadlock
                | FailureClass::Safety => HealthTier::Black,
                FailureClass::Permanent | FailureClass::Configuration => HealthTier::Red,
            };
            prop_assert_eq!(class.suggested_tier(), expected);
        }

        #[test]
        fn proptest_failure_class_serde_roundtrip(class in arb_failure_class()) {
            let json = serde_json::to_string(&class).unwrap();
            let roundtrip: FailureClass = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(roundtrip, class);
        }

        #[test]
        fn proptest_builder_preserves_explicit_fields(
            component in component_string(),
            scope_id in scope_id_string(),
            kind in arb_runtime_kind(),
            tier in arb_health_tier(),
            phase in arb_runtime_phase(),
            reason_code in reason_code_string(),
            correlation_id in correlation_string(),
            failure_class in arb_failure_class(),
            timestamp_ms in 1u64..u64::MAX,
            detail_text in nonempty_text_string(),
            detail_count in any::<u16>(),
            detail_flag in any::<bool>(),
        ) {
            let event = RuntimeTelemetryEventBuilder::new(&component, kind)
                .scope_id(&scope_id)
                .tier(tier)
                .phase(phase)
                .reason(&reason_code)
                .correlation(&correlation_id)
                .failure(failure_class)
                .timestamp_ms(timestamp_ms)
                .detail_str("scope_tier", &detail_text)
                .detail_u64("child_count", u64::from(detail_count))
                .detail_bool("active", detail_flag)
                .build();

            prop_assert_eq!(event.component, component);
            prop_assert_eq!(event.scope_id.as_deref(), Some(scope_id.as_str()));
            prop_assert_eq!(event.event_kind, kind);
            prop_assert_eq!(event.health_tier, tier);
            prop_assert_eq!(event.phase, phase);
            prop_assert_eq!(event.reason_code, reason_code);
            prop_assert_eq!(event.correlation_id, correlation_id);
            prop_assert_eq!(event.failure_class, Some(failure_class));
            prop_assert_eq!(event.timestamp_ms, timestamp_ms);
            prop_assert_eq!(event.details.get("scope_tier"), Some(&serde_json::json!(detail_text)));
            prop_assert_eq!(event.details.get("child_count"), Some(&serde_json::json!(u64::from(detail_count))));
            prop_assert_eq!(event.details.get("active"), Some(&serde_json::json!(detail_flag)));
        }

        #[test]
        fn proptest_builder_last_detail_write_wins(
            first in any::<u32>(),
            second in any::<u32>(),
        ) {
            let event = RuntimeTelemetryEventBuilder::new("rt.builder", RuntimeTelemetryKind::Heartbeat)
                .detail_u64("count", u64::from(first))
                .detail_u64("count", u64::from(second))
                .build();

            prop_assert_eq!(event.details.len(), 1);
            prop_assert_eq!(event.details.get("count"), Some(&serde_json::json!(u64::from(second))));
        }

        #[test]
        fn proptest_event_json_roundtrip_preserves_scalar_details(
            component in component_string(),
            reason_code in reason_code_string(),
            correlation_id in correlation_string(),
            detail_text in nonempty_text_string(),
            detail_count in any::<u16>(),
            detail_flag in any::<bool>(),
            detail_ratio in 0.0f64..10.0,
        ) {
            let event = RuntimeTelemetryEventBuilder::new(&component, RuntimeTelemetryKind::Heartbeat)
                .reason(&reason_code)
                .correlation(&correlation_id)
                .detail_str("scope_tier", &detail_text)
                .detail_u64("count", u64::from(detail_count))
                .detail_bool("active", detail_flag)
                .detail_f64("ratio", detail_ratio)
                .build();

            let json = serde_json::to_string(&event).unwrap();
            let roundtrip: RuntimeTelemetryEvent = serde_json::from_str(&json).unwrap();

            prop_assert_eq!(roundtrip.component, event.component);
            prop_assert_eq!(roundtrip.reason_code, event.reason_code);
            prop_assert_eq!(roundtrip.correlation_id, event.correlation_id);
            // Compare non-float details exactly, float with tolerance (JSON roundtrip precision).
            prop_assert_eq!(roundtrip.details.get("scope_tier"), event.details.get("scope_tier"));
            prop_assert_eq!(roundtrip.details.get("count"), event.details.get("count"));
            prop_assert_eq!(roundtrip.details.get("active"), event.details.get("active"));
            let orig = event.details.get("ratio").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let rt = roundtrip.details.get("ratio").and_then(|v| v.as_f64()).unwrap_or(0.0);
            prop_assert!((orig - rt).abs() < 1e-10, "ratio drift: {} vs {}", orig, rt);
        }

        #[test]
        fn proptest_non_empty_string_behavior(text in maybe_text_string()) {
            let expected = if text.is_empty() {
                None
            } else {
                Some(text.clone())
            };
            prop_assert_eq!(non_empty_string(&text), expected);
        }

        #[test]
        fn proptest_runtime_reason_code_fallbacks_only_on_empty(
            kind in arb_runtime_kind(),
            component in component_string(),
            correlation_id in maybe_correlation_string(),
            use_fallback in any::<bool>(),
            explicit_reason in reason_code_string(),
        ) {
            let reason_code = if use_fallback { String::new() } else { explicit_reason.clone() };
            let event = RuntimeTelemetryEvent {
                timestamp_ms: 7,
                component,
                scope_id: None,
                event_kind: kind,
                health_tier: HealthTier::Green,
                phase: RuntimePhase::Running,
                reason_code,
                correlation_id,
                failure_class: None,
                details: HashMap::new(),
            };

            let expected = if use_fallback {
                enum_string(&kind)
            } else {
                explicit_reason
            };
            prop_assert_eq!(runtime_reason_code(&event), expected);
        }

        #[test]
        fn proptest_runtime_record_id_includes_correlation_only_when_present(
            timestamp_ms in 1u64..u64::MAX,
            component in component_string(),
            reason_code in reason_code_string(),
            correlation_id in maybe_correlation_string(),
            kind in arb_runtime_kind(),
        ) {
            let event = RuntimeTelemetryEvent {
                timestamp_ms,
                component: component.clone(),
                scope_id: None,
                event_kind: kind,
                health_tier: HealthTier::Green,
                phase: RuntimePhase::Running,
                reason_code: reason_code.clone(),
                correlation_id: correlation_id.clone(),
                failure_class: None,
                details: HashMap::new(),
            };

            let expected = if correlation_id.is_empty() {
                format!("rt:{timestamp_ms}:{component}:{reason_code}")
            } else {
                format!("rt:{timestamp_ms}:{component}:{correlation_id}:{reason_code}")
            };
            prop_assert_eq!(runtime_record_id(&event), expected);
        }

        #[test]
        fn proptest_stable_hash_is_deterministic_hex(text in maybe_text_string()) {
            let hash = stable_hash(&text);
            prop_assert_eq!(hash.clone(), stable_hash(&text));
            prop_assert_eq!(hash.len(), 64);
            prop_assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
        }

        #[test]
        fn proptest_sanitize_runtime_details_preserves_safe_scalar_fields(
            queue_depth in any::<u32>(),
            active in any::<bool>(),
            scope_tier in nonempty_text_string(),
        ) {
            let details = HashMap::from([
                ("queue_depth".to_string(), serde_json::json!(queue_depth)),
                ("active".to_string(), serde_json::json!(active)),
                ("scope_tier".to_string(), serde_json::json!(scope_tier)),
            ]);

            let (attributes, sensitivity, redaction_strategy, redacted_fields) =
                sanitize_runtime_details(&details);

            prop_assert_eq!(attributes.get("queue_depth"), Some(&serde_json::json!(queue_depth)));
            prop_assert_eq!(attributes.get("active"), Some(&serde_json::json!(active)));
            prop_assert_eq!(attributes.get("scope_tier"), Some(&serde_json::json!(scope_tier)));
            prop_assert_eq!(sensitivity, DataSensitivity::Internal);
            prop_assert_eq!(redaction_strategy, RedactionStrategy::Passthrough);
            prop_assert_eq!(redacted_fields, 0);
        }

        #[test]
        fn proptest_sanitize_runtime_details_masks_unknown_complex_fields(
            payload in arb_unknown_complex_value(),
        ) {
            let details = HashMap::from([
                ("custom_payload".to_string(), payload),
                ("queue_depth".to_string(), serde_json::json!(7)),
            ]);

            let (attributes, sensitivity, redaction_strategy, redacted_fields) =
                sanitize_runtime_details(&details);

            prop_assert_eq!(attributes.get("custom_payload"), Some(&serde_json::json!("[REDACTED]")));
            prop_assert_eq!(attributes.get("queue_depth"), Some(&serde_json::json!(7)));
            prop_assert_eq!(sensitivity, DataSensitivity::Confidential);
            prop_assert_eq!(redaction_strategy, RedactionStrategy::Mask);
            prop_assert_eq!(redacted_fields, 1);
        }

        #[test]
        fn proptest_sanitize_runtime_details_always_redacts_sensitive_keys(
            key in arb_sensitive_key(),
            value in nonempty_text_string(),
        ) {
            let details = HashMap::from([(key.to_string(), serde_json::json!(value))]);
            let (attributes, sensitivity, redaction_strategy, redacted_fields) =
                sanitize_runtime_details(&details);

            prop_assert_eq!(attributes.get(key), Some(&serde_json::json!("[REDACTED]")));
            prop_assert_eq!(sensitivity, DataSensitivity::Restricted);
            prop_assert_eq!(redaction_strategy, RedactionStrategy::Mask);
            prop_assert_eq!(redacted_fields, 1);
        }

        #[test]
        fn proptest_log_append_sequence_and_counts(kinds in proptest::collection::vec(arb_runtime_kind(), 0..40)) {
            let mut log = RuntimeTelemetryLog::with_defaults();
            let mut last_sequence = 0u64;

            for (idx, kind) in kinds.iter().enumerate() {
                let sequence = log.emit(
                    RuntimeTelemetryEventBuilder::new("rt.log", *kind)
                        .reason(&format!("ops.running.event_{idx}"))
                );
                prop_assert!(sequence > last_sequence);
                last_sequence = sequence;
            }

            prop_assert_eq!(log.total_emitted(), kinds.len() as u64);
            prop_assert_eq!(log.sequence(), kinds.len() as u64);
            prop_assert_eq!(log.len(), kinds.len());
            prop_assert_eq!(log.total_evicted(), 0);
        }

        #[test]
        fn proptest_log_capacity_bound_and_eviction_count(
            max_events in 1usize..12usize,
            kinds in proptest::collection::vec(arb_runtime_kind(), 0..40),
        ) {
            let mut log = RuntimeTelemetryLog::new(RuntimeTelemetryLogConfig {
                max_events,
                enabled: true,
            });

            for (idx, kind) in kinds.iter().enumerate() {
                log.emit(
                    RuntimeTelemetryEventBuilder::new("rt.log", *kind)
                        .reason(&format!("ops.running.event_{idx}"))
                );
            }

            let expected_len = kinds.len().min(max_events);
            let expected_evicted = kinds.len().saturating_sub(max_events) as u64;

            prop_assert_eq!(log.len(), expected_len);
            prop_assert_eq!(log.total_emitted(), kinds.len() as u64);
            prop_assert_eq!(log.total_evicted(), expected_evicted);
            prop_assert_eq!(log.sequence(), kinds.len() as u64);
        }

        #[test]
        fn proptest_log_snapshot_counts_match_events(
            entries in proptest::collection::vec((arb_runtime_kind(), arb_health_tier()), 0..40),
        ) {
            let mut log = RuntimeTelemetryLog::with_defaults();
            let mut expected_tiers = [0u64; 4];
            let mut expected_categories: HashMap<String, u64> = HashMap::new();

            for (idx, (kind, tier)) in entries.iter().enumerate() {
                log.emit(
                    RuntimeTelemetryEventBuilder::new("rt.snapshot", *kind)
                        .tier(*tier)
                        .reason(&format!("ops.running.snapshot_{idx}"))
                );
                expected_tiers[tier.severity() as usize] += 1;
                *expected_categories.entry(kind.category().to_string()).or_default() += 1;
            }

            let snapshot = log.snapshot();
            prop_assert_eq!(snapshot.buffered_events, entries.len() as u64);
            prop_assert_eq!(snapshot.total_emitted, entries.len() as u64);
            prop_assert_eq!(snapshot.total_evicted, 0);
            prop_assert_eq!(snapshot.sequence, entries.len() as u64);
            prop_assert_eq!(snapshot.tier_counts, expected_tiers);
            prop_assert_eq!(snapshot.category_counts, expected_categories);
        }

        #[test]
        fn proptest_export_jsonl_line_count_matches_buffer(
            entries in proptest::collection::vec((arb_runtime_kind(), arb_health_tier()), 0..20),
        ) {
            let mut log = RuntimeTelemetryLog::with_defaults();

            for (idx, (kind, tier)) in entries.iter().enumerate() {
                log.emit(
                    RuntimeTelemetryEventBuilder::new("rt.export", *kind)
                        .tier(*tier)
                        .reason(&format!("ops.running.export_{idx}"))
                );
            }

            let exported = log.export_jsonl();
            if entries.is_empty() {
                prop_assert!(exported.is_empty());
            } else {
                let lines = exported.lines().collect::<Vec<_>>();
                prop_assert_eq!(lines.len(), entries.len());
                for line in lines {
                    let parsed: RuntimeTelemetryEvent = serde_json::from_str(line).unwrap();
                    prop_assert!(parsed.component.starts_with("rt.export"));
                }
            }
        }

        #[test]
        fn proptest_tier_transition_flags_match_direction(
            from in arb_health_tier(),
            to in arb_health_tier(),
            timestamp_ms in 0u64..u64::MAX,
            duration_in_previous_ms in 0u64..u64::MAX,
            component in component_string(),
            reason_code in reason_code_string(),
        ) {
            let record = TierTransitionRecord {
                timestamp_ms,
                component,
                from,
                to,
                reason_code,
                duration_in_previous_ms,
            };

            prop_assert_eq!(record.is_escalation(), to > from);
            prop_assert_eq!(record.is_recovery(), to < from);
        }

        #[test]
        fn proptest_tier_transition_to_event_preserves_context(
            from in arb_health_tier(),
            to in arb_health_tier(),
            timestamp_ms in 0u64..u64::MAX,
            duration_in_previous_ms in 0u64..u64::MAX,
            component in component_string(),
            reason_code in reason_code_string(),
            correlation_id in correlation_string(),
        ) {
            let record = TierTransitionRecord {
                timestamp_ms,
                component: component.clone(),
                from,
                to,
                reason_code: reason_code.clone(),
                duration_in_previous_ms,
            };

            let event = record.to_event(&correlation_id);

            prop_assert_eq!(event.timestamp_ms, timestamp_ms);
            prop_assert_eq!(event.component, component);
            prop_assert_eq!(event.event_kind, RuntimeTelemetryKind::TierTransition);
            prop_assert_eq!(event.health_tier, to);
            prop_assert_eq!(event.phase, RuntimePhase::Running);
            prop_assert_eq!(event.reason_code, reason_code);
            prop_assert_eq!(event.correlation_id, correlation_id);
            prop_assert_eq!(event.details.get("tier_from"), Some(&serde_json::json!(from.to_string())));
            prop_assert_eq!(event.details.get("tier_to"), Some(&serde_json::json!(to.to_string())));
            prop_assert_eq!(
                event.details.get("duration_in_previous_ms"),
                Some(&serde_json::json!(duration_in_previous_ms))
            );
        }
    }

    // ── HealthTier ──

    #[test]
    fn health_tier_severity_ordering() {
        assert_eq!(HealthTier::Green.severity(), 0);
        assert_eq!(HealthTier::Yellow.severity(), 1);
        assert_eq!(HealthTier::Red.severity(), 2);
        assert_eq!(HealthTier::Black.severity(), 3);
    }

    #[test]
    fn health_tier_comparison() {
        assert!(HealthTier::Green < HealthTier::Yellow);
        assert!(HealthTier::Yellow < HealthTier::Red);
        assert!(HealthTier::Red < HealthTier::Black);
    }

    #[test]
    fn health_tier_requires_attention() {
        assert!(!HealthTier::Green.requires_attention());
        assert!(!HealthTier::Yellow.requires_attention());
        assert!(HealthTier::Red.requires_attention());
        assert!(HealthTier::Black.requires_attention());
    }

    #[test]
    fn health_tier_is_degraded() {
        assert!(!HealthTier::Green.is_degraded());
        assert!(HealthTier::Yellow.is_degraded());
        assert!(HealthTier::Red.is_degraded());
        assert!(HealthTier::Black.is_degraded());
    }

    #[test]
    fn health_tier_from_ratio() {
        assert_eq!(HealthTier::from_ratio(0.0), HealthTier::Green);
        assert_eq!(HealthTier::from_ratio(0.49), HealthTier::Green);
        assert_eq!(HealthTier::from_ratio(0.50), HealthTier::Yellow);
        assert_eq!(HealthTier::from_ratio(0.79), HealthTier::Yellow);
        assert_eq!(HealthTier::from_ratio(0.80), HealthTier::Red);
        assert_eq!(HealthTier::from_ratio(0.94), HealthTier::Red);
        assert_eq!(HealthTier::from_ratio(0.95), HealthTier::Black);
        assert_eq!(HealthTier::from_ratio(1.0), HealthTier::Black);
    }

    #[test]
    fn health_tier_display() {
        assert_eq!(HealthTier::Green.to_string(), "green");
        assert_eq!(HealthTier::Yellow.to_string(), "yellow");
        assert_eq!(HealthTier::Red.to_string(), "red");
        assert_eq!(HealthTier::Black.to_string(), "black");
    }

    #[test]
    fn health_tier_serde_roundtrip() {
        for tier in [
            HealthTier::Green,
            HealthTier::Yellow,
            HealthTier::Red,
            HealthTier::Black,
        ] {
            let json = serde_json::to_string(&tier).unwrap();
            let rt: HealthTier = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, tier);
        }
    }

    #[test]
    fn health_tier_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(HealthTier::Green).unwrap(),
            serde_json::json!("green")
        );
        assert_eq!(
            serde_json::to_value(HealthTier::Black).unwrap(),
            serde_json::json!("black")
        );
    }

    // ── RuntimePhase ──

    #[test]
    fn runtime_phase_terminal() {
        assert!(RuntimePhase::Shutdown.is_terminal());
        assert!(!RuntimePhase::Running.is_terminal());
        assert!(!RuntimePhase::Draining.is_terminal());
    }

    #[test]
    fn runtime_phase_shutting_down() {
        assert!(RuntimePhase::Draining.is_shutting_down());
        assert!(RuntimePhase::Finalizing.is_shutting_down());
        assert!(RuntimePhase::Cancelling.is_shutting_down());
        assert!(!RuntimePhase::Running.is_shutting_down());
        assert!(!RuntimePhase::Init.is_shutting_down());
        assert!(!RuntimePhase::Shutdown.is_shutting_down());
    }

    #[test]
    fn runtime_phase_display() {
        assert_eq!(RuntimePhase::Init.to_string(), "init");
        assert_eq!(RuntimePhase::Running.to_string(), "running");
        assert_eq!(RuntimePhase::Draining.to_string(), "draining");
        assert_eq!(RuntimePhase::Shutdown.to_string(), "shutdown");
    }

    #[test]
    fn runtime_phase_serde_roundtrip() {
        for phase in [
            RuntimePhase::Init,
            RuntimePhase::Startup,
            RuntimePhase::Running,
            RuntimePhase::Draining,
            RuntimePhase::Finalizing,
            RuntimePhase::Shutdown,
            RuntimePhase::Cancelling,
            RuntimePhase::Recovering,
            RuntimePhase::Maintenance,
        ] {
            let json = serde_json::to_string(&phase).unwrap();
            let rt: RuntimePhase = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, phase);
        }
    }

    // ── RuntimeTelemetryKind ──

    #[test]
    fn event_kind_categories() {
        assert_eq!(RuntimeTelemetryKind::ScopeCreated.category(), "scope");
        assert_eq!(RuntimeTelemetryKind::ScopeStarted.category(), "scope");
        assert_eq!(RuntimeTelemetryKind::ScopeClosed.category(), "scope");

        assert_eq!(
            RuntimeTelemetryKind::CancellationRequested.category(),
            "cancellation"
        );
        assert_eq!(
            RuntimeTelemetryKind::GracePeriodExpired.category(),
            "cancellation"
        );

        assert_eq!(
            RuntimeTelemetryKind::TierTransition.category(),
            "backpressure"
        );
        assert_eq!(
            RuntimeTelemetryKind::ThrottleApplied.category(),
            "backpressure"
        );

        assert_eq!(RuntimeTelemetryKind::QueueDepthObserved.category(), "queue");
        assert_eq!(RuntimeTelemetryKind::ChannelClosed.category(), "queue");

        assert_eq!(RuntimeTelemetryKind::TransientError.category(), "error");
        assert_eq!(RuntimeTelemetryKind::PanicCaptured.category(), "error");

        assert_eq!(
            RuntimeTelemetryKind::ResourceObserved.category(),
            "resource"
        );

        assert_eq!(
            RuntimeTelemetryKind::SloMeasurement.category(),
            "operational"
        );
        assert_eq!(RuntimeTelemetryKind::Heartbeat.category(), "operational");
    }

    #[test]
    fn event_kind_serde_snake_case() {
        let json = serde_json::to_value(RuntimeTelemetryKind::ScopeCreated).unwrap();
        assert_eq!(json, serde_json::json!("scope_created"));

        let json = serde_json::to_value(RuntimeTelemetryKind::CancellationRequested).unwrap();
        assert_eq!(json, serde_json::json!("cancellation_requested"));

        let json = serde_json::to_value(RuntimeTelemetryKind::TierTransition).unwrap();
        assert_eq!(json, serde_json::json!("tier_transition"));

        let json = serde_json::to_value(RuntimeTelemetryKind::LoadShedding).unwrap();
        assert_eq!(json, serde_json::json!("load_shedding"));
    }

    #[test]
    fn event_kind_display() {
        assert_eq!(
            RuntimeTelemetryKind::ScopeCreated.to_string(),
            "scope_created"
        );
        assert_eq!(
            RuntimeTelemetryKind::TierTransition.to_string(),
            "tier_transition"
        );
    }

    // ── FailureClass ──

    #[test]
    fn failure_class_retryable() {
        assert!(FailureClass::Transient.is_retryable());
        assert!(FailureClass::Degraded.is_retryable());
        assert!(FailureClass::Timeout.is_retryable());
        assert!(!FailureClass::Permanent.is_retryable());
        assert!(!FailureClass::Corruption.is_retryable());
        assert!(!FailureClass::Panic.is_retryable());
    }

    #[test]
    fn failure_class_suggested_tier() {
        assert_eq!(FailureClass::Transient.suggested_tier(), HealthTier::Yellow);
        assert_eq!(FailureClass::Timeout.suggested_tier(), HealthTier::Yellow);
        assert_eq!(FailureClass::Degraded.suggested_tier(), HealthTier::Red);
        assert_eq!(FailureClass::Overload.suggested_tier(), HealthTier::Red);
        assert_eq!(FailureClass::Panic.suggested_tier(), HealthTier::Black);
        assert_eq!(FailureClass::Corruption.suggested_tier(), HealthTier::Black);
        assert_eq!(FailureClass::Safety.suggested_tier(), HealthTier::Black);
    }

    #[test]
    fn failure_class_serde_roundtrip() {
        for fc in [
            FailureClass::Transient,
            FailureClass::Permanent,
            FailureClass::Degraded,
            FailureClass::Overload,
            FailureClass::Corruption,
            FailureClass::Timeout,
            FailureClass::Panic,
            FailureClass::Deadlock,
            FailureClass::Safety,
            FailureClass::Configuration,
        ] {
            let json = serde_json::to_string(&fc).unwrap();
            let rt: FailureClass = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, fc);
        }
    }

    #[test]
    fn failure_class_display() {
        assert_eq!(FailureClass::Transient.to_string(), "transient");
        assert_eq!(FailureClass::Corruption.to_string(), "corruption");
        assert_eq!(FailureClass::Configuration.to_string(), "configuration");
    }

    // ── RuntimeTelemetryEvent builder ──

    #[test]
    fn builder_produces_valid_event() {
        let event =
            RuntimeTelemetryEventBuilder::new("rt.scope", RuntimeTelemetryKind::ScopeStarted)
                .scope_id("daemon:capture")
                .tier(HealthTier::Green)
                .phase(RuntimePhase::Startup)
                .reason("scope.startup.started")
                .correlation("cycle-42")
                .detail_str("scope_tier", "daemon")
                .detail_u64("child_count", 3)
                .build();

        assert_eq!(event.component, "rt.scope");
        assert_eq!(event.scope_id, Some("daemon:capture".to_string()));
        assert_eq!(event.event_kind, RuntimeTelemetryKind::ScopeStarted);
        assert_eq!(event.health_tier, HealthTier::Green);
        assert_eq!(event.phase, RuntimePhase::Startup);
        assert_eq!(event.reason_code, "scope.startup.started");
        assert_eq!(event.correlation_id, "cycle-42");
        assert!(event.failure_class.is_none());
        assert_eq!(
            event.details.get("scope_tier"),
            Some(&serde_json::json!("daemon"))
        );
        assert_eq!(
            event.details.get("child_count"),
            Some(&serde_json::json!(3))
        );
    }

    #[test]
    fn builder_with_failure_class() {
        let event =
            RuntimeTelemetryEventBuilder::new("rt.error", RuntimeTelemetryKind::PanicCaptured)
                .failure(FailureClass::Panic)
                .tier(HealthTier::Black)
                .reason("error.running.panic_captured")
                .detail_str("error_message", "index out of bounds")
                .build();

        assert_eq!(event.failure_class, Some(FailureClass::Panic));
        assert_eq!(event.health_tier, HealthTier::Black);
    }

    #[test]
    fn builder_defaults() {
        let event =
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat).build();

        assert_eq!(event.health_tier, HealthTier::Green);
        assert_eq!(event.phase, RuntimePhase::Running);
        assert!(event.scope_id.is_none());
        assert!(event.failure_class.is_none());
        assert!(event.details.is_empty());
        assert!(event.timestamp_ms > 0);
    }

    #[test]
    fn event_json_roundtrip() {
        let event =
            RuntimeTelemetryEventBuilder::new("rt.scope", RuntimeTelemetryKind::ScopeCreated)
                .scope_id("daemon:capture")
                .tier(HealthTier::Yellow)
                .phase(RuntimePhase::Startup)
                .reason("scope.init.created")
                .correlation("test-123")
                .failure(FailureClass::Transient)
                .detail_str("key", "value")
                .detail_u64("count", 42)
                .detail_f64("ratio", 0.75)
                .detail_bool("active", true)
                .build();

        let json_str = serde_json::to_string(&event).unwrap();
        let roundtripped: RuntimeTelemetryEvent = serde_json::from_str(&json_str).unwrap();

        assert_eq!(roundtripped.component, event.component);
        assert_eq!(roundtripped.scope_id, event.scope_id);
        assert_eq!(roundtripped.event_kind, event.event_kind);
        assert_eq!(roundtripped.health_tier, event.health_tier);
        assert_eq!(roundtripped.phase, event.phase);
        assert_eq!(roundtripped.reason_code, event.reason_code);
        assert_eq!(roundtripped.correlation_id, event.correlation_id);
        assert_eq!(roundtripped.failure_class, event.failure_class);
        assert_eq!(roundtripped.details.get("key"), event.details.get("key"));
        assert_eq!(
            roundtripped.details.get("count"),
            event.details.get("count")
        );
        assert_eq!(
            roundtripped.details.get("active"),
            event.details.get("active")
        );
    }

    #[test]
    fn event_json_has_required_fields() {
        let event = RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
            .reason("ops.running.heartbeat")
            .correlation("hb-1")
            .build();

        let json: serde_json::Value = serde_json::to_value(&event).unwrap();

        // All required fields present
        assert!(json.get("timestamp_ms").is_some());
        assert!(json.get("component").is_some());
        assert!(json.get("event_kind").is_some());
        assert!(json.get("health_tier").is_some());
        assert!(json.get("phase").is_some());
        assert!(json.get("reason_code").is_some());
        assert!(json.get("correlation_id").is_some());

        // Enums are strings
        assert!(json["event_kind"].is_string());
        assert!(json["health_tier"].is_string());
        assert!(json["phase"].is_string());
    }

    #[test]
    fn event_json_omits_none_fields() {
        let event =
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat).build();

        let json: serde_json::Value = serde_json::to_value(&event).unwrap();

        // Optional fields not present when None/empty
        assert!(json.get("scope_id").is_none());
        assert!(json.get("failure_class").is_none());
        assert!(json.get("details").is_none());
    }

    // ── UnifiedTelemetryRecord ──

    #[test]
    fn unified_runtime_record_redacts_sensitive_details() {
        let event =
            RuntimeTelemetryEventBuilder::new("rt.error", RuntimeTelemetryKind::TransientError)
                .scope_id("pane:7")
                .timestamp_ms(1111)
                .tier(HealthTier::Yellow)
                .phase(RuntimePhase::Recovering)
                .reason("error.recovering.transient")
                .correlation("corr-123")
                .failure(FailureClass::Transient)
                .detail_u64("queue_depth", 9)
                .detail_str("error_message", "token=super-secret")
                .detail_str("custom_context", "raw-user-content")
                .build();

        let record = UnifiedTelemetryRecord::from(&event);

        assert_eq!(record.source, UnifiedTelemetrySource::Runtime);
        assert_eq!(
            record.record_id,
            "rt:1111:rt.error:corr-123:error.recovering.transient"
        );
        assert_eq!(record.reason_code, "error.recovering.transient");
        assert_eq!(record.correlation_id.as_deref(), Some("corr-123"));
        assert_eq!(record.scope_id.as_deref(), Some("pane:7"));
        assert_eq!(record.health_tier, HealthTier::Yellow);
        assert_eq!(record.failure_class, Some(FailureClass::Transient));
        assert_eq!(record.sensitivity, DataSensitivity::Restricted);
        assert_eq!(record.redaction_strategy, RedactionStrategy::Mask);
        assert_eq!(
            record.attributes.get("queue_depth"),
            Some(&serde_json::json!(9))
        );
        assert_eq!(
            record.attributes.get("error_message"),
            Some(&serde_json::json!("[REDACTED]"))
        );
        assert_eq!(
            record.attributes.get("custom_context"),
            Some(&serde_json::json!("[REDACTED]"))
        );
    }

    #[test]
    fn unified_connector_record_maps_failure_and_payload_redaction() {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Outbound,
            "github",
            "outbound.notify",
            serde_json::json!({
                "token": "secret",
                "attempts": 2,
            }),
        )
        .with_event_id("evt-42")
        .with_correlation_id("corr-456")
        .with_timestamp_ms(2222)
        .with_severity(CanonicalSeverity::Critical)
        .with_failure(ConnectorFailureClass::Policy)
        .with_workflow_id("wf-9")
        .with_sandbox("zone-a", ConnectorCapability::SecretBroker)
        .with_connector_name("GitHub")
        .with_metadata("attempt", "2");

        let record = UnifiedTelemetryRecord::from(&event);

        assert_eq!(record.source, UnifiedTelemetrySource::Connector);
        assert_eq!(record.record_id, "evt-42");
        assert_eq!(record.source_schema_version.as_deref(), Some("1.0"));
        assert_eq!(record.component, "connector.github");
        assert_eq!(record.reason_code, "outbound.notify");
        assert_eq!(record.correlation_id.as_deref(), Some("corr-456"));
        assert_eq!(record.scope_id.as_deref(), Some("workflow:wf-9"));
        assert_eq!(record.health_tier, HealthTier::Black);
        assert_eq!(record.failure_class, Some(FailureClass::Safety));
        assert_eq!(record.sensitivity, DataSensitivity::Restricted);
        assert_eq!(record.redaction_strategy, RedactionStrategy::Remove);
        assert_eq!(
            record.attributes.get("payload_redacted"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            record.attributes.get("payload_field_count"),
            Some(&serde_json::json!(2))
        );
        assert_eq!(
            record.attributes.get("metadata_keys"),
            Some(&serde_json::json!(["attempt"]))
        );
    }

    #[test]
    fn unified_policy_record_uses_access_tier_and_safe_correlation() {
        let log = AuditLog::new(AuditLogConfig::default());
        let entry = log.append(
            AuditEventBuilder::new(
                AuditEventType::RecorderQueryPrivileged,
                ActorIdentity::new(ActorKind::Human, "session-123"),
                3333,
            )
            .with_decision(AuthzDecision::Deny)
            .with_pane_ids(vec![7])
            .with_query("secret")
            .with_result_count(4)
            .with_justification("triage")
            .with_details(serde_json::json!({
                "correlation_id": "corr-789",
                "raw": "keep out",
            })),
        );

        let record = UnifiedTelemetryRecord::from(&entry);

        assert_eq!(record.source, UnifiedTelemetrySource::Policy);
        assert_eq!(record.record_id, "audit-0");
        assert_eq!(
            record.source_schema_version.as_deref(),
            Some(AUDIT_SCHEMA_VERSION)
        );
        assert_eq!(record.component, "policy.recorder_audit");
        assert_eq!(record.reason_code, "policy.audit.recorder_query_privileged");
        assert_eq!(record.correlation_id.as_deref(), Some("corr-789"));
        assert_eq!(record.scope_id.as_deref(), Some("pane:7"));
        assert_eq!(record.health_tier, HealthTier::Red);
        assert_eq!(record.failure_class, Some(FailureClass::Safety));
        assert_eq!(record.sensitivity, DataSensitivity::Restricted);
        assert_eq!(record.redaction_strategy, RedactionStrategy::Remove);
        assert_eq!(
            record.attributes.get("query_redacted"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            record.attributes.get("query_length"),
            Some(&serde_json::json!(6))
        );
        assert_eq!(
            record.attributes.get("justification_length"),
            Some(&serde_json::json!(6))
        );
        assert!(record.attributes.contains_key("actor_identity_hash"));
    }

    #[test]
    fn unified_connector_broker_audit_hashes_ids_and_redacts_detail() {
        let event = CredentialAuditEvent {
            timestamp_ms: 4444,
            event_type: CredentialAuditType::LeaseIssued,
            credential_id: "cred-1".to_string(),
            connector_id: Some("slack-sync".to_string()),
            lease_id: Some("lease-9".to_string()),
            detail: "lease issued, expires at 9999".to_string(),
        };

        let record = UnifiedTelemetryRecord::from(&event);

        assert_eq!(record.source, UnifiedTelemetrySource::Connector);
        assert_eq!(record.component, "connector.credential_broker");
        assert_eq!(
            record.reason_code,
            "connector.credential_broker.lease_issued"
        );
        assert_eq!(
            record.record_id,
            format!("broker-audit:4444:{}", stable_hash("lease-9"))
        );
        assert_eq!(record.correlation_id, Some(stable_hash("lease-9")));
        assert_eq!(
            record.scope_id,
            Some(format!("connector:{}", stable_hash("slack-sync")))
        );
        assert_eq!(record.health_tier, HealthTier::Green);
        assert_eq!(record.failure_class, None);
        assert_eq!(record.sensitivity, DataSensitivity::Restricted);
        assert_eq!(record.redaction_strategy, RedactionStrategy::Remove);
        assert_eq!(
            record.attributes.get("credential_id_hash"),
            Some(&serde_json::json!(stable_hash("cred-1")))
        );
        assert_eq!(
            record.attributes.get("connector_id_hash"),
            Some(&serde_json::json!(stable_hash("slack-sync")))
        );
        assert_eq!(
            record.attributes.get("lease_id_hash"),
            Some(&serde_json::json!(stable_hash("lease-9")))
        );
        assert_eq!(
            record.attributes.get("detail_redacted"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn unified_connector_broker_access_denied_maps_to_safety() {
        let event = CredentialAuditEvent {
            timestamp_ms: 5555,
            event_type: CredentialAuditType::AccessDenied,
            credential_id: "cred-2".to_string(),
            connector_id: Some("github-sync".to_string()),
            lease_id: None,
            detail: "connector github-sync denied access to cred-2".to_string(),
        };

        let record = UnifiedTelemetryRecord::from(&event);

        assert_eq!(record.health_tier, HealthTier::Black);
        assert_eq!(record.failure_class, Some(FailureClass::Safety));
        assert_eq!(record.sensitivity, DataSensitivity::Restricted);
        assert_eq!(record.redaction_strategy, RedactionStrategy::Remove);
        assert_eq!(record.correlation_id, Some(stable_hash("github-sync")));
    }

    #[test]
    fn unified_connector_broker_snapshot_preserves_safe_counts() {
        let snapshot = CredentialBrokerTelemetrySnapshot {
            captured_at_ms: 6666,
            counters: frankenterm_core_connector_types::CredentialBrokerTelemetry {
                leases_issued: 4,
                leases_expired: 1,
                leases_revoked: 2,
                access_denied: 3,
                rotations_completed: 5,
                rotations_failed: 0,
                credentials_registered: 6,
                credentials_revoked: 1,
                providers_registered: 2,
            },
            active_leases: 2,
            active_credentials: 3,
            active_providers: 1,
        };

        let record = UnifiedTelemetryRecord::from(&snapshot);

        assert_eq!(record.source, UnifiedTelemetrySource::Connector);
        assert_eq!(record.component, "connector.credential_broker");
        assert_eq!(record.record_id, "broker-snapshot:6666");
        assert_eq!(record.reason_code, "connector.credential_broker.snapshot");
        assert_eq!(
            record.scope_id.as_deref(),
            Some("connector:credential_broker")
        );
        assert_eq!(record.health_tier, HealthTier::Green);
        assert_eq!(record.failure_class, None);
        assert_eq!(record.sensitivity, DataSensitivity::Confidential);
        assert_eq!(record.redaction_strategy, RedactionStrategy::Passthrough);
        assert_eq!(
            record.attributes.get("active_leases"),
            Some(&serde_json::json!(2))
        );
        assert_eq!(
            record.attributes.get("access_denied"),
            Some(&serde_json::json!(3))
        );
    }

    // ── RuntimeTelemetryLog ──

    #[test]
    fn log_append_and_retrieve() {
        let mut log = RuntimeTelemetryLog::with_defaults();

        let seq = log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                .reason("test"),
        );
        assert_eq!(seq, 1);
        assert_eq!(log.len(), 1);
        assert_eq!(log.total_emitted(), 1);
        assert_eq!(log.total_evicted(), 0);
    }

    #[test]
    fn log_fifo_eviction() {
        let mut log = RuntimeTelemetryLog::new(RuntimeTelemetryLogConfig {
            max_events: 3,
            enabled: true,
        });

        for i in 0..5 {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("event_{i}")),
            );
        }

        assert_eq!(log.len(), 3);
        assert_eq!(log.total_emitted(), 5);
        assert_eq!(log.total_evicted(), 2);

        // Oldest events evicted — remaining are events 2,3,4
        assert_eq!(log.events()[0].reason_code, "event_2");
        assert_eq!(log.events()[1].reason_code, "event_3");
        assert_eq!(log.events()[2].reason_code, "event_4");
    }

    #[test]
    fn log_disabled_does_not_collect() {
        let mut log = RuntimeTelemetryLog::new(RuntimeTelemetryLogConfig {
            max_events: 100,
            enabled: false,
        });

        let seq = log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                .reason("ignored"),
        );

        assert_eq!(seq, 0);
        assert!(log.is_empty());
        assert_eq!(log.total_emitted(), 0);
    }

    #[test]
    fn log_drain() {
        let mut log = RuntimeTelemetryLog::with_defaults();

        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                .reason("a"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                .reason("b"),
        );

        let drained = log.drain();
        assert_eq!(drained.len(), 2);
        assert!(log.is_empty());
        assert_eq!(log.total_emitted(), 2);
    }

    #[test]
    fn log_filter_by_kind() {
        let mut log = RuntimeTelemetryLog::with_defaults();

        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.scope", RuntimeTelemetryKind::ScopeCreated)
                .reason("a"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                .reason("b"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.scope", RuntimeTelemetryKind::ScopeCreated)
                .reason("c"),
        );

        let scope_events = log.filter_by_kind(RuntimeTelemetryKind::ScopeCreated);
        assert_eq!(scope_events.len(), 2);

        let hb_events = log.filter_by_kind(RuntimeTelemetryKind::Heartbeat);
        assert_eq!(hb_events.len(), 1);
    }

    #[test]
    fn log_filter_by_tier() {
        let mut log = RuntimeTelemetryLog::with_defaults();

        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::TierTransition)
                .tier(HealthTier::Yellow)
                .reason("a"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                .tier(HealthTier::Green)
                .reason("b"),
        );

        let yellow = log.filter_by_tier(HealthTier::Yellow);
        assert_eq!(yellow.len(), 1);
        assert_eq!(log.count_tier(HealthTier::Green), 1);
    }

    #[test]
    fn log_filter_by_component() {
        let mut log = RuntimeTelemetryLog::with_defaults();

        log.emit(
            RuntimeTelemetryEventBuilder::new(
                "rt.scope.capture",
                RuntimeTelemetryKind::ScopeStarted,
            )
            .reason("a"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new(
                "rt.backpressure",
                RuntimeTelemetryKind::ThrottleApplied,
            )
            .reason("b"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.scope.relay", RuntimeTelemetryKind::ScopeStarted)
                .reason("c"),
        );

        let scope_events = log.filter_by_component("rt.scope");
        assert_eq!(scope_events.len(), 2);
    }

    #[test]
    fn log_filter_by_correlation() {
        let mut log = RuntimeTelemetryLog::with_defaults();

        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::ScopeCreated)
                .correlation("cycle-1")
                .reason("a"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::ScopeStarted)
                .correlation("cycle-1")
                .reason("b"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::ScopeCreated)
                .correlation("cycle-2")
                .reason("c"),
        );

        let cycle1 = log.filter_by_correlation("cycle-1");
        assert_eq!(cycle1.len(), 2);
    }

    #[test]
    fn log_count_category() {
        let mut log = RuntimeTelemetryLog::with_defaults();

        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.scope", RuntimeTelemetryKind::ScopeCreated)
                .reason("a"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.scope", RuntimeTelemetryKind::ScopeStarted)
                .reason("b"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.bp", RuntimeTelemetryKind::TierTransition)
                .reason("c"),
        );

        assert_eq!(log.count_category("scope"), 2);
        assert_eq!(log.count_category("backpressure"), 1);
        assert_eq!(log.count_category("error"), 0);
    }

    #[test]
    fn log_export_jsonl() {
        let mut log = RuntimeTelemetryLog::with_defaults();

        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.a", RuntimeTelemetryKind::Heartbeat)
                .reason("first"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.b", RuntimeTelemetryKind::Heartbeat)
                .reason("second"),
        );

        let jsonl = log.export_jsonl();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 2);

        // Each line is valid JSON
        for line in &lines {
            let parsed: RuntimeTelemetryEvent = serde_json::from_str(line).unwrap();
            assert!(parsed.timestamp_ms > 0);
        }
    }

    #[test]
    fn log_snapshot() {
        let mut log = RuntimeTelemetryLog::with_defaults();

        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.scope", RuntimeTelemetryKind::ScopeCreated)
                .tier(HealthTier::Green)
                .reason("a"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.bp", RuntimeTelemetryKind::TierTransition)
                .tier(HealthTier::Yellow)
                .reason("b"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.err", RuntimeTelemetryKind::PanicCaptured)
                .tier(HealthTier::Black)
                .reason("c"),
        );

        let snap = log.snapshot();
        assert_eq!(snap.buffered_events, 3);
        assert_eq!(snap.total_emitted, 3);
        assert_eq!(snap.total_evicted, 0);
        assert_eq!(snap.tier_counts[0], 1); // green
        assert_eq!(snap.tier_counts[1], 1); // yellow
        assert_eq!(snap.tier_counts[3], 1); // black
        assert_eq!(snap.category_counts.get("scope"), Some(&1));
        assert_eq!(snap.category_counts.get("backpressure"), Some(&1));
        assert_eq!(snap.category_counts.get("error"), Some(&1));
    }

    // ── TierTransitionRecord ──

    #[test]
    fn tier_transition_escalation_recovery() {
        let escalation = TierTransitionRecord {
            timestamp_ms: 1000,
            component: "rt.backpressure".into(),
            from: HealthTier::Green,
            to: HealthTier::Yellow,
            reason_code: reason_codes::BACKPRESSURE_TIER_YELLOW.into(),
            duration_in_previous_ms: 5000,
        };
        assert!(escalation.is_escalation());
        assert!(!escalation.is_recovery());

        let recovery = TierTransitionRecord {
            timestamp_ms: 2000,
            component: "rt.backpressure".into(),
            from: HealthTier::Yellow,
            to: HealthTier::Green,
            reason_code: reason_codes::BACKPRESSURE_TIER_GREEN.into(),
            duration_in_previous_ms: 3000,
        };
        assert!(!recovery.is_escalation());
        assert!(recovery.is_recovery());
    }

    #[test]
    fn tier_transition_to_event() {
        let record = TierTransitionRecord {
            timestamp_ms: 1000,
            component: "rt.backpressure".into(),
            from: HealthTier::Green,
            to: HealthTier::Red,
            reason_code: reason_codes::BACKPRESSURE_TIER_RED.into(),
            duration_in_previous_ms: 5000,
        };

        let event = record.to_event("cycle-99");
        assert_eq!(event.event_kind, RuntimeTelemetryKind::TierTransition);
        assert_eq!(event.health_tier, HealthTier::Red);
        assert_eq!(event.correlation_id, "cycle-99");
        assert_eq!(
            event.details.get("tier_from"),
            Some(&serde_json::json!("green"))
        );
        assert_eq!(
            event.details.get("tier_to"),
            Some(&serde_json::json!("red"))
        );
    }

    // ── ScopeTelemetryEmitter ──

    #[test]
    fn scope_emitter_lifecycle() {
        let emitter = ScopeTelemetryEmitter::new("rt.scope", "daemon:capture", "session-1");

        let created = emitter.created("daemon");
        assert_eq!(created.event_kind, RuntimeTelemetryKind::ScopeCreated);
        assert_eq!(created.scope_id, Some("daemon:capture".to_string()));
        assert_eq!(created.phase, RuntimePhase::Init);

        let started = emitter.started();
        assert_eq!(started.event_kind, RuntimeTelemetryKind::ScopeStarted);
        assert_eq!(started.phase, RuntimePhase::Startup);

        let draining = emitter.draining("user-requested");
        assert_eq!(draining.event_kind, RuntimeTelemetryKind::ScopeDraining);
        assert_eq!(draining.phase, RuntimePhase::Draining);
        assert_eq!(
            draining.details.get("shutdown_reason"),
            Some(&serde_json::json!("user-requested"))
        );

        let finalizing = emitter.finalizing(5);
        assert_eq!(finalizing.event_kind, RuntimeTelemetryKind::ScopeFinalizing);
        assert_eq!(
            finalizing.details.get("finalizer_count"),
            Some(&serde_json::json!(5))
        );

        let closed = emitter.closed(12345);
        assert_eq!(closed.event_kind, RuntimeTelemetryKind::ScopeClosed);
        assert_eq!(closed.phase, RuntimePhase::Shutdown);
        assert_eq!(
            closed.details.get("total_duration_ms"),
            Some(&serde_json::json!(12345))
        );

        // All events share the same correlation_id
        for ev in [&created, &started, &draining, &finalizing, &closed] {
            assert_eq!(ev.correlation_id, "session-1");
        }
    }

    // ── CancellationTelemetryEmitter ──

    #[test]
    fn cancellation_emitter() {
        let emitter = CancellationTelemetryEmitter::new("rt.cancellation", "shutdown-1");

        let requested = emitter.requested("daemon:capture", "user-requested");
        assert_eq!(
            requested.event_kind,
            RuntimeTelemetryKind::CancellationRequested
        );
        assert_eq!(requested.phase, RuntimePhase::Cancelling);

        let propagated = emitter.propagated("root", 4);
        assert_eq!(
            propagated.event_kind,
            RuntimeTelemetryKind::CancellationPropagated
        );
        assert_eq!(
            propagated.details.get("child_count"),
            Some(&serde_json::json!(4))
        );

        let grace = emitter.grace_expired("daemon:capture", 5000);
        assert_eq!(grace.event_kind, RuntimeTelemetryKind::GracePeriodExpired);
        assert_eq!(grace.health_tier, HealthTier::Red);
        assert_eq!(
            grace.details.get("grace_period_ms"),
            Some(&serde_json::json!(5000))
        );
    }

    // ── Reason code constants ──

    #[test]
    fn reason_codes_follow_convention() {
        // All reason codes follow subsystem.phase.detail convention
        let all_codes = [
            reason_codes::SCOPE_INIT_CREATED,
            reason_codes::SCOPE_STARTUP_STARTED,
            reason_codes::SCOPE_DRAINING_STARTED,
            reason_codes::SCOPE_FINALIZING_STARTED,
            reason_codes::SCOPE_SHUTDOWN_CLOSED,
            reason_codes::CANCELLATION_REQUESTED,
            reason_codes::CANCELLATION_PROPAGATED,
            reason_codes::CANCELLATION_GRACE_EXPIRED,
            reason_codes::CANCELLATION_FINALIZER_OK,
            reason_codes::CANCELLATION_FINALIZER_FAILED,
            reason_codes::BACKPRESSURE_TIER_GREEN,
            reason_codes::BACKPRESSURE_TIER_YELLOW,
            reason_codes::BACKPRESSURE_TIER_RED,
            reason_codes::BACKPRESSURE_TIER_BLACK,
            reason_codes::BACKPRESSURE_THROTTLE_ON,
            reason_codes::BACKPRESSURE_THROTTLE_OFF,
            reason_codes::BACKPRESSURE_LOAD_SHEDDING,
            reason_codes::QUEUE_DEPTH_OBSERVED,
            reason_codes::QUEUE_CHANNEL_CLOSED,
            reason_codes::QUEUE_PERMIT_EXHAUSTED,
            reason_codes::ERROR_TRANSIENT,
            reason_codes::ERROR_PERMANENT,
            reason_codes::ERROR_PANIC,
            reason_codes::ERROR_INVARIANT,
            reason_codes::ERROR_SAFETY,
            reason_codes::RESOURCE_OBSERVED,
            reason_codes::RESOURCE_EXHAUSTED,
            reason_codes::OPS_SLO_MEASUREMENT,
            reason_codes::OPS_CONFIG_APPLIED,
            reason_codes::OPS_DIAGNOSTIC_EXPORTED,
            reason_codes::OPS_HEARTBEAT,
        ];

        for code in &all_codes {
            let parts: Vec<&str> = code.split('.').collect();
            assert!(
                parts.len() >= 2,
                "Reason code '{code}' must have at least subsystem.detail"
            );
            // All parts are snake_case (no uppercase, no hyphens)
            for part in &parts {
                assert!(
                    part.chars()
                        .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
                    "Reason code part '{part}' in '{code}' must be snake_case"
                );
            }
        }
    }

    // ── Sequence monotonicity ──

    #[test]
    fn log_sequence_monotonic() {
        let mut log = RuntimeTelemetryLog::with_defaults();

        let mut prev = 0;
        for _ in 0..10 {
            let seq = log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason("test"),
            );
            assert!(seq > prev, "Sequence must be monotonically increasing");
            prev = seq;
        }
    }

    // ── Policy metrics dashboard adapter ──

    #[test]
    fn policy_dashboard_adapter_healthy() {
        use crate::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem(
            "test",
            PolicySubsystemInput {
                evaluations: 100,
                denials: 2,
                ..Default::default()
            },
        );
        let dash = collector.dashboard(5000);
        let record = UnifiedTelemetryRecord::from_policy_metrics_dashboard(&dash);
        assert_eq!(record.component, "policy.metrics_dashboard");
        assert_eq!(record.health_tier, HealthTier::Green);
        assert!(record.failure_class.is_none());
        assert_eq!(record.timestamp_ms, 5000);
        assert_eq!(
            record.attributes["total_evaluations"],
            serde_json::json!(100)
        );
    }

    #[test]
    fn policy_dashboard_adapter_kill_switch() {
        use crate::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_kill_switch(true);
        let dash = collector.dashboard(6000);
        let record = UnifiedTelemetryRecord::from_policy_metrics_dashboard(&dash);
        assert_eq!(record.health_tier, HealthTier::Red);
        assert_eq!(record.failure_class, Some(FailureClass::Safety));
        assert_eq!(
            record.attributes["kill_switch_active"],
            serde_json::json!(true)
        );
    }

    #[test]
    fn policy_dashboard_adapter_invalid_chain() {
        use crate::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_audit_chain(50, false);
        let dash = collector.dashboard(7000);
        let record = UnifiedTelemetryRecord::from_policy_metrics_dashboard(&dash);
        assert_eq!(record.failure_class, Some(FailureClass::Corruption));
    }

    #[test]
    fn policy_dashboard_from_trait() {
        use crate::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        let dash = collector.dashboard(8000);
        let record = UnifiedTelemetryRecord::from(&dash);
        assert_eq!(record.component, "policy.metrics_dashboard");
    }

    // ── Compliance snapshot adapter ──

    #[test]
    fn compliance_snapshot_adapter_compliant() {
        use crate::policy_compliance::*;
        let mut engine = ComplianceEngine::new(100, 3_600_000);
        engine.record_evaluation(false); // not denied
        let snap = engine.snapshot(5000);
        let record = UnifiedTelemetryRecord::from_compliance_snapshot(&snap);
        assert_eq!(record.component, "policy.compliance_engine");
        assert_eq!(record.health_tier, HealthTier::Green);
        assert!(record.failure_class.is_none());
        assert_eq!(record.attributes["total_evaluations"], serde_json::json!(1));
    }

    #[test]
    fn compliance_snapshot_from_trait() {
        use crate::policy_compliance::*;
        let mut engine = ComplianceEngine::new(100, 3_600_000);
        let snap = engine.snapshot(5000);
        let record = UnifiedTelemetryRecord::from(&snap);
        assert_eq!(record.component, "policy.compliance_engine");
    }

    // ── Scheduler snapshot adapter ──

    #[test]
    fn scheduler_snapshot_adapter_healthy() {
        use crate::swarm_scheduler::*;
        let scheduler = SwarmScheduler::new(SchedulerConfig::default());
        let snap = scheduler.snapshot();
        let record = UnifiedTelemetryRecord::from_scheduler_snapshot(&snap, 9000);
        assert_eq!(record.component, "swarm.scheduler");
        assert_eq!(record.reason_code, "swarm.scheduler.snapshot");
        assert_eq!(record.health_tier, HealthTier::Green);
        assert!(record.failure_class.is_none());
        assert_eq!(record.source, UnifiedTelemetrySource::Runtime);
        assert_eq!(record.scope_id.as_deref(), Some("swarm:scheduler"));
        assert_eq!(record.attributes["fleet_agents"], serde_json::json!(0));
        assert_eq!(
            record.attributes["consecutive_scale_ops"],
            serde_json::json!(0)
        );
        assert_eq!(
            record.attributes["circuit_breaker_tripped"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn scheduler_snapshot_circuit_breaker_is_black() {
        use crate::swarm_scheduler::*;
        let mut config = SchedulerConfig::default();
        config.max_consecutive_scale_ops = 2;
        let scheduler = SwarmScheduler::new(config);
        let mut snap = scheduler.snapshot();
        snap.circuit_breaker_tripped_at = Some(8000);
        let record = UnifiedTelemetryRecord::from_scheduler_snapshot(&snap, 9000);
        assert_eq!(record.health_tier, HealthTier::Black);
        assert_eq!(record.failure_class, Some(FailureClass::Overload));
    }

    #[test]
    fn scheduler_snapshot_high_failure_rate() {
        use crate::swarm_scheduler::*;
        let scheduler = SwarmScheduler::new(SchedulerConfig::default());
        let mut snap = scheduler.snapshot();
        snap.agent_completed.insert("agent-1".to_string(), 10);
        snap.agent_failed.insert("agent-1".to_string(), 20);
        let record = UnifiedTelemetryRecord::from_scheduler_snapshot(&snap, 9000);
        assert_eq!(record.health_tier, HealthTier::Red);
        assert_eq!(record.failure_class, Some(FailureClass::Degraded));
        // failure rate = 20 / (10+20) ≈ 0.667
        let rate = record.attributes["fleet_failure_rate"].as_f64().unwrap();
        assert!(rate > 0.6 && rate < 0.7);
    }

    #[test]
    fn scheduler_snapshot_from_trait() {
        use crate::swarm_scheduler::*;
        let scheduler = SwarmScheduler::new(SchedulerConfig::default());
        let snap = scheduler.snapshot();
        let record = UnifiedTelemetryRecord::from(&snap);
        assert_eq!(record.component, "swarm.scheduler");
    }

    #[test]
    fn scheduler_snapshot_scale_event_counts() {
        use crate::swarm_scheduler::*;
        let scheduler = SwarmScheduler::new(SchedulerConfig::default());
        let mut snap = scheduler.snapshot();
        snap.scale_history.push(ScaleEvent {
            event_type: ScaleEventType::ScaleUp,
            timestamp_ms: 1000,
            reason: "high pressure".to_string(),
            fleet_size_before: 2,
            fleet_size_after: 4,
            decision: SchedulerDecision::ScaleUp {
                additional_agents: 2,
                reason: "high pressure".to_string(),
            },
        });
        snap.scale_history.push(ScaleEvent {
            event_type: ScaleEventType::ScaleDown,
            timestamp_ms: 2000,
            reason: "low pressure".to_string(),
            fleet_size_before: 4,
            fleet_size_after: 2,
            decision: SchedulerDecision::ScaleDown {
                remove_agents: vec!["agent-3".to_string(), "agent-4".to_string()],
                reason: "low pressure".to_string(),
            },
        });
        let record = UnifiedTelemetryRecord::from_scheduler_snapshot(&snap, 9000);
        assert_eq!(record.attributes["scale_ups"], serde_json::json!(1));
        assert_eq!(record.attributes["scale_downs"], serde_json::json!(1));
        assert_eq!(record.attributes["scale_history_len"], serde_json::json!(2));
    }

    // ── MuxPoolStats adapter ──

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn mux_pool_stats_adapter_healthy() {
        use crate::pool::PoolStats;
        use crate::vendored::MuxPoolStats;
        let stats = MuxPoolStats {
            pool: PoolStats {
                max_size: 8,
                idle_count: 6,
                active_count: 2,
                total_acquired: 100,
                total_returned: 98,
                total_evicted: 0,
                total_timeouts: 0,
            },
            connections_created: 10,
            connections_failed: 0,
            health_checks: 50,
            health_check_failures: 0,
            recovery_attempts: 0,
            recovery_successes: 0,
            permanent_failures: 0,
        };
        let record = UnifiedTelemetryRecord::from_mux_pool_stats(&stats, 7000);
        assert_eq!(record.component, "mux.pool");
        assert_eq!(record.reason_code, "mux.pool.snapshot");
        assert_eq!(record.health_tier, HealthTier::Green);
        assert!(record.failure_class.is_none());
        assert_eq!(record.source, UnifiedTelemetrySource::Runtime);
        assert_eq!(record.scope_id.as_deref(), Some("mux:pool"));
        assert_eq!(record.attributes["max_size"], serde_json::json!(8));
        assert_eq!(
            record.attributes["connections_created"],
            serde_json::json!(10)
        );
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn mux_pool_stats_permanent_failure_is_black() {
        use crate::pool::PoolStats;
        use crate::vendored::MuxPoolStats;
        let stats = MuxPoolStats {
            pool: PoolStats {
                max_size: 8,
                idle_count: 0,
                active_count: 0,
                total_acquired: 10,
                total_returned: 10,
                total_evicted: 0,
                total_timeouts: 5,
            },
            connections_created: 5,
            connections_failed: 5,
            health_checks: 10,
            health_check_failures: 10,
            recovery_attempts: 5,
            recovery_successes: 0,
            permanent_failures: 3,
        };
        let record = UnifiedTelemetryRecord::from_mux_pool_stats(&stats, 8000);
        assert_eq!(record.health_tier, HealthTier::Black);
        assert_eq!(record.failure_class, Some(FailureClass::Permanent));
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn mux_pool_stats_transient_failures_yellow() {
        use crate::pool::PoolStats;
        use crate::vendored::MuxPoolStats;
        let stats = MuxPoolStats {
            pool: PoolStats {
                max_size: 10,
                idle_count: 5,
                active_count: 3,
                total_acquired: 50,
                total_returned: 47,
                total_evicted: 0,
                total_timeouts: 0,
            },
            connections_created: 19,
            connections_failed: 1,
            health_checks: 20,
            health_check_failures: 1,
            recovery_attempts: 1,
            recovery_successes: 1,
            permanent_failures: 0,
        };
        let record = UnifiedTelemetryRecord::from_mux_pool_stats(&stats, 8000);
        assert_eq!(record.health_tier, HealthTier::Yellow);
        assert_eq!(record.failure_class, Some(FailureClass::Transient));
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn mux_pool_stats_from_trait() {
        use crate::pool::PoolStats;
        use crate::vendored::MuxPoolStats;
        let stats = MuxPoolStats {
            pool: PoolStats {
                max_size: 4,
                idle_count: 2,
                active_count: 1,
                total_acquired: 10,
                total_returned: 9,
                total_evicted: 0,
                total_timeouts: 0,
            },
            connections_created: 5,
            connections_failed: 0,
            health_checks: 10,
            health_check_failures: 0,
            recovery_attempts: 0,
            recovery_successes: 0,
            permanent_failures: 0,
        };
        let record = UnifiedTelemetryRecord::from(&stats);
        assert_eq!(record.component, "mux.pool");
    }

    #[cfg(all(feature = "vendored", unix))]
    #[test]
    fn mux_pool_stats_high_saturation_red() {
        use crate::pool::PoolStats;
        use crate::vendored::MuxPoolStats;
        let stats = MuxPoolStats {
            pool: PoolStats {
                max_size: 4,
                idle_count: 0,
                active_count: 4,
                total_acquired: 100,
                total_returned: 96,
                total_evicted: 0,
                total_timeouts: 2,
            },
            connections_created: 20,
            connections_failed: 5,
            health_checks: 30,
            health_check_failures: 2,
            recovery_attempts: 3,
            recovery_successes: 2,
            permanent_failures: 0,
        };
        let record = UnifiedTelemetryRecord::from_mux_pool_stats(&stats, 8500);
        // failure_ratio = 5/25 = 0.20, saturation = 4/4 = 1.0 → Red
        assert_eq!(record.health_tier, HealthTier::Red);
    }

    // ── QueueStats adapter ──

    #[test]
    fn queue_stats_adapter_healthy() {
        use crate::swarm_work_queue::QueueStats;
        let stats = QueueStats {
            total_items: 20,
            blocked: 2,
            ready: 5,
            in_progress: 3,
            completed: 10,
            failed: 0,
            cancelled: 0,
            active_agents: 4,
            completion_log_size: 10,
        };
        let record = UnifiedTelemetryRecord::from_queue_stats(&stats, 10000);
        assert_eq!(record.component, "swarm.work_queue");
        assert_eq!(record.reason_code, "swarm.queue.snapshot");
        assert_eq!(record.health_tier, HealthTier::Green);
        assert!(record.failure_class.is_none());
        assert_eq!(record.source, UnifiedTelemetrySource::Runtime);
        assert_eq!(record.scope_id.as_deref(), Some("swarm:work_queue"));
        assert_eq!(record.attributes["total_items"], serde_json::json!(20));
        assert_eq!(record.attributes["active_agents"], serde_json::json!(4));
    }

    #[test]
    fn queue_stats_high_failure_is_black() {
        use crate::swarm_work_queue::QueueStats;
        let stats = QueueStats {
            total_items: 20,
            blocked: 0,
            ready: 0,
            in_progress: 0,
            completed: 5,
            failed: 15,
            cancelled: 0,
            active_agents: 2,
            completion_log_size: 20,
        };
        let record = UnifiedTelemetryRecord::from_queue_stats(&stats, 10000);
        // failure_rate = 15/20 = 0.75 → Black
        assert_eq!(record.health_tier, HealthTier::Black);
        assert_eq!(record.failure_class, Some(FailureClass::Permanent));
    }

    #[test]
    fn queue_stats_high_blocked_ratio_red() {
        use crate::swarm_work_queue::QueueStats;
        let stats = QueueStats {
            total_items: 20,
            blocked: 9,
            ready: 1,
            in_progress: 0,
            completed: 10,
            failed: 0,
            cancelled: 0,
            active_agents: 1,
            completion_log_size: 10,
        };
        let record = UnifiedTelemetryRecord::from_queue_stats(&stats, 10000);
        // non_terminal = 20 - 10 = 10, blocked_ratio = 9/10 = 0.9 → Red
        assert_eq!(record.health_tier, HealthTier::Red);
        assert_eq!(record.failure_class, Some(FailureClass::Overload));
    }

    #[test]
    fn queue_stats_moderate_failure_yellow() {
        use crate::swarm_work_queue::QueueStats;
        let stats = QueueStats {
            total_items: 30,
            blocked: 2,
            ready: 3,
            in_progress: 5,
            completed: 18,
            failed: 2,
            cancelled: 0,
            active_agents: 3,
            completion_log_size: 20,
        };
        let record = UnifiedTelemetryRecord::from_queue_stats(&stats, 10000);
        // failure_rate = 2/20 = 0.10 → Yellow (>= 0.05)
        assert_eq!(record.health_tier, HealthTier::Yellow);
        assert_eq!(record.failure_class, Some(FailureClass::Degraded));
    }

    #[test]
    fn queue_stats_from_trait() {
        use crate::swarm_work_queue::QueueStats;
        let stats = QueueStats::default();
        let record = UnifiedTelemetryRecord::from(&stats);
        assert_eq!(record.component, "swarm.work_queue");
    }

    #[test]
    fn queue_stats_empty_queue_green() {
        use crate::swarm_work_queue::QueueStats;
        let stats = QueueStats::default();
        let record = UnifiedTelemetryRecord::from_queue_stats(&stats, 10000);
        assert_eq!(record.health_tier, HealthTier::Green);
        assert!(record.failure_class.is_none());
        // Verify all counters are zero
        assert_eq!(record.attributes["total_items"], serde_json::json!(0));
        assert_eq!(record.attributes["failed"], serde_json::json!(0));
    }
}
