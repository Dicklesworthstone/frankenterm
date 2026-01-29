//! Chaos testing harness for fault injection and resilience validation.
//!
//! Provides a [`FaultInjector`] that can be configured to inject failures
//! at specific points in the runtime pipeline.  When the global injector
//! is not initialized (production), the check is a single atomic load —
//! effectively zero overhead.
//!
//! # Design
//!
//! ```text
//! ChaosHarness
//!   ├── FaultInjector (global registry of active faults)
//!   │     ├── FaultPoint::DbWrite → FailNTimes(3)
//!   │     ├── FaultPoint::WeztermCliCall → FailWithProbability(0.5)
//!   │     └── FaultPoint::PatternDetect → AlwaysFail
//!   │
//!   ├── FaultLog (records every trigger for assertions)
//!   │     └── Vec<FaultTrigger { point, mode, timestamp }>
//!   │
//!   └── ChaosScenario (declarative scenario definition)
//!         ├── faults: Vec<(FaultPoint, FaultMode)>
//!         └── assertions: Vec<ChaosAssertion>
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use wa_core::chaos::{FaultInjector, FaultPoint, FaultMode};
//!
//! // Set up fault injection
//! let injector = FaultInjector::init_global();
//! injector.set_fault(FaultPoint::DbWrite, FaultMode::fail_n_times(3, "disk full"));
//!
//! // Check at injection point (production: no-op)
//! if let Err(e) = FaultInjector::check(FaultPoint::DbWrite) {
//!     // Handle injected fault
//! }
//!
//! // Review what happened
//! let log = injector.drain_log();
//! assert_eq!(log.len(), 3);
//!
//! // Clean up
//! FaultInjector::reset_global();
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Global fault injector instance.
static GLOBAL_INJECTOR: OnceLock<Arc<FaultInjector>> = OnceLock::new();

/// Points in the runtime pipeline where faults can be injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultPoint {
    /// WezTerm CLI calls (list_panes, capture, send_text).
    WeztermCliCall,
    /// Database write operations (segment persistence, event recording).
    DbWrite,
    /// Database read operations (queries, search, get_segments).
    DbRead,
    /// Pattern detection engine.
    PatternDetect,
    /// Retention cleanup in maintenance task.
    RetentionCleanup,
    /// Configuration hot-reload.
    ConfigReload,
    /// Webhook/notification delivery.
    WebhookDelivery,
}

impl std::fmt::Display for FaultPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WeztermCliCall => write!(f, "wezterm_cli_call"),
            Self::DbWrite => write!(f, "db_write"),
            Self::DbRead => write!(f, "db_read"),
            Self::PatternDetect => write!(f, "pattern_detect"),
            Self::RetentionCleanup => write!(f, "retention_cleanup"),
            Self::ConfigReload => write!(f, "config_reload"),
            Self::WebhookDelivery => write!(f, "webhook_delivery"),
        }
    }
}

/// How a fault should be injected.
#[derive(Debug, Clone)]
pub enum FaultMode {
    /// Always fail with the given error message.
    AlwaysFail { error: String },
    /// Fail the next N invocations, then succeed.
    FailNTimes { remaining: u32, error: String },
    /// Fail with a given probability (0.0–1.0).
    FailWithProbability { probability: f64, error: String },
    /// Succeed but add a delay (simulates slow I/O).
    Delay { delay_ms: u64 },
    /// Delay then fail.
    DelayThenFail { delay_ms: u64, error: String },
}

impl FaultMode {
    /// Create an always-fail fault.
    #[must_use]
    pub fn always_fail(error: impl Into<String>) -> Self {
        Self::AlwaysFail {
            error: error.into(),
        }
    }

    /// Create a fail-N-times fault.
    #[must_use]
    pub fn fail_n_times(n: u32, error: impl Into<String>) -> Self {
        Self::FailNTimes {
            remaining: n,
            error: error.into(),
        }
    }

    /// Create a probabilistic fault.
    #[must_use]
    pub fn fail_with_probability(probability: f64, error: impl Into<String>) -> Self {
        Self::FailWithProbability {
            probability: probability.clamp(0.0, 1.0),
            error: error.into(),
        }
    }

    /// Create a delay fault (no error, just slowness).
    #[must_use]
    pub fn delay(delay_ms: u64) -> Self {
        Self::Delay { delay_ms }
    }

    /// Create a delay-then-fail fault.
    #[must_use]
    pub fn delay_then_fail(delay_ms: u64, error: impl Into<String>) -> Self {
        Self::DelayThenFail {
            delay_ms,
            error: error.into(),
        }
    }
}

/// Record of a fault injection trigger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultTrigger {
    /// Which point was checked.
    pub point: FaultPoint,
    /// Whether the fault fired (error returned).
    pub fired: bool,
    /// Error message if fired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Timestamp (epoch ms).
    pub timestamp_ms: u64,
}

/// Assertion that can be checked after a chaos scenario.
#[derive(Debug, Clone)]
pub enum ChaosAssertion {
    /// A specific fault point must have fired at least N times.
    FaultFiredAtLeast(FaultPoint, usize),
    /// A specific fault point must NOT have fired.
    FaultNeverFired(FaultPoint),
    /// Total faults fired must be within range.
    TotalFaultsInRange(usize, usize),
}

/// Result of checking a chaos assertion.
#[derive(Debug, Clone)]
pub struct AssertionResult {
    pub assertion: String,
    pub passed: bool,
    pub detail: String,
}

/// A declarative chaos scenario combining faults and assertions.
#[derive(Debug, Clone)]
pub struct ChaosScenario {
    pub name: String,
    pub description: String,
    pub faults: Vec<(FaultPoint, FaultMode)>,
    pub assertions: Vec<ChaosAssertion>,
}

impl ChaosScenario {
    /// Create a new scenario builder.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            faults: Vec::new(),
            assertions: Vec::new(),
        }
    }

    /// Add a fault to the scenario.
    #[must_use]
    pub fn with_fault(mut self, point: FaultPoint, mode: FaultMode) -> Self {
        self.faults.push((point, mode));
        self
    }

    /// Add an assertion to the scenario.
    #[must_use]
    pub fn with_assertion(mut self, assertion: ChaosAssertion) -> Self {
        self.assertions.push(assertion);
        self
    }
}

/// Global fault injector for chaos testing.
///
/// Thread-safe.  All methods use internal locking.
pub struct FaultInjector {
    /// Active faults by injection point.
    faults: RwLock<HashMap<FaultPoint, FaultMode>>,
    /// Log of all fault checks (for assertions).
    log: Mutex<Vec<FaultTrigger>>,
    /// Simple counter for probabilistic faults (deterministic seed).
    counter: Mutex<u64>,
}

impl FaultInjector {
    /// Create a new empty injector.
    #[must_use]
    pub fn new() -> Self {
        Self {
            faults: RwLock::new(HashMap::new()),
            log: Mutex::new(Vec::new()),
            counter: Mutex::new(0),
        }
    }

    /// Initialize the global injector.  Returns a reference.
    ///
    /// Safe to call multiple times; subsequent calls return the
    /// existing instance.
    pub fn init_global() -> Arc<FaultInjector> {
        GLOBAL_INJECTOR
            .get_or_init(|| Arc::new(Self::new()))
            .clone()
    }

    /// Get the global injector, if initialized.
    pub fn global() -> Option<Arc<FaultInjector>> {
        GLOBAL_INJECTOR.get().cloned()
    }

    /// Check a fault point.  Returns `Ok(())` if no fault, or
    /// `Err(Error::Runtime(...))` if a fault fires.
    ///
    /// When the global injector is not initialized, this is a
    /// single failed `OnceLock::get()` — effectively a no-op.
    pub fn check(point: FaultPoint) -> Result<(), Error> {
        if let Some(injector) = GLOBAL_INJECTOR.get() {
            injector.check_point(point)
        } else {
            Ok(())
        }
    }

    /// Reset the global injector (clear all faults and logs).
    ///
    /// The `OnceLock` instance persists, but its internal state is
    /// cleared so subsequent tests start fresh.
    pub fn reset_global() {
        if let Some(injector) = GLOBAL_INJECTOR.get() {
            injector.clear_all();
        }
    }

    /// Set a fault for a specific injection point.
    pub fn set_fault(&self, point: FaultPoint, mode: FaultMode) {
        if let Ok(mut faults) = self.faults.write() {
            faults.insert(point, mode);
        }
    }

    /// Remove a fault for a specific injection point.
    pub fn remove_fault(&self, point: FaultPoint) {
        if let Ok(mut faults) = self.faults.write() {
            faults.remove(&point);
        }
    }

    /// Clear all faults and the trigger log.
    pub fn clear_all(&self) {
        if let Ok(mut faults) = self.faults.write() {
            faults.clear();
        }
        if let Ok(mut log) = self.log.lock() {
            log.clear();
        }
        if let Ok(mut counter) = self.counter.lock() {
            *counter = 0;
        }
    }

    /// Get a copy of the trigger log.
    #[must_use]
    pub fn get_log(&self) -> Vec<FaultTrigger> {
        self.log
            .lock()
            .map(|log| log.clone())
            .unwrap_or_default()
    }

    /// Drain and return the trigger log.
    pub fn drain_log(&self) -> Vec<FaultTrigger> {
        self.log
            .lock()
            .map(|mut log| std::mem::take(&mut *log))
            .unwrap_or_default()
    }

    /// Count how many times a specific fault point fired.
    #[must_use]
    pub fn fired_count(&self, point: FaultPoint) -> usize {
        self.log
            .lock()
            .map(|log| log.iter().filter(|t| t.point == point && t.fired).count())
            .unwrap_or(0)
    }

    /// Count total fault triggers across all points.
    #[must_use]
    pub fn total_fired(&self) -> usize {
        self.log
            .lock()
            .map(|log| log.iter().filter(|t| t.fired).count())
            .unwrap_or(0)
    }

    /// Check a fault point (instance method).
    fn check_point(&self, point: FaultPoint) -> Result<(), Error> {
        let should_fail = self.evaluate_fault(point);

        // Log the check
        let trigger = FaultTrigger {
            point,
            fired: should_fail.is_some(),
            error: should_fail.clone(),
            timestamp_ms: epoch_ms(),
        };
        if let Ok(mut log) = self.log.lock() {
            log.push(trigger);
        }

        match should_fail {
            Some(error_msg) => Err(Error::Runtime(format!(
                "chaos fault injected at {point}: {error_msg}"
            ))),
            None => Ok(()),
        }
    }

    /// Evaluate whether a fault should fire.  Returns the error
    /// message if it should, or `None` if the call should succeed.
    fn evaluate_fault(&self, point: FaultPoint) -> Option<String> {
        let mut faults = self.faults.write().ok()?;
        let mode = faults.get_mut(&point)?;

        match mode {
            FaultMode::AlwaysFail { error } => Some(error.clone()),

            FaultMode::FailNTimes { remaining, error } => {
                if *remaining > 0 {
                    *remaining -= 1;
                    Some(error.clone())
                } else {
                    // Exhausted — remove the fault
                    let _ = faults.remove(&point);
                    None
                }
            }

            FaultMode::FailWithProbability { probability, error } => {
                // Deterministic pseudo-random based on counter
                let counter = self
                    .counter
                    .lock()
                    .map(|mut c| {
                        *c = c.wrapping_add(1);
                        *c
                    })
                    .unwrap_or(0);

                // Simple hash to get a value in [0, 1)
                let hash = ((counter.wrapping_mul(6364136223846793005))
                    .wrapping_add(1442695040888963407))
                    >> 32;
                let rand_val = (hash as f64) / (u32::MAX as f64);

                if rand_val < *probability {
                    Some(error.clone())
                } else {
                    None
                }
            }

            FaultMode::Delay { delay_ms } => {
                // Block the current thread for the specified duration.
                // In async contexts this is intentionally blocking to
                // simulate slow I/O at the syscall level.
                std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
                None
            }

            FaultMode::DelayThenFail { delay_ms, error } => {
                std::thread::sleep(std::time::Duration::from_millis(*delay_ms));
                Some(error.clone())
            }
        }
    }

    /// Apply a scenario's faults to this injector.
    pub fn apply_scenario(&self, scenario: &ChaosScenario) {
        self.clear_all();
        for (point, mode) in &scenario.faults {
            self.set_fault(*point, mode.clone());
        }
    }

    /// Check a scenario's assertions against the current log.
    #[must_use]
    pub fn check_assertions(&self, scenario: &ChaosScenario) -> Vec<AssertionResult> {
        let log = self.get_log();
        let mut results = Vec::new();

        for assertion in &scenario.assertions {
            let result = match assertion {
                ChaosAssertion::FaultFiredAtLeast(point, min_count) => {
                    let actual = log.iter().filter(|t| t.point == *point && t.fired).count();
                    AssertionResult {
                        assertion: format!("{point} fired >= {min_count} times"),
                        passed: actual >= *min_count,
                        detail: format!("actual: {actual}"),
                    }
                }

                ChaosAssertion::FaultNeverFired(point) => {
                    let actual = log.iter().filter(|t| t.point == *point && t.fired).count();
                    AssertionResult {
                        assertion: format!("{point} never fired"),
                        passed: actual == 0,
                        detail: format!("actual: {actual}"),
                    }
                }

                ChaosAssertion::TotalFaultsInRange(min, max) => {
                    let total = log.iter().filter(|t| t.fired).count();
                    AssertionResult {
                        assertion: format!("total faults in [{min}, {max}]"),
                        passed: total >= *min && total <= *max,
                        detail: format!("actual: {total}"),
                    }
                }
            };
            results.push(result);
        }

        results
    }
}

impl Default for FaultInjector {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary of a chaos test run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChaosReport {
    pub scenario_name: String,
    pub total_checks: usize,
    pub total_faults_fired: usize,
    pub faults_by_point: HashMap<String, usize>,
    pub assertions_passed: usize,
    pub assertions_failed: usize,
    pub all_passed: bool,
}

impl ChaosReport {
    /// Build a report from a scenario and injector state.
    #[must_use]
    pub fn from_scenario(injector: &FaultInjector, scenario: &ChaosScenario) -> Self {
        let log = injector.get_log();
        let assertion_results = injector.check_assertions(scenario);

        let mut faults_by_point: HashMap<String, usize> = HashMap::new();
        for trigger in &log {
            if trigger.fired {
                *faults_by_point
                    .entry(trigger.point.to_string())
                    .or_insert(0) += 1;
            }
        }

        Self {
            scenario_name: scenario.name.clone(),
            total_checks: log.len(),
            total_faults_fired: log.iter().filter(|t| t.fired).count(),
            faults_by_point,
            assertions_passed: assertion_results.iter().filter(|r| r.passed).count(),
            assertions_failed: assertion_results.iter().filter(|r| !r.passed).count(),
            all_passed: assertion_results.iter().all(|r| r.passed),
        }
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

// ── Pre-built scenarios ─────────────────────────────────────────────

/// Pre-built chaos scenarios for common resilience tests.
pub mod scenarios {
    use super::*;

    /// Scenario 1: Database writes fail repeatedly.
    ///
    /// Validates that the system queues writes and continues
    /// observing when the database becomes unavailable.
    #[must_use]
    pub fn db_write_failure() -> ChaosScenario {
        ChaosScenario::new(
            "db_write_failure",
            "Database write operations fail 5 times, then recover",
        )
        .with_fault(FaultPoint::DbWrite, FaultMode::fail_n_times(5, "disk full"))
        .with_assertion(ChaosAssertion::FaultFiredAtLeast(FaultPoint::DbWrite, 5))
    }

    /// Scenario 2: WezTerm CLI becomes unavailable.
    ///
    /// Validates that the system degrades gracefully when WezTerm
    /// CLI calls fail and recovers when it comes back.
    #[must_use]
    pub fn wezterm_unavailable() -> ChaosScenario {
        ChaosScenario::new(
            "wezterm_unavailable",
            "WezTerm CLI calls fail 3 times, simulating process crash",
        )
        .with_fault(
            FaultPoint::WeztermCliCall,
            FaultMode::fail_n_times(3, "wezterm not running"),
        )
        .with_assertion(ChaosAssertion::FaultFiredAtLeast(
            FaultPoint::WeztermCliCall,
            3,
        ))
    }

    /// Scenario 3: Pattern detection engine errors.
    ///
    /// Validates that the ingest pipeline continues when the
    /// pattern engine fails on specific inputs.
    #[must_use]
    pub fn pattern_engine_failure() -> ChaosScenario {
        ChaosScenario::new(
            "pattern_engine_failure",
            "Pattern detection fails, system skips detection and continues ingesting",
        )
        .with_fault(
            FaultPoint::PatternDetect,
            FaultMode::always_fail("regex timeout"),
        )
        .with_assertion(ChaosAssertion::FaultFiredAtLeast(
            FaultPoint::PatternDetect,
            1,
        ))
        .with_assertion(ChaosAssertion::FaultNeverFired(FaultPoint::DbWrite))
    }

    /// Scenario 4: Database corruption (read failures).
    ///
    /// Validates that the system handles read failures gracefully
    /// without cascading into write failures.
    #[must_use]
    pub fn db_corruption() -> ChaosScenario {
        ChaosScenario::new(
            "db_corruption",
            "Database reads fail, simulating file corruption",
        )
        .with_fault(FaultPoint::DbRead, FaultMode::always_fail("database disk image is malformed"))
        .with_assertion(ChaosAssertion::FaultFiredAtLeast(FaultPoint::DbRead, 1))
    }

    /// Scenario 5: Maintenance cleanup fails under load.
    ///
    /// Validates that retention cleanup failures don't crash the
    /// system or corrupt the database.
    #[must_use]
    pub fn maintenance_failure() -> ChaosScenario {
        ChaosScenario::new(
            "maintenance_failure",
            "Retention cleanup fails, system continues without pruning",
        )
        .with_fault(
            FaultPoint::RetentionCleanup,
            FaultMode::fail_n_times(3, "database is locked"),
        )
        .with_assertion(ChaosAssertion::FaultFiredAtLeast(
            FaultPoint::RetentionCleanup,
            3,
        ))
        .with_assertion(ChaosAssertion::FaultNeverFired(FaultPoint::DbWrite))
    }

    /// Scenario 6: Multiple simultaneous failures.
    ///
    /// Validates that the system handles multiple subsystem failures
    /// concurrently without deadlocking or crashing.
    #[must_use]
    pub fn cascading_failures() -> ChaosScenario {
        ChaosScenario::new(
            "cascading_failures",
            "DB writes and WezTerm CLI fail simultaneously",
        )
        .with_fault(FaultPoint::DbWrite, FaultMode::fail_n_times(3, "disk full"))
        .with_fault(
            FaultPoint::WeztermCliCall,
            FaultMode::fail_n_times(2, "wezterm crashed"),
        )
        .with_assertion(ChaosAssertion::FaultFiredAtLeast(FaultPoint::DbWrite, 3))
        .with_assertion(ChaosAssertion::FaultFiredAtLeast(
            FaultPoint::WeztermCliCall,
            2,
        ))
        .with_assertion(ChaosAssertion::TotalFaultsInRange(5, 10))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() {
        // Ensure global injector exists and is clean
        FaultInjector::init_global();
        FaultInjector::reset_global();
    }

    #[test]
    fn injector_no_faults_succeeds() {
        let injector = FaultInjector::new();
        assert!(injector.check_point(FaultPoint::DbWrite).is_ok());
        assert!(injector.check_point(FaultPoint::WeztermCliCall).is_ok());
    }

    #[test]
    fn always_fail_mode() {
        let injector = FaultInjector::new();
        injector.set_fault(FaultPoint::DbWrite, FaultMode::always_fail("disk full"));

        assert!(injector.check_point(FaultPoint::DbWrite).is_err());
        assert!(injector.check_point(FaultPoint::DbWrite).is_err());
        assert!(injector.check_point(FaultPoint::DbWrite).is_err());
        assert_eq!(injector.fired_count(FaultPoint::DbWrite), 3);
    }

    #[test]
    fn fail_n_times_mode() {
        let injector = FaultInjector::new();
        injector.set_fault(FaultPoint::DbWrite, FaultMode::fail_n_times(2, "locked"));

        assert!(injector.check_point(FaultPoint::DbWrite).is_err());
        assert!(injector.check_point(FaultPoint::DbWrite).is_err());
        // Third call should succeed (exhausted)
        assert!(injector.check_point(FaultPoint::DbWrite).is_ok());
        assert!(injector.check_point(FaultPoint::DbWrite).is_ok());

        assert_eq!(injector.fired_count(FaultPoint::DbWrite), 2);
    }

    #[test]
    fn fail_with_probability_deterministic() {
        let injector = FaultInjector::new();
        injector.set_fault(
            FaultPoint::DbWrite,
            FaultMode::fail_with_probability(0.5, "random failure"),
        );

        // Run multiple times — some should fail, some should succeed
        let mut failures = 0;
        let mut successes = 0;
        for _ in 0..20 {
            if injector.check_point(FaultPoint::DbWrite).is_err() {
                failures += 1;
            } else {
                successes += 1;
            }
        }

        // With 50% probability over 20 trials, expect both to be > 0
        assert!(failures > 0, "expected some failures, got 0");
        assert!(successes > 0, "expected some successes, got 0");
    }

    #[test]
    fn independent_fault_points() {
        let injector = FaultInjector::new();
        injector.set_fault(FaultPoint::DbWrite, FaultMode::always_fail("disk full"));

        // DbWrite should fail
        assert!(injector.check_point(FaultPoint::DbWrite).is_err());
        // Other points should succeed
        assert!(injector.check_point(FaultPoint::DbRead).is_ok());
        assert!(injector.check_point(FaultPoint::WeztermCliCall).is_ok());
        assert!(injector.check_point(FaultPoint::PatternDetect).is_ok());
    }

    #[test]
    fn remove_fault() {
        let injector = FaultInjector::new();
        injector.set_fault(FaultPoint::DbWrite, FaultMode::always_fail("error"));

        assert!(injector.check_point(FaultPoint::DbWrite).is_err());
        injector.remove_fault(FaultPoint::DbWrite);
        assert!(injector.check_point(FaultPoint::DbWrite).is_ok());
    }

    #[test]
    fn clear_all() {
        let injector = FaultInjector::new();
        injector.set_fault(FaultPoint::DbWrite, FaultMode::always_fail("error"));
        injector.set_fault(
            FaultPoint::WeztermCliCall,
            FaultMode::always_fail("error"),
        );

        let _ = injector.check_point(FaultPoint::DbWrite);
        assert_eq!(injector.get_log().len(), 1);

        injector.clear_all();

        assert!(injector.check_point(FaultPoint::DbWrite).is_ok());
        assert!(injector.check_point(FaultPoint::WeztermCliCall).is_ok());
        assert_eq!(injector.get_log().len(), 2); // new log after clear
    }

    #[test]
    fn drain_log() {
        let injector = FaultInjector::new();
        injector.set_fault(FaultPoint::DbWrite, FaultMode::always_fail("error"));
        let _ = injector.check_point(FaultPoint::DbWrite);
        let _ = injector.check_point(FaultPoint::DbWrite);

        let log = injector.drain_log();
        assert_eq!(log.len(), 2);
        assert!(log[0].fired);
        assert!(log[1].fired);

        // Log should be empty after drain
        assert!(injector.get_log().is_empty());
    }

    #[test]
    fn fault_trigger_serialization() {
        let trigger = FaultTrigger {
            point: FaultPoint::DbWrite,
            fired: true,
            error: Some("disk full".to_string()),
            timestamp_ms: 1_234_567_890,
        };

        let json = serde_json::to_string(&trigger).unwrap();
        let parsed: FaultTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.point, FaultPoint::DbWrite);
        assert!(parsed.fired);
        assert_eq!(parsed.error.as_deref(), Some("disk full"));
    }

    #[test]
    fn scenario_db_write_failure() {
        let injector = FaultInjector::new();
        let scenario = scenarios::db_write_failure();
        injector.apply_scenario(&scenario);

        // Simulate 7 write attempts
        for _ in 0..7 {
            let _ = injector.check_point(FaultPoint::DbWrite);
        }

        let results = injector.check_assertions(&scenario);
        assert!(
            results.iter().all(|r| r.passed),
            "assertions: {results:?}"
        );

        let report = ChaosReport::from_scenario(&injector, &scenario);
        assert!(report.all_passed);
        assert_eq!(report.total_faults_fired, 5);
    }

    #[test]
    fn scenario_wezterm_unavailable() {
        let injector = FaultInjector::new();
        let scenario = scenarios::wezterm_unavailable();
        injector.apply_scenario(&scenario);

        // Simulate 5 CLI calls
        for _ in 0..5 {
            let _ = injector.check_point(FaultPoint::WeztermCliCall);
        }

        let results = injector.check_assertions(&scenario);
        assert!(
            results.iter().all(|r| r.passed),
            "assertions: {results:?}"
        );
    }

    #[test]
    fn scenario_pattern_engine_failure() {
        let injector = FaultInjector::new();
        let scenario = scenarios::pattern_engine_failure();
        injector.apply_scenario(&scenario);

        // Pattern detection fails
        assert!(injector.check_point(FaultPoint::PatternDetect).is_err());
        // But DB writes succeed (independent)
        assert!(injector.check_point(FaultPoint::DbWrite).is_ok());

        let results = injector.check_assertions(&scenario);
        assert!(
            results.iter().all(|r| r.passed),
            "assertions: {results:?}"
        );
    }

    #[test]
    fn scenario_cascading_failures() {
        let injector = FaultInjector::new();
        let scenario = scenarios::cascading_failures();
        injector.apply_scenario(&scenario);

        // Interleave DB and CLI failures
        for _ in 0..5 {
            let _ = injector.check_point(FaultPoint::DbWrite);
            let _ = injector.check_point(FaultPoint::WeztermCliCall);
        }

        let results = injector.check_assertions(&scenario);
        assert!(
            results.iter().all(|r| r.passed),
            "assertions: {results:?}"
        );

        let report = ChaosReport::from_scenario(&injector, &scenario);
        assert!(report.all_passed);
        assert_eq!(report.total_faults_fired, 5); // 3 DB + 2 CLI
    }

    #[test]
    fn report_serialization() {
        let injector = FaultInjector::new();
        let scenario = scenarios::db_write_failure();
        injector.apply_scenario(&scenario);

        for _ in 0..7 {
            let _ = injector.check_point(FaultPoint::DbWrite);
        }

        let report = ChaosReport::from_scenario(&injector, &scenario);
        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: ChaosReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.scenario_name, "db_write_failure");
        assert!(parsed.all_passed);
    }

    #[test]
    fn global_injector_check_without_init() {
        // Static check — without init, should succeed
        // Note: other tests may have called init_global, so we just
        // verify the API doesn't panic.
        let _ = FaultInjector::check(FaultPoint::DbWrite);
    }

    #[test]
    fn global_injector_lifecycle() {
        setup();
        let injector = FaultInjector::init_global();

        injector.set_fault(FaultPoint::DbWrite, FaultMode::fail_n_times(1, "test"));
        assert!(FaultInjector::check(FaultPoint::DbWrite).is_err());
        assert!(FaultInjector::check(FaultPoint::DbWrite).is_ok());

        FaultInjector::reset_global();
        assert!(FaultInjector::check(FaultPoint::DbWrite).is_ok());
    }

    #[test]
    fn fault_point_display() {
        assert_eq!(FaultPoint::WeztermCliCall.to_string(), "wezterm_cli_call");
        assert_eq!(FaultPoint::DbWrite.to_string(), "db_write");
        assert_eq!(FaultPoint::DbRead.to_string(), "db_read");
        assert_eq!(FaultPoint::PatternDetect.to_string(), "pattern_detect");
        assert_eq!(
            FaultPoint::RetentionCleanup.to_string(),
            "retention_cleanup"
        );
        assert_eq!(FaultPoint::ConfigReload.to_string(), "config_reload");
        assert_eq!(
            FaultPoint::WebhookDelivery.to_string(),
            "webhook_delivery"
        );
    }

    #[test]
    fn assertion_fault_never_fired_passes_when_clean() {
        let injector = FaultInjector::new();
        let scenario = ChaosScenario::new("test", "test")
            .with_assertion(ChaosAssertion::FaultNeverFired(FaultPoint::DbWrite));

        // Don't trigger any faults
        let _ = injector.check_point(FaultPoint::DbRead);

        let results = injector.check_assertions(&scenario);
        assert!(results[0].passed);
    }

    #[test]
    fn assertion_fault_never_fired_fails_when_fired() {
        let injector = FaultInjector::new();
        injector.set_fault(FaultPoint::DbWrite, FaultMode::always_fail("err"));
        let scenario = ChaosScenario::new("test", "test")
            .with_assertion(ChaosAssertion::FaultNeverFired(FaultPoint::DbWrite));

        let _ = injector.check_point(FaultPoint::DbWrite);

        let results = injector.check_assertions(&scenario);
        assert!(!results[0].passed);
    }

    #[test]
    fn total_faults_in_range_assertion() {
        let injector = FaultInjector::new();
        injector.set_fault(FaultPoint::DbWrite, FaultMode::fail_n_times(3, "err"));

        let scenario = ChaosScenario::new("test", "test")
            .with_assertion(ChaosAssertion::TotalFaultsInRange(2, 4));

        for _ in 0..5 {
            let _ = injector.check_point(FaultPoint::DbWrite);
        }

        let results = injector.check_assertions(&scenario);
        assert!(results[0].passed, "3 faults should be in [2, 4]");
    }
}
