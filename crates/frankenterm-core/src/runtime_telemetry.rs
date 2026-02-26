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
use std::collections::HashMap;
use std::fmt;
use std::time::SystemTime;

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

            Self::QueueDepthObserved
            | Self::ChannelClosed
            | Self::PermitExhausted => "queue",

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
        self.event
            .details
            .insert(key.to_string(), serde_json::Value::String(value.to_string()));
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
        self.events
            .iter()
            .filter(|e| e.health_tier == tier)
            .count()
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
    pub fn grace_expired(
        &self,
        scope_id: &str,
        grace_period_ms: u64,
    ) -> RuntimeTelemetryEvent {
        RuntimeTelemetryEventBuilder::new(
            &self.component,
            RuntimeTelemetryKind::GracePeriodExpired,
        )
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

        assert_eq!(
            RuntimeTelemetryKind::QueueDepthObserved.category(),
            "queue"
        );
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
        assert_eq!(
            roundtripped.details.get("key"),
            event.details.get("key")
        );
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
        let event =
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
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
            RuntimeTelemetryEventBuilder::new("rt.scope.capture", RuntimeTelemetryKind::ScopeStarted)
                .reason("a"),
        );
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.backpressure", RuntimeTelemetryKind::ThrottleApplied)
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
        assert_eq!(
            grace.event_kind,
            RuntimeTelemetryKind::GracePeriodExpired
        );
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
}
