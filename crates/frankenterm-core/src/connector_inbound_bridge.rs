//! Inbound connector bridge: connector signals → workflows / robot events.
//!
//! Routes inbound connector signals (webhooks, streams, polls) into
//! FrankenTerm's event bus as `PatternDetected` events, enabling
//! workflow automation and robot-mode event subscriptions.
//!
//! Key concerns:
//! - Deduplication via correlation IDs (bounded LRU cache)
//! - Signal-to-Detection mapping with configurable rules
//! - Fail-closed on unknown signal types (configurable)
//! - Structured tracing for end-to-end correlation
//!
//! Part of ft-3681t.5.7.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::connector_host_runtime::{ConnectorFailureClass, ConnectorLifecyclePhase};
use crate::events::{Event, EventBus};
use crate::patterns::{AgentType, Detection, Severity};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the inbound connector bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorInboundBridgeConfig {
    /// Maximum dedup cache entries before oldest are evicted.
    #[serde(default = "default_dedup_capacity")]
    pub dedup_capacity: usize,
    /// Time-to-live for dedup entries (seconds).
    #[serde(default = "default_dedup_ttl_secs")]
    pub dedup_ttl_secs: u64,
    /// Whether to reject signals with unknown signal_kind.
    #[serde(default)]
    pub reject_unknown_kinds: bool,
    /// Custom signal-to-rule_id mapping overrides.
    /// Key: "connector_name.signal_kind", Value: rule_id prefix.
    #[serde(default)]
    pub rule_id_overrides: HashMap<String, String>,
}

fn default_dedup_capacity() -> usize {
    4096
}

fn default_dedup_ttl_secs() -> u64 {
    300
}

impl Default for ConnectorInboundBridgeConfig {
    fn default() -> Self {
        Self {
            dedup_capacity: default_dedup_capacity(),
            dedup_ttl_secs: default_dedup_ttl_secs(),
            reject_unknown_kinds: false,
            rule_id_overrides: HashMap::new(),
        }
    }
}

// =============================================================================
// Signal types
// =============================================================================

/// Kind of inbound connector signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorSignalKind {
    /// Webhook delivery (HTTP callback from external service).
    Webhook,
    /// Stream event (persistent connection push).
    Stream,
    /// Poll result (periodic check returning new data).
    Poll,
    /// Lifecycle transition (connector state change).
    Lifecycle,
    /// Health check result (probe success/failure).
    HealthCheck,
    /// Failure report (connector error/crash).
    Failure,
    /// Custom / user-defined signal kind.
    Custom,
}

impl ConnectorSignalKind {
    /// Stable string label for rule_id construction.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::Stream => "stream",
            Self::Poll => "poll",
            Self::Lifecycle => "lifecycle",
            Self::HealthCheck => "health_check",
            Self::Failure => "failure",
            Self::Custom => "custom",
        }
    }
}

impl std::fmt::Display for ConnectorSignalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An inbound signal from a connector.
///
/// This is the primary input to the bridge. Connectors produce these
/// and the bridge translates them into `Detection`s published on the event bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorSignal {
    /// Source connector identifier (e.g., "github", "slack", "jira").
    pub source_connector: String,
    /// Kind of signal.
    pub signal_kind: ConnectorSignalKind,
    /// Opaque correlation ID for deduplication and tracing.
    /// Signals with the same correlation_id within the TTL window are deduplicated.
    pub correlation_id: Option<String>,
    /// Timestamp of the signal at the source (millis since epoch).
    pub timestamp_ms: u64,
    /// Target pane ID (if the signal is pane-scoped).
    pub pane_id: Option<u64>,
    /// Structured payload from the connector.
    pub payload: serde_json::Value,
    /// Optional sub-type for more specific routing (e.g., "pr_opened", "push").
    pub sub_type: Option<String>,
    /// Optional lifecycle phase (for lifecycle signals).
    pub lifecycle_phase: Option<ConnectorLifecyclePhase>,
    /// Optional failure class (for failure signals).
    pub failure_class: Option<ConnectorFailureClass>,
}

impl ConnectorSignal {
    /// Create a new connector signal.
    #[must_use]
    pub fn new(
        source_connector: impl Into<String>,
        signal_kind: ConnectorSignalKind,
        payload: serde_json::Value,
    ) -> Self {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            source_connector: source_connector.into(),
            signal_kind,
            correlation_id: None,
            timestamp_ms: now_ms,
            pane_id: None,
            payload,
            sub_type: None,
            lifecycle_phase: None,
            failure_class: None,
        }
    }

    /// Set the correlation ID.
    #[must_use]
    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    /// Set the target pane ID.
    #[must_use]
    pub fn with_pane_id(mut self, pane_id: u64) -> Self {
        self.pane_id = Some(pane_id);
        self
    }

    /// Set the sub-type.
    #[must_use]
    pub fn with_sub_type(mut self, sub_type: impl Into<String>) -> Self {
        self.sub_type = Some(sub_type.into());
        self
    }

    /// Set the timestamp.
    #[must_use]
    pub fn with_timestamp_ms(mut self, ts: u64) -> Self {
        self.timestamp_ms = ts;
        self
    }

    /// Set the lifecycle phase (for lifecycle signals).
    #[must_use]
    pub fn with_lifecycle_phase(mut self, phase: ConnectorLifecyclePhase) -> Self {
        self.lifecycle_phase = Some(phase);
        self
    }

    /// Set the failure class (for failure signals).
    #[must_use]
    pub fn with_failure_class(mut self, class: ConnectorFailureClass) -> Self {
        self.failure_class = Some(class);
        self
    }

    /// Construct the rule_id for this signal.
    ///
    /// Format: `connector.<source>:<kind>[.<sub_type>]`
    ///
    /// Examples:
    /// - `connector.github:webhook.push`
    /// - `connector.slack:stream.message`
    /// - `connector.jira:poll`
    /// - `connector.myconn:lifecycle.ready`
    #[must_use]
    pub fn rule_id(&self) -> String {
        let base = format!(
            "connector.{}:{}",
            self.source_connector,
            self.signal_kind.as_str()
        );
        if let Some(ref sub) = self.sub_type {
            format!("{base}.{sub}")
        } else if let Some(phase) = self.lifecycle_phase {
            format!("{base}.{}", phase_label(phase))
        } else if let Some(ref class) = self.failure_class {
            format!("{base}.{}", class.as_str())
        } else {
            base
        }
    }

    /// Determine the severity from signal kind and payload.
    #[must_use]
    pub fn severity(&self) -> Severity {
        match self.signal_kind {
            ConnectorSignalKind::Failure => Severity::Critical,
            ConnectorSignalKind::HealthCheck => {
                if self.failure_class.is_some() {
                    Severity::Warning
                } else {
                    Severity::Info
                }
            }
            ConnectorSignalKind::Lifecycle => Severity::Info,
            _ => Severity::Info,
        }
    }
}

/// Map lifecycle phase to a stable label.
fn phase_label(phase: ConnectorLifecyclePhase) -> &'static str {
    phase.as_str()
}

// =============================================================================
// Bridge result
// =============================================================================

/// Outcome of routing a single signal through the bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeRouteResult {
    /// The rule_id assigned to this signal.
    pub rule_id: String,
    /// Whether the signal was a duplicate (and therefore skipped).
    pub deduplicated: bool,
    /// Number of EventBus subscribers that received the event.
    pub delivered_count: usize,
    /// Correlation ID (echoed back for tracing).
    pub correlation_id: Option<String>,
}

// =============================================================================
// Errors
// =============================================================================

/// Errors from the inbound connector bridge.
#[derive(Debug, Error)]
pub enum ConnectorBridgeError {
    #[error("duplicate signal (correlation_id={0})")]
    DuplicateSignal(String),
    #[error("unknown signal kind rejected by policy")]
    UnknownKindRejected,
    #[error("no pane_id on signal (required for event routing)")]
    MissingPaneId,
    #[error("signal mapping failed: {0}")]
    MappingFailed(String),
}

// =============================================================================
// Deduplicator
// =============================================================================

/// Entry in the dedup cache.
#[derive(Debug, Clone)]
struct DedupEntry {
    correlation_id: String,
    inserted_at_ms: u64,
}

/// Bounded, TTL-aware deduplicator for connector signals.
///
/// Uses an LRU-ordered VecDeque with capacity cap and time-based expiry.
#[derive(Debug)]
pub struct SignalDeduplicator {
    entries: VecDeque<DedupEntry>,
    capacity: usize,
    ttl_ms: u64,
}

impl SignalDeduplicator {
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

        // Check for existing entry
        if self
            .entries
            .iter()
            .any(|e| e.correlation_id == correlation_id)
        {
            return false;
        }

        // Evict oldest if at capacity
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }

        self.entries.push_back(DedupEntry {
            correlation_id: correlation_id.to_string(),
            inserted_at_ms: now_ms,
        });
        true
    }

    /// Remove expired entries.
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

/// Telemetry counters for the inbound bridge.
#[derive(Debug, Default)]
pub struct ConnectorBridgeTelemetry {
    pub signals_received: u64,
    pub signals_routed: u64,
    pub signals_deduplicated: u64,
    pub signals_rejected: u64,
    pub events_published: u64,
}

/// Serializable telemetry snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectorBridgeTelemetrySnapshot {
    pub signals_received: u64,
    pub signals_routed: u64,
    pub signals_deduplicated: u64,
    pub signals_rejected: u64,
    pub events_published: u64,
}

impl ConnectorBridgeTelemetry {
    /// Take a snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ConnectorBridgeTelemetrySnapshot {
        ConnectorBridgeTelemetrySnapshot {
            signals_received: self.signals_received,
            signals_routed: self.signals_routed,
            signals_deduplicated: self.signals_deduplicated,
            signals_rejected: self.signals_rejected,
            events_published: self.events_published,
        }
    }
}

// =============================================================================
// Bridge
// =============================================================================

/// Inbound connector bridge.
///
/// Routes connector signals into the FrankenTerm event bus as
/// `Event::PatternDetected`, enabling workflow triggers and robot-mode
/// event subscriptions.
pub struct ConnectorInboundBridge {
    event_bus: Arc<EventBus>,
    deduplicator: SignalDeduplicator,
    config: ConnectorInboundBridgeConfig,
    telemetry: ConnectorBridgeTelemetry,
}

impl ConnectorInboundBridge {
    /// Create a new inbound bridge.
    #[must_use]
    pub fn new(event_bus: Arc<EventBus>, config: ConnectorInboundBridgeConfig) -> Self {
        let dedup_ttl = Duration::from_secs(config.dedup_ttl_secs);
        Self {
            event_bus,
            deduplicator: SignalDeduplicator::new(config.dedup_capacity, dedup_ttl),
            config,
            telemetry: ConnectorBridgeTelemetry::default(),
        }
    }

    /// Route a connector signal through the bridge.
    ///
    /// Steps:
    /// 1. Deduplication check (if correlation_id present)
    /// 2. Signal-kind validation (if reject_unknown_kinds)
    /// 3. Map signal → Detection
    /// 4. Publish as PatternDetected on EventBus
    ///
    /// Returns a `BridgeRouteResult` describing what happened.
    pub fn route_signal(
        &mut self,
        signal: &ConnectorSignal,
    ) -> Result<BridgeRouteResult, ConnectorBridgeError> {
        self.telemetry.signals_received += 1;

        // 1. Deduplication
        if let Some(ref cid) = signal.correlation_id {
            let now_ms = signal.timestamp_ms;
            if !self.deduplicator.check_and_record(cid, now_ms) {
                self.telemetry.signals_deduplicated += 1;
                debug!(
                    correlation_id = %cid,
                    source = %signal.source_connector,
                    kind = %signal.signal_kind,
                    "inbound signal deduplicated"
                );
                return Ok(BridgeRouteResult {
                    rule_id: signal.rule_id(),
                    deduplicated: true,
                    delivered_count: 0,
                    correlation_id: signal.correlation_id.clone(),
                });
            }
        }

        // 2. Unknown-kind rejection
        if self.config.reject_unknown_kinds && signal.signal_kind == ConnectorSignalKind::Custom {
            self.telemetry.signals_rejected += 1;
            warn!(
                source = %signal.source_connector,
                kind = %signal.signal_kind,
                "unknown signal kind rejected"
            );
            return Err(ConnectorBridgeError::UnknownKindRejected);
        }

        // 3. Map to Detection
        let rule_id = self.resolve_rule_id(signal);
        let detection = Self::map_to_detection(signal, &rule_id);

        // 4. Determine pane_id (default to 0 for system-level signals)
        let pane_id = signal.pane_id.unwrap_or(0);

        // 5. Publish
        let event = Event::PatternDetected {
            pane_id,
            pane_uuid: None,
            detection,
            event_id: None,
        };

        let delivered = self.event_bus.publish(event);
        self.telemetry.signals_routed += 1;
        self.telemetry.events_published += 1;

        info!(
            rule_id = %rule_id,
            source = %signal.source_connector,
            kind = %signal.signal_kind,
            pane_id = pane_id,
            delivered = delivered,
            correlation_id = ?signal.correlation_id,
            "connector signal routed to event bus"
        );

        Ok(BridgeRouteResult {
            rule_id,
            deduplicated: false,
            delivered_count: delivered,
            correlation_id: signal.correlation_id.clone(),
        })
    }

    /// Resolve the rule_id, checking config overrides first.
    fn resolve_rule_id(&self, signal: &ConnectorSignal) -> String {
        let lookup_key = format!(
            "{}.{}",
            signal.source_connector,
            signal.signal_kind.as_str()
        );

        if let Some(override_prefix) = self.config.rule_id_overrides.get(&lookup_key) {
            if let Some(ref sub) = signal.sub_type {
                format!("{override_prefix}.{sub}")
            } else {
                override_prefix.clone()
            }
        } else {
            signal.rule_id()
        }
    }

    /// Map a connector signal to a Detection.
    fn map_to_detection(signal: &ConnectorSignal, rule_id: &str) -> Detection {
        let event_type = format!(
            "connector.{}",
            signal.sub_type.as_deref().unwrap_or(signal.signal_kind.as_str())
        );

        // Build extracted data with standard fields
        let mut extracted = signal.payload.clone();
        if let serde_json::Value::Object(ref mut map) = extracted {
            map.insert(
                "source_connector".into(),
                serde_json::Value::String(signal.source_connector.clone()),
            );
            map.insert(
                "signal_kind".into(),
                serde_json::Value::String(signal.signal_kind.as_str().to_string()),
            );
            if let Some(ref cid) = signal.correlation_id {
                map.insert(
                    "correlation_id".into(),
                    serde_json::Value::String(cid.clone()),
                );
            }
            if let Some(phase) = signal.lifecycle_phase {
                map.insert(
                    "lifecycle_phase".into(),
                    serde_json::Value::String(phase_label(phase).to_string()),
                );
            }
            if let Some(ref class) = signal.failure_class {
                map.insert(
                    "failure_class".into(),
                    serde_json::Value::String(class.as_str().to_string()),
                );
            }
        }

        Detection {
            rule_id: rule_id.to_string(),
            agent_type: AgentType::Unknown,
            event_type,
            severity: signal.severity(),
            confidence: 1.0,
            extracted,
            matched_text: format!(
                "connector signal: {} {}",
                signal.source_connector,
                signal.signal_kind.as_str()
            ),
            span: (0, 0),
        }
    }

    /// Get a telemetry snapshot.
    #[must_use]
    pub fn telemetry_snapshot(&self) -> ConnectorBridgeTelemetrySnapshot {
        self.telemetry.snapshot()
    }

    /// Get the dedup cache size.
    #[must_use]
    pub fn dedup_cache_len(&self) -> usize {
        self.deduplicator.len()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bus() -> Arc<EventBus> {
        Arc::new(EventBus::new(64))
    }

    fn default_bridge(bus: Arc<EventBus>) -> ConnectorInboundBridge {
        ConnectorInboundBridge::new(bus, ConnectorInboundBridgeConfig::default())
    }

    fn test_signal(source: &str, kind: ConnectorSignalKind) -> ConnectorSignal {
        ConnectorSignal::new(source, kind, serde_json::json!({"key": "value"}))
    }

    // ── Signal construction ─────────────────────────────────────────────

    #[test]
    fn signal_rule_id_basic() {
        let sig = test_signal("github", ConnectorSignalKind::Webhook);
        assert_eq!(sig.rule_id(), "connector.github:webhook");
    }

    #[test]
    fn signal_rule_id_with_sub_type() {
        let sig =
            test_signal("github", ConnectorSignalKind::Webhook).with_sub_type("push");
        assert_eq!(sig.rule_id(), "connector.github:webhook.push");
    }

    #[test]
    fn signal_rule_id_with_lifecycle_phase() {
        let sig = test_signal("myconn", ConnectorSignalKind::Lifecycle)
            .with_lifecycle_phase(ConnectorLifecyclePhase::Running);
        assert_eq!(sig.rule_id(), "connector.myconn:lifecycle.running");
    }

    #[test]
    fn signal_rule_id_with_failure_class() {
        let sig = test_signal("myconn", ConnectorSignalKind::Failure)
            .with_failure_class(ConnectorFailureClass::Network);
        assert_eq!(sig.rule_id(), "connector.myconn:failure.network");
    }

    #[test]
    fn signal_sub_type_takes_precedence_over_lifecycle() {
        let sig = test_signal("myconn", ConnectorSignalKind::Lifecycle)
            .with_sub_type("custom_event")
            .with_lifecycle_phase(ConnectorLifecyclePhase::Failed);
        assert_eq!(sig.rule_id(), "connector.myconn:lifecycle.custom_event");
    }

    #[test]
    fn signal_severity_failure_is_critical() {
        let sig = test_signal("x", ConnectorSignalKind::Failure);
        assert_eq!(sig.severity(), Severity::Critical);
    }

    #[test]
    fn signal_severity_webhook_is_info() {
        let sig = test_signal("x", ConnectorSignalKind::Webhook);
        assert_eq!(sig.severity(), Severity::Info);
    }

    #[test]
    fn signal_severity_health_check_with_failure_is_warning() {
        let sig = test_signal("x", ConnectorSignalKind::HealthCheck)
            .with_failure_class(ConnectorFailureClass::Timeout);
        assert_eq!(sig.severity(), Severity::Warning);
    }

    #[test]
    fn signal_severity_health_check_ok_is_info() {
        let sig = test_signal("x", ConnectorSignalKind::HealthCheck);
        assert_eq!(sig.severity(), Severity::Info);
    }

    #[test]
    fn signal_builder_chain() {
        let sig = ConnectorSignal::new(
            "slack",
            ConnectorSignalKind::Stream,
            serde_json::json!({"channel": "#general"}),
        )
        .with_correlation_id("abc-123")
        .with_pane_id(42)
        .with_sub_type("message")
        .with_timestamp_ms(1000);

        assert_eq!(sig.source_connector, "slack");
        assert_eq!(sig.correlation_id.as_deref(), Some("abc-123"));
        assert_eq!(sig.pane_id, Some(42));
        assert_eq!(sig.sub_type.as_deref(), Some("message"));
        assert_eq!(sig.timestamp_ms, 1000);
    }

    // ── Deduplicator ────────────────────────────────────────────────────

    #[test]
    fn dedup_new_id_returns_true() {
        let mut dedup = SignalDeduplicator::new(10, Duration::from_secs(60));
        assert!(dedup.check_and_record("id-1", 1000));
        assert_eq!(dedup.len(), 1);
    }

    #[test]
    fn dedup_duplicate_returns_false() {
        let mut dedup = SignalDeduplicator::new(10, Duration::from_secs(60));
        assert!(dedup.check_and_record("id-1", 1000));
        assert!(!dedup.check_and_record("id-1", 1500));
        assert_eq!(dedup.len(), 1);
    }

    #[test]
    fn dedup_expired_entry_allows_resend() {
        let mut dedup = SignalDeduplicator::new(10, Duration::from_millis(100));
        assert!(dedup.check_and_record("id-1", 1000));
        // After TTL expiry
        assert!(dedup.check_and_record("id-1", 1200));
    }

    #[test]
    fn dedup_capacity_eviction() {
        let mut dedup = SignalDeduplicator::new(3, Duration::from_secs(60));
        assert!(dedup.check_and_record("a", 100));
        assert!(dedup.check_and_record("b", 200));
        assert!(dedup.check_and_record("c", 300));
        assert_eq!(dedup.len(), 3);

        // Adding 4th evicts oldest ("a")
        assert!(dedup.check_and_record("d", 400));
        assert_eq!(dedup.len(), 3);

        // "a" was evicted, so it's treated as new
        assert!(dedup.check_and_record("a", 500));
    }

    #[test]
    fn dedup_empty_initially() {
        let dedup = SignalDeduplicator::new(10, Duration::from_secs(60));
        assert!(dedup.is_empty());
        assert_eq!(dedup.len(), 0);
    }

    // ── Bridge routing ──────────────────────────────────────────────────

    #[test]
    fn bridge_routes_signal_to_event_bus() {
        let bus = make_bus();
        let _sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        let sig = test_signal("github", ConnectorSignalKind::Webhook)
            .with_pane_id(1)
            .with_sub_type("push")
            .with_timestamp_ms(1000);

        let result = bridge.route_signal(&sig).unwrap();
        assert_eq!(result.rule_id, "connector.github:webhook.push");
        assert!(!result.deduplicated);
        assert!(result.delivered_count > 0);

        let snap = bridge.telemetry_snapshot();
        assert_eq!(snap.signals_received, 1);
        assert_eq!(snap.signals_routed, 1);
        assert_eq!(snap.events_published, 1);
    }

    #[test]
    fn bridge_deduplicates_by_correlation_id() {
        let bus = make_bus();
        let _sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        let sig = test_signal("github", ConnectorSignalKind::Webhook)
            .with_correlation_id("dup-1")
            .with_timestamp_ms(1000);

        let r1 = bridge.route_signal(&sig).unwrap();
        assert!(!r1.deduplicated);

        let r2 = bridge.route_signal(&sig).unwrap();
        assert!(r2.deduplicated);
        assert_eq!(r2.delivered_count, 0);

        let snap = bridge.telemetry_snapshot();
        assert_eq!(snap.signals_received, 2);
        assert_eq!(snap.signals_routed, 1);
        assert_eq!(snap.signals_deduplicated, 1);
    }

    #[test]
    fn bridge_no_dedup_without_correlation_id() {
        let bus = make_bus();
        let _sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        let sig = test_signal("github", ConnectorSignalKind::Webhook)
            .with_timestamp_ms(1000);

        let r1 = bridge.route_signal(&sig).unwrap();
        assert!(!r1.deduplicated);

        let r2 = bridge.route_signal(&sig).unwrap();
        assert!(!r2.deduplicated);

        assert_eq!(bridge.telemetry_snapshot().signals_routed, 2);
    }

    #[test]
    fn bridge_rejects_unknown_kind_when_configured() {
        let bus = make_bus();
        let config = ConnectorInboundBridgeConfig {
            reject_unknown_kinds: true,
            ..Default::default()
        };
        let mut bridge = ConnectorInboundBridge::new(bus, config);

        let sig = test_signal("x", ConnectorSignalKind::Custom);
        let err = bridge.route_signal(&sig).unwrap_err();
        assert!(matches!(err, ConnectorBridgeError::UnknownKindRejected));

        assert_eq!(bridge.telemetry_snapshot().signals_rejected, 1);
    }

    #[test]
    fn bridge_allows_custom_kind_by_default() {
        let bus = make_bus();
        let _sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        let sig = test_signal("x", ConnectorSignalKind::Custom);
        let result = bridge.route_signal(&sig).unwrap();
        assert!(!result.deduplicated);
    }

    #[test]
    fn bridge_rule_id_override() {
        let bus = make_bus();
        let _sub = bus.subscribe_detections();
        let mut overrides = HashMap::new();
        overrides.insert(
            "github.webhook".to_string(),
            "custom.gh_event".to_string(),
        );
        let config = ConnectorInboundBridgeConfig {
            rule_id_overrides: overrides,
            ..Default::default()
        };
        let mut bridge = ConnectorInboundBridge::new(bus, config);

        let sig = test_signal("github", ConnectorSignalKind::Webhook)
            .with_sub_type("push");

        let result = bridge.route_signal(&sig).unwrap();
        assert_eq!(result.rule_id, "custom.gh_event.push");
    }

    #[test]
    fn bridge_default_pane_id_zero() {
        let bus = make_bus();
        let mut sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        let sig = test_signal("x", ConnectorSignalKind::Webhook)
            .with_timestamp_ms(1000);
        bridge.route_signal(&sig).unwrap();

        let event = sub.try_recv().unwrap().unwrap();
        assert_eq!(event.pane_id(), Some(0));
    }

    #[test]
    fn bridge_preserves_pane_id() {
        let bus = make_bus();
        let mut sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        let sig = test_signal("x", ConnectorSignalKind::Webhook)
            .with_pane_id(42)
            .with_timestamp_ms(1000);
        bridge.route_signal(&sig).unwrap();

        let event = sub.try_recv().unwrap().unwrap();
        assert_eq!(event.pane_id(), Some(42));
    }

    #[test]
    fn bridge_detection_has_correct_fields() {
        let bus = make_bus();
        let mut sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        let sig = test_signal("slack", ConnectorSignalKind::Stream)
            .with_sub_type("message")
            .with_correlation_id("cid-99")
            .with_pane_id(7)
            .with_timestamp_ms(5000);

        bridge.route_signal(&sig).unwrap();

        let event = sub.try_recv().unwrap();
        if let Ok(Event::PatternDetected { detection, .. }) = event {
            assert_eq!(detection.rule_id, "connector.slack:stream.message");
            assert_eq!(detection.confidence, 1.0);
            assert_eq!(detection.severity, Severity::Info);
            assert_eq!(detection.agent_type, AgentType::Unknown);
            assert!(detection.event_type.contains("connector."));

            // Extracted should contain enriched fields
            let map = detection.extracted.as_object().unwrap();
            assert_eq!(
                map.get("source_connector").and_then(|v| v.as_str()),
                Some("slack")
            );
            assert_eq!(
                map.get("correlation_id").and_then(|v| v.as_str()),
                Some("cid-99")
            );
            assert_eq!(
                map.get("signal_kind").and_then(|v| v.as_str()),
                Some("stream")
            );
        } else {
            panic!("expected PatternDetected event");
        }
    }

    #[test]
    fn bridge_lifecycle_signal_enriches_extracted() {
        let bus = make_bus();
        let mut sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        let sig = test_signal("myconn", ConnectorSignalKind::Lifecycle)
            .with_lifecycle_phase(ConnectorLifecyclePhase::Degraded)
            .with_timestamp_ms(1000);

        bridge.route_signal(&sig).unwrap();

        let event = sub.try_recv().unwrap();
        if let Ok(Event::PatternDetected { detection, .. }) = event {
            let map = detection.extracted.as_object().unwrap();
            assert_eq!(
                map.get("lifecycle_phase").and_then(|v| v.as_str()),
                Some("degraded")
            );
        } else {
            panic!("expected PatternDetected");
        }
    }

    #[test]
    fn bridge_failure_signal_enriches_extracted() {
        let bus = make_bus();
        let mut sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        let sig = test_signal("ext", ConnectorSignalKind::Failure)
            .with_failure_class(ConnectorFailureClass::Auth)
            .with_timestamp_ms(1000);

        bridge.route_signal(&sig).unwrap();

        let event = sub.try_recv().unwrap();
        if let Ok(Event::PatternDetected { detection, .. }) = event {
            assert_eq!(detection.severity, Severity::Critical);
            let map = detection.extracted.as_object().unwrap();
            assert_eq!(
                map.get("failure_class").and_then(|v| v.as_str()),
                Some("auth")
            );
        } else {
            panic!("expected PatternDetected");
        }
    }

    // ── Telemetry ───────────────────────────────────────────────────────

    #[test]
    fn telemetry_initial_zero() {
        let bus = make_bus();
        let bridge = default_bridge(bus);
        let snap = bridge.telemetry_snapshot();
        assert_eq!(snap.signals_received, 0);
        assert_eq!(snap.signals_routed, 0);
        assert_eq!(snap.signals_deduplicated, 0);
        assert_eq!(snap.signals_rejected, 0);
        assert_eq!(snap.events_published, 0);
    }

    #[test]
    fn telemetry_accumulates() {
        let bus = make_bus();
        let _sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        for i in 0..5 {
            let sig = test_signal("x", ConnectorSignalKind::Webhook)
                .with_correlation_id(format!("id-{i}"))
                .with_timestamp_ms(i * 100 + 1000);
            bridge.route_signal(&sig).unwrap();
        }

        let snap = bridge.telemetry_snapshot();
        assert_eq!(snap.signals_received, 5);
        assert_eq!(snap.signals_routed, 5);
        assert_eq!(snap.events_published, 5);
        assert_eq!(snap.signals_deduplicated, 0);
    }

    // ── Serialization ───────────────────────────────────────────────────

    #[test]
    fn signal_kind_serde_roundtrip() {
        let kind = ConnectorSignalKind::Webhook;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"webhook\"");
        let back: ConnectorSignalKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    #[test]
    fn signal_serde_roundtrip() {
        let sig = test_signal("gh", ConnectorSignalKind::Webhook)
            .with_sub_type("pr_opened")
            .with_correlation_id("cid-1")
            .with_pane_id(3)
            .with_timestamp_ms(12345);

        let json = serde_json::to_string(&sig).unwrap();
        let back: ConnectorSignal = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source_connector, "gh");
        assert_eq!(back.signal_kind, ConnectorSignalKind::Webhook);
        assert_eq!(back.sub_type.as_deref(), Some("pr_opened"));
        assert_eq!(back.correlation_id.as_deref(), Some("cid-1"));
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = ConnectorInboundBridgeConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: ConnectorInboundBridgeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dedup_capacity, 4096);
        assert_eq!(back.dedup_ttl_secs, 300);
        assert!(!back.reject_unknown_kinds);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let snap = ConnectorBridgeTelemetrySnapshot {
            signals_received: 10,
            signals_routed: 8,
            signals_deduplicated: 1,
            signals_rejected: 1,
            events_published: 8,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ConnectorBridgeTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn route_result_serde_roundtrip() {
        let result = BridgeRouteResult {
            rule_id: "connector.gh:webhook.push".to_string(),
            deduplicated: false,
            delivered_count: 3,
            correlation_id: Some("abc".to_string()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: BridgeRouteResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rule_id, result.rule_id);
        assert_eq!(back.deduplicated, result.deduplicated);
    }

    // ── Display implementations ─────────────────────────────────────────

    #[test]
    fn signal_kind_display() {
        assert_eq!(ConnectorSignalKind::Webhook.to_string(), "webhook");
        assert_eq!(ConnectorSignalKind::Stream.to_string(), "stream");
        assert_eq!(ConnectorSignalKind::Poll.to_string(), "poll");
        assert_eq!(ConnectorSignalKind::Lifecycle.to_string(), "lifecycle");
        assert_eq!(ConnectorSignalKind::HealthCheck.to_string(), "health_check");
        assert_eq!(ConnectorSignalKind::Failure.to_string(), "failure");
        assert_eq!(ConnectorSignalKind::Custom.to_string(), "custom");
    }

    #[test]
    fn bridge_error_display() {
        let e = ConnectorBridgeError::DuplicateSignal("x".into());
        assert!(e.to_string().contains("x"));

        let e = ConnectorBridgeError::UnknownKindRejected;
        assert!(e.to_string().contains("unknown"));

        let e = ConnectorBridgeError::MissingPaneId;
        assert!(e.to_string().contains("pane_id"));
    }

    // ── Edge cases ──────────────────────────────────────────────────────

    #[test]
    fn bridge_handles_empty_payload() {
        let bus = make_bus();
        let _sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        let sig = ConnectorSignal::new(
            "x",
            ConnectorSignalKind::Poll,
            serde_json::json!({}),
        )
        .with_timestamp_ms(1000);

        let result = bridge.route_signal(&sig).unwrap();
        assert!(!result.deduplicated);
    }

    #[test]
    fn bridge_handles_non_object_payload() {
        let bus = make_bus();
        let _sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        // Array payload instead of object — enrichment fields won't be injected
        let sig = ConnectorSignal::new(
            "x",
            ConnectorSignalKind::Webhook,
            serde_json::json!([1, 2, 3]),
        )
        .with_timestamp_ms(1000);

        let result = bridge.route_signal(&sig).unwrap();
        assert!(!result.deduplicated);
    }

    #[test]
    fn bridge_many_signals_stress() {
        let bus = make_bus();
        let _sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        for i in 0..1000 {
            let sig = test_signal("load_test", ConnectorSignalKind::Webhook)
                .with_correlation_id(format!("stress-{i}"))
                .with_timestamp_ms(i);
            bridge.route_signal(&sig).unwrap();
        }

        let snap = bridge.telemetry_snapshot();
        assert_eq!(snap.signals_received, 1000);
        assert_eq!(snap.signals_routed, 1000);
    }

    #[test]
    fn dedup_cache_len_exposed() {
        let bus = make_bus();
        let _sub = bus.subscribe_detections();
        let mut bridge = default_bridge(bus);

        assert_eq!(bridge.dedup_cache_len(), 0);

        let sig = test_signal("x", ConnectorSignalKind::Webhook)
            .with_correlation_id("a")
            .with_timestamp_ms(100);
        bridge.route_signal(&sig).unwrap();
        assert_eq!(bridge.dedup_cache_len(), 1);
    }
}
