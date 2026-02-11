//! Circuit breaker infrastructure for reliability hardening.
//!
//! Provides a small state machine with cooldowns and status reporting.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::degradation::{DegradationManager, Subsystem};

/// Configuration for a circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before opening the circuit.
    pub failure_threshold: u32,
    /// Number of consecutive successes required to close from half-open.
    pub success_threshold: u32,
    /// Cooldown duration while the circuit is open.
    pub open_cooldown: Duration,
}

impl CircuitBreakerConfig {
    /// Create a new configuration.
    #[must_use]
    pub fn new(failure_threshold: u32, success_threshold: u32, open_cooldown: Duration) -> Self {
        Self {
            failure_threshold: failure_threshold.max(1),
            success_threshold: success_threshold.max(1),
            open_cooldown,
        }
    }
}

/// Default circuit breaker configuration for WezTerm CLI operations.
impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            success_threshold: 1,
            open_cooldown: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone)]
enum CircuitState {
    Closed,
    Open { opened_at: Instant },
    HalfOpen { successes: u32 },
}

const CASCADE_WINDOW: Duration = Duration::from_secs(30);
const CASCADE_SUBSYSTEM_THRESHOLD: usize = 2;

#[derive(Debug, Clone)]
struct CircuitOpenRecord {
    circuit: String,
    subsystem: Option<Subsystem>,
    opened_at: Instant,
}

#[derive(Debug, Clone)]
struct CascadeEvent {
    circuits: Vec<String>,
    subsystems: Vec<Subsystem>,
    window: Duration,
}

#[derive(Debug, Default)]
struct CascadeTracker {
    open_events: VecDeque<CircuitOpenRecord>,
    last_cascade_at: Option<Instant>,
}

impl CascadeTracker {
    fn record_open(&mut self, circuit: &str, subsystem: Option<Subsystem>) -> Option<CascadeEvent> {
        let now = Instant::now();

        while let Some(front) = self.open_events.front() {
            if now.duration_since(front.opened_at) > CASCADE_WINDOW {
                self.open_events.pop_front();
            } else {
                break;
            }
        }

        self.open_events.push_back(CircuitOpenRecord {
            circuit: circuit.to_string(),
            subsystem,
            opened_at: now,
        });

        let mut subsystems: BTreeSet<Subsystem> = BTreeSet::new();
        let mut circuits: BTreeSet<String> = BTreeSet::new();
        for event in &self.open_events {
            if let Some(subsystem) = event.subsystem {
                subsystems.insert(subsystem);
                circuits.insert(event.circuit.clone());
            }
        }

        if subsystems.len() < CASCADE_SUBSYSTEM_THRESHOLD {
            return None;
        }

        if self
            .last_cascade_at
            .is_some_and(|ts| now.duration_since(ts) <= CASCADE_WINDOW)
        {
            return None;
        }

        self.last_cascade_at = Some(now);

        Some(CascadeEvent {
            circuits: circuits.into_iter().collect(),
            subsystems: subsystems.into_iter().collect(),
            window: CASCADE_WINDOW,
        })
    }
}

static CASCADE_TRACKER: OnceLock<Mutex<CascadeTracker>> = OnceLock::new();

fn subsystem_for_circuit(name: &str) -> Option<Subsystem> {
    match name {
        "wezterm_cli" => Some(Subsystem::WeztermCli),
        "mux_connection" => Some(Subsystem::MuxConnection),
        "capture_pipeline" => Some(Subsystem::Capture),
        "db_write" => Some(Subsystem::DbWrite),
        "pattern_engine" => Some(Subsystem::PatternEngine),
        "workflow_engine" => Some(Subsystem::WorkflowEngine),
        _ => None,
    }
}

fn handle_circuit_opened(name: &str, failures: u32, threshold: u32) {
    let subsystem = subsystem_for_circuit(name);
    if subsystem.is_some() {
        let _ = DegradationManager::init_global();
    }

    if let Some(subsystem) = subsystem {
        crate::degradation::enter_degraded(
            subsystem,
            format!(
                "circuit breaker `{name}` opened after {failures} consecutive failures (threshold {threshold})"
            ),
        );
    }

    let tracker = CASCADE_TRACKER.get_or_init(|| Mutex::new(CascadeTracker::default()));
    let cascade = match tracker.lock() {
        Ok(mut guard) => guard.record_open(name, subsystem),
        Err(poisoned) => poisoned.into_inner().record_open(name, subsystem),
    };

    if let Some(cascade) = cascade {
        error!(
            circuits = ?cascade.circuits,
            subsystems = ?cascade.subsystems,
            window_ms = cascade.window.as_millis() as u64,
            "Circuit breaker cascade detected"
        );

        crate::degradation::enter_degraded(
            Subsystem::WorkflowEngine,
            format!(
                "circuit breaker cascade detected: {}",
                cascade.circuits.join(", ")
            ),
        );
    }
}

fn handle_circuit_closed(name: &str) {
    let Some(subsystem) = subsystem_for_circuit(name) else {
        return;
    };
    if DegradationManager::global().is_some() {
        crate::degradation::recover(subsystem);
    }
}

/// Public-facing circuit state for status reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitStateKind {
    Closed,
    Open,
    HalfOpen,
}

/// Snapshot of circuit breaker status for reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerStatus {
    pub state: CircuitStateKind,
    pub consecutive_failures: u32,
    pub failure_threshold: u32,
    pub success_threshold: u32,
    pub open_cooldown_ms: u64,
    pub open_for_ms: Option<u64>,
    pub cooldown_remaining_ms: Option<u64>,
    pub half_open_successes: Option<u32>,
}

impl Default for CircuitBreakerStatus {
    fn default() -> Self {
        Self {
            state: CircuitStateKind::Closed,
            consecutive_failures: 0,
            failure_threshold: 0,
            success_threshold: 0,
            open_cooldown_ms: 0,
            open_for_ms: None,
            cooldown_remaining_ms: None,
            half_open_successes: None,
        }
    }
}

/// Circuit breaker state machine.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    name: String,
    config: CircuitBreakerConfig,
    state: CircuitState,
    consecutive_failures: u32,
}

impl CircuitBreaker {
    /// Create a new circuit breaker from configuration.
    #[must_use]
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self::with_name("unnamed", config)
    }

    /// Create a new circuit breaker with a stable name.
    #[must_use]
    pub fn with_name(name: impl Into<String>, config: CircuitBreakerConfig) -> Self {
        Self {
            name: name.into(),
            config,
            state: CircuitState::Closed,
            consecutive_failures: 0,
        }
    }

    /// Check whether an operation is allowed to proceed.
    ///
    /// Returns `true` if allowed; `false` if the circuit is open.
    pub fn allow(&mut self) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open { opened_at } => {
                if opened_at.elapsed() >= self.config.open_cooldown {
                    self.state = CircuitState::HalfOpen { successes: 0 };
                    info!(
                        circuit = %self.name,
                        "Circuit transitioned to half-open after cooldown"
                    );
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen { .. } => true,
        }
    }

    /// Record a successful operation.
    pub fn record_success(&mut self) {
        match self.state {
            CircuitState::Closed => {
                self.consecutive_failures = 0;
            }
            CircuitState::HalfOpen { successes } => {
                let successes = successes + 1;
                if successes >= self.config.success_threshold {
                    self.consecutive_failures = 0;
                    self.state = CircuitState::Closed;
                    info!(circuit = %self.name, "Circuit closed after successful probe");
                    handle_circuit_closed(&self.name);
                } else {
                    self.state = CircuitState::HalfOpen { successes };
                }
            }
            CircuitState::Open { .. } => {
                // Ignore successes while open (no operations should run).
            }
        }
    }

    /// Record a failed operation.
    pub fn record_failure(&mut self) {
        match self.state {
            CircuitState::Closed => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                if self.consecutive_failures >= self.config.failure_threshold {
                    self.state = CircuitState::Open {
                        opened_at: Instant::now(),
                    };
                    warn!(
                        circuit = %self.name,
                        failures = self.consecutive_failures,
                        threshold = self.config.failure_threshold,
                        "Circuit opened after consecutive failures"
                    );
                    handle_circuit_opened(
                        &self.name,
                        self.consecutive_failures,
                        self.config.failure_threshold,
                    );
                }
            }
            CircuitState::HalfOpen { .. } => {
                self.state = CircuitState::Open {
                    opened_at: Instant::now(),
                };
                warn!(circuit = %self.name, "Circuit re-opened after half-open failure");
                handle_circuit_opened(
                    &self.name,
                    self.consecutive_failures,
                    self.config.failure_threshold,
                );
            }
            CircuitState::Open { .. } => {
                // Already open; keep cooldown ticking.
            }
        }
    }

    /// Return a status snapshot for reporting.
    #[must_use]
    pub fn status(&self) -> CircuitBreakerStatus {
        match self.state {
            CircuitState::Closed => CircuitBreakerStatus {
                state: CircuitStateKind::Closed,
                consecutive_failures: self.consecutive_failures,
                failure_threshold: self.config.failure_threshold,
                success_threshold: self.config.success_threshold,
                open_cooldown_ms: self.config.open_cooldown.as_millis() as u64,
                open_for_ms: None,
                cooldown_remaining_ms: None,
                half_open_successes: None,
            },
            CircuitState::Open { opened_at } => {
                let elapsed = opened_at.elapsed();
                let remaining = self.config.open_cooldown.checked_sub(elapsed);
                CircuitBreakerStatus {
                    state: CircuitStateKind::Open,
                    consecutive_failures: self.consecutive_failures,
                    failure_threshold: self.config.failure_threshold,
                    success_threshold: self.config.success_threshold,
                    open_cooldown_ms: self.config.open_cooldown.as_millis() as u64,
                    open_for_ms: Some(elapsed.as_millis() as u64),
                    cooldown_remaining_ms: remaining.map(|d| d.as_millis() as u64),
                    half_open_successes: None,
                }
            }
            CircuitState::HalfOpen { successes } => CircuitBreakerStatus {
                state: CircuitStateKind::HalfOpen,
                consecutive_failures: self.consecutive_failures,
                failure_threshold: self.config.failure_threshold,
                success_threshold: self.config.success_threshold,
                open_cooldown_ms: self.config.open_cooldown.as_millis() as u64,
                open_for_ms: None,
                cooldown_remaining_ms: None,
                half_open_successes: Some(successes),
            },
        }
    }
}

/// Snapshot of a named circuit breaker for reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerSnapshot {
    pub name: String,
    pub status: CircuitBreakerStatus,
}

static CIRCUIT_REGISTRY: OnceLock<RwLock<BTreeMap<String, Arc<Mutex<CircuitBreaker>>>>> =
    OnceLock::new();

/// Get or register a named circuit breaker.
#[must_use]
pub fn get_or_register_circuit(
    name: impl Into<String>,
    config: CircuitBreakerConfig,
) -> Arc<Mutex<CircuitBreaker>> {
    let name = name.into();
    let registry = CIRCUIT_REGISTRY.get_or_init(|| RwLock::new(BTreeMap::new()));

    if let Ok(read_guard) = registry.read() {
        if let Some(existing) = read_guard.get(&name) {
            return Arc::clone(existing);
        }
    }

    let mut write_guard = match registry.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    write_guard
        .entry(name.clone())
        .or_insert_with(|| Arc::new(Mutex::new(CircuitBreaker::with_name(name.clone(), config))))
        .clone()
}

/// Ensure default circuits exist for status reporting.
pub fn ensure_default_circuits() {
    let defaults = [
        "wezterm_cli",
        "mux_connection",
        "capture_pipeline",
        "caut_cli",
        "browser_auth",
        "webhook",
    ];
    for name in defaults {
        let _ = get_or_register_circuit(name, CircuitBreakerConfig::default());
    }
}

/// Snapshot current circuit breaker statuses.
#[must_use]
pub fn circuit_snapshots() -> Vec<CircuitBreakerSnapshot> {
    let registry = CIRCUIT_REGISTRY.get_or_init(|| RwLock::new(BTreeMap::new()));
    let read_guard = match registry.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    read_guard
        .iter()
        .map(|(name, breaker)| {
            let status = match breaker.lock() {
                Ok(guard) => guard.status(),
                Err(poisoned) => poisoned.into_inner().status(),
            };
            CircuitBreakerSnapshot {
                name: name.clone(),
                status,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::degradation::{OverallStatus, active_degradations, recover};

    #[test]
    fn circuit_opens_after_threshold() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(2, 1, Duration::from_secs(10)));

        assert!(breaker.allow());
        breaker.record_failure();
        assert!(matches!(breaker.status().state, CircuitStateKind::Closed));

        breaker.record_failure();
        let status = breaker.status();
        assert!(matches!(status.state, CircuitStateKind::Open));
        assert!(status.cooldown_remaining_ms.is_some());
    }

    #[test]
    fn circuit_half_open_closes_on_success() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(1, 1, Duration::from_millis(0)));

        breaker.record_failure();
        // Cooldown is zero, so allow transitions to half-open.
        assert!(breaker.allow());
        assert!(matches!(breaker.status().state, CircuitStateKind::HalfOpen));

        breaker.record_success();
        assert!(matches!(breaker.status().state, CircuitStateKind::Closed));
    }

    #[test]
    fn circuit_half_open_failure_reopens() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(1, 2, Duration::from_millis(0)));

        breaker.record_failure();
        assert!(breaker.allow());
        assert!(matches!(breaker.status().state, CircuitStateKind::HalfOpen));

        breaker.record_failure();
        assert!(matches!(breaker.status().state, CircuitStateKind::Open));
    }

    #[test]
    fn circuit_open_enters_degraded_mode_for_mapped_subsystem() {
        let _ = DegradationManager::init_global();
        recover(Subsystem::WeztermCli);

        let mut breaker = CircuitBreaker::with_name(
            "wezterm_cli",
            CircuitBreakerConfig::new(1, 1, Duration::from_secs(10)),
        );
        breaker.record_failure();

        let degradations = active_degradations();
        assert!(
            degradations
                .iter()
                .any(|s| s.subsystem == Subsystem::WeztermCli)
        );
        recover(Subsystem::WeztermCli);
    }

    #[test]
    fn cascade_detection_degrades_workflow_engine() {
        let _ = DegradationManager::init_global();
        recover(Subsystem::WeztermCli);
        recover(Subsystem::MuxConnection);
        recover(Subsystem::WorkflowEngine);

        let tracker = CASCADE_TRACKER.get_or_init(|| Mutex::new(CascadeTracker::default()));
        match tracker.lock() {
            Ok(mut guard) => {
                guard.open_events.clear();
                guard.last_cascade_at = None;
            }
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.open_events.clear();
                guard.last_cascade_at = None;
            }
        }

        let mut cli = CircuitBreaker::with_name(
            "wezterm_cli",
            CircuitBreakerConfig::new(1, 1, Duration::from_secs(10)),
        );
        cli.record_failure();
        let mut mux = CircuitBreaker::with_name(
            "mux_connection",
            CircuitBreakerConfig::new(1, 1, Duration::from_secs(10)),
        );
        mux.record_failure();

        assert_eq!(
            crate::degradation::overall_status(),
            OverallStatus::Degraded
        );
        let degradations = active_degradations();
        assert!(
            degradations
                .iter()
                .any(|s| s.subsystem == Subsystem::WorkflowEngine)
        );

        recover(Subsystem::WeztermCli);
        recover(Subsystem::MuxConnection);
        recover(Subsystem::WorkflowEngine);
    }
}
