//! Canonical connector event/data model with schema evolution tooling.
//!
//! Defines normalized schemas for connector events, actions, and outcomes
//! across inbound/outbound/lifecycle flows. Provides schema versioning,
//! compatibility checks, and indexing contracts for search and policy.
//!
//! Part of ft-3681t.5.12.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::connector_host_runtime::{
    ConnectorCapability, ConnectorFailureClass, ConnectorLifecyclePhase,
};
use crate::connector_inbound_bridge::ConnectorSignalKind;
use crate::connector_outbound_bridge::{
    ConnectorActionKind, OutboundEventSource, OutboundSeverity,
};

// =============================================================================
// Schema version
// =============================================================================

/// Current schema version for the canonical event model.
pub const CANONICAL_SCHEMA_VERSION: u32 = 1;

/// Schema version metadata for evolution tracking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaVersion {
    /// Major version — breaking changes.
    pub major: u32,
    /// Minor version — backward-compatible additions.
    pub minor: u32,
}

impl SchemaVersion {
    #[must_use]
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }

    /// Check if this version is compatible with another.
    /// Compatible means same major, and this minor >= other minor.
    #[must_use]
    pub fn is_compatible_with(&self, other: &Self) -> bool {
        self.major == other.major && self.minor >= other.minor
    }

    /// Current schema version.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            major: CANONICAL_SCHEMA_VERSION,
            minor: 0,
        }
    }
}

impl Default for SchemaVersion {
    fn default() -> Self {
        Self::current()
    }
}

impl std::fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

// =============================================================================
// Canonical event direction
// =============================================================================

/// Direction of a canonical connector event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventDirection {
    /// External system → FrankenTerm (webhook, stream, poll).
    Inbound,
    /// FrankenTerm → external system (notify, ticket, audit).
    Outbound,
    /// Internal lifecycle event (connector state change).
    Lifecycle,
}

impl EventDirection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
            Self::Lifecycle => "lifecycle",
        }
    }
}

impl std::fmt::Display for EventDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// =============================================================================
// Canonical connector event
// =============================================================================

/// The canonical normalized representation of any connector event.
///
/// This is the common format for inbound signals, outbound actions,
/// and lifecycle transitions — suitable for storage, indexing, search,
/// policy evaluation, and audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalConnectorEvent {
    /// Schema version for forward compatibility.
    pub schema_version: SchemaVersion,
    /// Direction of the event flow.
    pub direction: EventDirection,
    /// Unique event identifier (generated).
    pub event_id: String,
    /// Correlation ID for cross-system tracing and dedup.
    pub correlation_id: String,
    /// Timestamp at the event source (millis since epoch).
    pub timestamp_ms: u64,

    // --- Source identification ---
    /// Connector that produced or receives this event.
    pub connector_id: String,
    /// Human-readable connector display name (if known).
    pub connector_name: Option<String>,

    // --- Event classification ---
    /// Canonical event type key (e.g., "inbound.webhook.push", "outbound.notify").
    pub event_type: String,
    /// Severity level.
    pub severity: CanonicalSeverity,

    // --- Inbound-specific fields ---
    /// Signal kind (inbound only).
    pub signal_kind: Option<ConnectorSignalKind>,
    /// Signal sub-type (e.g., "pr_opened", "push").
    pub signal_sub_type: Option<String>,

    // --- Outbound-specific fields ---
    /// Event source classification (outbound only).
    pub event_source: Option<OutboundEventSource>,
    /// Action kind dispatched (outbound only).
    pub action_kind: Option<ConnectorActionKind>,

    // --- Lifecycle fields ---
    /// Lifecycle phase (lifecycle events).
    pub lifecycle_phase: Option<ConnectorLifecyclePhase>,
    /// Failure classification (failure events).
    pub failure_class: Option<ConnectorFailureClass>,

    // --- Scope ---
    /// Pane ID (if event is pane-scoped).
    pub pane_id: Option<u64>,
    /// Workflow ID (if event is workflow-scoped).
    pub workflow_id: Option<String>,

    // --- Sandbox context ---
    /// Sandbox zone the connector operates in.
    pub zone_id: Option<String>,
    /// Required capability for the operation.
    pub capability: Option<ConnectorCapability>,

    // --- Payload ---
    /// Opaque structured payload (connector-specific data).
    pub payload: serde_json::Value,

    // --- Extensible metadata ---
    /// Additional key-value metadata for forward compatibility.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl CanonicalConnectorEvent {
    /// Create a new canonical event with required fields.
    #[must_use]
    pub fn new(
        direction: EventDirection,
        connector_id: impl Into<String>,
        event_type: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            schema_version: SchemaVersion::current(),
            direction,
            event_id: generate_event_id(now_ms),
            correlation_id: generate_event_id(now_ms),
            timestamp_ms: now_ms,
            connector_id: connector_id.into(),
            connector_name: None,
            event_type: event_type.into(),
            severity: CanonicalSeverity::Info,
            signal_kind: None,
            signal_sub_type: None,
            event_source: None,
            action_kind: None,
            lifecycle_phase: None,
            failure_class: None,
            pane_id: None,
            workflow_id: None,
            zone_id: None,
            capability: None,
            payload,
            metadata: BTreeMap::new(),
        }
    }

    /// Set the correlation ID.
    #[must_use]
    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = id.into();
        self
    }

    /// Set the event ID.
    #[must_use]
    pub fn with_event_id(mut self, id: impl Into<String>) -> Self {
        self.event_id = id.into();
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
    pub fn with_severity(mut self, severity: CanonicalSeverity) -> Self {
        self.severity = severity;
        self
    }

    /// Set inbound signal fields.
    #[must_use]
    pub fn with_signal(mut self, kind: ConnectorSignalKind, sub_type: Option<String>) -> Self {
        self.signal_kind = Some(kind);
        self.signal_sub_type = sub_type;
        self
    }

    /// Set outbound action fields.
    #[must_use]
    pub fn with_action(mut self, source: OutboundEventSource, kind: ConnectorActionKind) -> Self {
        self.event_source = Some(source);
        self.action_kind = Some(kind);
        self
    }

    /// Set lifecycle fields.
    #[must_use]
    pub fn with_lifecycle(mut self, phase: ConnectorLifecyclePhase) -> Self {
        self.lifecycle_phase = Some(phase);
        self
    }

    /// Set failure classification.
    #[must_use]
    pub fn with_failure(mut self, class: ConnectorFailureClass) -> Self {
        self.failure_class = Some(class);
        self
    }

    /// Set pane scope.
    #[must_use]
    pub fn with_pane_id(mut self, pane_id: u64) -> Self {
        self.pane_id = Some(pane_id);
        self
    }

    /// Set workflow scope.
    #[must_use]
    pub fn with_workflow_id(mut self, wf_id: impl Into<String>) -> Self {
        self.workflow_id = Some(wf_id.into());
        self
    }

    /// Set sandbox context.
    #[must_use]
    pub fn with_sandbox(
        mut self,
        zone_id: impl Into<String>,
        capability: ConnectorCapability,
    ) -> Self {
        self.zone_id = Some(zone_id.into());
        self.capability = Some(capability);
        self
    }

    /// Set connector display name.
    #[must_use]
    pub fn with_connector_name(mut self, name: impl Into<String>) -> Self {
        self.connector_name = Some(name.into());
        self
    }

    /// Add a metadata key-value pair.
    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Construct the canonical rule_id for indexing.
    ///
    /// Format: `<direction>.<connector>.<event_type>`
    #[must_use]
    pub fn rule_id(&self) -> String {
        format!(
            "{}.{}.{}",
            self.direction.as_str(),
            self.connector_id,
            self.event_type
        )
    }

    /// Check if this event is a failure event.
    #[must_use]
    pub fn is_failure(&self) -> bool {
        self.failure_class.is_some()
            || self.lifecycle_phase == Some(ConnectorLifecyclePhase::Failed)
            || self.severity == CanonicalSeverity::Critical
    }
}

/// Generate a unique event ID based on timestamp and random suffix.
fn generate_event_id(timestamp_ms: u64) -> String {
    // Simple deterministic-ish ID for reproducibility in tests
    format!(
        "evt-{timestamp_ms}-{:04x}",
        timestamp_ms.wrapping_mul(2654435761) as u16
    )
}

// =============================================================================
// Canonical severity
// =============================================================================

/// Unified severity levels across all connector event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalSeverity {
    Info,
    Warning,
    Critical,
}

impl Default for CanonicalSeverity {
    fn default() -> Self {
        Self::Info
    }
}

impl CanonicalSeverity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }

    /// Convert from outbound severity.
    #[must_use]
    pub fn from_outbound(s: OutboundSeverity) -> Self {
        match s {
            OutboundSeverity::Info => Self::Info,
            OutboundSeverity::Warning => Self::Warning,
            OutboundSeverity::Critical => Self::Critical,
        }
    }
}

impl std::fmt::Display for CanonicalSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// =============================================================================
// Schema evolution registry
// =============================================================================

/// Schema field definition for evolution tracking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaFieldDef {
    /// Field name.
    pub name: String,
    /// Field type as a string (e.g., "string", "u64", "Option<string>").
    pub field_type: String,
    /// Whether the field is required (vs optional).
    pub required: bool,
    /// Schema version when this field was introduced.
    pub introduced_in: SchemaVersion,
    /// Schema version when this field was deprecated (if applicable).
    pub deprecated_in: Option<SchemaVersion>,
    /// Description of the field's purpose.
    pub description: String,
}

/// Schema evolution registry tracking field additions/deprecations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaEvolutionRegistry {
    /// Current schema version.
    pub current_version: SchemaVersion,
    /// All field definitions (current + historical).
    pub fields: Vec<SchemaFieldDef>,
}

impl SchemaEvolutionRegistry {
    /// Create a new registry at the current version with initial fields.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current_version: SchemaVersion::current(),
            fields: Self::v1_fields(),
        }
    }

    /// Initial v1.0 field definitions.
    fn v1_fields() -> Vec<SchemaFieldDef> {
        let v1 = SchemaVersion::new(1, 0);
        vec![
            SchemaFieldDef {
                name: "schema_version".to_string(),
                field_type: "SchemaVersion".to_string(),
                required: true,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Schema version for forward compatibility".to_string(),
            },
            SchemaFieldDef {
                name: "direction".to_string(),
                field_type: "EventDirection".to_string(),
                required: true,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Event flow direction (inbound/outbound/lifecycle)".to_string(),
            },
            SchemaFieldDef {
                name: "event_id".to_string(),
                field_type: "string".to_string(),
                required: true,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Unique event identifier".to_string(),
            },
            SchemaFieldDef {
                name: "correlation_id".to_string(),
                field_type: "string".to_string(),
                required: true,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Cross-system correlation and dedup key".to_string(),
            },
            SchemaFieldDef {
                name: "timestamp_ms".to_string(),
                field_type: "u64".to_string(),
                required: true,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Event timestamp (millis since epoch)".to_string(),
            },
            SchemaFieldDef {
                name: "connector_id".to_string(),
                field_type: "string".to_string(),
                required: true,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Source/target connector identifier".to_string(),
            },
            SchemaFieldDef {
                name: "event_type".to_string(),
                field_type: "string".to_string(),
                required: true,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Canonical event type key".to_string(),
            },
            SchemaFieldDef {
                name: "severity".to_string(),
                field_type: "CanonicalSeverity".to_string(),
                required: true,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Event severity level".to_string(),
            },
            SchemaFieldDef {
                name: "payload".to_string(),
                field_type: "json".to_string(),
                required: true,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Opaque connector-specific data".to_string(),
            },
            SchemaFieldDef {
                name: "signal_kind".to_string(),
                field_type: "Option<ConnectorSignalKind>".to_string(),
                required: false,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Inbound signal classification".to_string(),
            },
            SchemaFieldDef {
                name: "action_kind".to_string(),
                field_type: "Option<ConnectorActionKind>".to_string(),
                required: false,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Outbound action classification".to_string(),
            },
            SchemaFieldDef {
                name: "pane_id".to_string(),
                field_type: "Option<u64>".to_string(),
                required: false,
                introduced_in: v1.clone(),
                deprecated_in: None,
                description: "Pane scope (if applicable)".to_string(),
            },
            SchemaFieldDef {
                name: "metadata".to_string(),
                field_type: "BTreeMap<string, string>".to_string(),
                required: false,
                introduced_in: v1,
                deprecated_in: None,
                description: "Extensible key-value metadata".to_string(),
            },
        ]
    }

    /// Get all required fields for a given version.
    #[must_use]
    pub fn required_fields_for(&self, version: &SchemaVersion) -> Vec<&SchemaFieldDef> {
        self.fields
            .iter()
            .filter(|f| {
                f.required
                    && f.introduced_in.major <= version.major
                    && f.introduced_in.minor <= version.minor
                    && f.deprecated_in.as_ref().is_none_or(|d| {
                        d.major > version.major
                            || (d.major == version.major && d.minor > version.minor)
                    })
            })
            .collect()
    }

    /// Get all fields (required + optional) for a given version.
    #[must_use]
    pub fn all_fields_for(&self, version: &SchemaVersion) -> Vec<&SchemaFieldDef> {
        self.fields
            .iter()
            .filter(|f| {
                f.introduced_in.major <= version.major && f.introduced_in.minor <= version.minor
            })
            .collect()
    }

    /// Check if an event is valid against the current schema.
    #[must_use]
    pub fn validate_event(&self, event: &CanonicalConnectorEvent) -> SchemaValidationResult {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        // Version compatibility
        if !self
            .current_version
            .is_compatible_with(&event.schema_version)
        {
            errors.push(format!(
                "schema version {} is not compatible with current {}",
                event.schema_version, self.current_version
            ));
        }

        // Required field checks
        if event.event_id.is_empty() {
            errors.push("event_id must not be empty".to_string());
        }
        if event.correlation_id.is_empty() {
            errors.push("correlation_id must not be empty".to_string());
        }
        if event.connector_id.is_empty() {
            errors.push("connector_id must not be empty".to_string());
        }
        if event.event_type.is_empty() {
            errors.push("event_type must not be empty".to_string());
        }
        if event.timestamp_ms == 0 {
            warnings.push("timestamp_ms is zero (may indicate unset)".to_string());
        }

        // Direction-specific validation
        match event.direction {
            EventDirection::Inbound => {
                if event.signal_kind.is_none() {
                    warnings.push("inbound event missing signal_kind".to_string());
                }
            }
            EventDirection::Outbound => {
                if event.action_kind.is_none() {
                    warnings.push("outbound event missing action_kind".to_string());
                }
            }
            EventDirection::Lifecycle => {
                if event.lifecycle_phase.is_none() {
                    warnings.push("lifecycle event missing lifecycle_phase".to_string());
                }
            }
        }

        SchemaValidationResult {
            valid: errors.is_empty(),
            errors,
            warnings,
        }
    }
}

impl Default for SchemaEvolutionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of schema validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

// =============================================================================
// Schema compatibility checker
// =============================================================================

/// Check compatibility between two schema versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatibilityReport {
    /// Source version being checked.
    pub source: SchemaVersion,
    /// Target version being checked against.
    pub target: SchemaVersion,
    /// Whether the source is fully compatible with the target.
    pub compatible: bool,
    /// Fields present in target but missing in source.
    pub missing_fields: Vec<String>,
    /// Fields deprecated between source and target.
    pub deprecated_fields: Vec<String>,
}

/// Check compatibility between two schema versions using a registry.
#[must_use]
pub fn check_compatibility(
    registry: &SchemaEvolutionRegistry,
    source: &SchemaVersion,
    target: &SchemaVersion,
) -> CompatibilityReport {
    let source_fields: Vec<String> = registry
        .all_fields_for(source)
        .iter()
        .map(|f| f.name.clone())
        .collect();

    let target_required: Vec<String> = registry
        .required_fields_for(target)
        .iter()
        .map(|f| f.name.clone())
        .collect();

    let missing: Vec<String> = target_required
        .iter()
        .filter(|f| !source_fields.contains(f))
        .cloned()
        .collect();

    let deprecated: Vec<String> = registry
        .fields
        .iter()
        .filter(|f| {
            f.deprecated_in.as_ref().is_some_and(|d| {
                d.major <= target.major
                    && (d.major < target.major || d.minor <= target.minor)
                    && f.introduced_in.major <= source.major
                    && (f.introduced_in.major < source.major
                        || f.introduced_in.minor <= source.minor)
            })
        })
        .map(|f| f.name.clone())
        .collect();

    CompatibilityReport {
        source: source.clone(),
        target: target.clone(),
        compatible: missing.is_empty(),
        missing_fields: missing,
        deprecated_fields: deprecated,
    }
}

// =============================================================================
// Indexing contract
// =============================================================================

/// Defines which fields are indexed for search and policy queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingContract {
    /// Fields that are full-text searchable.
    pub searchable_fields: Vec<String>,
    /// Fields that are filterable (exact match / range).
    pub filterable_fields: Vec<String>,
    /// Fields that are sortable.
    pub sortable_fields: Vec<String>,
    /// Fields used as facets for aggregation.
    pub facet_fields: Vec<String>,
}

impl IndexingContract {
    /// Default indexing contract for canonical events.
    #[must_use]
    pub fn default_contract() -> Self {
        Self {
            searchable_fields: vec![
                "event_type".to_string(),
                "connector_id".to_string(),
                "connector_name".to_string(),
                "correlation_id".to_string(),
                "signal_sub_type".to_string(),
                "workflow_id".to_string(),
            ],
            filterable_fields: vec![
                "direction".to_string(),
                "severity".to_string(),
                "signal_kind".to_string(),
                "action_kind".to_string(),
                "event_source".to_string(),
                "lifecycle_phase".to_string(),
                "failure_class".to_string(),
                "pane_id".to_string(),
                "zone_id".to_string(),
                "capability".to_string(),
                "connector_id".to_string(),
            ],
            sortable_fields: vec!["timestamp_ms".to_string(), "severity".to_string()],
            facet_fields: vec![
                "direction".to_string(),
                "severity".to_string(),
                "connector_id".to_string(),
                "signal_kind".to_string(),
                "action_kind".to_string(),
                "failure_class".to_string(),
            ],
        }
    }

    /// Validate that a field is indexable.
    #[must_use]
    pub fn is_searchable(&self, field: &str) -> bool {
        self.searchable_fields.iter().any(|f| f == field)
    }

    /// Validate that a field is filterable.
    #[must_use]
    pub fn is_filterable(&self, field: &str) -> bool {
        self.filterable_fields.iter().any(|f| f == field)
    }
}

// =============================================================================
// Conversion helpers (from domain types to canonical)
// =============================================================================

/// Convert an inbound connector signal to a canonical event.
#[must_use]
pub fn from_inbound_signal(
    signal: &crate::connector_inbound_bridge::ConnectorSignal,
) -> CanonicalConnectorEvent {
    let severity = match signal.severity() {
        crate::patterns::Severity::Critical => CanonicalSeverity::Critical,
        crate::patterns::Severity::Warning => CanonicalSeverity::Warning,
        crate::patterns::Severity::Info => CanonicalSeverity::Info,
    };

    let event_type = format!("inbound.{}", signal.signal_kind.as_str());

    let mut event = CanonicalConnectorEvent::new(
        EventDirection::Inbound,
        &signal.source_connector,
        event_type,
        signal.payload.clone(),
    )
    .with_severity(severity)
    .with_signal(signal.signal_kind, signal.sub_type.clone())
    .with_timestamp_ms(signal.timestamp_ms);

    if let Some(ref cid) = signal.correlation_id {
        event = event.with_correlation_id(cid.clone());
    }
    if let Some(pane_id) = signal.pane_id {
        event = event.with_pane_id(pane_id);
    }
    if let Some(phase) = signal.lifecycle_phase {
        event = event.with_lifecycle(phase);
    }
    if let Some(ref class) = signal.failure_class {
        event = event.with_failure(*class);
    }

    event
}

/// Convert an outbound event + action to a canonical event.
#[must_use]
pub fn from_outbound_action(
    event: &crate::connector_outbound_bridge::OutboundEvent,
    action: &crate::connector_outbound_bridge::ConnectorAction,
) -> CanonicalConnectorEvent {
    let severity = CanonicalSeverity::from_outbound(event.severity);
    let event_type = format!("outbound.{}", action.action_kind.as_str());

    let mut canonical = CanonicalConnectorEvent::new(
        EventDirection::Outbound,
        &action.target_connector,
        event_type,
        action.params.clone(),
    )
    .with_severity(severity)
    .with_action(event.source, action.action_kind)
    .with_correlation_id(&action.correlation_id)
    .with_timestamp_ms(action.created_at_ms);

    if let Some(pane_id) = event.pane_id {
        canonical = canonical.with_pane_id(pane_id);
    }
    if let Some(ref wf_id) = event.workflow_id {
        canonical = canonical.with_workflow_id(wf_id.clone());
    }

    canonical
}

/// Convert a lifecycle phase change to a canonical event.
#[must_use]
pub fn from_lifecycle_transition(
    connector_id: impl Into<String>,
    phase: ConnectorLifecyclePhase,
    timestamp_ms: u64,
) -> CanonicalConnectorEvent {
    let severity = match phase {
        ConnectorLifecyclePhase::Failed => CanonicalSeverity::Critical,
        ConnectorLifecyclePhase::Degraded => CanonicalSeverity::Warning,
        _ => CanonicalSeverity::Info,
    };

    CanonicalConnectorEvent::new(
        EventDirection::Lifecycle,
        connector_id,
        format!("lifecycle.{}", phase.as_str()),
        serde_json::json!({"phase": phase.as_str()}),
    )
    .with_severity(severity)
    .with_lifecycle(phase)
    .with_timestamp_ms(timestamp_ms)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Schema version ----

    #[test]
    fn connector_event_model_schema_version_current() {
        let v = SchemaVersion::current();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 0);
    }

    #[test]
    fn connector_event_model_schema_version_compatibility() {
        let v1_0 = SchemaVersion::new(1, 0);
        let v1_1 = SchemaVersion::new(1, 1);
        let v2_0 = SchemaVersion::new(2, 0);

        assert!(v1_0.is_compatible_with(&v1_0));
        assert!(v1_1.is_compatible_with(&v1_0));
        assert!(!v1_0.is_compatible_with(&v1_1)); // missing fields
        assert!(!v2_0.is_compatible_with(&v1_0)); // major mismatch
        assert!(!v1_0.is_compatible_with(&v2_0));
    }

    #[test]
    fn connector_event_model_schema_version_display() {
        assert_eq!(format!("{}", SchemaVersion::new(1, 2)), "1.2");
    }

    // ---- Event direction ----

    #[test]
    fn connector_event_model_direction_labels() {
        assert_eq!(EventDirection::Inbound.as_str(), "inbound");
        assert_eq!(EventDirection::Outbound.as_str(), "outbound");
        assert_eq!(EventDirection::Lifecycle.as_str(), "lifecycle");
    }

    // ---- Canonical severity ----

    #[test]
    fn connector_event_model_severity_labels() {
        assert_eq!(CanonicalSeverity::Info.as_str(), "info");
        assert_eq!(CanonicalSeverity::Warning.as_str(), "warning");
        assert_eq!(CanonicalSeverity::Critical.as_str(), "critical");
    }

    #[test]
    fn connector_event_model_severity_default() {
        assert_eq!(CanonicalSeverity::default(), CanonicalSeverity::Info);
    }

    #[test]
    fn connector_event_model_severity_from_outbound() {
        assert_eq!(
            CanonicalSeverity::from_outbound(OutboundSeverity::Warning),
            CanonicalSeverity::Warning
        );
    }

    // ---- Canonical event ----

    #[test]
    fn connector_event_model_event_builder() {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Inbound,
            "github",
            "inbound.webhook.push",
            serde_json::json!({"ref": "main"}),
        )
        .with_correlation_id("corr-1")
        .with_pane_id(42)
        .with_severity(CanonicalSeverity::Warning)
        .with_signal(ConnectorSignalKind::Webhook, Some("push".to_string()))
        .with_metadata("repo", "frankenterm");

        assert_eq!(event.direction, EventDirection::Inbound);
        assert_eq!(event.connector_id, "github");
        assert_eq!(event.correlation_id, "corr-1");
        assert_eq!(event.pane_id, Some(42));
        assert_eq!(event.severity, CanonicalSeverity::Warning);
        assert_eq!(event.signal_kind, Some(ConnectorSignalKind::Webhook));
        assert_eq!(event.signal_sub_type.as_deref(), Some("push"));
        assert_eq!(
            event.metadata.get("repo").map(|s| s.as_str()),
            Some("frankenterm")
        );
    }

    #[test]
    fn connector_event_model_event_rule_id() {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Inbound,
            "github",
            "webhook.push",
            serde_json::json!({}),
        );
        assert_eq!(event.rule_id(), "inbound.github.webhook.push");
    }

    #[test]
    fn connector_event_model_event_is_failure() {
        let normal = CanonicalConnectorEvent::new(
            EventDirection::Inbound,
            "test",
            "test",
            serde_json::json!({}),
        );
        assert!(!normal.is_failure());

        let failed_phase = normal
            .clone()
            .with_lifecycle(ConnectorLifecyclePhase::Failed);
        assert!(failed_phase.is_failure());

        let critical = CanonicalConnectorEvent::new(
            EventDirection::Inbound,
            "test",
            "test",
            serde_json::json!({}),
        )
        .with_severity(CanonicalSeverity::Critical);
        assert!(critical.is_failure());

        let with_failure_class = CanonicalConnectorEvent::new(
            EventDirection::Inbound,
            "test",
            "test",
            serde_json::json!({}),
        )
        .with_failure(ConnectorFailureClass::Network);
        assert!(with_failure_class.is_failure());
    }

    #[test]
    fn connector_event_model_event_serde_roundtrip() {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Outbound,
            "slack",
            "outbound.notify",
            serde_json::json!({"channel": "#alerts"}),
        )
        .with_action(
            OutboundEventSource::PatternDetected,
            ConnectorActionKind::Notify,
        )
        .with_severity(CanonicalSeverity::Warning)
        .with_event_id("evt-test")
        .with_correlation_id("corr-test")
        .with_timestamp_ms(5000);

        let json = serde_json::to_string(&event).unwrap();
        let deserialized: CanonicalConnectorEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.direction, EventDirection::Outbound);
        assert_eq!(deserialized.connector_id, "slack");
        assert_eq!(deserialized.event_id, "evt-test");
        assert_eq!(deserialized.correlation_id, "corr-test");
        assert_eq!(deserialized.severity, CanonicalSeverity::Warning);
        assert_eq!(deserialized.action_kind, Some(ConnectorActionKind::Notify));
    }

    // ---- Schema evolution registry ----

    #[test]
    fn connector_event_model_registry_v1_fields() {
        let registry = SchemaEvolutionRegistry::new();
        let required = registry.required_fields_for(&SchemaVersion::current());
        assert!(required.len() >= 8);

        let required_names: Vec<&str> = required.iter().map(|f| f.name.as_str()).collect();
        assert!(required_names.contains(&"event_id"));
        assert!(required_names.contains(&"correlation_id"));
        assert!(required_names.contains(&"connector_id"));
        assert!(required_names.contains(&"event_type"));
        assert!(required_names.contains(&"timestamp_ms"));
    }

    #[test]
    fn connector_event_model_registry_optional_fields() {
        let registry = SchemaEvolutionRegistry::new();
        let all = registry.all_fields_for(&SchemaVersion::current());
        let optional: Vec<&SchemaFieldDef> = all.iter().filter(|f| !f.required).copied().collect();

        let optional_names: Vec<&str> = optional.iter().map(|f| f.name.as_str()).collect();
        assert!(optional_names.contains(&"signal_kind"));
        assert!(optional_names.contains(&"action_kind"));
        assert!(optional_names.contains(&"pane_id"));
        assert!(optional_names.contains(&"metadata"));
    }

    // ---- Schema validation ----

    #[test]
    fn connector_event_model_validation_valid_event() {
        let registry = SchemaEvolutionRegistry::new();
        let event = CanonicalConnectorEvent::new(
            EventDirection::Inbound,
            "github",
            "webhook.push",
            serde_json::json!({}),
        )
        .with_signal(ConnectorSignalKind::Webhook, None);

        let result = registry.validate_event(&event);
        assert!(result.valid);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn connector_event_model_validation_missing_required_fields() {
        let registry = SchemaEvolutionRegistry::new();
        let mut event = CanonicalConnectorEvent::new(
            EventDirection::Inbound,
            "github",
            "test",
            serde_json::json!({}),
        );
        event.event_id = String::new();
        event.connector_id = String::new();

        let result = registry.validate_event(&event);
        assert!(!result.valid);
        assert!(result.errors.len() >= 2);
    }

    #[test]
    fn connector_event_model_validation_direction_specific_warnings() {
        let registry = SchemaEvolutionRegistry::new();

        // Inbound without signal_kind
        let inbound = CanonicalConnectorEvent::new(
            EventDirection::Inbound,
            "test",
            "test",
            serde_json::json!({}),
        );
        let result = registry.validate_event(&inbound);
        assert!(result.valid); // warnings don't fail validation
        assert!(!result.warnings.is_empty());

        // Outbound without action_kind
        let outbound = CanonicalConnectorEvent::new(
            EventDirection::Outbound,
            "test",
            "test",
            serde_json::json!({}),
        );
        let result = registry.validate_event(&outbound);
        assert!(result.valid);
        assert!(!result.warnings.is_empty());
    }

    #[test]
    fn connector_event_model_validation_incompatible_version() {
        let registry = SchemaEvolutionRegistry::new();
        let mut event = CanonicalConnectorEvent::new(
            EventDirection::Inbound,
            "test",
            "test",
            serde_json::json!({}),
        );
        event.schema_version = SchemaVersion::new(99, 0);

        let result = registry.validate_event(&event);
        assert!(!result.valid);
    }

    // ---- Compatibility checking ----

    #[test]
    fn connector_event_model_compatibility_same_version() {
        let registry = SchemaEvolutionRegistry::new();
        let v1 = SchemaVersion::new(1, 0);
        let report = check_compatibility(&registry, &v1, &v1);
        assert!(report.compatible);
        assert!(report.missing_fields.is_empty());
    }

    // ---- Indexing contract ----

    #[test]
    fn connector_event_model_indexing_contract_defaults() {
        let contract = IndexingContract::default_contract();
        assert!(contract.is_searchable("event_type"));
        assert!(contract.is_searchable("connector_id"));
        assert!(contract.is_filterable("direction"));
        assert!(contract.is_filterable("severity"));
        assert!(contract.is_filterable("signal_kind"));
        assert!(!contract.is_searchable("nonexistent_field"));
    }

    // ---- Conversion: inbound signal ----

    #[test]
    fn connector_event_model_from_inbound_signal() {
        use crate::connector_inbound_bridge::ConnectorSignal;

        let signal = ConnectorSignal::new(
            "github",
            ConnectorSignalKind::Webhook,
            serde_json::json!({"action": "opened"}),
        )
        .with_sub_type("pr_opened")
        .with_correlation_id("gh-123")
        .with_pane_id(42)
        .with_timestamp_ms(5000);

        let canonical = from_inbound_signal(&signal);
        assert_eq!(canonical.direction, EventDirection::Inbound);
        assert_eq!(canonical.connector_id, "github");
        assert_eq!(canonical.correlation_id, "gh-123");
        assert_eq!(canonical.pane_id, Some(42));
        assert_eq!(canonical.signal_kind, Some(ConnectorSignalKind::Webhook));
        assert_eq!(canonical.signal_sub_type.as_deref(), Some("pr_opened"));
        assert_eq!(canonical.event_type, "inbound.webhook");
    }

    // ---- Conversion: outbound action ----

    #[test]
    fn connector_event_model_from_outbound_action() {
        use crate::connector_outbound_bridge::{ConnectorAction, OutboundEvent};

        let event = OutboundEvent::new(
            OutboundEventSource::PatternDetected,
            "pattern.ci_failure",
            serde_json::json!({"build": 42}),
        )
        .with_pane_id(10)
        .with_workflow_id("wf-1")
        .with_severity(OutboundSeverity::Critical);

        let action = ConnectorAction {
            target_connector: "slack".to_string(),
            action_kind: ConnectorActionKind::Notify,
            correlation_id: "out-corr-1".to_string(),
            params: serde_json::json!({"channel": "#ci"}),
            created_at_ms: 6000,
        };

        let canonical = from_outbound_action(&event, &action);
        assert_eq!(canonical.direction, EventDirection::Outbound);
        assert_eq!(canonical.connector_id, "slack");
        assert_eq!(canonical.correlation_id, "out-corr-1");
        assert_eq!(canonical.pane_id, Some(10));
        assert_eq!(canonical.workflow_id.as_deref(), Some("wf-1"));
        assert_eq!(canonical.severity, CanonicalSeverity::Critical);
        assert_eq!(canonical.action_kind, Some(ConnectorActionKind::Notify));
        assert_eq!(canonical.event_type, "outbound.notify");
    }

    // ---- Conversion: lifecycle transition ----

    #[test]
    fn connector_event_model_from_lifecycle_transition() {
        let event =
            from_lifecycle_transition("my-connector", ConnectorLifecyclePhase::Running, 7000);
        assert_eq!(event.direction, EventDirection::Lifecycle);
        assert_eq!(event.connector_id, "my-connector");
        assert_eq!(event.severity, CanonicalSeverity::Info);
        assert_eq!(
            event.lifecycle_phase,
            Some(ConnectorLifecyclePhase::Running)
        );
        assert_eq!(event.event_type, "lifecycle.running");

        let failed = from_lifecycle_transition("bad-conn", ConnectorLifecyclePhase::Failed, 8000);
        assert_eq!(failed.severity, CanonicalSeverity::Critical);
        assert!(failed.is_failure());
    }

    // ---- Schema field def serde ----

    #[test]
    fn connector_event_model_field_def_serde() {
        let field = SchemaFieldDef {
            name: "test_field".to_string(),
            field_type: "string".to_string(),
            required: true,
            introduced_in: SchemaVersion::new(1, 0),
            deprecated_in: None,
            description: "A test field".to_string(),
        };
        let json = serde_json::to_string(&field).unwrap();
        let deserialized: SchemaFieldDef = serde_json::from_str(&json).unwrap();
        assert_eq!(field, deserialized);
    }

    // ---- Validation result serde ----

    #[test]
    fn connector_event_model_validation_result_serde() {
        let result = SchemaValidationResult {
            valid: false,
            errors: vec!["missing field".to_string()],
            warnings: vec!["optional missing".to_string()],
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: SchemaValidationResult = serde_json::from_str(&json).unwrap();
        assert!(!deserialized.valid);
        assert_eq!(deserialized.errors.len(), 1);
    }

    // ---- Event with sandbox context ----

    #[test]
    fn connector_event_model_event_with_sandbox() {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Outbound,
            "test",
            "outbound.invoke",
            serde_json::json!({}),
        )
        .with_sandbox("zone.prod", ConnectorCapability::Invoke);

        assert_eq!(event.zone_id.as_deref(), Some("zone.prod"));
        assert_eq!(event.capability, Some(ConnectorCapability::Invoke));
    }

    // ---- Event with connector name ----

    #[test]
    fn connector_event_model_event_with_connector_name() {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Inbound,
            "gh-events",
            "webhook",
            serde_json::json!({}),
        )
        .with_connector_name("GitHub Events Connector");

        assert_eq!(
            event.connector_name.as_deref(),
            Some("GitHub Events Connector")
        );
    }
}
