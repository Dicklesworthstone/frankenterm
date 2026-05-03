use super::{
    DomainHealth, FaultDomain, FaultIsolationConfig, FaultIsolationDegradation,
    FaultIsolationManager, FaultIsolationSnapshot,
};
use serde::{Deserialize, Serialize};
use std::{fmt, mem};

/// A structured log entry emitted at every health transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultTransitionLog {
    /// Monotonic sequence number.
    pub seq: u64,
    /// Timestamp in epoch microseconds.
    pub timestamp_us: u64,
    /// Domain that transitioned.
    pub domain: FaultDomain,
    /// Previous health state.
    pub from: DomainHealth,
    /// New health state.
    pub to: DomainHealth,
    /// Reason code for the transition.
    pub reason_code: FaultReasonCode,
    /// Human-readable description.
    pub description: String,
    /// Correlation ID for grouping related transitions.
    pub correlation_id: u64,
}

/// Reason codes for fault transitions; stable taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FaultReasonCode {
    /// External fault reported.
    FaultRecorded,
    /// Auto-isolation threshold exceeded.
    AutoIsolated,
    /// Restart attempted.
    RestartAttempted,
    /// Restart completed successfully.
    RestartSucceeded,
    /// Restart failed.
    RestartFailed,
    /// Manual degradation marking.
    ManualDegraded,
    /// Manual un-isolation (operator override).
    ManualUnIsolate,
    /// System reset.
    Reset,
    /// Deterministic replay of recorded events.
    Replay,
}

impl fmt::Display for FaultReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FaultRecorded => write!(f, "fault-recorded"),
            Self::AutoIsolated => write!(f, "auto-isolated"),
            Self::RestartAttempted => write!(f, "restart-attempted"),
            Self::RestartSucceeded => write!(f, "restart-succeeded"),
            Self::RestartFailed => write!(f, "restart-failed"),
            Self::ManualDegraded => write!(f, "manual-degraded"),
            Self::ManualUnIsolate => write!(f, "manual-un-isolate"),
            Self::Reset => write!(f, "reset"),
            Self::Replay => write!(f, "replay"),
        }
    }
}

/// Wraps `FaultIsolationManager` with automatic structured transition logging
/// and deterministic replay support.
pub struct InstrumentedFaultManager {
    inner: FaultIsolationManager,
    log: Vec<FaultTransitionLog>,
    next_seq: u64,
    correlation_counter: u64,
}

impl InstrumentedFaultManager {
    /// Create a new instrumented manager.
    pub fn new(config: FaultIsolationConfig) -> Self {
        Self {
            inner: FaultIsolationManager::new(config),
            log: Vec::new(),
            next_seq: 0,
            correlation_counter: 0,
        }
    }

    /// Allocate a new correlation ID (monotonic).
    fn next_correlation(&mut self) -> u64 {
        self.correlation_counter += 1;
        self.correlation_counter
    }

    /// Emit a transition log entry.
    fn emit(
        &mut self,
        domain: FaultDomain,
        from: DomainHealth,
        to: DomainHealth,
        reason_code: FaultReasonCode,
        description: String,
        timestamp_us: u64,
        correlation_id: u64,
    ) {
        if from == to {
            return;
        }
        let entry = FaultTransitionLog {
            seq: self.next_seq,
            timestamp_us,
            domain,
            from,
            to,
            reason_code,
            description,
            correlation_id,
        };
        self.next_seq += 1;
        self.log.push(entry);
    }

    /// Record a fault with automatic transition logging.
    pub fn record_fault(&mut self, domain: FaultDomain, description: String, timestamp_us: u64) {
        let before = self.inner.domain_health(domain);
        let corr = self.next_correlation();
        self.inner
            .record_fault(domain, description.clone(), timestamp_us);
        let after = self.inner.domain_health(domain);
        let reason = if after == DomainHealth::Isolated {
            FaultReasonCode::AutoIsolated
        } else {
            FaultReasonCode::FaultRecorded
        };
        self.emit(
            domain,
            before,
            after,
            reason,
            description,
            timestamp_us,
            corr,
        );
    }

    /// Attempt restart with transition logging.
    pub fn attempt_restart(&mut self, domain: FaultDomain, timestamp_us: u64) -> bool {
        let before = self.inner.domain_health(domain);
        let corr = self.next_correlation();
        let ok = self.inner.attempt_restart(domain, timestamp_us);
        if ok {
            let after = self.inner.domain_health(domain);
            self.emit(
                domain,
                before,
                after,
                FaultReasonCode::RestartAttempted,
                "restart initiated".to_string(),
                timestamp_us,
                corr,
            );
        }
        ok
    }

    /// Restart succeeded with transition logging.
    pub fn restart_succeeded(&mut self, domain: FaultDomain, timestamp_us: u64) {
        let before = self.inner.domain_health(domain);
        let corr = self.next_correlation();
        self.inner.restart_succeeded(domain);
        let after = self.inner.domain_health(domain);
        self.emit(
            domain,
            before,
            after,
            FaultReasonCode::RestartSucceeded,
            "restart completed".to_string(),
            timestamp_us,
            corr,
        );
    }

    /// Restart failed with transition logging.
    pub fn restart_failed(&mut self, domain: FaultDomain, timestamp_us: u64) {
        let before = self.inner.domain_health(domain);
        let corr = self.next_correlation();
        self.inner.restart_failed(domain, timestamp_us);
        let after = self.inner.domain_health(domain);
        let reason = if after == DomainHealth::Isolated {
            FaultReasonCode::AutoIsolated
        } else {
            FaultReasonCode::RestartFailed
        };
        self.emit(
            domain,
            before,
            after,
            reason,
            "restart failed".to_string(),
            timestamp_us,
            corr,
        );
    }

    /// Mark degraded with transition logging.
    pub fn mark_degraded(&mut self, domain: FaultDomain, timestamp_us: u64) {
        let before = self.inner.domain_health(domain);
        let corr = self.next_correlation();
        self.inner.mark_degraded(domain);
        let after = self.inner.domain_health(domain);
        self.emit(
            domain,
            before,
            after,
            FaultReasonCode::ManualDegraded,
            "manual degradation".to_string(),
            timestamp_us,
            corr,
        );
    }

    /// Un-isolate with transition logging.
    pub fn un_isolate(&mut self, domain: FaultDomain, timestamp_us: u64) {
        let before = self.inner.domain_health(domain);
        let corr = self.next_correlation();
        self.inner.un_isolate(domain);
        let after = self.inner.domain_health(domain);
        self.emit(
            domain,
            before,
            after,
            FaultReasonCode::ManualUnIsolate,
            "manual un-isolate".to_string(),
            timestamp_us,
            corr,
        );
    }

    /// Delegate to inner.
    pub fn domain_health(&self, domain: FaultDomain) -> DomainHealth {
        self.inner.domain_health(domain)
    }

    /// Delegate to inner.
    pub fn snapshot(&self) -> FaultIsolationSnapshot {
        self.inner.snapshot()
    }

    /// Delegate to inner.
    pub fn detect_degradation(&self) -> FaultIsolationDegradation {
        self.inner.detect_degradation()
    }

    /// Delegate to inner.
    pub fn all_healthy(&self) -> bool {
        self.inner.all_healthy()
    }

    /// Delegate to inner.
    pub fn has_isolated_domains(&self) -> bool {
        self.inner.has_isolated_domains()
    }

    /// Delegate to inner.
    pub fn isolated_domains(&self) -> Vec<FaultDomain> {
        self.inner.isolated_domains()
    }

    /// Get the transition log.
    pub fn transition_log(&self) -> &[FaultTransitionLog] {
        &self.log
    }

    /// Drain and return transition log entries.
    pub fn drain_log(&mut self) -> Vec<FaultTransitionLog> {
        mem::take(&mut self.log)
    }

    /// Deterministic replay: apply a sequence of fault events and produce
    /// the exact same state plus transition log as the original execution.
    pub fn replay(config: FaultIsolationConfig, events: &[ReplayableEvent]) -> Self {
        let mut mgr = Self::new(config);
        for event in events {
            match event.action {
                ReplayAction::RecordFault => {
                    mgr.record_fault(event.domain, event.description.clone(), event.timestamp_us);
                }
                ReplayAction::AttemptRestart => {
                    let _ = mgr.attempt_restart(event.domain, event.timestamp_us);
                }
                ReplayAction::RestartSucceeded => {
                    mgr.restart_succeeded(event.domain, event.timestamp_us);
                }
                ReplayAction::RestartFailed => {
                    mgr.restart_failed(event.domain, event.timestamp_us);
                }
                ReplayAction::MarkDegraded => {
                    mgr.mark_degraded(event.domain, event.timestamp_us);
                }
                ReplayAction::UnIsolate => {
                    mgr.un_isolate(event.domain, event.timestamp_us);
                }
            }
        }
        for entry in &mut mgr.log {
            entry.reason_code = match entry.reason_code {
                FaultReasonCode::AutoIsolated => FaultReasonCode::AutoIsolated,
                _ => FaultReasonCode::Replay,
            };
        }
        mgr
    }

    /// Access inner manager.
    pub fn inner(&self) -> &FaultIsolationManager {
        &self.inner
    }

    /// Total transition log entries emitted.
    pub fn log_count(&self) -> usize {
        self.log.len()
    }
}

/// Action for deterministic replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReplayAction {
    /// Record a fault.
    RecordFault,
    /// Attempt restart.
    AttemptRestart,
    /// Restart succeeded.
    RestartSucceeded,
    /// Restart failed.
    RestartFailed,
    /// Mark degraded.
    MarkDegraded,
    /// Un-isolate.
    UnIsolate,
}

impl fmt::Display for ReplayAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RecordFault => write!(f, "record-fault"),
            Self::AttemptRestart => write!(f, "attempt-restart"),
            Self::RestartSucceeded => write!(f, "restart-succeeded"),
            Self::RestartFailed => write!(f, "restart-failed"),
            Self::MarkDegraded => write!(f, "mark-degraded"),
            Self::UnIsolate => write!(f, "un-isolate"),
        }
    }
}

/// A replayable event for deterministic replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayableEvent {
    /// Domain this event targets.
    pub domain: FaultDomain,
    /// Timestamp in epoch microseconds.
    pub timestamp_us: u64,
    /// Action to replay.
    pub action: ReplayAction,
    /// Description for `RecordFault` actions.
    pub description: String,
}
