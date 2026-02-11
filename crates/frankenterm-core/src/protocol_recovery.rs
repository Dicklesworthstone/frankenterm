//! Protocol error auto-recovery for WezTerm mux connections (wa-2c50).
//!
//! When the mux protocol enters a corrupted state (e.g., client/server out of
//! sync on the PDU stream), naively retrying on the *same* connection fails
//! because the buffer state is inconsistent. This module provides:
//!
//! - **Error classification**: categorize `DirectMuxError` into Recoverable,
//!   Transient, or Permanent — driving retry/reconnect decisions.
//! - **`RecoveryEngine`**: wraps operations with a [`CircuitBreaker`] and
//!   retry logic for transparent recovery from protocol corruption.
//! - **Frame corruption detection**: heuristics to detect when a PDU stream
//!   is corrupted vs a transient I/O hiccup.
//! - **Degradation integration**: reports sustained failures to the
//!   [`DegradationManager`] so the system can adapt.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitStateKind};

/// Classification of protocol errors to drive recovery strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolErrorKind {
    /// Connection state is corrupted — must drop connection and reconnect.
    Recoverable,
    /// Temporary condition — retry after backoff.
    Transient,
    /// Unrecoverable — do not retry.
    Permanent,
}

impl std::fmt::Display for ProtocolErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recoverable => write!(f, "recoverable"),
            Self::Transient => write!(f, "transient"),
            Self::Permanent => write!(f, "permanent"),
        }
    }
}

/// Classify an error message string into a recovery category.
#[must_use]
pub fn classify_error_message(msg: &str) -> ProtocolErrorKind {
    let lower = msg.to_lowercase();

    if lower.contains("codec version mismatch")
        || lower.contains("incompatible")
        || lower.contains("socket path not found")
        || lower.contains("proxy command not supported")
    {
        return ProtocolErrorKind::Permanent;
    }

    if lower.contains("unexpected response")
        || lower.contains("disconnected")
        || lower.contains("codec error")
        || lower.contains("frame exceeded max size")
        || lower.contains("remote error")
    {
        return ProtocolErrorKind::Recoverable;
    }

    if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("connection refused")
    {
        return ProtocolErrorKind::Transient;
    }

    if lower.contains("io error") {
        if lower.contains("broken pipe")
            || lower.contains("connection reset")
            || lower.contains("not connected")
        {
            return ProtocolErrorKind::Recoverable;
        }
        if lower.contains("would block")
            || lower.contains("interrupted")
            || lower.contains("timed out")
        {
            return ProtocolErrorKind::Transient;
        }
        return ProtocolErrorKind::Recoverable;
    }

    if lower.contains("socket not found") {
        return ProtocolErrorKind::Transient;
    }

    ProtocolErrorKind::Recoverable
}

/// Classify a `DirectMuxError` directly when the `vendored` feature is active.
#[cfg(all(feature = "vendored", unix))]
#[must_use]
pub fn classify_mux_error(err: &crate::vendored::DirectMuxError) -> ProtocolErrorKind {
    use crate::vendored::DirectMuxError;
    match err {
        DirectMuxError::SocketPathMissing | DirectMuxError::ProxyUnsupported => {
            ProtocolErrorKind::Permanent
        }
        DirectMuxError::IncompatibleCodec { .. } => ProtocolErrorKind::Permanent,

        DirectMuxError::Disconnected
        | DirectMuxError::UnexpectedResponse { .. }
        | DirectMuxError::Codec(_)
        | DirectMuxError::FrameTooLarge { .. }
        | DirectMuxError::RemoteError(_) => ProtocolErrorKind::Recoverable,

        DirectMuxError::ConnectTimeout(_)
        | DirectMuxError::ReadTimeout
        | DirectMuxError::WriteTimeout => ProtocolErrorKind::Transient,

        DirectMuxError::SocketNotFound(_) => ProtocolErrorKind::Transient,

        DirectMuxError::Io(io_err) => match io_err.kind() {
            std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::ConnectionAborted => ProtocolErrorKind::Recoverable,
            std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut => ProtocolErrorKind::Transient,
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::AddrNotAvailable => {
                ProtocolErrorKind::Transient
            }
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::InvalidInput => {
                ProtocolErrorKind::Permanent
            }
            _ => ProtocolErrorKind::Recoverable,
        },
    }
}

/// Configuration for protocol error recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryConfig {
    pub enabled: bool,
    pub max_retries: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub backoff_factor: f64,
    pub jitter_fraction: f64,
    pub circuit_failure_threshold: u32,
    pub circuit_success_threshold: u32,
    pub circuit_cooldown: Duration,
    pub report_degradation: bool,
    pub permanent_failure_limit: u32,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_millis(500),
            backoff_factor: 2.0,
            jitter_fraction: 0.1,
            circuit_failure_threshold: 5,
            circuit_success_threshold: 2,
            circuit_cooldown: Duration::from_secs(15),
            report_degradation: true,
            permanent_failure_limit: 3,
        }
    }
}

impl RecoveryConfig {
    #[must_use]
    pub fn for_capture() -> Self {
        Self {
            max_retries: 2,
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(100),
            circuit_failure_threshold: 3,
            circuit_success_threshold: 1,
            circuit_cooldown: Duration::from_secs(5),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn for_interactive() -> Self {
        Self {
            max_retries: 5,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
            circuit_failure_threshold: 8,
            circuit_success_threshold: 2,
            circuit_cooldown: Duration::from_secs(30),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let base_ms =
            self.initial_delay.as_millis() as f64 * self.backoff_factor.powi(attempt as i32);
        let capped_ms = base_ms.min(self.max_delay.as_millis() as f64);
        let jitter_range = capped_ms * self.jitter_fraction;
        let jitter_seed = ((attempt as f64 * 7.13).sin().abs()) * 2.0 - 1.0;
        let jittered_ms = (capped_ms + jitter_range * jitter_seed).max(1.0);
        Duration::from_millis(jittered_ms as u64)
    }
}

/// Metrics for protocol recovery operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryStats {
    pub total_operations: u64,
    pub first_try_successes: u64,
    pub retry_successes: u64,
    pub total_retries: u64,
    pub recoverable_failures: u64,
    pub transient_failures: u64,
    pub permanent_failures: u64,
    pub circuit_rejections: u64,
    pub consecutive_permanent: u64,
    pub circuit_state: String,
}

#[derive(Debug)]
struct RecoveryCounters {
    total_operations: AtomicU64,
    first_try_successes: AtomicU64,
    retry_successes: AtomicU64,
    total_retries: AtomicU64,
    recoverable_failures: AtomicU64,
    transient_failures: AtomicU64,
    permanent_failures: AtomicU64,
    circuit_rejections: AtomicU64,
    consecutive_permanent: AtomicU64,
}

impl RecoveryCounters {
    fn new() -> Self {
        Self {
            total_operations: AtomicU64::new(0),
            first_try_successes: AtomicU64::new(0),
            retry_successes: AtomicU64::new(0),
            total_retries: AtomicU64::new(0),
            recoverable_failures: AtomicU64::new(0),
            transient_failures: AtomicU64::new(0),
            permanent_failures: AtomicU64::new(0),
            circuit_rejections: AtomicU64::new(0),
            consecutive_permanent: AtomicU64::new(0),
        }
    }

    fn snapshot(&self, circuit: &CircuitBreaker) -> RecoveryStats {
        let status = circuit.status();
        RecoveryStats {
            total_operations: self.total_operations.load(Ordering::Relaxed),
            first_try_successes: self.first_try_successes.load(Ordering::Relaxed),
            retry_successes: self.retry_successes.load(Ordering::Relaxed),
            total_retries: self.total_retries.load(Ordering::Relaxed),
            recoverable_failures: self.recoverable_failures.load(Ordering::Relaxed),
            transient_failures: self.transient_failures.load(Ordering::Relaxed),
            permanent_failures: self.permanent_failures.load(Ordering::Relaxed),
            circuit_rejections: self.circuit_rejections.load(Ordering::Relaxed),
            consecutive_permanent: self.consecutive_permanent.load(Ordering::Relaxed),
            circuit_state: format!("{:?}", status.state),
        }
    }
}

/// The outcome of a single recovery-wrapped operation.
#[derive(Debug)]
pub struct RecoveryOutcome<T> {
    pub result: Result<T, RecoveryError>,
    pub attempts: u32,
    pub error_kinds: Vec<ProtocolErrorKind>,
}

/// Errors from the recovery layer.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("circuit breaker open; retry after cooldown")]
    CircuitOpen,
    #[error("max retries ({attempts}) exhausted: {last_error}")]
    RetriesExhausted {
        attempts: u32,
        last_error: String,
        last_kind: ProtocolErrorKind,
    },
    #[error("permanent protocol error: {0}")]
    Permanent(String),
    #[error("permanent failure limit ({limit}) reached; manual reset required")]
    PermanentLimitReached { limit: u32 },
    #[error("protocol recovery disabled")]
    Disabled,
}

impl RecoveryError {
    #[must_use]
    pub fn is_circuit_open(&self) -> bool {
        matches!(self, Self::CircuitOpen)
    }

    #[must_use]
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            Self::Permanent(_) | Self::PermanentLimitReached { .. }
        )
    }
}

/// Protocol recovery engine with circuit breaker and retry logic.
pub struct RecoveryEngine {
    config: RecoveryConfig,
    circuit: CircuitBreaker,
    counters: Arc<RecoveryCounters>,
}

impl RecoveryEngine {
    #[must_use]
    pub fn new(config: RecoveryConfig) -> Self {
        let cb_config = CircuitBreakerConfig {
            failure_threshold: config.circuit_failure_threshold,
            success_threshold: config.circuit_success_threshold,
            open_cooldown: config.circuit_cooldown,
        };
        Self {
            config,
            circuit: CircuitBreaker::with_name("protocol_recovery", cb_config),
            counters: Arc::new(RecoveryCounters::new()),
        }
    }

    #[must_use]
    pub fn with_name(name: impl Into<String>, config: RecoveryConfig) -> Self {
        let cb_config = CircuitBreakerConfig {
            failure_threshold: config.circuit_failure_threshold,
            success_threshold: config.circuit_success_threshold,
            open_cooldown: config.circuit_cooldown,
        };
        Self {
            config,
            circuit: CircuitBreaker::with_name(name, cb_config),
            counters: Arc::new(RecoveryCounters::new()),
        }
    }

    #[must_use]
    pub fn stats(&self) -> RecoveryStats {
        self.counters.snapshot(&self.circuit)
    }

    #[must_use]
    pub fn circuit_state(&self) -> CircuitStateKind {
        self.circuit.status().state
    }

    #[must_use]
    pub fn config(&self) -> &RecoveryConfig {
        &self.config
    }

    #[must_use]
    pub fn is_available(&mut self) -> bool {
        if !self.config.enabled {
            return false;
        }
        let consecutive = self.counters.consecutive_permanent.load(Ordering::Relaxed);
        if consecutive >= u64::from(self.config.permanent_failure_limit) {
            return false;
        }
        self.circuit.allow()
    }

    pub fn reset_permanent_counter(&self) {
        self.counters
            .consecutive_permanent
            .store(0, Ordering::Relaxed);
    }

    pub async fn execute<T, E, F, Fut, C>(
        &mut self,
        mut operation: F,
        classify: C,
    ) -> RecoveryOutcome<T>
    where
        E: std::fmt::Display,
        F: FnMut(u32) -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        C: Fn(&E) -> ProtocolErrorKind,
    {
        self.counters
            .total_operations
            .fetch_add(1, Ordering::Relaxed);

        if !self.config.enabled {
            return RecoveryOutcome {
                result: Err(RecoveryError::Disabled),
                attempts: 0,
                error_kinds: vec![],
            };
        }

        let consecutive = self.counters.consecutive_permanent.load(Ordering::Relaxed);
        if consecutive >= u64::from(self.config.permanent_failure_limit) {
            return RecoveryOutcome {
                result: Err(RecoveryError::PermanentLimitReached {
                    limit: self.config.permanent_failure_limit,
                }),
                attempts: 0,
                error_kinds: vec![],
            };
        }

        if !self.circuit.allow() {
            self.counters
                .circuit_rejections
                .fetch_add(1, Ordering::Relaxed);
            return RecoveryOutcome {
                result: Err(RecoveryError::CircuitOpen),
                attempts: 0,
                error_kinds: vec![],
            };
        }

        let max_attempts = self.config.max_retries + 1;
        let mut error_kinds = Vec::new();
        let mut last_error_msg = String::new();
        let mut last_kind = ProtocolErrorKind::Recoverable;

        for attempt in 0..max_attempts {
            match operation(attempt).await {
                Ok(value) => {
                    self.circuit.record_success();
                    self.counters
                        .consecutive_permanent
                        .store(0, Ordering::Relaxed);
                    if attempt == 0 {
                        self.counters
                            .first_try_successes
                            .fetch_add(1, Ordering::Relaxed);
                    } else {
                        self.counters
                            .retry_successes
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    return RecoveryOutcome {
                        result: Ok(value),
                        attempts: attempt + 1,
                        error_kinds,
                    };
                }
                Err(err) => {
                    let kind = classify(&err);
                    last_error_msg = err.to_string();
                    last_kind = kind;
                    error_kinds.push(kind);

                    match kind {
                        ProtocolErrorKind::Permanent => {
                            self.circuit.record_failure();
                            self.counters
                                .permanent_failures
                                .fetch_add(1, Ordering::Relaxed);
                            self.counters
                                .consecutive_permanent
                                .fetch_add(1, Ordering::Relaxed);
                            if self.config.report_degradation {
                                report_mux_degradation(&last_error_msg);
                            }
                            return RecoveryOutcome {
                                result: Err(RecoveryError::Permanent(last_error_msg)),
                                attempts: attempt + 1,
                                error_kinds,
                            };
                        }
                        ProtocolErrorKind::Recoverable => {
                            self.circuit.record_failure();
                            self.counters
                                .recoverable_failures
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        ProtocolErrorKind::Transient => {
                            self.counters
                                .transient_failures
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    if attempt + 1 < max_attempts {
                        self.counters.total_retries.fetch_add(1, Ordering::Relaxed);
                        let delay = self.config.delay_for_attempt(attempt);
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        if self.config.report_degradation {
            report_mux_degradation(&last_error_msg);
        }

        RecoveryOutcome {
            result: Err(RecoveryError::RetriesExhausted {
                attempts: max_attempts,
                last_error: last_error_msg,
                last_kind,
            }),
            attempts: max_attempts,
            error_kinds,
        }
    }
}

fn report_mux_degradation(reason: &str) {
    crate::degradation::enter_degraded(
        crate::degradation::Subsystem::MuxConnection,
        format!("protocol recovery failed: {reason}"),
    );
}

/// Heuristics for detecting frame-level protocol corruption.
pub struct FrameCorruptionDetector {
    unexpected_count: u32,
    codec_error_count: u32,
    window_size: u32,
    corruption_threshold: u32,
    window_ops: u32,
}

impl FrameCorruptionDetector {
    #[must_use]
    pub fn new(window_size: u32, corruption_threshold: u32) -> Self {
        Self {
            unexpected_count: 0,
            codec_error_count: 0,
            window_size,
            corruption_threshold,
            window_ops: 0,
        }
    }

    pub fn record_success(&mut self) {
        self.window_ops += 1;
        self.maybe_rotate_window();
    }

    pub fn record_error(&mut self, kind: ProtocolErrorKind, error_msg: &str) -> bool {
        self.window_ops += 1;
        if kind == ProtocolErrorKind::Recoverable {
            let lower = error_msg.to_lowercase();
            if lower.contains("unexpected response") {
                self.unexpected_count += 1;
            } else if lower.contains("codec") || lower.contains("frame exceeded") {
                self.codec_error_count += 1;
            }
        }
        self.maybe_rotate_window();
        self.is_corrupted()
    }

    #[must_use]
    pub fn is_corrupted(&self) -> bool {
        (self.unexpected_count + self.codec_error_count) >= self.corruption_threshold
    }

    pub fn reset(&mut self) {
        self.unexpected_count = 0;
        self.codec_error_count = 0;
        self.window_ops = 0;
    }

    #[must_use]
    pub fn error_counts(&self) -> (u32, u32) {
        (self.unexpected_count, self.codec_error_count)
    }

    fn maybe_rotate_window(&mut self) {
        if self.window_ops >= self.window_size {
            self.unexpected_count /= 2;
            self.codec_error_count /= 2;
            self.window_ops = 0;
        }
    }
}

impl Default for FrameCorruptionDetector {
    fn default() -> Self {
        Self::new(100, 3)
    }
}

/// Health status of a mux connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionHealth {
    Healthy,
    Degraded,
    Corrupted,
    Dead,
}

/// Per-connection health tracker.
pub struct ConnectionHealthTracker {
    detector: FrameCorruptionDetector,
    consecutive_successes: u32,
    consecutive_failures: u32,
    health: ConnectionHealth,
}

impl ConnectionHealthTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            detector: FrameCorruptionDetector::default(),
            consecutive_successes: 0,
            consecutive_failures: 0,
            health: ConnectionHealth::Healthy,
        }
    }

    pub fn record_success(&mut self) -> ConnectionHealth {
        self.detector.record_success();
        self.consecutive_successes += 1;
        self.consecutive_failures = 0;
        if self.health == ConnectionHealth::Degraded && self.consecutive_successes >= 5 {
            self.health = ConnectionHealth::Healthy;
        }
        self.health
    }

    pub fn record_error(&mut self, kind: ProtocolErrorKind, msg: &str) -> ConnectionHealth {
        let corrupted = self.detector.record_error(kind, msg);
        self.consecutive_successes = 0;
        self.consecutive_failures += 1;
        match kind {
            ProtocolErrorKind::Permanent => self.health = ConnectionHealth::Dead,
            ProtocolErrorKind::Recoverable => {
                self.health = if corrupted {
                    ConnectionHealth::Corrupted
                } else {
                    ConnectionHealth::Degraded
                };
            }
            ProtocolErrorKind::Transient => {
                if self.consecutive_failures >= 3 {
                    self.health = ConnectionHealth::Degraded;
                }
            }
        }
        self.health
    }

    #[must_use]
    pub fn health(&self) -> ConnectionHealth {
        self.health
    }

    pub fn reset(&mut self) {
        self.detector.reset();
        self.consecutive_successes = 0;
        self.consecutive_failures = 0;
        self.health = ConnectionHealth::Healthy;
    }
}

impl Default for ConnectionHealthTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn classify_unexpected_response() {
        assert_eq!(
            classify_error_message(
                "unexpected response: expected ListPanesResponse, got UnitResponse"
            ),
            ProtocolErrorKind::Recoverable
        );
    }

    #[test]
    fn classify_disconnected() {
        assert_eq!(
            classify_error_message("mux socket disconnected"),
            ProtocolErrorKind::Recoverable
        );
    }

    #[test]
    fn classify_codec_error() {
        assert_eq!(
            classify_error_message("codec error: invalid"),
            ProtocolErrorKind::Recoverable
        );
    }

    #[test]
    fn classify_frame_too_large() {
        assert_eq!(
            classify_error_message("frame exceeded max size (4194304 bytes)"),
            ProtocolErrorKind::Recoverable
        );
    }

    #[test]
    fn classify_connect_timeout() {
        assert_eq!(
            classify_error_message("connect to mux socket timed out"),
            ProtocolErrorKind::Transient
        );
    }

    #[test]
    fn classify_read_timeout() {
        assert_eq!(
            classify_error_message("read from mux socket timed out"),
            ProtocolErrorKind::Transient
        );
    }

    #[test]
    fn classify_socket_not_found() {
        assert_eq!(
            classify_error_message("mux socket not found at /tmp/wez.sock"),
            ProtocolErrorKind::Transient
        );
    }

    #[test]
    fn classify_incompatible_codec() {
        assert_eq!(
            classify_error_message("codec version mismatch: local 4 != remote 3"),
            ProtocolErrorKind::Permanent
        );
    }

    #[test]
    fn classify_socket_path_missing() {
        assert_eq!(
            classify_error_message("mux socket path not found; set WEZTERM_UNIX_SOCKET"),
            ProtocolErrorKind::Permanent
        );
    }

    #[test]
    fn classify_io_broken_pipe() {
        assert_eq!(
            classify_error_message("io error: broken pipe"),
            ProtocolErrorKind::Recoverable
        );
    }

    #[test]
    fn classify_io_would_block() {
        assert_eq!(
            classify_error_message("io error: would block"),
            ProtocolErrorKind::Transient
        );
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(
            classify_error_message("something unknown"),
            ProtocolErrorKind::Recoverable
        );
    }

    #[test]
    fn default_config_is_reasonable() {
        let config = RecoveryConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_retries, 3);
        assert!(config.max_delay > config.initial_delay);
    }

    #[test]
    fn capture_config_faster() {
        let c = RecoveryConfig::for_capture();
        let d = RecoveryConfig::default();
        assert!(c.initial_delay < d.initial_delay);
    }

    #[test]
    fn interactive_config_more_patient() {
        let i = RecoveryConfig::for_interactive();
        let d = RecoveryConfig::default();
        assert!(i.max_retries >= d.max_retries);
    }

    #[test]
    fn delay_capped_at_max() {
        let config = RecoveryConfig {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(200),
            backoff_factor: 10.0,
            jitter_fraction: 0.0,
            ..RecoveryConfig::default()
        };
        assert!(config.delay_for_attempt(10) <= Duration::from_millis(201));
    }

    #[test]
    fn detector_starts_clean() {
        let d = FrameCorruptionDetector::default();
        assert!(!d.is_corrupted());
    }

    #[test]
    fn detector_detects_corruption() {
        let mut d = FrameCorruptionDetector::new(100, 3);
        d.record_error(ProtocolErrorKind::Recoverable, "unexpected response: X");
        d.record_error(ProtocolErrorKind::Recoverable, "unexpected response: Y");
        assert!(!d.is_corrupted());
        d.record_error(ProtocolErrorKind::Recoverable, "unexpected response: Z");
        assert!(d.is_corrupted());
    }

    #[test]
    fn detector_ignores_transient() {
        let mut d = FrameCorruptionDetector::new(100, 2);
        d.record_error(ProtocolErrorKind::Transient, "timeout");
        d.record_error(ProtocolErrorKind::Transient, "timeout");
        assert!(!d.is_corrupted());
    }

    #[test]
    fn detector_resets() {
        let mut d = FrameCorruptionDetector::new(100, 2);
        d.record_error(ProtocolErrorKind::Recoverable, "unexpected response: X");
        d.record_error(ProtocolErrorKind::Recoverable, "unexpected response: Y");
        assert!(d.is_corrupted());
        d.reset();
        assert!(!d.is_corrupted());
    }

    #[test]
    fn detector_window_rotation() {
        let mut d = FrameCorruptionDetector::new(5, 4);
        d.record_error(ProtocolErrorKind::Recoverable, "unexpected response: X");
        d.record_error(ProtocolErrorKind::Recoverable, "unexpected response: Y");
        assert_eq!(d.error_counts(), (2, 0));
        d.record_success();
        d.record_success();
        d.record_success();
        assert_eq!(d.error_counts(), (1, 0));
    }

    #[test]
    fn tracker_starts_healthy() {
        assert_eq!(
            ConnectionHealthTracker::new().health(),
            ConnectionHealth::Healthy
        );
    }

    #[test]
    fn tracker_degrades_on_transient() {
        let mut t = ConnectionHealthTracker::new();
        t.record_error(ProtocolErrorKind::Transient, "timeout");
        t.record_error(ProtocolErrorKind::Transient, "timeout");
        assert_eq!(t.health(), ConnectionHealth::Healthy);
        t.record_error(ProtocolErrorKind::Transient, "timeout");
        assert_eq!(t.health(), ConnectionHealth::Degraded);
    }

    #[test]
    fn tracker_recovers_after_successes() {
        let mut t = ConnectionHealthTracker::new();
        for _ in 0..3 {
            t.record_error(ProtocolErrorKind::Transient, "timeout");
        }
        assert_eq!(t.health(), ConnectionHealth::Degraded);
        for _ in 0..5 {
            t.record_success();
        }
        assert_eq!(t.health(), ConnectionHealth::Healthy);
    }

    #[test]
    fn tracker_corrupted_on_repeated_unexpected() {
        let mut t = ConnectionHealthTracker::new();
        t.record_error(ProtocolErrorKind::Recoverable, "unexpected response: X");
        t.record_error(ProtocolErrorKind::Recoverable, "unexpected response: Y");
        t.record_error(ProtocolErrorKind::Recoverable, "unexpected response: Z");
        assert_eq!(t.health(), ConnectionHealth::Corrupted);
    }

    #[test]
    fn tracker_dead_on_permanent() {
        let mut t = ConnectionHealthTracker::new();
        t.record_error(ProtocolErrorKind::Permanent, "codec mismatch");
        assert_eq!(t.health(), ConnectionHealth::Dead);
    }

    #[test]
    fn tracker_reset_restores_healthy() {
        let mut t = ConnectionHealthTracker::new();
        t.record_error(ProtocolErrorKind::Permanent, "dead");
        t.reset();
        assert_eq!(t.health(), ConnectionHealth::Healthy);
    }

    #[tokio::test]
    async fn engine_succeeds_first_try() {
        let mut e = RecoveryEngine::new(RecoveryConfig::default());
        let o = e
            .execute(
                |_| async { Ok::<_, String>(42) },
                |_: &String| ProtocolErrorKind::Transient,
            )
            .await;
        assert_eq!(o.result.unwrap(), 42);
        assert_eq!(o.attempts, 1);
        assert_eq!(e.stats().first_try_successes, 1);
    }

    #[tokio::test]
    async fn engine_retries_transient() {
        let cc = Arc::new(AtomicU32::new(0));
        let cc2 = cc.clone();
        let config = RecoveryConfig {
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        let o = e
            .execute(
                move |_| {
                    let cc = cc2.clone();
                    async move {
                        let n = cc.fetch_add(1, Ordering::Relaxed);
                        if n < 2 {
                            Err("read from mux socket timed out".into())
                        } else {
                            Ok(99)
                        }
                    }
                },
                |err: &String| classify_error_message(err),
            )
            .await;
        assert_eq!(o.result.unwrap(), 99);
        assert_eq!(o.attempts, 3);
        assert_eq!(e.stats().transient_failures, 2);
    }

    #[tokio::test]
    async fn engine_stops_on_permanent() {
        let config = RecoveryConfig {
            initial_delay: Duration::from_millis(1),
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        let o = e
            .execute(
                |_| async {
                    Err::<i32, _>("codec version mismatch: local 4 != remote 3".to_string())
                },
                |err: &String| classify_error_message(err),
            )
            .await;
        assert!(matches!(o.result.unwrap_err(), RecoveryError::Permanent(_)));
        assert_eq!(o.attempts, 1);
    }

    #[tokio::test]
    async fn engine_exhausts_retries() {
        let config = RecoveryConfig {
            max_retries: 2,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            report_degradation: false,
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        let o = e
            .execute(
                |_| async { Err::<i32, _>("mux socket disconnected".to_string()) },
                |err: &String| classify_error_message(err),
            )
            .await;
        assert!(matches!(
            o.result.unwrap_err(),
            RecoveryError::RetriesExhausted { .. }
        ));
        assert_eq!(o.attempts, 3);
    }

    #[tokio::test]
    async fn engine_circuit_breaker_opens() {
        let config = RecoveryConfig {
            max_retries: 0,
            circuit_failure_threshold: 2,
            circuit_cooldown: Duration::from_secs(60),
            initial_delay: Duration::from_millis(1),
            report_degradation: false,
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        for _ in 0..2 {
            let _ = e
                .execute(
                    |_| async { Err::<i32, _>("mux socket disconnected".to_string()) },
                    |err: &String| classify_error_message(err),
                )
                .await;
        }
        let o = e
            .execute(
                |_| async { Ok::<_, String>(42) },
                |_: &String| ProtocolErrorKind::Transient,
            )
            .await;
        assert!(matches!(o.result.unwrap_err(), RecoveryError::CircuitOpen));
    }

    #[tokio::test]
    async fn engine_permanent_limit() {
        let config = RecoveryConfig {
            max_retries: 0,
            permanent_failure_limit: 2,
            initial_delay: Duration::from_millis(1),
            report_degradation: false,
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        for _ in 0..2 {
            let _ = e
                .execute(
                    |_| async {
                        Err::<i32, _>("codec version mismatch: local 4 != remote 3".to_string())
                    },
                    |err: &String| classify_error_message(err),
                )
                .await;
        }
        let o = e
            .execute(
                |_| async { Ok::<_, String>(42) },
                |_: &String| ProtocolErrorKind::Transient,
            )
            .await;
        assert!(matches!(
            o.result.unwrap_err(),
            RecoveryError::PermanentLimitReached { .. }
        ));
    }

    #[tokio::test]
    async fn engine_disabled() {
        let mut e = RecoveryEngine::new(RecoveryConfig {
            enabled: false,
            ..RecoveryConfig::default()
        });
        let o = e
            .execute(
                |_| async { Ok::<_, String>(42) },
                |_: &String| ProtocolErrorKind::Transient,
            )
            .await;
        assert!(matches!(o.result.unwrap_err(), RecoveryError::Disabled));
    }

    #[tokio::test]
    async fn engine_permanent_counter_resets() {
        let config = RecoveryConfig {
            max_retries: 0,
            permanent_failure_limit: 3,
            circuit_failure_threshold: 10,
            initial_delay: Duration::from_millis(1),
            report_degradation: false,
            ..RecoveryConfig::default()
        };
        let mut e = RecoveryEngine::new(config);
        let _ = e
            .execute(
                |_| async {
                    Err::<i32, _>("codec version mismatch: local 4 != remote 3".to_string())
                },
                |err: &String| classify_error_message(err),
            )
            .await;
        assert_eq!(e.stats().consecutive_permanent, 1);
        let _ = e
            .execute(
                |_| async { Ok::<_, String>(42) },
                |_: &String| ProtocolErrorKind::Transient,
            )
            .await;
        assert_eq!(e.stats().consecutive_permanent, 0);
    }

    #[test]
    fn error_kind_display() {
        assert_eq!(ProtocolErrorKind::Recoverable.to_string(), "recoverable");
        assert_eq!(ProtocolErrorKind::Transient.to_string(), "transient");
        assert_eq!(ProtocolErrorKind::Permanent.to_string(), "permanent");
    }

    #[test]
    fn error_kind_serde_roundtrip() {
        for kind in [
            ProtocolErrorKind::Recoverable,
            ProtocolErrorKind::Transient,
            ProtocolErrorKind::Permanent,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: ProtocolErrorKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn recovery_stats_serde() {
        let stats = RecoveryStats {
            total_operations: 100,
            first_try_successes: 90,
            retry_successes: 5,
            total_retries: 10,
            recoverable_failures: 3,
            transient_failures: 4,
            permanent_failures: 1,
            circuit_rejections: 2,
            consecutive_permanent: 0,
            circuit_state: "Closed".into(),
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: RecoveryStats = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_operations, 100);
    }

    #[test]
    fn recovery_error_helpers() {
        assert!(RecoveryError::CircuitOpen.is_circuit_open());
        assert!(RecoveryError::Permanent("x".into()).is_permanent());
        assert!(!RecoveryError::Disabled.is_permanent());
    }

    #[test]
    fn connection_health_serde() {
        for h in [
            ConnectionHealth::Healthy,
            ConnectionHealth::Degraded,
            ConnectionHealth::Corrupted,
            ConnectionHealth::Dead,
        ] {
            let json = serde_json::to_string(&h).unwrap();
            let back: ConnectionHealth = serde_json::from_str(&json).unwrap();
            assert_eq!(back, h);
        }
    }
}
