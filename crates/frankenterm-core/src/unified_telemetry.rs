//! Unified telemetry schema for cross-subsystem observability.
//!
//! Provides a single envelope and tagged-union representation that wraps all
//! per-subsystem telemetry snapshots (policy, connectors, swarm, mux, storage,
//! runtime) with shared correlation identifiers, trace context, and redaction
//! semantics.
//!
//! # Design
//!
//! Every subsystem already emits its own snapshot struct (`PolicyEngineTelemetrySnapshot`,
//! `MuxPoolStats`, `SchedulerSnapshot`, etc.).  This module does **not** replace them;
//! it wraps them in a common [`TelemetryEnvelope`] that adds:
//!
//! - **Trace context** (`trace_id`, `span_id`, `parent_span_id`) for distributed tracing
//! - **Correlation ID** for request-scoped causality across layers
//! - **Source attribution** (CLI / MCP / Web / Internal)
//! - **Redaction label** (Public / Internal / Sensitive / PII) with scrubbed-field tracking
//! - **Health status** roll-up per subsystem
//!
//! The top-level [`UnifiedFleetSnapshot`] aggregates envelopes from every layer
//! into one serializable document suitable for dashboards, alerting, and audit.
//!
//! # Bead
//!
//! Implements ft-3681t.7.1 — unified telemetry schema across mux/swarm/connectors/policy.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Trace context
// ---------------------------------------------------------------------------

/// Distributed trace context carried on every telemetry envelope.
///
/// Modeled after W3C Trace Context but simplified for in-process use.
/// `trace_id` and `span_id` are hex-encoded 128-bit and 64-bit values
/// respectively (or empty when tracing is not active).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    /// 128-bit trace identifier (hex, 32 chars).  Empty string when unset.
    #[serde(default)]
    pub trace_id: String,
    /// 64-bit span identifier (hex, 16 chars).  Empty string when unset.
    #[serde(default)]
    pub span_id: String,
    /// Parent span, if this envelope was triggered by a prior span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
}

impl TraceContext {
    /// Create a new context with the given trace and span IDs.
    pub fn new(trace_id: impl Into<String>, span_id: impl Into<String>) -> Self {
        Self {
            trace_id: trace_id.into(),
            span_id: span_id.into(),
            parent_span_id: None,
        }
    }

    /// Attach a parent span.
    #[must_use]
    pub fn with_parent(mut self, parent: impl Into<String>) -> Self {
        self.parent_span_id = Some(parent.into());
        self
    }

    /// True when no trace is active.
    pub fn is_empty(&self) -> bool {
        self.trace_id.is_empty() && self.span_id.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Correlation
// ---------------------------------------------------------------------------

/// Free-form correlation identifier linking telemetry across subsystems.
///
/// Typically the request/action ID that triggered a cascade of subsystem work
/// (e.g. a `ft robot send` command ID that flows through policy → workflow →
/// mux → connector).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrelationId(pub String);

impl CorrelationId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Source attribution
// ---------------------------------------------------------------------------

/// The interface surface that originated this telemetry event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetrySource {
    /// Human CLI (`ft status`, `ft robot state`).
    Cli,
    /// MCP tool invocation (`ft mcp serve`).
    Mcp,
    /// Web/HTTP API (feature-gated).
    Web,
    /// Internal runtime (watcher loop, scheduler tick, health probe).
    Internal,
}

impl Default for TelemetrySource {
    fn default() -> Self {
        Self::Internal
    }
}

// ---------------------------------------------------------------------------
// Redaction / data classification
// ---------------------------------------------------------------------------

/// Data classification label controlling redaction and export policy.
///
/// Subsystems tag their snapshots with the most restrictive label applicable.
/// Downstream consumers (dashboards, JSONL export, MCP responses) must honour
/// this label:
///
/// - `Public` — safe for external dashboards and logs.
/// - `Internal` — safe within the fleet but not for external exposure.
/// - `Sensitive` — requires scrubbing before any persistence.
/// - `Pii` — must be stripped entirely before leaving the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionLabel {
    Public,
    Internal,
    Sensitive,
    Pii,
}

impl Default for RedactionLabel {
    fn default() -> Self {
        Self::Internal
    }
}

impl RedactionLabel {
    /// True when this label requires scrubbing before persistence.
    pub fn requires_scrub(&self) -> bool {
        matches!(self, Self::Sensitive | Self::Pii)
    }

    /// The more restrictive of two labels.
    #[must_use]
    pub fn max_restriction(self, other: Self) -> Self {
        std::cmp::max(self, other)
    }
}

/// Redaction metadata attached to an envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionMeta {
    /// Effective classification for the envelope payload.
    pub label: RedactionLabel,
    /// Field paths that were scrubbed before serialization (dot-separated).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scrubbed_fields: Vec<String>,
}

// ---------------------------------------------------------------------------
// Health status
// ---------------------------------------------------------------------------

/// Roll-up health status for a subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Operating normally.
    Healthy,
    /// Degraded but functional.
    Degraded,
    /// Non-functional or erroring.
    Unhealthy,
    /// Status cannot be determined (subsystem not initialized or timed out).
    Unknown,
}

impl Default for HealthStatus {
    fn default() -> Self {
        Self::Unknown
    }
}

impl HealthStatus {
    /// Combine two statuses, keeping the worse.
    #[must_use]
    pub fn worst(self, other: Self) -> Self {
        use HealthStatus::*;
        match (self, other) {
            (Unhealthy, _) | (_, Unhealthy) => Unhealthy,
            (Unknown, _) | (_, Unknown) => Unknown,
            (Degraded, _) | (_, Degraded) => Degraded,
            _ => Healthy,
        }
    }
}

// ---------------------------------------------------------------------------
// Subsystem tag
// ---------------------------------------------------------------------------

/// Identifies the subsystem layer that produced a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubsystemLayer {
    /// Policy engine and governance subsystems.
    Policy,
    /// Connector fabric (governor, registry, reliability, bundles, mesh, credentials).
    Connector,
    /// Swarm orchestration (scheduler, work queue).
    Swarm,
    /// Mux connection pool and transport.
    Mux,
    /// Storage and search pipeline.
    Storage,
    /// Core runtime (resource monitoring, histograms, counters).
    Runtime,
    /// Ingest and tailer (capture budgets, delta extraction).
    Ingest,
}

// ---------------------------------------------------------------------------
// Subsystem payload (tagged union)
// ---------------------------------------------------------------------------

/// Tagged union of all subsystem telemetry snapshots.
///
/// Each variant wraps the native snapshot type from its respective module.
/// Serialized with `serde(tag = "subsystem")` for machine-parseable dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "subsystem", rename_all = "snake_case")]
pub enum SubsystemPayload {
    /// Aggregated policy engine snapshot (all 21 subsystems).
    Policy(PolicyPayload),
    /// Swarm scheduler snapshot.
    SwarmScheduler(SwarmSchedulerPayload),
    /// Swarm work queue snapshot.
    SwarmWorkQueue(SwarmWorkQueuePayload),
    /// Mux connection pool stats.
    #[cfg(feature = "vendored")]
    MuxPool(MuxPoolPayload),
    /// Core telemetry (resources, histograms, counters).
    Runtime(RuntimePayload),
    /// Storage telemetry.
    Storage(StoragePayload),
    /// Ingest/tailer budget snapshot.
    Ingest(IngestPayload),
}

/// Wrapper for policy engine snapshot — keeps serde flat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyPayload {
    pub snapshot: crate::policy::PolicyEngineTelemetrySnapshot,
}

/// Wrapper for swarm scheduler snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmSchedulerPayload {
    pub snapshot: crate::swarm_scheduler::SchedulerSnapshot,
}

/// Wrapper for swarm work queue snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmWorkQueuePayload {
    pub snapshot: crate::swarm_work_queue::QueueSnapshot,
}

/// Wrapper for mux pool stats.
#[cfg(feature = "vendored")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuxPoolPayload {
    pub snapshot: crate::vendored::mux_pool::MuxPoolStats,
}

/// Wrapper for core runtime telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePayload {
    pub snapshot: crate::telemetry::TelemetrySnapshot,
}

/// Wrapper for storage telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoragePayload {
    pub snapshot: crate::storage_telemetry::StoragePipelineSnapshot,
}

/// Wrapper for ingest/tailer budget snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestPayload {
    pub snapshot: crate::tailer::SchedulerSnapshot,
}

// ---------------------------------------------------------------------------
// Telemetry envelope
// ---------------------------------------------------------------------------

/// Universal envelope wrapping any subsystem snapshot with trace context,
/// correlation, source, redaction, and health metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryEnvelope {
    /// Wall-clock capture time (epoch milliseconds).
    pub captured_at_ms: u64,
    /// Monotonic sequence number within this process lifetime.
    pub sequence: u64,
    /// Subsystem layer tag for fast dispatch.
    pub layer: SubsystemLayer,
    /// Distributed trace context.
    #[serde(default)]
    pub trace: TraceContext,
    /// Request-scoped correlation identifier.
    #[serde(default)]
    pub correlation_id: CorrelationId,
    /// Interface surface that triggered collection.
    #[serde(default)]
    pub source: TelemetrySource,
    /// Actor identifier (agent name, user, service principal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Session identifier for multi-session correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Originating pane ID (when the trigger is pane-scoped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Data classification and scrubbing metadata.
    #[serde(default)]
    pub redaction: RedactionMeta,
    /// Roll-up health status of the producing subsystem.
    #[serde(default)]
    pub health: HealthStatus,
    /// Subsystem-specific payload.
    pub payload: SubsystemPayload,
    /// Arbitrary key-value metadata for extensibility.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Causality links
// ---------------------------------------------------------------------------

/// A directed causality edge between two telemetry events.
///
/// Records that event A (identified by its correlation ID and span) caused
/// event B, with an optional latency measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalityLink {
    /// Correlation ID of the upstream (cause) event.
    pub cause_correlation_id: String,
    /// Span ID of the upstream event.
    pub cause_span_id: String,
    /// Correlation ID of the downstream (effect) event.
    pub effect_correlation_id: String,
    /// Span ID of the downstream event.
    pub effect_span_id: String,
    /// Latency from cause to effect in microseconds (if measured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_us: Option<u64>,
    /// Human-readable label for the edge (e.g. "policy→workflow").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

// ---------------------------------------------------------------------------
// Unified fleet snapshot (top-level aggregation)
// ---------------------------------------------------------------------------

/// Top-level fleet-wide telemetry snapshot.
///
/// Aggregates envelopes from every subsystem layer into a single serializable
/// document.  Suitable for `ft doctor --json`, dashboard polling, and JSONL
/// export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedFleetSnapshot {
    /// Schema version for forward compatibility.
    pub schema_version: String,
    /// Wall-clock time when the fleet snapshot was assembled (epoch ms).
    pub assembled_at_ms: u64,
    /// Per-subsystem envelopes, in collection order.
    pub envelopes: Vec<TelemetryEnvelope>,
    /// Cross-subsystem causality links collected during this snapshot window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causality: Vec<CausalityLink>,
    /// Aggregate health: worst across all envelopes.
    pub fleet_health: HealthStatus,
    /// Per-layer health summary.
    pub layer_health: HashMap<String, HealthStatus>,
    /// Effective redaction ceiling (most restrictive label across envelopes).
    pub redaction_ceiling: RedactionLabel,
}

/// Current schema version.
pub const SCHEMA_VERSION: &str = "1.0.0";

impl UnifiedFleetSnapshot {
    /// Build a fleet snapshot from a set of envelopes and causality links.
    pub fn from_envelopes(
        assembled_at_ms: u64,
        envelopes: Vec<TelemetryEnvelope>,
        causality: Vec<CausalityLink>,
    ) -> Self {
        let fleet_health = envelopes
            .iter()
            .fold(HealthStatus::Healthy, |acc, e| acc.worst(e.health));

        let mut layer_health: HashMap<String, HealthStatus> = HashMap::new();
        for env in &envelopes {
            let key = serde_json::to_value(&env.layer)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| format!("{:?}", env.layer));
            let entry = layer_health.entry(key).or_insert(HealthStatus::Healthy);
            *entry = entry.worst(env.health);
        }

        let redaction_ceiling = envelopes.iter().fold(RedactionLabel::Public, |acc, e| {
            acc.max_restriction(e.redaction.label)
        });

        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            assembled_at_ms,
            envelopes,
            causality,
            fleet_health,
            layer_health,
            redaction_ceiling,
        }
    }

    /// Number of envelopes in the snapshot.
    pub fn envelope_count(&self) -> usize {
        self.envelopes.len()
    }

    /// Filter envelopes to a specific subsystem layer.
    pub fn envelopes_for_layer(&self, layer: SubsystemLayer) -> Vec<&TelemetryEnvelope> {
        self.envelopes.iter().filter(|e| e.layer == layer).collect()
    }
}

// ---------------------------------------------------------------------------
// Envelope builder
// ---------------------------------------------------------------------------

/// Fluent builder for constructing [`TelemetryEnvelope`] instances.
#[derive(Debug)]
pub struct EnvelopeBuilder {
    captured_at_ms: u64,
    sequence: u64,
    layer: SubsystemLayer,
    trace: TraceContext,
    correlation_id: CorrelationId,
    source: TelemetrySource,
    actor_id: Option<String>,
    session_id: Option<String>,
    pane_id: Option<u64>,
    redaction: RedactionMeta,
    health: HealthStatus,
    extra: HashMap<String, serde_json::Value>,
}

impl EnvelopeBuilder {
    /// Start building an envelope for the given layer and capture time.
    pub fn new(layer: SubsystemLayer, captured_at_ms: u64) -> Self {
        Self {
            captured_at_ms,
            sequence: 0,
            layer,
            trace: TraceContext::default(),
            correlation_id: CorrelationId::default(),
            source: TelemetrySource::Internal,
            actor_id: None,
            session_id: None,
            pane_id: None,
            redaction: RedactionMeta::default(),
            health: HealthStatus::Unknown,
            extra: HashMap::new(),
        }
    }

    /// Set the monotonic sequence number.
    #[must_use]
    pub fn sequence(mut self, seq: u64) -> Self {
        self.sequence = seq;
        self
    }

    /// Set the trace context.
    #[must_use]
    pub fn trace(mut self, trace: TraceContext) -> Self {
        self.trace = trace;
        self
    }

    /// Set the correlation ID.
    #[must_use]
    pub fn correlation(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = CorrelationId::new(id);
        self
    }

    /// Set the originating source.
    #[must_use]
    pub fn source(mut self, source: TelemetrySource) -> Self {
        self.source = source;
        self
    }

    /// Set the actor identifier.
    #[must_use]
    pub fn actor(mut self, actor: impl Into<String>) -> Self {
        self.actor_id = Some(actor.into());
        self
    }

    /// Set the session identifier.
    #[must_use]
    pub fn session(mut self, session: impl Into<String>) -> Self {
        self.session_id = Some(session.into());
        self
    }

    /// Set the originating pane ID.
    #[must_use]
    pub fn pane(mut self, pane_id: u64) -> Self {
        self.pane_id = Some(pane_id);
        self
    }

    /// Set the redaction metadata.
    #[must_use]
    pub fn redaction(mut self, meta: RedactionMeta) -> Self {
        self.redaction = meta;
        self
    }

    /// Set the health status.
    #[must_use]
    pub fn health(mut self, health: HealthStatus) -> Self {
        self.health = health;
        self
    }

    /// Add arbitrary metadata.
    #[must_use]
    pub fn extra(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }

    /// Finalize with the given payload.
    pub fn build(self, payload: SubsystemPayload) -> TelemetryEnvelope {
        TelemetryEnvelope {
            captured_at_ms: self.captured_at_ms,
            sequence: self.sequence,
            layer: self.layer,
            trace: self.trace,
            correlation_id: self.correlation_id,
            source: self.source,
            actor_id: self.actor_id,
            session_id: self.session_id,
            pane_id: self.pane_id,
            redaction: self.redaction,
            health: self.health,
            payload,
            extra: self.extra,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_time() -> u64 {
        1_710_000_000_000 // 2024-03-09 approx
    }

    // -- TraceContext --

    #[test]
    fn trace_context_empty_by_default() {
        let ctx = TraceContext::default();
        assert!(ctx.is_empty());
        assert!(ctx.parent_span_id.is_none());
    }

    #[test]
    fn trace_context_new_and_with_parent() {
        let ctx = TraceContext::new("abc123", "span1").with_parent("parent0");
        assert!(!ctx.is_empty());
        assert_eq!(ctx.trace_id, "abc123");
        assert_eq!(ctx.span_id, "span1");
        assert_eq!(ctx.parent_span_id.as_deref(), Some("parent0"));
    }

    #[test]
    fn trace_context_serde_roundtrip() {
        let ctx = TraceContext::new("t1", "s1").with_parent("p0");
        let json = serde_json::to_string(&ctx).unwrap();
        let back: TraceContext = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx, back);
    }

    // -- CorrelationId --

    #[test]
    fn correlation_id_display() {
        let cid = CorrelationId::new("req-42");
        assert_eq!(cid.to_string(), "req-42");
        assert!(!cid.is_empty());
    }

    #[test]
    fn correlation_id_default_is_empty() {
        let cid = CorrelationId::default();
        assert!(cid.is_empty());
    }

    // -- RedactionLabel --

    #[test]
    fn redaction_label_ordering() {
        assert!(RedactionLabel::Public < RedactionLabel::Internal);
        assert!(RedactionLabel::Internal < RedactionLabel::Sensitive);
        assert!(RedactionLabel::Sensitive < RedactionLabel::Pii);
    }

    #[test]
    fn redaction_label_max_restriction() {
        assert_eq!(
            RedactionLabel::Public.max_restriction(RedactionLabel::Sensitive),
            RedactionLabel::Sensitive
        );
        assert_eq!(
            RedactionLabel::Pii.max_restriction(RedactionLabel::Internal),
            RedactionLabel::Pii
        );
    }

    #[test]
    fn redaction_label_requires_scrub() {
        assert!(!RedactionLabel::Public.requires_scrub());
        assert!(!RedactionLabel::Internal.requires_scrub());
        assert!(RedactionLabel::Sensitive.requires_scrub());
        assert!(RedactionLabel::Pii.requires_scrub());
    }

    // -- HealthStatus --

    #[test]
    fn health_status_worst() {
        assert_eq!(
            HealthStatus::Healthy.worst(HealthStatus::Healthy),
            HealthStatus::Healthy
        );
        assert_eq!(
            HealthStatus::Healthy.worst(HealthStatus::Degraded),
            HealthStatus::Degraded
        );
        assert_eq!(
            HealthStatus::Degraded.worst(HealthStatus::Unknown),
            HealthStatus::Unknown
        );
        assert_eq!(
            HealthStatus::Unknown.worst(HealthStatus::Unhealthy),
            HealthStatus::Unhealthy
        );
        assert_eq!(
            HealthStatus::Unhealthy.worst(HealthStatus::Healthy),
            HealthStatus::Unhealthy
        );
    }

    // -- EnvelopeBuilder --

    #[test]
    fn envelope_builder_constructs_valid_envelope() {
        let payload = SubsystemPayload::Ingest(IngestPayload {
            snapshot: crate::tailer::SchedulerSnapshot {
                budget_active: false,
                max_captures_per_sec: 0,
                max_bytes_per_sec: 0,
                captures_remaining: 0,
                bytes_remaining: 0,
                total_rate_limited: 0,
                total_byte_budget_exceeded: 0,
                total_throttle_events: 0,
                tracked_panes: 5,
            },
        });

        let env = EnvelopeBuilder::new(SubsystemLayer::Ingest, sample_time())
            .sequence(1)
            .trace(TraceContext::new("t1", "s1"))
            .correlation("req-1")
            .source(TelemetrySource::Cli)
            .actor("PinkForge")
            .session("sess-42")
            .pane(3)
            .health(HealthStatus::Healthy)
            .build(payload);

        assert_eq!(env.captured_at_ms, sample_time());
        assert_eq!(env.sequence, 1);
        assert_eq!(env.layer, SubsystemLayer::Ingest);
        assert_eq!(env.trace.trace_id, "t1");
        assert_eq!(env.correlation_id.0, "req-1");
        assert_eq!(env.source, TelemetrySource::Cli);
        assert_eq!(env.actor_id.as_deref(), Some("PinkForge"));
        assert_eq!(env.session_id.as_deref(), Some("sess-42"));
        assert_eq!(env.pane_id, Some(3));
        assert_eq!(env.health, HealthStatus::Healthy);
    }

    // -- UnifiedFleetSnapshot --

    #[test]
    fn fleet_snapshot_empty() {
        let snap = UnifiedFleetSnapshot::from_envelopes(sample_time(), vec![], vec![]);
        assert_eq!(snap.schema_version, SCHEMA_VERSION);
        assert_eq!(snap.envelope_count(), 0);
        assert_eq!(snap.fleet_health, HealthStatus::Healthy);
        assert_eq!(snap.redaction_ceiling, RedactionLabel::Public);
        assert!(snap.layer_health.is_empty());
    }

    #[test]
    fn fleet_snapshot_aggregates_health_and_redaction() {
        let healthy_env = EnvelopeBuilder::new(SubsystemLayer::Policy, sample_time())
            .health(HealthStatus::Healthy)
            .redaction(RedactionMeta {
                label: RedactionLabel::Internal,
                scrubbed_fields: vec![],
            })
            .build(SubsystemPayload::Ingest(IngestPayload {
                snapshot: crate::tailer::SchedulerSnapshot {
                    budget_active: false,
                    max_captures_per_sec: 0,
                    max_bytes_per_sec: 0,
                    captures_remaining: 0,
                    bytes_remaining: 0,
                    total_rate_limited: 0,
                    total_byte_budget_exceeded: 0,
                    total_throttle_events: 0,
                    tracked_panes: 0,
                },
            }));

        let degraded_env = EnvelopeBuilder::new(SubsystemLayer::Mux, sample_time())
            .health(HealthStatus::Degraded)
            .redaction(RedactionMeta {
                label: RedactionLabel::Sensitive,
                scrubbed_fields: vec!["credentials.token".into()],
            })
            .build(SubsystemPayload::Ingest(IngestPayload {
                snapshot: crate::tailer::SchedulerSnapshot {
                    budget_active: false,
                    max_captures_per_sec: 0,
                    max_bytes_per_sec: 0,
                    captures_remaining: 0,
                    bytes_remaining: 0,
                    total_rate_limited: 0,
                    total_byte_budget_exceeded: 0,
                    total_throttle_events: 0,
                    tracked_panes: 0,
                },
            }));

        let snap = UnifiedFleetSnapshot::from_envelopes(
            sample_time(),
            vec![healthy_env, degraded_env],
            vec![],
        );

        assert_eq!(snap.envelope_count(), 2);
        assert_eq!(snap.fleet_health, HealthStatus::Degraded);
        assert_eq!(snap.redaction_ceiling, RedactionLabel::Sensitive);
        assert_eq!(
            snap.layer_health.get("policy"),
            Some(&HealthStatus::Healthy)
        );
        assert_eq!(snap.layer_health.get("mux"), Some(&HealthStatus::Degraded));
    }

    #[test]
    fn fleet_snapshot_filter_by_layer() {
        let e1 = EnvelopeBuilder::new(SubsystemLayer::Swarm, sample_time()).build(
            SubsystemPayload::Ingest(IngestPayload {
                snapshot: crate::tailer::SchedulerSnapshot {
                    budget_active: false,
                    max_captures_per_sec: 0,
                    max_bytes_per_sec: 0,
                    captures_remaining: 0,
                    bytes_remaining: 0,
                    total_rate_limited: 0,
                    total_byte_budget_exceeded: 0,
                    total_throttle_events: 0,
                    tracked_panes: 0,
                },
            }),
        );
        let e2 = EnvelopeBuilder::new(SubsystemLayer::Policy, sample_time()).build(
            SubsystemPayload::Ingest(IngestPayload {
                snapshot: crate::tailer::SchedulerSnapshot {
                    budget_active: false,
                    max_captures_per_sec: 0,
                    max_bytes_per_sec: 0,
                    captures_remaining: 0,
                    bytes_remaining: 0,
                    total_rate_limited: 0,
                    total_byte_budget_exceeded: 0,
                    total_throttle_events: 0,
                    tracked_panes: 0,
                },
            }),
        );
        let e3 = EnvelopeBuilder::new(SubsystemLayer::Swarm, sample_time()).build(
            SubsystemPayload::Ingest(IngestPayload {
                snapshot: crate::tailer::SchedulerSnapshot {
                    budget_active: false,
                    max_captures_per_sec: 0,
                    max_bytes_per_sec: 0,
                    captures_remaining: 0,
                    bytes_remaining: 0,
                    total_rate_limited: 0,
                    total_byte_budget_exceeded: 0,
                    total_throttle_events: 0,
                    tracked_panes: 0,
                },
            }),
        );

        let snap = UnifiedFleetSnapshot::from_envelopes(sample_time(), vec![e1, e2, e3], vec![]);

        assert_eq!(snap.envelopes_for_layer(SubsystemLayer::Swarm).len(), 2);
        assert_eq!(snap.envelopes_for_layer(SubsystemLayer::Policy).len(), 1);
        assert_eq!(snap.envelopes_for_layer(SubsystemLayer::Mux).len(), 0);
    }

    // -- CausalityLink --

    #[test]
    fn causality_link_serde_roundtrip() {
        let link = CausalityLink {
            cause_correlation_id: "req-1".into(),
            cause_span_id: "s1".into(),
            effect_correlation_id: "req-2".into(),
            effect_span_id: "s2".into(),
            latency_us: Some(420),
            label: Some("policy→workflow".into()),
        };
        let json = serde_json::to_string(&link).unwrap();
        let back: CausalityLink = serde_json::from_str(&json).unwrap();
        assert_eq!(link, back);
    }

    // -- Serde roundtrip for full envelope --

    #[test]
    fn envelope_serde_roundtrip() {
        let env = EnvelopeBuilder::new(SubsystemLayer::Ingest, sample_time())
            .sequence(42)
            .trace(TraceContext::new("trace-abc", "span-1").with_parent("span-0"))
            .correlation("corr-99")
            .source(TelemetrySource::Mcp)
            .actor("agent-x")
            .health(HealthStatus::Healthy)
            .redaction(RedactionMeta {
                label: RedactionLabel::Internal,
                scrubbed_fields: vec![],
            })
            .build(SubsystemPayload::Ingest(IngestPayload {
                snapshot: crate::tailer::SchedulerSnapshot {
                    budget_active: true,
                    max_captures_per_sec: 100,
                    max_bytes_per_sec: 1_000_000,
                    captures_remaining: 50,
                    bytes_remaining: 500_000,
                    total_rate_limited: 3,
                    total_byte_budget_exceeded: 1,
                    total_throttle_events: 4,
                    tracked_panes: 10,
                },
            }));

        let json = serde_json::to_string_pretty(&env).unwrap();
        let back: TelemetryEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.captured_at_ms, sample_time());
        assert_eq!(back.sequence, 42);
        assert_eq!(back.trace.trace_id, "trace-abc");
        assert_eq!(back.correlation_id.0, "corr-99");
        assert_eq!(back.source, TelemetrySource::Mcp);
        assert_eq!(back.health, HealthStatus::Healthy);
    }

    // -- Fleet snapshot serde roundtrip --

    #[test]
    fn fleet_snapshot_serde_roundtrip() {
        let env = EnvelopeBuilder::new(SubsystemLayer::Runtime, sample_time())
            .health(HealthStatus::Healthy)
            .build(SubsystemPayload::Ingest(IngestPayload {
                snapshot: crate::tailer::SchedulerSnapshot {
                    budget_active: false,
                    max_captures_per_sec: 0,
                    max_bytes_per_sec: 0,
                    captures_remaining: 0,
                    bytes_remaining: 0,
                    total_rate_limited: 0,
                    total_byte_budget_exceeded: 0,
                    total_throttle_events: 0,
                    tracked_panes: 0,
                },
            }));

        let snap = UnifiedFleetSnapshot::from_envelopes(sample_time(), vec![env], vec![]);
        let json = serde_json::to_string(&snap).unwrap();
        let back: UnifiedFleetSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert_eq!(back.envelope_count(), 1);
        assert_eq!(back.fleet_health, HealthStatus::Healthy);
    }
}
