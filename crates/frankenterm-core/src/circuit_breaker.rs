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

    // --- Config tests ---

    #[test]
    fn config_clamps_zero_thresholds_to_one() {
        let config = CircuitBreakerConfig::new(0, 0, Duration::from_secs(5));
        assert_eq!(config.failure_threshold, 1);
        assert_eq!(config.success_threshold, 1);
        assert_eq!(config.open_cooldown, Duration::from_secs(5));
    }

    #[test]
    fn config_preserves_nonzero_thresholds() {
        let config = CircuitBreakerConfig::new(5, 3, Duration::from_millis(500));
        assert_eq!(config.failure_threshold, 5);
        assert_eq!(config.success_threshold, 3);
        assert_eq!(config.open_cooldown, Duration::from_millis(500));
    }

    #[test]
    fn default_config_values() {
        let config = CircuitBreakerConfig::default();
        assert_eq!(config.failure_threshold, 3);
        assert_eq!(config.success_threshold, 1);
        assert_eq!(config.open_cooldown, Duration::from_secs(10));
    }

    // --- State machine: closed state ---

    #[test]
    fn closed_allows_operations() {
        let mut breaker = CircuitBreaker::new(CircuitBreakerConfig::default());
        for _ in 0..10 {
            assert!(breaker.allow());
        }
    }

    #[test]
    fn success_in_closed_resets_failure_counter() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(3, 1, Duration::from_secs(10)));

        breaker.record_failure();
        breaker.record_failure();
        assert_eq!(breaker.status().consecutive_failures, 2);

        breaker.record_success();
        assert_eq!(breaker.status().consecutive_failures, 0);
        assert_eq!(breaker.status().state, CircuitStateKind::Closed);
    }

    #[test]
    fn failures_below_threshold_stay_closed() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(5, 1, Duration::from_secs(10)));

        for i in 1..5 {
            breaker.record_failure();
            assert_eq!(breaker.status().state, CircuitStateKind::Closed);
            assert_eq!(breaker.status().consecutive_failures, i);
        }
    }

    // --- State machine: open state ---

    #[test]
    fn open_blocks_operations() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(1, 1, Duration::from_secs(60)));

        breaker.record_failure();
        assert_eq!(breaker.status().state, CircuitStateKind::Open);
        assert!(!breaker.allow());
        assert!(!breaker.allow());
    }

    #[test]
    fn failure_while_open_is_noop() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(1, 1, Duration::from_secs(60)));

        breaker.record_failure(); // opens
        let failures_before = breaker.status().consecutive_failures;
        breaker.record_failure(); // should be ignored
        assert_eq!(breaker.status().consecutive_failures, failures_before);
        assert_eq!(breaker.status().state, CircuitStateKind::Open);
    }

    #[test]
    fn success_while_open_is_ignored() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(1, 1, Duration::from_secs(60)));

        breaker.record_failure(); // opens
        breaker.record_success(); // should be ignored — no operations should run while open
        assert_eq!(breaker.status().state, CircuitStateKind::Open);
    }

    // --- State machine: half-open state ---

    #[test]
    fn multiple_successes_needed_to_close_from_half_open() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(1, 3, Duration::from_millis(0)));

        breaker.record_failure(); // open
        assert!(breaker.allow()); // half-open (0ms cooldown)
        assert_eq!(breaker.status().state, CircuitStateKind::HalfOpen);
        assert_eq!(breaker.status().half_open_successes, Some(0));

        breaker.record_success();
        assert_eq!(breaker.status().state, CircuitStateKind::HalfOpen);
        assert_eq!(breaker.status().half_open_successes, Some(1));

        breaker.record_success();
        assert_eq!(breaker.status().state, CircuitStateKind::HalfOpen);
        assert_eq!(breaker.status().half_open_successes, Some(2));

        breaker.record_success();
        assert_eq!(breaker.status().state, CircuitStateKind::Closed);
        assert_eq!(breaker.status().consecutive_failures, 0);
    }

    #[test]
    fn half_open_allows_operations() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(1, 2, Duration::from_millis(0)));

        breaker.record_failure();
        assert!(breaker.allow()); // transitions to half-open
        assert!(breaker.allow()); // still allowed in half-open
    }

    // --- Status snapshot tests ---

    #[test]
    fn closed_status_snapshot() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(3, 2, Duration::from_secs(10)));
        breaker.record_failure();

        let status = breaker.status();
        assert_eq!(status.state, CircuitStateKind::Closed);
        assert_eq!(status.consecutive_failures, 1);
        assert_eq!(status.failure_threshold, 3);
        assert_eq!(status.success_threshold, 2);
        assert_eq!(status.open_cooldown_ms, 10_000);
        assert!(status.open_for_ms.is_none());
        assert!(status.cooldown_remaining_ms.is_none());
        assert!(status.half_open_successes.is_none());
    }

    #[test]
    fn open_status_snapshot_has_timing() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(1, 1, Duration::from_secs(60)));
        breaker.record_failure();

        let status = breaker.status();
        assert_eq!(status.state, CircuitStateKind::Open);
        assert!(status.open_for_ms.is_some());
        assert!(status.cooldown_remaining_ms.is_some());
        assert!(status.half_open_successes.is_none());
    }

    #[test]
    fn half_open_status_snapshot() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(1, 3, Duration::from_millis(0)));
        breaker.record_failure();
        breaker.allow(); // transitions to half-open

        let status = breaker.status();
        assert_eq!(status.state, CircuitStateKind::HalfOpen);
        assert_eq!(status.half_open_successes, Some(0));
        assert!(status.open_for_ms.is_none());
    }

    // --- Serde roundtrip tests ---

    #[test]
    fn circuit_state_kind_serde_roundtrip() {
        for kind in [
            CircuitStateKind::Closed,
            CircuitStateKind::Open,
            CircuitStateKind::HalfOpen,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: CircuitStateKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn circuit_state_kind_rename_all() {
        assert_eq!(
            serde_json::to_string(&CircuitStateKind::Closed).unwrap(),
            "\"closed\""
        );
        assert_eq!(
            serde_json::to_string(&CircuitStateKind::Open).unwrap(),
            "\"open\""
        );
        assert_eq!(
            serde_json::to_string(&CircuitStateKind::HalfOpen).unwrap(),
            "\"half_open\""
        );
    }

    #[test]
    fn circuit_breaker_status_serde_roundtrip() {
        let status = CircuitBreakerStatus {
            state: CircuitStateKind::Open,
            consecutive_failures: 5,
            failure_threshold: 3,
            success_threshold: 2,
            open_cooldown_ms: 10_000,
            open_for_ms: Some(3_500),
            cooldown_remaining_ms: Some(6_500),
            half_open_successes: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: CircuitBreakerStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state, status.state);
        assert_eq!(back.consecutive_failures, status.consecutive_failures);
        assert_eq!(back.open_for_ms, status.open_for_ms);
        assert_eq!(back.cooldown_remaining_ms, status.cooldown_remaining_ms);
    }

    #[test]
    fn circuit_breaker_status_default() {
        let status = CircuitBreakerStatus::default();
        assert_eq!(status.state, CircuitStateKind::Closed);
        assert_eq!(status.consecutive_failures, 0);
        assert!(status.open_for_ms.is_none());
        assert!(status.half_open_successes.is_none());
    }

    #[test]
    fn circuit_breaker_snapshot_serde_roundtrip() {
        let snapshot = CircuitBreakerSnapshot {
            name: "test_circuit".to_string(),
            status: CircuitBreakerStatus {
                state: CircuitStateKind::HalfOpen,
                consecutive_failures: 2,
                failure_threshold: 3,
                success_threshold: 1,
                open_cooldown_ms: 5000,
                open_for_ms: None,
                cooldown_remaining_ms: None,
                half_open_successes: Some(1),
            },
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let back: CircuitBreakerSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "test_circuit");
        assert_eq!(back.status.state, CircuitStateKind::HalfOpen);
        assert_eq!(back.status.half_open_successes, Some(1));
    }

    // --- CascadeTracker tests ---

    #[test]
    fn cascade_tracker_no_cascade_with_single_subsystem() {
        let mut tracker = CascadeTracker::default();
        let result = tracker.record_open("wezterm_cli", Some(Subsystem::WeztermCli));
        assert!(result.is_none());
    }

    #[test]
    fn cascade_tracker_triggers_on_two_subsystems() {
        let mut tracker = CascadeTracker::default();
        tracker.record_open("wezterm_cli", Some(Subsystem::WeztermCli));
        let result = tracker.record_open("mux_connection", Some(Subsystem::MuxConnection));
        assert!(result.is_some());
        let cascade = result.unwrap();
        assert_eq!(cascade.subsystems.len(), 2);
        assert_eq!(cascade.circuits.len(), 2);
    }

    #[test]
    fn cascade_tracker_no_cascade_without_subsystem() {
        let mut tracker = CascadeTracker::default();
        tracker.record_open("custom_a", None);
        let result = tracker.record_open("custom_b", None);
        assert!(result.is_none());
    }

    #[test]
    fn cascade_tracker_dedup_within_window() {
        let mut tracker = CascadeTracker::default();
        tracker.record_open("wezterm_cli", Some(Subsystem::WeztermCli));
        let first = tracker.record_open("mux_connection", Some(Subsystem::MuxConnection));
        assert!(first.is_some());

        // Second cascade in same window should be suppressed
        let second = tracker.record_open("capture_pipeline", Some(Subsystem::Capture));
        assert!(second.is_none());
    }

    #[test]
    fn cascade_tracker_same_subsystem_twice_no_cascade() {
        let mut tracker = CascadeTracker::default();
        tracker.record_open("wezterm_cli", Some(Subsystem::WeztermCli));
        let result = tracker.record_open("wezterm_cli_2", Some(Subsystem::WeztermCli));
        assert!(result.is_none()); // same subsystem, not enough distinct subsystems
    }

    // --- subsystem_for_circuit mapping ---

    #[test]
    fn subsystem_mapping_covers_known_circuits() {
        assert_eq!(
            subsystem_for_circuit("wezterm_cli"),
            Some(Subsystem::WeztermCli)
        );
        assert_eq!(
            subsystem_for_circuit("mux_connection"),
            Some(Subsystem::MuxConnection)
        );
        assert_eq!(
            subsystem_for_circuit("capture_pipeline"),
            Some(Subsystem::Capture)
        );
        assert_eq!(
            subsystem_for_circuit("db_write"),
            Some(Subsystem::DbWrite)
        );
        assert_eq!(
            subsystem_for_circuit("pattern_engine"),
            Some(Subsystem::PatternEngine)
        );
        assert_eq!(
            subsystem_for_circuit("workflow_engine"),
            Some(Subsystem::WorkflowEngine)
        );
    }

    #[test]
    fn subsystem_mapping_returns_none_for_unknown() {
        assert_eq!(subsystem_for_circuit("unknown_circuit"), None);
        assert_eq!(subsystem_for_circuit(""), None);
        assert_eq!(subsystem_for_circuit("caut_cli"), None);
    }

    // --- Full lifecycle test ---

    #[test]
    fn full_lifecycle_closed_open_half_open_closed() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(2, 2, Duration::from_millis(0)));

        // Closed
        assert_eq!(breaker.status().state, CircuitStateKind::Closed);
        assert!(breaker.allow());

        // One failure keeps closed
        breaker.record_failure();
        assert_eq!(breaker.status().state, CircuitStateKind::Closed);
        assert_eq!(breaker.status().consecutive_failures, 1);

        // Second failure opens
        breaker.record_failure();
        assert_eq!(breaker.status().state, CircuitStateKind::Open);
        assert_eq!(breaker.status().consecutive_failures, 2);

        // allow() transitions to half-open (0ms cooldown)
        assert!(breaker.allow());
        assert_eq!(breaker.status().state, CircuitStateKind::HalfOpen);

        // First success in half-open
        breaker.record_success();
        assert_eq!(breaker.status().state, CircuitStateKind::HalfOpen);
        assert_eq!(breaker.status().half_open_successes, Some(1));

        // Second success closes circuit
        breaker.record_success();
        assert_eq!(breaker.status().state, CircuitStateKind::Closed);
        assert_eq!(breaker.status().consecutive_failures, 0);
    }

    #[test]
    fn reopening_from_half_open_then_recovering() {
        let mut breaker =
            CircuitBreaker::new(CircuitBreakerConfig::new(1, 2, Duration::from_millis(0)));

        // Open -> half-open
        breaker.record_failure();
        assert!(breaker.allow());
        assert_eq!(breaker.status().state, CircuitStateKind::HalfOpen);

        // Fail in half-open -> back to open
        breaker.record_failure();
        assert_eq!(breaker.status().state, CircuitStateKind::Open);

        // Recover: open -> half-open -> closed
        assert!(breaker.allow());
        breaker.record_success();
        breaker.record_success();
        assert_eq!(breaker.status().state, CircuitStateKind::Closed);
    }

    // --- with_name constructor ---

    #[test]
    fn with_name_sets_name() {
        let breaker = CircuitBreaker::with_name(
            "my_circuit",
            CircuitBreakerConfig::default(),
        );
        assert_eq!(breaker.name, "my_circuit");
    }

    #[test]
    fn unnamed_constructor_uses_default_name() {
        let breaker = CircuitBreaker::new(CircuitBreakerConfig::default());
        assert_eq!(breaker.name, "unnamed");
    }
}
