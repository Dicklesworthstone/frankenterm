//! Connector-specific reliability controls: circuit breakers, dead-letter queues,
//! and replay tooling for handling external system outages.
//!
//! Layers on top of the generic `circuit_breaker`, `retry`, and `backpressure`
//! modules with connector-aware error classification, bounded dead-letter storage,
//! and replay orchestration for failed connector actions.
//!
//! Part of ft-3681t.5.15.

use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use crate::connector_outbound_bridge::{ConnectorAction, ConnectorActionKind};
use crate::retry::RetryPolicy;

// =============================================================================
// Connector error classification
// =============================================================================

/// Connector-specific error classification for retry/circuit-breaker decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorErrorKind {
    /// Transient network issue — safe to retry immediately.
    Transient,
    /// Rate-limited by upstream — retry after backoff.
    RateLimited,
    /// Authentication/authorization failure — not retryable without credential refresh.
    AuthFailure,
    /// Request was malformed or rejected by the connector — not retryable.
    Permanent,
    /// Upstream service unavailable — retry with circuit breaker.
    ServiceUnavailable,
    /// Timeout waiting for response — safe to retry.
    Timeout,
    /// Unknown error — classify as transient for safety.
    Unknown,
}

impl ConnectorErrorKind {
    /// Whether this error kind is retryable.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Transient | Self::RateLimited | Self::ServiceUnavailable | Self::Timeout | Self::Unknown
        )
    }

    /// Whether this error should trip the circuit breaker.
    #[must_use]
    pub const fn trips_breaker(self) -> bool {
        matches!(
            self,
            Self::ServiceUnavailable | Self::Timeout | Self::Transient
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::RateLimited => "rate_limited",
            Self::AuthFailure => "auth_failure",
            Self::Permanent => "permanent",
            Self::ServiceUnavailable => "service_unavailable",
            Self::Timeout => "timeout",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ConnectorErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classify an error message string into a connector error kind.
#[must_use]
pub fn classify_connector_error(msg: &str) -> ConnectorErrorKind {
    let lower = msg.to_lowercase();
    if lower.contains("rate limit") || lower.contains("429") || lower.contains("too many requests") {
        ConnectorErrorKind::RateLimited
    } else if lower.contains("unauthorized") || lower.contains("forbidden") || lower.contains("401") || lower.contains("403") {
        ConnectorErrorKind::AuthFailure
    } else if lower.contains("timeout") || lower.contains("timed out") || lower.contains("deadline exceeded") {
        ConnectorErrorKind::Timeout
    } else if lower.contains("service unavailable") || lower.contains("503") || lower.contains("502") || lower.contains("504") {
        ConnectorErrorKind::ServiceUnavailable
    } else if lower.contains("not found") || lower.contains("404") || lower.contains("invalid") || lower.contains("malformed") {
        ConnectorErrorKind::Permanent
    } else if lower.contains("connection") || lower.contains("network") || lower.contains("dns") || lower.contains("reset") {
        ConnectorErrorKind::Transient
    } else {
        ConnectorErrorKind::Unknown
    }
}

// =============================================================================
// Dead-letter queue entry
// =============================================================================

/// A failed connector action stored in the dead-letter queue for later replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetterEntry {
    /// Sequential ID within the DLQ.
    pub id: u64,
    /// The failed action.
    pub action: ConnectorAction,
    /// Error message from the last attempt.
    pub last_error: String,
    /// Classification of the last error.
    pub error_kind: ConnectorErrorKind,
    /// Number of delivery attempts so far.
    pub attempt_count: u32,
    /// Timestamp of first failure (millis since epoch).
    pub first_failed_at_ms: u64,
    /// Timestamp of last failure (millis since epoch).
    pub last_failed_at_ms: u64,
    /// Whether this entry has been manually marked for skip/discard.
    pub discarded: bool,
}

impl DeadLetterEntry {
    /// Create a new dead-letter entry.
    #[must_use]
    pub fn new(
        id: u64,
        action: ConnectorAction,
        error: impl Into<String>,
        error_kind: ConnectorErrorKind,
        timestamp_ms: u64,
    ) -> Self {
        Self {
            id,
            action,
            last_error: error.into(),
            error_kind,
            attempt_count: 1,
            first_failed_at_ms: timestamp_ms,
            last_failed_at_ms: timestamp_ms,
            discarded: false,
        }
    }

    /// Record a retry attempt that failed.
    pub fn record_retry_failure(
        &mut self,
        error: impl Into<String>,
        error_kind: ConnectorErrorKind,
        timestamp_ms: u64,
    ) {
        self.attempt_count += 1;
        self.last_error = error.into();
        self.error_kind = error_kind;
        self.last_failed_at_ms = timestamp_ms;
    }

    /// Age of this entry since first failure (millis).
    #[must_use]
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.first_failed_at_ms)
    }

    /// Whether this entry has exceeded the maximum retry count.
    #[must_use]
    pub fn exceeded_max_retries(&self, max: u32) -> bool {
        self.attempt_count >= max
    }
}

// =============================================================================
// Dead-letter queue
// =============================================================================

/// Configuration for the dead-letter queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetterQueueConfig {
    /// Maximum entries in the DLQ before oldest are evicted.
    pub max_entries: usize,
    /// Maximum age before entries are auto-discarded (millis).
    pub max_age_ms: u64,
    /// Maximum retry attempts before an entry is permanently discarded.
    pub max_retries: u32,
}

impl Default for DeadLetterQueueConfig {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            max_age_ms: 24 * 60 * 60 * 1000, // 24 hours
            max_retries: 5,
        }
    }
}

/// Bounded dead-letter queue for failed connector actions.
///
/// Stores failed actions for later replay or manual intervention.
/// Entries are evicted when the queue is full (oldest first) or
/// when they exceed the configured age/retry limits.
#[derive(Debug)]
pub struct DeadLetterQueue {
    config: DeadLetterQueueConfig,
    entries: VecDeque<DeadLetterEntry>,
    next_id: u64,
    telemetry: DeadLetterTelemetry,
}

impl DeadLetterQueue {
    /// Create a new dead-letter queue with the given configuration.
    #[must_use]
    pub fn new(config: DeadLetterQueueConfig) -> Self {
        Self {
            config,
            entries: VecDeque::new(),
            next_id: 1,
            telemetry: DeadLetterTelemetry::default(),
        }
    }

    /// Enqueue a failed action.
    ///
    /// Returns the assigned DLQ entry ID.
    pub fn enqueue(
        &mut self,
        action: ConnectorAction,
        error: impl Into<String>,
        error_kind: ConnectorErrorKind,
        timestamp_ms: u64,
    ) -> u64 {
        // Evict oldest if at capacity
        while self.entries.len() >= self.config.max_entries {
            self.entries.pop_front();
            self.telemetry.evictions += 1;
        }

        let id = self.next_id;
        self.next_id += 1;
        self.entries.push_back(DeadLetterEntry::new(
            id,
            action,
            error,
            error_kind,
            timestamp_ms,
        ));
        self.telemetry.total_enqueued += 1;
        id
    }

    /// Get the current queue depth (non-discarded entries).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.entries.iter().filter(|e| !e.discarded).count()
    }

    /// Get all non-discarded entries.
    #[must_use]
    pub fn pending_entries(&self) -> Vec<&DeadLetterEntry> {
        self.entries.iter().filter(|e| !e.discarded).collect()
    }

    /// Get entries eligible for replay (non-discarded, retryable, within limits).
    #[must_use]
    pub fn replayable_entries(&self, now_ms: u64) -> Vec<&DeadLetterEntry> {
        self.entries
            .iter()
            .filter(|e| {
                !e.discarded
                    && e.error_kind.is_retryable()
                    && !e.exceeded_max_retries(self.config.max_retries)
                    && e.age_ms(now_ms) <= self.config.max_age_ms
            })
            .collect()
    }

    /// Mark an entry as discarded (will not be replayed).
    pub fn discard(&mut self, id: u64) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.discarded = true;
            self.telemetry.discarded += 1;
            true
        } else {
            false
        }
    }

    /// Record a retry attempt on a DLQ entry.
    pub fn record_retry(
        &mut self,
        id: u64,
        error: impl Into<String>,
        error_kind: ConnectorErrorKind,
        timestamp_ms: u64,
    ) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.record_retry_failure(error, error_kind, timestamp_ms);
            self.telemetry.retry_attempts += 1;
            true
        } else {
            false
        }
    }

    /// Remove an entry after successful replay.
    pub fn remove(&mut self, id: u64) -> Option<DeadLetterEntry> {
        if let Some(pos) = self.entries.iter().position(|e| e.id == id) {
            let entry = self.entries.remove(pos);
            self.telemetry.replayed_ok += 1;
            entry
        } else {
            None
        }
    }

    /// Purge entries that exceed age or retry limits.
    pub fn purge_expired(&mut self, now_ms: u64) -> usize {
        let max_age = self.config.max_age_ms;
        let max_retries = self.config.max_retries;
        let before = self.entries.len();

        self.entries.retain(|e| {
            !(e.age_ms(now_ms) > max_age || e.exceeded_max_retries(max_retries))
        });

        let purged = before - self.entries.len();
        self.telemetry.purged += purged as u64;
        purged
    }

    /// Get telemetry snapshot.
    #[must_use]
    pub fn telemetry_snapshot(&self) -> DeadLetterTelemetrySnapshot {
        self.telemetry.snapshot(self.entries.len())
    }
}

// =============================================================================
// DLQ telemetry
// =============================================================================

#[derive(Debug, Default)]
struct DeadLetterTelemetry {
    total_enqueued: u64,
    replayed_ok: u64,
    retry_attempts: u64,
    evictions: u64,
    discarded: u64,
    purged: u64,
}

impl DeadLetterTelemetry {
    fn snapshot(&self, current_depth: usize) -> DeadLetterTelemetrySnapshot {
        DeadLetterTelemetrySnapshot {
            total_enqueued: self.total_enqueued,
            current_depth: current_depth as u64,
            replayed_ok: self.replayed_ok,
            retry_attempts: self.retry_attempts,
            evictions: self.evictions,
            discarded: self.discarded,
            purged: self.purged,
        }
    }
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

// =============================================================================
// Connector circuit breaker presets
// =============================================================================

/// Preset circuit breaker configuration for connector operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorCircuitConfig {
    /// Failure threshold before opening the circuit.
    pub failure_threshold: u32,
    /// Successes needed in half-open to close the circuit.
    pub success_threshold: u32,
    /// Cooldown duration before probing half-open.
    pub cooldown: Duration,
}

impl Default for ConnectorCircuitConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            success_threshold: 2,
            cooldown: Duration::from_secs(30),
        }
    }
}

impl ConnectorCircuitConfig {
    /// Aggressive preset for critical connectors (fail fast, recover fast).
    #[must_use]
    pub fn critical() -> Self {
        Self {
            failure_threshold: 3,
            success_threshold: 1,
            cooldown: Duration::from_secs(10),
        }
    }

    /// Lenient preset for non-critical connectors.
    #[must_use]
    pub fn lenient() -> Self {
        Self {
            failure_threshold: 10,
            success_threshold: 3,
            cooldown: Duration::from_secs(60),
        }
    }

    /// Convert to a generic `CircuitBreakerConfig`.
    #[must_use]
    pub fn to_breaker_config(&self) -> CircuitBreakerConfig {
        CircuitBreakerConfig::new(
            self.failure_threshold,
            self.success_threshold,
            self.cooldown,
        )
    }
}

// =============================================================================
// Connector retry presets
// =============================================================================

/// Preset retry policy for connector operations.
impl RetryPolicy {
    /// Retry policy for connector webhook deliveries.
    #[must_use]
    pub fn connector_webhook() -> Self {
        Self {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            backoff_factor: 2.0,
            jitter_percent: 0.15,
            max_attempts: Some(5),
        }
    }

    /// Retry policy for connector API calls.
    #[must_use]
    pub fn connector_api() -> Self {
        Self {
            initial_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(10),
            backoff_factor: 2.0,
            jitter_percent: 0.10,
            max_attempts: Some(3),
        }
    }

    /// Retry policy for connector stream reconnection.
    #[must_use]
    pub fn connector_stream() -> Self {
        Self {
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            backoff_factor: 1.5,
            jitter_percent: 0.20,
            max_attempts: Some(10),
        }
    }
}

// =============================================================================
// Replay plan
// =============================================================================

/// A plan for replaying dead-letter entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayPlan {
    /// IDs of entries to replay in order.
    pub entry_ids: Vec<u64>,
    /// Retry policy to use for replay attempts.
    pub policy: ReplayPolicy,
    /// Maximum entries to attempt in a single replay run.
    pub batch_size: usize,
    /// Whether to stop on first failure.
    pub stop_on_failure: bool,
}

/// Policy for replay behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayPolicy {
    /// Delay between replay attempts within a batch.
    pub inter_attempt_delay_ms: u64,
    /// Maximum concurrent replay operations.
    pub concurrency: usize,
    /// Whether to re-enqueue failed replays back into the DLQ.
    pub re_enqueue_on_failure: bool,
}

impl Default for ReplayPolicy {
    fn default() -> Self {
        Self {
            inter_attempt_delay_ms: 100,
            concurrency: 1,
            re_enqueue_on_failure: true,
        }
    }
}

/// Result of a replay run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayResult {
    /// Number of entries successfully replayed.
    pub succeeded: usize,
    /// Number of entries that failed replay.
    pub failed: usize,
    /// Number of entries skipped (discarded or expired).
    pub skipped: usize,
    /// IDs of entries that failed (for manual inspection).
    pub failed_ids: Vec<u64>,
}

impl ReplayResult {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            succeeded: 0,
            failed: 0,
            skipped: 0,
            failed_ids: Vec::new(),
        }
    }
}

// =============================================================================
// Connector reliability controller
// =============================================================================

/// Configuration for the connector reliability controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorReliabilityConfig {
    /// Circuit breaker settings.
    pub circuit: ConnectorCircuitConfig,
    /// Dead-letter queue settings.
    pub dlq: DeadLetterQueueConfig,
    /// Whether to auto-enqueue failed actions to the DLQ.
    pub auto_dlq: bool,
    /// Maximum queue depth before shedding load.
    pub shed_threshold: usize,
}

impl Default for ConnectorReliabilityConfig {
    fn default() -> Self {
        Self {
            circuit: ConnectorCircuitConfig::default(),
            dlq: DeadLetterQueueConfig::default(),
            auto_dlq: true,
            shed_threshold: 800,
        }
    }
}

/// Per-connector reliability controller combining circuit breaker + DLQ.
///
/// Each connector gets its own controller instance, managing its own
/// circuit state and dead-letter queue independently.
#[derive(Debug)]
pub struct ConnectorReliabilityController {
    /// Connector identifier.
    connector_id: String,
    /// Circuit breaker for this connector.
    circuit: CircuitBreaker,
    /// Dead-letter queue for failed actions.
    dlq: DeadLetterQueue,
    /// Configuration.
    config: ConnectorReliabilityConfig,
    /// Telemetry counters.
    telemetry: ControllerTelemetry,
}

impl ConnectorReliabilityController {
    /// Create a new reliability controller for a connector.
    #[must_use]
    pub fn new(connector_id: impl Into<String>, config: ConnectorReliabilityConfig) -> Self {
        let cid = connector_id.into();
        let circuit = CircuitBreaker::with_name(
            format!("connector.{cid}"),
            config.circuit.to_breaker_config(),
        );
        let dlq = DeadLetterQueue::new(config.dlq.clone());
        Self {
            connector_id: cid,
            circuit,
            dlq,
            config,
            telemetry: ControllerTelemetry::default(),
        }
    }

    /// Check if the connector's circuit allows an operation.
    pub fn allow_operation(&mut self) -> bool {
        let allowed = self.circuit.allow();
        if !allowed {
            self.telemetry.circuit_rejections += 1;
        }
        self.telemetry.operations_attempted += 1;
        allowed
    }

    /// Record a successful operation.
    pub fn record_success(&mut self) {
        self.circuit.record_success();
        self.telemetry.operations_succeeded += 1;
    }

    /// Record a failed operation, optionally enqueuing to DLQ.
    ///
    /// Returns the DLQ entry ID if the action was enqueued.
    pub fn record_failure(
        &mut self,
        action: &ConnectorAction,
        error: impl Into<String>,
        error_kind: ConnectorErrorKind,
        timestamp_ms: u64,
    ) -> Option<u64> {
        let err_str = error.into();

        // Trip circuit breaker if applicable
        if error_kind.trips_breaker() {
            self.circuit.record_failure();
        }
        self.telemetry.operations_failed += 1;

        // Auto-enqueue to DLQ if configured
        if self.config.auto_dlq && error_kind.is_retryable() {
            let id = self.dlq.enqueue(action.clone(), &err_str, error_kind, timestamp_ms);
            Some(id)
        } else {
            None
        }
    }

    /// Get the connector ID.
    #[must_use]
    pub fn connector_id(&self) -> &str {
        &self.connector_id
    }

    /// Check if the DLQ is above the shedding threshold.
    #[must_use]
    pub fn is_shedding(&self) -> bool {
        self.dlq.depth() >= self.config.shed_threshold
    }

    /// Get the dead-letter queue (immutable access).
    #[must_use]
    pub fn dlq(&self) -> &DeadLetterQueue {
        &self.dlq
    }

    /// Get the dead-letter queue (mutable access).
    pub fn dlq_mut(&mut self) -> &mut DeadLetterQueue {
        &mut self.dlq
    }

    /// Get circuit breaker status.
    #[must_use]
    pub fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
        self.circuit.status()
    }

    /// Build a replay plan for eligible DLQ entries.
    #[must_use]
    pub fn build_replay_plan(
        &self,
        now_ms: u64,
        batch_size: usize,
        stop_on_failure: bool,
    ) -> ReplayPlan {
        let replayable = self.dlq.replayable_entries(now_ms);
        let entry_ids: Vec<u64> = replayable.iter().take(batch_size).map(|e| e.id).collect();

        ReplayPlan {
            entry_ids,
            policy: ReplayPolicy::default(),
            batch_size,
            stop_on_failure,
        }
    }

    /// Get telemetry snapshot.
    #[must_use]
    pub fn telemetry_snapshot(&self) -> ConnectorReliabilitySnapshot {
        ConnectorReliabilitySnapshot {
            connector_id: self.connector_id.clone(),
            operations_attempted: self.telemetry.operations_attempted,
            operations_succeeded: self.telemetry.operations_succeeded,
            operations_failed: self.telemetry.operations_failed,
            circuit_rejections: self.telemetry.circuit_rejections,
            dlq: self.dlq.telemetry_snapshot(),
        }
    }
}

// =============================================================================
// Controller telemetry
// =============================================================================

#[derive(Debug, Default)]
struct ControllerTelemetry {
    operations_attempted: u64,
    operations_succeeded: u64,
    operations_failed: u64,
    circuit_rejections: u64,
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

// =============================================================================
// Multi-connector reliability registry
// =============================================================================

/// Registry managing reliability controllers for multiple connectors.
#[derive(Debug)]
pub struct ReliabilityRegistry {
    controllers: Vec<ConnectorReliabilityController>,
    default_config: ConnectorReliabilityConfig,
}

impl ReliabilityRegistry {
    /// Create a new registry with default configuration.
    #[must_use]
    pub fn new(default_config: ConnectorReliabilityConfig) -> Self {
        Self {
            controllers: Vec::new(),
            default_config,
        }
    }

    /// Get or create a controller for a connector.
    pub fn get_or_create(&mut self, connector_id: &str) -> &mut ConnectorReliabilityController {
        let pos = self.controllers.iter().position(|c| c.connector_id == connector_id);
        match pos {
            Some(idx) => &mut self.controllers[idx],
            None => {
                self.controllers.push(ConnectorReliabilityController::new(
                    connector_id,
                    self.default_config.clone(),
                ));
                self.controllers.last_mut().unwrap()
            }
        }
    }

    /// Get a controller by connector ID (immutable).
    #[must_use]
    pub fn get(&self, connector_id: &str) -> Option<&ConnectorReliabilityController> {
        self.controllers.iter().find(|c| c.connector_id == connector_id)
    }

    /// Get all registered connector IDs.
    #[must_use]
    pub fn connector_ids(&self) -> Vec<&str> {
        self.controllers.iter().map(|c| c.connector_id.as_str()).collect()
    }

    /// Get telemetry snapshots for all connectors.
    #[must_use]
    pub fn all_snapshots(&self) -> Vec<ConnectorReliabilitySnapshot> {
        self.controllers.iter().map(|c| c.telemetry_snapshot()).collect()
    }

    /// Purge expired DLQ entries across all connectors.
    pub fn purge_all_expired(&mut self, now_ms: u64) -> usize {
        self.controllers.iter_mut().map(|c| c.dlq_mut().purge_expired(now_ms)).sum()
    }

    /// Total DLQ depth across all connectors.
    #[must_use]
    pub fn total_dlq_depth(&self) -> usize {
        self.controllers.iter().map(|c| c.dlq().depth()).sum()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_action(connector: &str, kind: ConnectorActionKind) -> ConnectorAction {
        ConnectorAction {
            target_connector: connector.to_string(),
            action_kind: kind,
            correlation_id: format!("corr-{connector}"),
            params: serde_json::json!({"test": true}),
            created_at_ms: 1000,
        }
    }

    // ---- Error classification ----

    #[test]
    fn connector_reliability_classify_rate_limit() {
        assert_eq!(
            classify_connector_error("429 Too Many Requests"),
            ConnectorErrorKind::RateLimited
        );
        assert_eq!(
            classify_connector_error("rate limit exceeded"),
            ConnectorErrorKind::RateLimited
        );
    }

    #[test]
    fn connector_reliability_classify_auth() {
        assert_eq!(
            classify_connector_error("401 Unauthorized"),
            ConnectorErrorKind::AuthFailure
        );
        assert_eq!(
            classify_connector_error("403 Forbidden"),
            ConnectorErrorKind::AuthFailure
        );
    }

    #[test]
    fn connector_reliability_classify_timeout() {
        assert_eq!(
            classify_connector_error("request timed out after 30s"),
            ConnectorErrorKind::Timeout
        );
        assert_eq!(
            classify_connector_error("deadline exceeded"),
            ConnectorErrorKind::Timeout
        );
    }

    #[test]
    fn connector_reliability_classify_service_unavailable() {
        assert_eq!(
            classify_connector_error("503 Service Unavailable"),
            ConnectorErrorKind::ServiceUnavailable
        );
    }

    #[test]
    fn connector_reliability_classify_permanent() {
        assert_eq!(
            classify_connector_error("404 Not Found"),
            ConnectorErrorKind::Permanent
        );
        assert_eq!(
            classify_connector_error("invalid request body"),
            ConnectorErrorKind::Permanent
        );
    }

    #[test]
    fn connector_reliability_classify_transient() {
        assert_eq!(
            classify_connector_error("connection reset by peer"),
            ConnectorErrorKind::Transient
        );
    }

    #[test]
    fn connector_reliability_classify_unknown() {
        assert_eq!(
            classify_connector_error("something weird happened"),
            ConnectorErrorKind::Unknown
        );
    }

    #[test]
    fn connector_reliability_error_retryability() {
        assert!(ConnectorErrorKind::Transient.is_retryable());
        assert!(ConnectorErrorKind::RateLimited.is_retryable());
        assert!(ConnectorErrorKind::ServiceUnavailable.is_retryable());
        assert!(ConnectorErrorKind::Timeout.is_retryable());
        assert!(ConnectorErrorKind::Unknown.is_retryable());
        assert!(!ConnectorErrorKind::AuthFailure.is_retryable());
        assert!(!ConnectorErrorKind::Permanent.is_retryable());
    }

    #[test]
    fn connector_reliability_error_trips_breaker() {
        assert!(ConnectorErrorKind::ServiceUnavailable.trips_breaker());
        assert!(ConnectorErrorKind::Timeout.trips_breaker());
        assert!(ConnectorErrorKind::Transient.trips_breaker());
        assert!(!ConnectorErrorKind::RateLimited.trips_breaker());
        assert!(!ConnectorErrorKind::AuthFailure.trips_breaker());
        assert!(!ConnectorErrorKind::Permanent.trips_breaker());
    }

    // ---- Dead-letter queue ----

    #[test]
    fn connector_reliability_dlq_enqueue_and_depth() {
        let mut dlq = DeadLetterQueue::new(DeadLetterQueueConfig::default());
        assert_eq!(dlq.depth(), 0);

        let action = sample_action("slack", ConnectorActionKind::Notify);
        let id = dlq.enqueue(action, "timeout", ConnectorErrorKind::Timeout, 1000);
        assert_eq!(id, 1);
        assert_eq!(dlq.depth(), 1);
    }

    #[test]
    fn connector_reliability_dlq_bounded_capacity() {
        let config = DeadLetterQueueConfig {
            max_entries: 3,
            ..Default::default()
        };
        let mut dlq = DeadLetterQueue::new(config);

        for i in 0..5 {
            let action = sample_action("test", ConnectorActionKind::Notify);
            dlq.enqueue(action, format!("error-{i}"), ConnectorErrorKind::Transient, 1000 + i);
        }

        // Should only have 3 entries (oldest evicted)
        assert_eq!(dlq.entries.len(), 3);
        let snap = dlq.telemetry_snapshot();
        assert_eq!(snap.total_enqueued, 5);
        assert_eq!(snap.evictions, 2);
    }

    #[test]
    fn connector_reliability_dlq_discard() {
        let mut dlq = DeadLetterQueue::new(DeadLetterQueueConfig::default());
        let action = sample_action("test", ConnectorActionKind::Notify);
        let id = dlq.enqueue(action, "err", ConnectorErrorKind::Transient, 1000);

        assert_eq!(dlq.depth(), 1);
        assert!(dlq.discard(id));
        assert_eq!(dlq.depth(), 0); // discarded entries don't count
    }

    #[test]
    fn connector_reliability_dlq_replayable_filters() {
        let config = DeadLetterQueueConfig {
            max_retries: 2,
            max_age_ms: 10_000,
            ..Default::default()
        };
        let mut dlq = DeadLetterQueue::new(config);

        // Retryable entry
        let a1 = sample_action("test", ConnectorActionKind::Notify);
        let id1 = dlq.enqueue(a1, "timeout", ConnectorErrorKind::Timeout, 1000);

        // Non-retryable entry (auth failure)
        let a2 = sample_action("test", ConnectorActionKind::Ticket);
        dlq.enqueue(a2, "auth", ConnectorErrorKind::AuthFailure, 1000);

        // Exceeded retries entry
        let a3 = sample_action("test", ConnectorActionKind::AuditLog);
        let id3 = dlq.enqueue(a3, "err", ConnectorErrorKind::Transient, 1000);
        dlq.record_retry(id3, "err2", ConnectorErrorKind::Transient, 2000);

        let replayable = dlq.replayable_entries(2000);
        assert_eq!(replayable.len(), 1);
        assert_eq!(replayable[0].id, id1);
    }

    #[test]
    fn connector_reliability_dlq_remove_on_success() {
        let mut dlq = DeadLetterQueue::new(DeadLetterQueueConfig::default());
        let action = sample_action("test", ConnectorActionKind::Notify);
        let id = dlq.enqueue(action, "err", ConnectorErrorKind::Transient, 1000);

        let removed = dlq.remove(id);
        assert!(removed.is_some());
        assert_eq!(dlq.depth(), 0);
        assert_eq!(dlq.telemetry_snapshot().replayed_ok, 1);
    }

    #[test]
    fn connector_reliability_dlq_purge_expired() {
        let config = DeadLetterQueueConfig {
            max_age_ms: 5000,
            max_retries: 100,
            ..Default::default()
        };
        let mut dlq = DeadLetterQueue::new(config);

        let a1 = sample_action("test", ConnectorActionKind::Notify);
        dlq.enqueue(a1, "err", ConnectorErrorKind::Transient, 1000);

        let a2 = sample_action("test", ConnectorActionKind::Ticket);
        dlq.enqueue(a2, "err", ConnectorErrorKind::Transient, 5000);

        // At time 7000, first entry is 6000ms old (> 5000 max)
        let purged = dlq.purge_expired(7000);
        assert_eq!(purged, 1);
        assert_eq!(dlq.entries.len(), 1);
    }

    // ---- Controller ----

    #[test]
    fn connector_reliability_controller_allow_and_record() {
        let mut ctrl = ConnectorReliabilityController::new(
            "slack",
            ConnectorReliabilityConfig::default(),
        );

        assert!(ctrl.allow_operation());
        ctrl.record_success();

        let snap = ctrl.telemetry_snapshot();
        assert_eq!(snap.operations_attempted, 1);
        assert_eq!(snap.operations_succeeded, 1);
        assert_eq!(snap.connector_id, "slack");
    }

    #[test]
    fn connector_reliability_controller_failure_enqueues_dlq() {
        let mut ctrl = ConnectorReliabilityController::new(
            "github",
            ConnectorReliabilityConfig::default(),
        );

        let action = sample_action("github", ConnectorActionKind::Notify);
        let dlq_id = ctrl.record_failure(
            &action,
            "503 Service Unavailable",
            ConnectorErrorKind::ServiceUnavailable,
            1000,
        );

        assert!(dlq_id.is_some());
        assert_eq!(ctrl.dlq().depth(), 1);
    }

    #[test]
    fn connector_reliability_controller_permanent_not_enqueued() {
        let mut ctrl = ConnectorReliabilityController::new(
            "test",
            ConnectorReliabilityConfig::default(),
        );

        let action = sample_action("test", ConnectorActionKind::Notify);
        let dlq_id = ctrl.record_failure(
            &action,
            "404 Not Found",
            ConnectorErrorKind::Permanent,
            1000,
        );

        assert!(dlq_id.is_none());
        assert_eq!(ctrl.dlq().depth(), 0);
    }

    #[test]
    fn connector_reliability_controller_circuit_trips() {
        let config = ConnectorReliabilityConfig {
            circuit: ConnectorCircuitConfig {
                failure_threshold: 2,
                success_threshold: 1,
                cooldown: Duration::from_secs(60),
            },
            ..Default::default()
        };
        let mut ctrl = ConnectorReliabilityController::new("test", config);

        let action = sample_action("test", ConnectorActionKind::Notify);

        // Trip the circuit with 2 failures
        ctrl.record_failure(&action, "timeout", ConnectorErrorKind::Timeout, 1000);
        ctrl.record_failure(&action, "timeout", ConnectorErrorKind::Timeout, 2000);

        // Circuit should be open now
        assert!(!ctrl.allow_operation());
        let snap = ctrl.telemetry_snapshot();
        assert_eq!(snap.circuit_rejections, 1);
    }

    #[test]
    fn connector_reliability_controller_replay_plan() {
        let mut ctrl = ConnectorReliabilityController::new(
            "slack",
            ConnectorReliabilityConfig::default(),
        );

        // Add some failures
        for i in 0..5 {
            let action = sample_action("slack", ConnectorActionKind::Notify);
            ctrl.record_failure(
                &action,
                "service unavailable",
                ConnectorErrorKind::ServiceUnavailable,
                1000 + i * 100,
            );
        }

        let plan = ctrl.build_replay_plan(2000, 3, true);
        assert_eq!(plan.entry_ids.len(), 3);
        assert!(plan.stop_on_failure);
    }

    // ---- Registry ----

    #[test]
    fn connector_reliability_registry_get_or_create() {
        let mut registry = ReliabilityRegistry::new(ConnectorReliabilityConfig::default());

        registry.get_or_create("slack");
        registry.get_or_create("github");
        registry.get_or_create("slack"); // should reuse

        assert_eq!(registry.connector_ids().len(), 2);
    }

    #[test]
    fn connector_reliability_registry_total_depth() {
        let mut registry = ReliabilityRegistry::new(ConnectorReliabilityConfig::default());

        let action = sample_action("slack", ConnectorActionKind::Notify);
        registry.get_or_create("slack").record_failure(
            &action,
            "err",
            ConnectorErrorKind::Transient,
            1000,
        );

        let action2 = sample_action("github", ConnectorActionKind::Ticket);
        registry.get_or_create("github").record_failure(
            &action2,
            "err",
            ConnectorErrorKind::Transient,
            1000,
        );

        assert_eq!(registry.total_dlq_depth(), 2);
    }

    #[test]
    fn connector_reliability_registry_purge_all() {
        let config = ConnectorReliabilityConfig {
            dlq: DeadLetterQueueConfig {
                max_age_ms: 5000,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut registry = ReliabilityRegistry::new(config);

        let action = sample_action("test", ConnectorActionKind::Notify);
        registry.get_or_create("test").record_failure(
            &action,
            "err",
            ConnectorErrorKind::Transient,
            1000,
        );

        let purged = registry.purge_all_expired(100_000);
        assert_eq!(purged, 1);
        assert_eq!(registry.total_dlq_depth(), 0);
    }

    // ---- Serde roundtrips ----

    #[test]
    fn connector_reliability_error_kind_serde() {
        for kind in [
            ConnectorErrorKind::Transient,
            ConnectorErrorKind::RateLimited,
            ConnectorErrorKind::AuthFailure,
            ConnectorErrorKind::Permanent,
            ConnectorErrorKind::ServiceUnavailable,
            ConnectorErrorKind::Timeout,
            ConnectorErrorKind::Unknown,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: ConnectorErrorKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn connector_reliability_dlq_entry_serde() {
        let action = sample_action("test", ConnectorActionKind::Notify);
        let entry = DeadLetterEntry::new(1, action, "timeout", ConnectorErrorKind::Timeout, 1000);
        let json = serde_json::to_string(&entry).unwrap();
        let back: DeadLetterEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.id, back.id);
        assert_eq!(entry.error_kind, back.error_kind);
        assert_eq!(entry.attempt_count, back.attempt_count);
    }

    #[test]
    fn connector_reliability_snapshot_serde() {
        let snap = ConnectorReliabilitySnapshot {
            connector_id: "test".to_string(),
            operations_attempted: 10,
            operations_succeeded: 8,
            operations_failed: 2,
            circuit_rejections: 0,
            dlq: DeadLetterTelemetrySnapshot {
                total_enqueued: 2,
                current_depth: 1,
                replayed_ok: 1,
                retry_attempts: 3,
                evictions: 0,
                discarded: 0,
                purged: 0,
            },
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ConnectorReliabilitySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn connector_reliability_replay_plan_serde() {
        let plan = ReplayPlan {
            entry_ids: vec![1, 2, 3],
            policy: ReplayPolicy::default(),
            batch_size: 10,
            stop_on_failure: false,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: ReplayPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entry_ids.len(), 3);
        assert!(!back.stop_on_failure);
    }

    // ---- Presets ----

    #[test]
    fn connector_reliability_circuit_presets() {
        let critical = ConnectorCircuitConfig::critical();
        assert_eq!(critical.failure_threshold, 3);

        let lenient = ConnectorCircuitConfig::lenient();
        assert_eq!(lenient.failure_threshold, 10);

        let default = ConnectorCircuitConfig::default();
        assert_eq!(default.failure_threshold, 5);
    }

    #[test]
    fn connector_reliability_retry_presets() {
        let webhook = RetryPolicy::connector_webhook();
        assert_eq!(webhook.max_attempts, Some(5));

        let api = RetryPolicy::connector_api();
        assert_eq!(api.max_attempts, Some(3));

        let stream = RetryPolicy::connector_stream();
        assert_eq!(stream.max_attempts, Some(10));
    }

    // ---- Dead-letter entry helpers ----

    #[test]
    fn connector_reliability_entry_age_and_retries() {
        let action = sample_action("test", ConnectorActionKind::Notify);
        let mut entry = DeadLetterEntry::new(1, action, "err", ConnectorErrorKind::Transient, 1000);

        assert_eq!(entry.age_ms(5000), 4000);
        assert!(!entry.exceeded_max_retries(3));

        entry.record_retry_failure("err2", ConnectorErrorKind::Transient, 2000);
        entry.record_retry_failure("err3", ConnectorErrorKind::Transient, 3000);
        assert!(entry.exceeded_max_retries(3));
        assert_eq!(entry.attempt_count, 3);
        assert_eq!(entry.last_failed_at_ms, 3000);
    }

    // ---- Shedding ----

    #[test]
    fn connector_reliability_shedding_threshold() {
        let config = ConnectorReliabilityConfig {
            shed_threshold: 2,
            ..Default::default()
        };
        let mut ctrl = ConnectorReliabilityController::new("test", config);

        assert!(!ctrl.is_shedding());

        let action = sample_action("test", ConnectorActionKind::Notify);
        ctrl.record_failure(&action, "err", ConnectorErrorKind::Transient, 1000);
        ctrl.record_failure(&action, "err", ConnectorErrorKind::Transient, 2000);

        assert!(ctrl.is_shedding());
    }
}
