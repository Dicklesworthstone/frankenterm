use super::{InvariantDomain, LatencyStage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Circuit breaker state for a latency stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BreakerState {
    /// Normal operation: requests flow through.
    Closed,
    /// Tripped: requests are rejected immediately.
    Open,
    /// Probing: a limited number of requests are allowed through to test recovery.
    HalfOpen,
}

impl fmt::Display for BreakerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "closed"),
            Self::Open => write!(f, "open"),
            Self::HalfOpen => write!(f, "half-open"),
        }
    }
}

/// Configuration for a stage circuit breaker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageBreakerConfig {
    /// Failure count threshold to trip the breaker.
    pub failure_threshold: u32,
    /// Duration to stay open before transitioning to half-open, in microseconds.
    pub open_duration_us: u64,
    /// Number of probe requests allowed in half-open state.
    pub half_open_max_probes: u32,
    /// Success count in half-open to close the breaker.
    pub half_open_success_threshold: u32,
}

impl Default for StageBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_duration_us: 1_000_000,
            half_open_max_probes: 3,
            half_open_success_threshold: 2,
        }
    }
}

/// Per-stage breaker state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageBreakerState {
    /// Stage this breaker guards.
    pub stage: LatencyStage,
    /// Current state.
    pub state: BreakerState,
    /// Consecutive failure count.
    pub consecutive_failures: u32,
    /// Timestamp when breaker was opened (0 if never).
    pub opened_at_us: u64,
    /// Probe count in current half-open window.
    pub half_open_probes: u32,
    /// Successful probes in half-open.
    pub half_open_successes: u32,
    /// Total trips.
    pub total_trips: u64,
    /// Total recoveries.
    pub total_recoveries: u64,
}

/// A recovery step in a choreography sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryStep {
    /// Stage being recovered.
    pub stage: LatencyStage,
    /// Step number in the sequence.
    pub step_number: u32,
    /// Action description.
    pub action: String,
    /// Whether this step requires all previous steps to succeed.
    pub requires_prior_success: bool,
    /// Timeout for this step, in microseconds.
    pub timeout_us: u64,
}

/// Recovery choreography outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChoreographyOutcome {
    /// All stages recovered successfully.
    FullRecovery,
    /// Some stages recovered, others remain degraded.
    PartialRecovery {
        recovered: Vec<LatencyStage>,
        failed: Vec<LatencyStage>,
    },
    /// Recovery was aborted, for example because of timeout or cascade failure.
    Aborted { reason: String },
}

impl fmt::Display for ChoreographyOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FullRecovery => write!(f, "full-recovery"),
            Self::PartialRecovery { recovered, failed } => {
                write!(
                    f,
                    "partial({} ok, {} failed)",
                    recovered.len(),
                    failed.len()
                )
            }
            Self::Aborted { reason } => write!(f, "aborted: {reason}"),
        }
    }
}

/// Snapshot of the circuit breaker manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakerManagerSnapshot {
    /// Per-stage states.
    pub stages: Vec<StageBreakerState>,
    /// Total trips across all stages.
    pub total_trips: u64,
    /// Total recoveries across all stages.
    pub total_recoveries: u64,
}

/// Degradation state for the breaker manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BreakerManagerDegradation {
    /// All breakers closed.
    Healthy,
    /// Some breakers open or half-open.
    BreakerTripped { open_count: usize },
    /// Many breakers tripped; cascade risk.
    CascadeRisk { open_count: usize },
}

impl fmt::Display for BreakerManagerDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::BreakerTripped { open_count } => write!(f, "tripped({open_count})"),
            Self::CascadeRisk { open_count } => write!(f, "cascade-risk({open_count})"),
        }
    }
}

/// Log entry for breaker events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakerLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Stage affected.
    pub stage: LatencyStage,
    /// Previous state.
    pub from_state: BreakerState,
    /// New state.
    pub to_state: BreakerState,
    /// Reason for transition.
    pub reason: String,
}

/// The circuit breaker manager for latency stages.
pub struct BreakerManager {
    config: StageBreakerConfig,
    states: HashMap<LatencyStage, StageBreakerState>,
}

impl BreakerManager {
    /// Create a new breaker manager with the given config.
    pub fn new(config: StageBreakerConfig) -> Self {
        let mut states = HashMap::new();
        for stage in LatencyStage::PIPELINE_STAGES {
            states.insert(
                *stage,
                StageBreakerState {
                    stage: *stage,
                    state: BreakerState::Closed,
                    consecutive_failures: 0,
                    opened_at_us: 0,
                    half_open_probes: 0,
                    half_open_successes: 0,
                    total_trips: 0,
                    total_recoveries: 0,
                },
            );
        }
        Self { config, states }
    }

    /// Record a failure for a stage.
    #[allow(clippy::similar_names)]
    pub fn record_failure(&mut self, stage: LatencyStage, timestamp_us: u64) {
        let threshold = self.config.failure_threshold;
        let Some(state) = self.states.get_mut(&stage) else {
            return;
        };
        match state.state {
            BreakerState::Closed => {
                state.consecutive_failures += 1;
                if state.consecutive_failures >= threshold {
                    state.state = BreakerState::Open;
                    state.opened_at_us = timestamp_us;
                    state.total_trips += 1;
                }
            }
            BreakerState::HalfOpen => {
                state.state = BreakerState::Open;
                state.opened_at_us = timestamp_us;
                state.half_open_probes = 0;
                state.half_open_successes = 0;
            }
            BreakerState::Open => {}
        }
    }

    /// Record a success for a stage.
    #[allow(clippy::similar_names)]
    pub fn record_success(&mut self, stage: LatencyStage) {
        let success_threshold = self.config.half_open_success_threshold;
        let Some(state) = self.states.get_mut(&stage) else {
            return;
        };
        match state.state {
            BreakerState::Closed => {
                state.consecutive_failures = 0;
            }
            BreakerState::HalfOpen => {
                state.half_open_successes += 1;
                if state.half_open_successes >= success_threshold {
                    state.state = BreakerState::Closed;
                    state.consecutive_failures = 0;
                    state.half_open_probes = 0;
                    state.half_open_successes = 0;
                    state.total_recoveries += 1;
                }
            }
            BreakerState::Open => {}
        }
    }

    /// Check if a request should be allowed through for a stage.
    #[allow(clippy::similar_names)]
    pub fn allow_request(&mut self, stage: LatencyStage, current_us: u64) -> bool {
        let open_duration = self.config.open_duration_us;
        let max_probes = self.config.half_open_max_probes;
        let Some(state) = self.states.get_mut(&stage) else {
            return true;
        };
        match state.state {
            BreakerState::Closed => true,
            BreakerState::Open => {
                if current_us.saturating_sub(state.opened_at_us) >= open_duration {
                    state.state = BreakerState::HalfOpen;
                    state.half_open_probes = 1;
                    state.half_open_successes = 0;
                    true
                } else {
                    false
                }
            }
            BreakerState::HalfOpen => {
                if state.half_open_probes < max_probes {
                    state.half_open_probes += 1;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Get the state of a stage's breaker.
    pub fn breaker_state(&self, stage: LatencyStage) -> BreakerState {
        self.states
            .get(&stage)
            .map_or(BreakerState::Closed, |s| s.state)
    }

    /// Count of open (tripped) breakers.
    pub fn open_count(&self) -> usize {
        self.states
            .values()
            .filter(|s| s.state == BreakerState::Open || s.state == BreakerState::HalfOpen)
            .count()
    }

    /// Whether all breakers are closed.
    pub fn all_closed(&self) -> bool {
        self.states
            .values()
            .all(|s| s.state == BreakerState::Closed)
    }

    /// Get a snapshot.
    pub fn snapshot(&self) -> BreakerManagerSnapshot {
        let stages: Vec<StageBreakerState> = LatencyStage::PIPELINE_STAGES
            .iter()
            .filter_map(|s| self.states.get(s).cloned())
            .collect();
        let total_trips = stages.iter().map(|s| s.total_trips).sum();
        let total_recoveries = stages.iter().map(|s| s.total_recoveries).sum();
        BreakerManagerSnapshot {
            stages,
            total_trips,
            total_recoveries,
        }
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> BreakerManagerDegradation {
        let open_count = self.open_count();
        if open_count == 0 {
            BreakerManagerDegradation::Healthy
        } else if open_count >= 3 {
            BreakerManagerDegradation::CascadeRisk { open_count }
        } else {
            BreakerManagerDegradation::BreakerTripped { open_count }
        }
    }

    /// Create a log entry.
    pub fn log_entry(
        &self,
        stage: LatencyStage,
        from: BreakerState,
        to: BreakerState,
        reason: String,
        timestamp_us: u64,
    ) -> BreakerLogEntry {
        BreakerLogEntry {
            timestamp_us,
            stage,
            from_state: from,
            to_state: to,
            reason,
        }
    }

    /// Reset all breakers to closed.
    pub fn reset(&mut self) {
        for state in self.states.values_mut() {
            state.state = BreakerState::Closed;
            state.consecutive_failures = 0;
            state.opened_at_us = 0;
            state.half_open_probes = 0;
            state.half_open_successes = 0;
            state.total_trips = 0;
            state.total_recoveries = 0;
        }
    }

    /// Access config.
    pub fn config(&self) -> &StageBreakerConfig {
        &self.config
    }

    /// Total trips across all stages.
    pub fn total_trips(&self) -> u64 {
        self.states.values().map(|s| s.total_trips).sum()
    }

    /// Total recoveries across all stages.
    pub fn total_recoveries(&self) -> u64 {
        self.states.values().map(|s| s.total_recoveries).sum()
    }

    /// Total consecutive failures across all stages.
    pub fn total_consecutive_failures(&self) -> u32 {
        self.states.values().map(|s| s.consecutive_failures).sum()
    }

    /// Stages currently in the Open state.
    pub fn open_stages(&self) -> Vec<LatencyStage> {
        self.states
            .iter()
            .filter(|(_, s)| s.state == BreakerState::Open)
            .map(|(stage, _)| *stage)
            .collect()
    }

    /// Stages currently in the HalfOpen state.
    pub fn half_open_stages(&self) -> Vec<LatencyStage> {
        self.states
            .iter()
            .filter(|(_, s)| s.state == BreakerState::HalfOpen)
            .map(|(stage, _)| *stage)
            .collect()
    }

    /// Stages currently in the Closed state.
    pub fn closed_stages(&self) -> Vec<LatencyStage> {
        self.states
            .iter()
            .filter(|(_, s)| s.state == BreakerState::Closed)
            .map(|(stage, _)| *stage)
            .collect()
    }

    /// Generate a recovery choreography plan for all open/half-open stages.
    /// Returns a list of recovery steps ordered by pipeline position.
    pub fn plan_recovery(&self) -> Vec<RecoveryStep> {
        let mut stages: Vec<LatencyStage> = self
            .states
            .iter()
            .filter(|(_, s)| s.state != BreakerState::Closed)
            .map(|(stage, _)| *stage)
            .collect();
        stages.sort_by_key(|s| {
            LatencyStage::PIPELINE_STAGES
                .iter()
                .position(|p| p == s)
                .unwrap_or(usize::MAX)
        });
        stages
            .iter()
            .enumerate()
            .map(|(i, stage)| RecoveryStep {
                stage: *stage,
                step_number: i as u32,
                action: format!("recover-{stage}"),
                requires_prior_success: i > 0,
                timeout_us: self.config.open_duration_us,
            })
            .collect()
    }

    /// Execute a recovery plan by transitioning open breakers to half-open
    /// for probing. Returns the number of breakers transitioned.
    pub fn initiate_recovery(&mut self, current_us: u64) -> u32 {
        let mut transitioned = 0u32;
        let open_duration = self.config.open_duration_us;
        for state in self.states.values_mut() {
            if state.state == BreakerState::Open
                && current_us.saturating_sub(state.opened_at_us) >= open_duration
            {
                state.state = BreakerState::HalfOpen;
                state.half_open_probes = 0;
                state.half_open_successes = 0;
                transitioned += 1;
            }
        }
        transitioned
    }

    /// Map `BreakerManager` degradation to `InvariantDomain` for reporting.
    pub fn to_invariant_domain() -> InvariantDomain {
        InvariantDomain::Recovery
    }

    /// Availability ratio: fraction of stages with closed breakers (0.0..=1.0).
    pub fn availability(&self) -> f64 {
        let total = self.states.len() as f64;
        if total == 0.0 {
            return 1.0;
        }
        let closed = self
            .states
            .values()
            .filter(|s| s.state == BreakerState::Closed)
            .count() as f64;
        closed / total
    }

    /// Record a batch of failures for a single stage, for example from trace replay.
    pub fn record_failures_batch(&mut self, stage: LatencyStage, count: u32, timestamp_us: u64) {
        for i in 0..count {
            self.record_failure(stage, timestamp_us + u64::from(i));
        }
    }

    /// Get per-stage breaker state.
    pub fn stage_state(&self, stage: LatencyStage) -> Option<&StageBreakerState> {
        self.states.get(&stage)
    }
}
