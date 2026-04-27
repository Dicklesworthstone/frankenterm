//! Connector control-plane type definitions leaf crate (ft-dfd16).
//!
//! Holds pure data snapshots/config fragments shared by `frankenterm-core`
//! connector modules and in-core policy/runtime telemetry aggregation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// =============================================================================
// Credential broker telemetry
// =============================================================================

/// Audit event for credential operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialAuditEvent {
    pub timestamp_ms: u64,
    pub event_type: CredentialAuditType,
    pub credential_id: String,
    pub connector_id: Option<String>,
    pub lease_id: Option<String>,
    pub detail: String,
}

/// Types of credential audit events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialAuditType {
    CredentialRegistered,
    LeaseIssued,
    LeaseExpired,
    LeaseRevoked,
    CredentialRotated,
    CredentialRevoked,
    CredentialExpired,
    AccessDenied,
    ProviderRegistered,
    ProviderStatusChanged,
}

impl std::fmt::Display for CredentialAuditType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CredentialRegistered => f.write_str("credential_registered"),
            Self::LeaseIssued => f.write_str("lease_issued"),
            Self::LeaseExpired => f.write_str("lease_expired"),
            Self::LeaseRevoked => f.write_str("lease_revoked"),
            Self::CredentialRotated => f.write_str("credential_rotated"),
            Self::CredentialRevoked => f.write_str("credential_revoked"),
            Self::CredentialExpired => f.write_str("credential_expired"),
            Self::AccessDenied => f.write_str("access_denied"),
            Self::ProviderRegistered => f.write_str("provider_registered"),
            Self::ProviderStatusChanged => f.write_str("provider_status_changed"),
        }
    }
}

/// Telemetry counters for the credential broker.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialBrokerTelemetry {
    pub leases_issued: u64,
    pub leases_expired: u64,
    pub leases_revoked: u64,
    pub access_denied: u64,
    pub rotations_completed: u64,
    pub rotations_failed: u64,
    pub credentials_registered: u64,
    pub credentials_revoked: u64,
    pub providers_registered: u64,
}

/// Snapshot of broker telemetry (serializable for diagnostics).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialBrokerTelemetrySnapshot {
    pub captured_at_ms: u64,
    pub counters: CredentialBrokerTelemetry,
    pub active_leases: u32,
    pub active_credentials: u32,
    pub active_providers: u32,
}

// =============================================================================
// Connector governor telemetry
// =============================================================================

/// Serializable quota state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuotaSnapshot {
    pub used: u64,
    pub max: u64,
    pub remaining: u64,
    pub usage_fraction: f64,
    pub total_lifetime: u64,
    pub window_ms: u64,
}

/// Serializable cost budget snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CostBudgetSnapshot {
    pub window_cost_cents: u64,
    pub max_cost_cents: u64,
    pub remaining_cents: u64,
    pub usage_fraction: f64,
    pub total_lifetime_cents: u64,
    pub window_ms: u64,
}

/// Serializable queue backpressure snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueueBackpressureSnapshot {
    pub current_depth: usize,
    pub max_depth: usize,
    pub peak_depth: usize,
    pub depth_fraction: f64,
    pub total_enqueued: u64,
    pub total_rejected: u64,
}

/// Serializable per-connector governor snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorGovernorSnapshot {
    pub connector_id: String,
    pub rate_limit_fill_ratio: f64,
    pub quota: QuotaSnapshot,
    pub cost: CostBudgetSnapshot,
    pub backoff_active: bool,
    pub backoff_remaining_ms: u64,
    pub consecutive_failures: u32,
}

/// Serializable governor telemetry snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GovernorTelemetrySnapshot {
    pub evaluations: u64,
    pub allows: u64,
    pub throttles: u64,
    pub rejections: u64,
}

/// Full governor snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernorSnapshot {
    pub global_rate_fill_ratio: f64,
    pub global_quota: QuotaSnapshot,
    pub queue: QueueBackpressureSnapshot,
    pub connectors: Vec<ConnectorGovernorSnapshot>,
    pub telemetry: GovernorTelemetrySnapshot,
}

// =============================================================================
// Registry / reliability / bundle / mesh telemetry
// =============================================================================

/// Snapshot for connector registry serialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryTelemetrySnapshot {
    pub packages_registered: u64,
    pub packages_verified: u64,
    pub digest_failures: u64,
    pub trust_denials: u64,
    pub capability_denials: u64,
    pub transparency_checks: u64,
    pub lookups: u64,
}

/// Serializable DLQ telemetry snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeadLetterTelemetrySnapshot {
    pub total_enqueued: u64,
    pub current_depth: u64,
    pub replayed_ok: u64,
    pub retry_attempts: u64,
    pub evictions: u64,
    pub discarded: u64,
    pub purged: u64,
}

/// Serializable reliability controller telemetry snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectorReliabilitySnapshot {
    pub connector_id: String,
    pub operations_attempted: u64,
    pub operations_succeeded: u64,
    pub operations_failed: u64,
    pub circuit_rejections: u64,
    pub dlq: DeadLetterTelemetrySnapshot,
}

/// Configuration for the audit-chain ingestion pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IngestionPipelineConfig {
    /// Maximum events ingested per second (0 = unlimited).
    pub max_ingest_per_sec: u64,
    /// Whether to record lifecycle events.
    pub ingest_lifecycle: bool,
    /// Whether to record inbound signal events.
    pub ingest_inbound: bool,
    /// Whether to record outbound action events.
    pub ingest_outbound: bool,
    /// Minimum severity to ingest (events below this are filtered).
    pub min_severity_level: u32,
    /// Maximum audit trail entries to retain.
    pub max_audit_entries: usize,
}

impl Default for IngestionPipelineConfig {
    fn default() -> Self {
        Self {
            max_ingest_per_sec: 0,
            ingest_lifecycle: true,
            ingest_inbound: true,
            ingest_outbound: true,
            min_severity_level: 0,
            max_audit_entries: 4096,
        }
    }
}

/// Telemetry counters for the ingestion pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IngestionTelemetry {
    pub events_received: u64,
    pub events_recorded: u64,
    pub events_filtered: u64,
    pub events_rejected: u64,
    pub lifecycle_events: u64,
    pub inbound_events: u64,
    pub outbound_events: u64,
}

/// Telemetry snapshot for the ingestion pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestionTelemetrySnapshot {
    pub captured_at_ms: u64,
    pub counters: IngestionTelemetry,
    pub audit_chain_length: usize,
    pub pipeline_config: IngestionPipelineConfig,
}

/// Telemetry counters for the bundle registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BundleRegistryTelemetry {
    pub bundles_registered: u64,
    pub bundles_removed: u64,
    pub bundles_updated: u64,
    pub validations_run: u64,
    pub validation_failures: u64,
}

/// Telemetry snapshot for the bundle registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleRegistrySnapshot {
    pub captured_at_ms: u64,
    pub counters: BundleRegistryTelemetry,
    pub bundle_count: usize,
    pub audit_log_length: usize,
    pub bundles_by_tier: BTreeMap<String, usize>,
    pub bundles_by_category: BTreeMap<String, usize>,
}

/// Serializable connector mesh telemetry snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshTelemetrySnapshot {
    pub hosts_registered: u64,
    pub hosts_deregistered: u64,
    pub zones_created: u64,
    pub routing_requests: u64,
    pub routing_successes: u64,
    pub routing_failures: u64,
    pub health_updates: u64,
    pub failure_events: u64,
    pub heartbeats_received: u64,
}
