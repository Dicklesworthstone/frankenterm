//! Simulation guardrails and fail-closed safety controls (ft-og6q6.4.4).
//!
//! Provides:
//! - [`SimulationGuard`] — Thread-local simulation flag with leak detection.
//! - [`ResourceLimits`] — Configurable event/time/memory/concurrency caps.
//! - [`ResourceTracker`] — Runtime enforcement of resource limits.
//! - [`WatchdogConfig`] — Stall detection and force-termination policy.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// ============================================================================
// SimulationGuard — thread-local simulation flag
// ============================================================================

thread_local! {
    static SIMULATION_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII guard that sets the simulation flag on creation and clears on drop.
pub struct SimulationGuard {
    _private: (),
}

impl SimulationGuard {
    /// Enter simulation mode. Panics if already in simulation mode.
    #[must_use]
    pub fn enter() -> Self {
        SIMULATION_ACTIVE.with(|f| {
            assert!(
                !f.get(),
                "SimulationGuard::enter called while already in simulation mode"
            );
            f.set(true);
        });
        Self { _private: () }
    }

    /// Check if simulation mode is currently active (thread-local).
    #[must_use]
    pub fn is_active() -> bool {
        SIMULATION_ACTIVE.with(|f| f.get())
    }

    /// Assert that we are NOT in simulation mode. Use in live side-effect code.
    ///
    /// Panics with a clear message if simulation mode is active.
    pub fn assert_not_simulating(operation: &str) {
        assert!(
            !Self::is_active(),
            "SIMULATION SAFETY VIOLATION: attempted live operation '{}' \
             during counterfactual simulation. This indicates a barrier \
             leak — all side effects must go through SideEffectBarrier.",
            operation
        );
    }
}

impl Drop for SimulationGuard {
    fn drop(&mut self) {
        SIMULATION_ACTIVE.with(|f| {
            f.set(false);
        });
    }
}

// ============================================================================
// ResourceLimits — configurable caps
// ============================================================================

/// Configurable resource limits for replay/simulation runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Maximum events to process before halting.
    pub max_events: u64,
    /// Maximum wall-clock time in milliseconds.
    pub max_wall_clock_ms: u64,
    /// Approximate memory warning threshold (event count heuristic).
    pub memory_warning_events: u64,
    /// Maximum concurrent replays allowed.
    pub max_concurrent: u32,
    /// Watchdog timeout: force-terminate if no progress for this many ms.
    pub watchdog_timeout_ms: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_events: 1_000_000,
            max_wall_clock_ms: 30 * 60 * 1000, // 30 minutes
            memory_warning_events: 500_000,
            max_concurrent: 4,
            watchdog_timeout_ms: 60_000, // 1 minute
        }
    }
}

impl ResourceLimits {
    /// Load from TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self, String> {
        toml::from_str(toml_str).map_err(|e| format!("resource limits parse error: {e}"))
    }
}

// ============================================================================
// LimitViolation — what limit was exceeded
// ============================================================================

/// Which resource limit was violated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitViolation {
    /// Exceeded max_events.
    MaxEvents { limit: u64, actual: u64 },
    /// Exceeded max_wall_clock_ms.
    MaxWallClock { limit_ms: u64, elapsed_ms: u64 },
    /// Memory warning threshold reached.
    MemoryWarning { threshold: u64, current: u64 },
    /// Max concurrent replays exceeded.
    MaxConcurrent { limit: u32, current: u32 },
    /// Watchdog detected stall (no progress).
    WatchdogTimeout { timeout_ms: u64, stall_ms: u64 },
    /// Simulation barrier leak detected.
    BarrierLeak { operation: String },
}

impl std::fmt::Display for LimitViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MaxEvents { limit, actual } => {
                write!(f, "max events exceeded: {actual}/{limit}")
            }
            Self::MaxWallClock {
                limit_ms,
                elapsed_ms,
            } => write!(f, "max wall clock exceeded: {elapsed_ms}ms/{limit_ms}ms"),
            Self::MemoryWarning { threshold, current } => {
                write!(
                    f,
                    "memory warning: {current} events (threshold {threshold})"
                )
            }
            Self::MaxConcurrent { limit, current } => {
                write!(f, "max concurrent exceeded: {current}/{limit}")
            }
            Self::WatchdogTimeout {
                timeout_ms,
                stall_ms,
            } => write!(
                f,
                "watchdog timeout: no progress for {stall_ms}ms (timeout {timeout_ms}ms)"
            ),
            Self::BarrierLeak { operation } => {
                write!(f, "simulation barrier leak: {operation}")
            }
        }
    }
}

// ============================================================================
// ResourceTracker — runtime limit enforcement
// ============================================================================

/// Tracks resource usage and enforces limits during a replay/simulation run.
pub struct ResourceTracker {
    limits: ResourceLimits,
    inner: Mutex<TrackerInner>,
    /// Atomic event counter for lock-free fast path.
    event_count: AtomicU64,
    /// Whether a memory warning has been issued.
    memory_warned: AtomicBool,
}

struct TrackerInner {
    /// Start time for wall-clock tracking (ms since epoch).
    start_wall_ms: u64,
    /// Last progress timestamp (ms since epoch).
    last_progress_ms: u64,
    /// Violations detected.
    violations: Vec<LimitViolation>,
    /// Whether the run has been halted.
    halted: bool,
}

/// Result of checking resource limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    /// Within limits, continue.
    Ok,
    /// Warning issued but can continue.
    Warning(LimitViolation),
    /// Limit exceeded, must halt.
    Halt(LimitViolation),
}

impl ResourceTracker {
    /// Create a new tracker with the given limits.
    #[must_use]
    pub fn new(limits: ResourceLimits, start_wall_ms: u64) -> Self {
        Self {
            limits,
            inner: Mutex::new(TrackerInner {
                start_wall_ms,
                last_progress_ms: start_wall_ms,
                violations: Vec::new(),
                halted: false,
            }),
            event_count: AtomicU64::new(0),
            memory_warned: AtomicBool::new(false),
        }
    }

    /// Record an event and check limits.
    pub fn record_event(&self, current_wall_ms: u64) -> CheckResult {
        let count = self.event_count.fetch_add(1, Ordering::Relaxed) + 1;

        // Fast path: check event limit.
        if self.limits.max_events > 0 && count > self.limits.max_events {
            let violation = LimitViolation::MaxEvents {
                limit: self.limits.max_events,
                actual: count,
            };
            self.record_violation(violation.clone());
            return CheckResult::Halt(violation);
        }

        // Memory warning (once).
        if self.limits.memory_warning_events > 0
            && count >= self.limits.memory_warning_events
            && !self.memory_warned.swap(true, Ordering::Relaxed)
        {
            let warning = LimitViolation::MemoryWarning {
                threshold: self.limits.memory_warning_events,
                current: count,
            };
            self.record_violation(warning.clone());
            // Update progress.
            self.update_progress(current_wall_ms);
            return CheckResult::Warning(warning);
        }

        // Wall-clock check.
        let mut inner = self.inner.lock().unwrap();
        if self.limits.max_wall_clock_ms > 0 {
            let elapsed = current_wall_ms.saturating_sub(inner.start_wall_ms);
            if elapsed > self.limits.max_wall_clock_ms {
                let violation = LimitViolation::MaxWallClock {
                    limit_ms: self.limits.max_wall_clock_ms,
                    elapsed_ms: elapsed,
                };
                inner.violations.push(violation.clone());
                inner.halted = true;
                return CheckResult::Halt(violation);
            }
        }

        // Update progress.
        inner.last_progress_ms = current_wall_ms;

        CheckResult::Ok
    }

    /// Check watchdog: has there been progress recently?
    pub fn check_watchdog(&self, current_wall_ms: u64) -> CheckResult {
        let inner = self.inner.lock().unwrap();
        if self.limits.watchdog_timeout_ms == 0 {
            return CheckResult::Ok;
        }
        let stall = current_wall_ms.saturating_sub(inner.last_progress_ms);
        if stall > self.limits.watchdog_timeout_ms {
            let violation = LimitViolation::WatchdogTimeout {
                timeout_ms: self.limits.watchdog_timeout_ms,
                stall_ms: stall,
            };
            return CheckResult::Halt(violation);
        }
        CheckResult::Ok
    }

    /// Get current event count.
    #[must_use]
    pub fn event_count(&self) -> u64 {
        self.event_count.load(Ordering::Relaxed)
    }

    /// Whether the tracker has halted the run.
    #[must_use]
    pub fn is_halted(&self) -> bool {
        self.inner.lock().unwrap().halted
    }

    /// Get all violations.
    #[must_use]
    pub fn violations(&self) -> Vec<LimitViolation> {
        self.inner.lock().unwrap().violations.clone()
    }

    fn record_violation(&self, violation: LimitViolation) {
        let mut inner = self.inner.lock().unwrap();
        inner.violations.push(violation);
        inner.halted = true;
    }

    fn update_progress(&self, current_wall_ms: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.last_progress_ms = current_wall_ms;
    }
}

// ============================================================================
// ConcurrencyGate — limits concurrent replays
// ============================================================================

/// Tracks concurrent replay count and enforces the limit.
#[derive(Debug)]
pub struct ConcurrencyGate {
    max_concurrent: u32,
    current: AtomicU64,
}

/// RAII token that decrements concurrency count on drop.
#[derive(Debug)]
pub struct ConcurrencyToken<'a> {
    gate: &'a ConcurrencyGate,
}

impl Drop for ConcurrencyToken<'_> {
    fn drop(&mut self) {
        self.gate.current.fetch_sub(1, Ordering::Relaxed);
    }
}

impl ConcurrencyGate {
    /// Create with max concurrent limit.
    #[must_use]
    pub fn new(max_concurrent: u32) -> Self {
        Self {
            max_concurrent,
            current: AtomicU64::new(0),
        }
    }

    /// Try to acquire a concurrency slot.
    pub fn try_acquire(&self) -> Result<ConcurrencyToken<'_>, LimitViolation> {
        let current = self.current.fetch_add(1, Ordering::Relaxed) + 1;
        if current > self.max_concurrent as u64 {
            self.current.fetch_sub(1, Ordering::Relaxed);
            return Err(LimitViolation::MaxConcurrent {
                limit: self.max_concurrent,
                current: current as u32,
            });
        }
        Ok(ConcurrencyToken { gate: self })
    }

    /// Current count.
    #[must_use]
    pub fn current(&self) -> u32 {
        self.current.load(Ordering::Relaxed) as u32
    }
}

// ============================================================================
// GuardrailReport — summary of all safety checks
// ============================================================================

/// Summary of guardrail checks for a replay run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardrailReport {
    /// Events processed.
    pub events_processed: u64,
    /// Violations encountered.
    pub violations: Vec<LimitViolation>,
    /// Whether the run was halted by a guardrail.
    pub halted_by_guardrail: bool,
    /// Whether cleanup completed successfully.
    pub cleanup_complete: bool,
}

impl GuardrailReport {
    /// Create from a tracker.
    #[must_use]
    pub fn from_tracker(tracker: &ResourceTracker, cleanup_complete: bool) -> Self {
        Self {
            events_processed: tracker.event_count(),
            violations: tracker.violations(),
            halted_by_guardrail: tracker.is_halted(),
            cleanup_complete,
        }
    }

    /// Whether the run completed safely.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        self.violations.is_empty() || (!self.halted_by_guardrail && self.cleanup_complete)
    }

    /// Export as JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── SimulationGuard ─────────────────────────────────────────────────

    #[test]
    fn guard_sets_and_clears_flag() {
        assert!(!SimulationGuard::is_active());
        {
            let _guard = SimulationGuard::enter();
            assert!(SimulationGuard::is_active());
        }
        assert!(!SimulationGuard::is_active());
    }

    #[test]
    #[should_panic(expected = "SIMULATION SAFETY VIOLATION")]
    fn guard_panics_on_live_operation() {
        let _guard = SimulationGuard::enter();
        SimulationGuard::assert_not_simulating("write_to_pane");
    }

    #[test]
    fn assert_not_simulating_ok_outside() {
        SimulationGuard::assert_not_simulating("write_to_pane");
        // Should not panic.
    }

    #[test]
    #[should_panic(expected = "already in simulation mode")]
    fn guard_double_enter_panics() {
        let _g1 = SimulationGuard::enter();
        let _g2 = SimulationGuard::enter(); // Should panic.
    }

    // ── ResourceLimits ──────────────────────────────────────────────────

    #[test]
    fn default_limits_sensible() {
        let limits = ResourceLimits::default();
        assert_eq!(limits.max_events, 1_000_000);
        assert_eq!(limits.max_wall_clock_ms, 30 * 60 * 1000);
        assert_eq!(limits.max_concurrent, 4);
        assert_eq!(limits.watchdog_timeout_ms, 60_000);
    }

    #[test]
    fn limits_from_toml() {
        let toml = r#"
max_events = 500
max_wall_clock_ms = 10000
memory_warning_events = 200
max_concurrent = 2
watchdog_timeout_ms = 5000
"#;
        let limits = ResourceLimits::from_toml(toml).unwrap();
        assert_eq!(limits.max_events, 500);
        assert_eq!(limits.max_concurrent, 2);
    }

    #[test]
    fn limits_serde_roundtrip() {
        let limits = ResourceLimits::default();
        let json = serde_json::to_string(&limits).unwrap();
        let restored: ResourceLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.max_events, limits.max_events);
    }

    // ── ResourceTracker ─────────────────────────────────────────────────

    #[test]
    fn tracker_events_within_limit() {
        let limits = ResourceLimits {
            max_events: 10,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..10u64 {
            let result = tracker.record_event(i);
            assert_eq!(result, CheckResult::Ok);
        }
        assert_eq!(tracker.event_count(), 10);
    }

    #[test]
    fn tracker_halts_at_event_limit() {
        let limits = ResourceLimits {
            max_events: 5,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..5u64 {
            tracker.record_event(i);
        }
        let result = tracker.record_event(5);
        let is_halt = matches!(result, CheckResult::Halt(LimitViolation::MaxEvents { .. }));
        assert!(is_halt);
        assert!(tracker.is_halted());
    }

    #[test]
    fn tracker_wall_clock_halt() {
        let limits = ResourceLimits {
            max_events: 0,
            max_wall_clock_ms: 1000,
            memory_warning_events: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        // Within limit.
        assert_eq!(tracker.record_event(500), CheckResult::Ok);
        // Exceed limit.
        let result = tracker.record_event(1500);
        let is_halt = matches!(
            result,
            CheckResult::Halt(LimitViolation::MaxWallClock { .. })
        );
        assert!(is_halt);
    }

    #[test]
    fn tracker_memory_warning() {
        let limits = ResourceLimits {
            max_events: 0,
            max_wall_clock_ms: 0,
            memory_warning_events: 5,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..4u64 {
            assert_eq!(tracker.record_event(i), CheckResult::Ok);
        }
        // 5th event triggers warning.
        let result = tracker.record_event(4);
        let is_warning = matches!(
            result,
            CheckResult::Warning(LimitViolation::MemoryWarning { .. })
        );
        assert!(is_warning);
        // Subsequent events don't re-warn.
        assert_eq!(tracker.record_event(5), CheckResult::Ok);
    }

    #[test]
    fn tracker_watchdog_detects_stall() {
        let limits = ResourceLimits {
            max_events: 0,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            watchdog_timeout_ms: 1000,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        tracker.record_event(100); // progress at 100ms.
        // Check at 200ms: not stalled.
        assert_eq!(tracker.check_watchdog(200), CheckResult::Ok);
        // Check at 1200ms: stalled.
        let result = tracker.check_watchdog(1200);
        let is_halt = matches!(
            result,
            CheckResult::Halt(LimitViolation::WatchdogTimeout { .. })
        );
        assert!(is_halt);
    }

    #[test]
    fn tracker_violations_accumulate() {
        let limits = ResourceLimits {
            max_events: 3,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..5u64 {
            tracker.record_event(i);
        }
        let violations = tracker.violations();
        assert!(!violations.is_empty());
    }

    // ── ConcurrencyGate ─────────────────────────────────────────────────

    #[test]
    fn concurrency_gate_allows_within_limit() {
        let gate = ConcurrencyGate::new(2);
        let t1 = gate.try_acquire().unwrap();
        let t2 = gate.try_acquire().unwrap();
        assert_eq!(gate.current(), 2);
        drop(t1);
        assert_eq!(gate.current(), 1);
        drop(t2);
        assert_eq!(gate.current(), 0);
    }

    #[test]
    fn concurrency_gate_rejects_excess() {
        let gate = ConcurrencyGate::new(1);
        let _t1 = gate.try_acquire().unwrap();
        let result = gate.try_acquire();
        assert!(result.is_err());
        let is_concurrent = matches!(result.unwrap_err(), LimitViolation::MaxConcurrent { .. });
        assert!(is_concurrent);
    }

    #[test]
    fn concurrency_gate_releases_on_drop() {
        let gate = ConcurrencyGate::new(1);
        {
            let _t = gate.try_acquire().unwrap();
            assert_eq!(gate.current(), 1);
        }
        assert_eq!(gate.current(), 0);
        // Can acquire again.
        let _t2 = gate.try_acquire().unwrap();
        assert_eq!(gate.current(), 1);
    }

    // ── GuardrailReport ─────────────────────────────────────────────────

    #[test]
    fn report_from_clean_tracker() {
        let limits = ResourceLimits::default();
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..10u64 {
            tracker.record_event(i);
        }
        let report = GuardrailReport::from_tracker(&tracker, true);
        assert_eq!(report.events_processed, 10);
        assert!(report.violations.is_empty());
        assert!(!report.halted_by_guardrail);
        assert!(report.cleanup_complete);
        assert!(report.is_safe());
    }

    #[test]
    fn report_from_halted_tracker() {
        let limits = ResourceLimits {
            max_events: 3,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..5u64 {
            tracker.record_event(i);
        }
        let report = GuardrailReport::from_tracker(&tracker, true);
        assert!(report.halted_by_guardrail);
        assert!(!report.violations.is_empty());
    }

    #[test]
    fn report_json_roundtrip() {
        let limits = ResourceLimits::default();
        let tracker = ResourceTracker::new(limits, 0);
        tracker.record_event(0);
        let report = GuardrailReport::from_tracker(&tracker, true);
        let json = report.to_json();
        let restored: GuardrailReport = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.events_processed, report.events_processed);
    }

    // ── LimitViolation display ──────────────────────────────────────────

    #[test]
    fn violation_display() {
        let v = LimitViolation::MaxEvents {
            limit: 100,
            actual: 101,
        };
        assert!(v.to_string().contains("101"));
        assert!(v.to_string().contains("100"));
    }

    #[test]
    fn violation_serde_roundtrip() {
        let v = LimitViolation::WatchdogTimeout {
            timeout_ms: 5000,
            stall_ms: 6000,
        };
        let json = serde_json::to_string(&v).unwrap();
        let restored: LimitViolation = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, v);
    }

    #[test]
    fn barrier_leak_violation() {
        let v = LimitViolation::BarrierLeak {
            operation: "write_pane".into(),
        };
        assert!(v.to_string().contains("write_pane"));
    }

    // ── Disabled limits (0 means no limit) ──────────────────────────────

    #[test]
    fn disabled_event_limit() {
        let limits = ResourceLimits {
            max_events: 0,
            max_wall_clock_ms: 0,
            memory_warning_events: 0,
            watchdog_timeout_ms: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        for i in 0..1000u64 {
            assert_eq!(tracker.record_event(i), CheckResult::Ok);
        }
    }

    #[test]
    fn disabled_watchdog() {
        let limits = ResourceLimits {
            watchdog_timeout_ms: 0,
            ..Default::default()
        };
        let tracker = ResourceTracker::new(limits, 0);
        assert_eq!(tracker.check_watchdog(999_999), CheckResult::Ok);
    }
}
