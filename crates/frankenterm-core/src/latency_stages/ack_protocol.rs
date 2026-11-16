use super::{InvariantDomain, LatencyStage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Phase in the immediate-ack / deferred-completion protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AckPhase {
    /// Fast path: produce an immediate user-visible acknowledgment.
    ImmediateAck,
    /// Slow path: deferred processing with progress tracking.
    DeferredCompletion,
}

impl fmt::Display for AckPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ImmediateAck => write!(f, "immediate-ack"),
            Self::DeferredCompletion => write!(f, "deferred-completion"),
        }
    }
}

/// Reason code for deferred-completion outcome.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompletionReason {
    /// Completed successfully.
    Success,
    /// Timed out waiting for slow path.
    Timeout,
    /// Upstream stage failed (breaker tripped, storage error, etc.).
    UpstreamFailure { stage: LatencyStage, detail: String },
    /// Cancelled by user or system.
    Cancelled { reason: String },
}

impl fmt::Display for CompletionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => write!(f, "success"),
            Self::Timeout => write!(f, "timeout"),
            Self::UpstreamFailure { stage, detail } => {
                write!(f, "upstream-failure({stage}: {detail})")
            }
            Self::Cancelled { reason } => write!(f, "cancelled({reason})"),
        }
    }
}

/// Immediate-ack token: a lightweight receipt returned to the user on the fast path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckToken {
    /// Unique correlation ID linking ack to deferred completion.
    pub correlation_id: u64,
    /// Timestamp of ack generation (μs).
    pub acked_at_us: u64,
    /// The stage that produced the ack.
    pub source_stage: LatencyStage,
    /// Human-readable summary for display.
    pub summary: String,
}

/// Deferred-completion result: delivered after slow-path processing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredResult {
    /// Correlation ID matching the AckToken.
    pub correlation_id: u64,
    /// Completion timestamp (μs).
    pub completed_at_us: u64,
    /// Reason code.
    pub reason: CompletionReason,
    /// Wall-clock latency from ack to completion (μs).
    pub deferred_latency_us: u64,
    /// Optional explanation for the user (ft why style).
    pub explanation: Option<String>,
}

/// Configuration for the ack/completion protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckProtocolConfig {
    /// Max time to wait for immediate ack before downgrading (μs).
    pub ack_deadline_us: u64,
    /// Max time to wait for deferred completion (μs).
    pub completion_deadline_us: u64,
    /// Whether to show progress updates to user during deferred phase.
    pub show_progress: bool,
    /// Minimum interval between progress updates (μs).
    pub progress_interval_us: u64,
}

impl Default for AckProtocolConfig {
    fn default() -> Self {
        Self {
            ack_deadline_us: 50_000,           // 50ms — must feel instant.
            completion_deadline_us: 5_000_000, // 5s — user patience limit.
            show_progress: true,
            progress_interval_us: 500_000, // 500ms between updates.
        }
    }
}

/// Progress update during deferred phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressUpdate {
    /// Correlation ID.
    pub correlation_id: u64,
    /// Timestamp of progress report (μs).
    pub timestamp_us: u64,
    /// Fraction complete (0.0..=1.0).
    pub fraction: f64,
    /// Human-readable status message.
    pub message: String,
}

/// Snapshot of the ack protocol manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckProtocolSnapshot {
    /// Total ack tokens issued.
    pub total_acks: u64,
    /// Total deferred completions.
    pub total_completions: u64,
    /// Total timeouts.
    pub total_timeouts: u64,
    /// Pending (acked but not completed).
    pub pending_count: u64,
}

/// Degradation level for the protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AckProtocolDegradation {
    /// All requests completing within deadlines.
    Healthy,
    /// Some acks are slow (above ack_deadline).
    AckSlow { slow_count: u64 },
    /// Deferred completions timing out.
    CompletionTimeout { timeout_count: u64 },
}

impl fmt::Display for AckProtocolDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::AckSlow { slow_count } => write!(f, "ack-slow({slow_count})"),
            Self::CompletionTimeout { timeout_count } => {
                write!(f, "completion-timeout({timeout_count})")
            }
        }
    }
}

/// Log entry for ack protocol events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckProtocolLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Phase at which the event occurred.
    pub phase: AckPhase,
    /// Correlation ID.
    pub correlation_id: u64,
    /// Event description.
    pub event: String,
}

/// Manages the immediate-ack / deferred-completion UX protocol.
pub struct AckProtocolManager {
    config: AckProtocolConfig,
    next_correlation_id: u64,
    /// Pending acks: correlation_id → AckToken.
    pending: HashMap<u64, AckToken>,
    total_acks: u64,
    total_completions: u64,
    total_timeouts: u64,
    total_cancellations: u64,
    slow_ack_count: u64,
}

impl AckProtocolManager {
    /// Create a new protocol manager.
    pub fn new(config: AckProtocolConfig) -> Self {
        Self {
            config,
            next_correlation_id: 1,
            pending: HashMap::new(),
            total_acks: 0,
            total_completions: 0,
            total_timeouts: 0,
            total_cancellations: 0,
            slow_ack_count: 0,
        }
    }

    /// Issue an immediate ack. Returns a token for the caller.
    pub fn issue_ack(
        &mut self,
        stage: LatencyStage,
        summary: String,
        timestamp_us: u64,
    ) -> AckToken {
        let cid = self.next_correlation_id;
        self.next_correlation_id += 1;
        let token = AckToken {
            correlation_id: cid,
            acked_at_us: timestamp_us,
            source_stage: stage,
            summary,
        };
        self.pending.insert(cid, token.clone());
        self.total_acks += 1;
        token
    }

    /// Complete a deferred operation. Returns the result with latency info.
    pub fn complete(
        &mut self,
        correlation_id: u64,
        reason: CompletionReason,
        timestamp_us: u64,
    ) -> Option<DeferredResult> {
        let token = self.pending.remove(&correlation_id)?;
        let deferred_latency_us = timestamp_us.saturating_sub(token.acked_at_us);
        let is_timeout = matches!(reason, CompletionReason::Timeout);
        let is_cancel = matches!(reason, CompletionReason::Cancelled { .. });
        if is_timeout {
            self.total_timeouts += 1;
        } else if is_cancel {
            self.total_cancellations += 1;
        }
        self.total_completions += 1;
        Some(DeferredResult {
            correlation_id,
            completed_at_us: timestamp_us,
            reason,
            deferred_latency_us,
            explanation: None,
        })
    }

    /// Record a slow ack (ack took longer than ack_deadline).
    pub fn record_slow_ack(&mut self) {
        self.slow_ack_count += 1;
    }

    /// Check for timed-out pending operations and complete them.
    pub fn sweep_timeouts(&mut self, current_us: u64) -> Vec<DeferredResult> {
        let deadline = self.config.completion_deadline_us;
        let expired: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, token)| current_us.saturating_sub(token.acked_at_us) >= deadline)
            .map(|(cid, _)| *cid)
            .collect();
        let mut results = Vec::new();
        for cid in expired {
            if let Some(result) = self.complete(cid, CompletionReason::Timeout, current_us) {
                results.push(result);
            }
        }
        results
    }

    /// Number of pending (acked but not completed) operations.
    pub fn pending_count(&self) -> u64 {
        self.pending.len() as u64
    }

    /// Get a snapshot.
    pub fn snapshot(&self) -> AckProtocolSnapshot {
        AckProtocolSnapshot {
            total_acks: self.total_acks,
            total_completions: self.total_completions,
            total_timeouts: self.total_timeouts,
            pending_count: self.pending_count(),
        }
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> AckProtocolDegradation {
        if self.total_timeouts > 0 {
            AckProtocolDegradation::CompletionTimeout {
                timeout_count: self.total_timeouts,
            }
        } else if self.slow_ack_count > 0 {
            AckProtocolDegradation::AckSlow {
                slow_count: self.slow_ack_count,
            }
        } else {
            AckProtocolDegradation::Healthy
        }
    }

    /// Create a log entry.
    pub fn log_entry(
        &self,
        phase: AckPhase,
        correlation_id: u64,
        event: String,
        timestamp_us: u64,
    ) -> AckProtocolLogEntry {
        AckProtocolLogEntry {
            timestamp_us,
            phase,
            correlation_id,
            event,
        }
    }

    /// Reset counters.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.total_acks = 0;
        self.total_completions = 0;
        self.total_timeouts = 0;
        self.total_cancellations = 0;
        self.slow_ack_count = 0;
    }

    /// Access config.
    pub fn config(&self) -> &AckProtocolConfig {
        &self.config
    }

    // ── F3 Impl: Bridge methods ──

    /// Total acks issued.
    pub fn total_acks(&self) -> u64 {
        self.total_acks
    }

    /// Total completions (success + timeout + cancel).
    pub fn total_completions(&self) -> u64 {
        self.total_completions
    }

    /// Total timeouts.
    pub fn total_timeouts(&self) -> u64 {
        self.total_timeouts
    }

    /// Total cancellations.
    pub fn total_cancellations(&self) -> u64 {
        self.total_cancellations
    }

    /// Total slow acks recorded.
    pub fn slow_ack_count(&self) -> u64 {
        self.slow_ack_count
    }

    /// Completion rate: total_completions / total_acks.
    pub fn completion_rate(&self) -> f64 {
        if self.total_acks == 0 {
            return 1.0;
        }
        self.total_completions as f64 / self.total_acks as f64
    }

    /// Timeout rate: total_timeouts / total_completions.
    pub fn timeout_rate(&self) -> f64 {
        if self.total_completions == 0 {
            return 0.0;
        }
        self.total_timeouts as f64 / self.total_completions as f64
    }

    /// Whether there are any pending operations.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Get a pending token by correlation ID.
    pub fn get_pending(&self, correlation_id: u64) -> Option<&AckToken> {
        self.pending.get(&correlation_id)
    }

    /// Complete with explanation.
    pub fn complete_with_explanation(
        &mut self,
        correlation_id: u64,
        reason: CompletionReason,
        timestamp_us: u64,
        explanation: String,
    ) -> Option<DeferredResult> {
        self.complete(correlation_id, reason, timestamp_us)
            .map(|mut r| {
                r.explanation = Some(explanation);
                r
            })
    }

    /// Issue an ack and immediately check if it was slow.
    pub fn issue_ack_checked(
        &mut self,
        stage: LatencyStage,
        summary: String,
        request_received_us: u64,
        ack_sent_us: u64,
    ) -> AckToken {
        let token = self.issue_ack(stage, summary, ack_sent_us);
        if ack_sent_us.saturating_sub(request_received_us) > self.config.ack_deadline_us {
            self.record_slow_ack();
        }
        token
    }

    /// Map AckProtocol to InvariantDomain.
    pub fn to_invariant_domain() -> InvariantDomain {
        InvariantDomain::Composition
    }

    /// Generate a progress update for a pending operation.
    pub fn make_progress(
        &self,
        correlation_id: u64,
        fraction: f64,
        message: String,
        timestamp_us: u64,
    ) -> Option<ProgressUpdate> {
        if !self.pending.contains_key(&correlation_id) {
            return None;
        }
        Some(ProgressUpdate {
            correlation_id,
            timestamp_us,
            fraction: fraction.clamp(0.0, 1.0),
            message,
        })
    }
}
