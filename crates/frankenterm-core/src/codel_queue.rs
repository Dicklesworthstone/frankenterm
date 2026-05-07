//! br-ft-codel: alien-uplift CoDel (Controlled Delay) for
//! latency-based queue management in capacity decisions.
//!
//! Implements the CoDel algorithm (Nichols & Jacobson 2012,
//! "Controlling Queue Delay", ACM Queue 10(5)) — a queue-management
//! algorithm that uses **latency** (sojourn time) as the primary
//! signal instead of queue depth. CoDel is "no-knobs":
//! parameter-free across deployment regimes (5ms target + 100ms
//! interval are universal defaults).
//!
//! # Why CoDel for the capacity governor
//!
//! [`crate::capacity_governor::CapacityGovernor`] today drives
//! throttle/block decisions from CPU + memory utilization
//! thresholds (0.80 throttle, 0.95 block). Those signals are good
//! at catching resource saturation but blind to **queue-induced
//! latency**: a workload can sit in queue for tens of seconds
//! while CPU + memory look healthy because the workers are pegged
//! on long-running work. The operator-facing latency tail spikes
//! while the threshold-based governor sees green.
//!
//! CoDel adds a complementary signal: track the sojourn time
//! (enqueue → dequeue duration) of every workload and start
//! shedding when the **minimum** sojourn over a sliding interval
//! exceeds the target. The minimum-not-mean choice is what makes
//! CoDel robust to bursts: a brief spike pulls the mean up but
//! doesn't pull the minimum up unless EVERY observation in the
//! interval was above target — a true sustained queue-induced
//! delay condition.
//!
//! # Algorithm
//!
//! Two parameters (Nichols & Jacobson universal defaults):
//! - `target` = 5ms (acceptable steady-state queueing delay)
//! - `interval` = 100ms (sliding window for the minimum)
//!
//! State machine: `NotDropping` ⇄ `Dropping`.
//! - Enter `Dropping` when: `min_sojourn` over the most recent
//!   `interval` continuously exceeds `target` for at least
//!   `interval` (i.e., we've waited a full interval and the
//!   minimum is still bad).
//! - In `Dropping`: drop one observation per scheduled tick; next
//!   tick at `interval / sqrt(drop_count)` (control law gives
//!   square-root cadence — drops accelerate as count climbs,
//!   then taper as the queue drains).
//! - Exit `Dropping` when `current_sojourn ≤ target`.
//!
//! # Adapted shape for capacity decisions (vs raw packet queues)
//!
//! Packet queues drop packets; capacity governors throttle
//! requests. The translation:
//! - `record_sojourn(ms)` = called when a workload completes its
//!   wait + starts executing. The duration from enqueue to start
//!   is the sojourn.
//! - `should_drop()` = called when a new workload arrives. Returns
//!   true when CoDel says we're in the Dropping state — caller
//!   responds with `GovernorDecision::Throttle` (or `Block` if
//!   the caller wants stricter behavior).
//!
//! # What ships
//!
//! - [`CodelQueue`] — the algorithm state machine.
//! - [`CodelConfig`] — target + interval (defaults match Nichols
//!   & Jacobson universals).
//! - [`CodelSnapshot`] — serde snapshot for capacity-doctor
//!   reports.
//!
//! # What is deferred
//!
//! - Wiring into [`crate::capacity_governor::CapacityGovernor::compute_decision`]
//!   so the throttle/block choice has CoDel as a complementary
//!   signal alongside CPU + memory thresholds.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// br-ft-codel: configuration for the CoDel algorithm. Defaults
/// match the Nichols & Jacobson 2012 universal recommendations
/// (5ms target, 100ms interval); these have been validated across
/// many deployment regimes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodelConfig {
    /// Acceptable steady-state queueing delay. Default: 5ms.
    pub target_ms: u64,
    /// Sliding window for the minimum-sojourn check. Default: 100ms.
    pub interval_ms: u64,
}

impl Default for CodelConfig {
    fn default() -> Self {
        Self {
            target_ms: 5,
            interval_ms: 100,
        }
    }
}

impl CodelConfig {
    fn target(&self) -> Duration {
        Duration::from_millis(self.target_ms.max(1))
    }

    fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.max(self.target_ms.max(1)))
    }
}

/// br-ft-codel: state of the CoDel state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodelState {
    /// Sojourn times healthy — admit normally.
    NotDropping,
    /// Sojourn-time-induced delay observed — controlled drop active.
    Dropping,
}

/// br-ft-codel: CoDel queue-management state machine.
///
/// See module docstring for the algorithm + adaptation for capacity
/// decisions. Single-threaded use; wrap in `Mutex` for shared
/// access (typical: per-WorkloadCategory in CapacityGovernor).
#[derive(Debug, Clone)]
pub struct CodelQueue {
    config: CodelConfig,
    state: CodelState,
    /// Earliest moment the minimum-sojourn rule could have been
    /// continuously above target. None until we observe the first
    /// above-target sojourn after a healthy period.
    above_target_since: Option<Instant>,
    /// Time of last drop (used to compute the next-drop cadence).
    last_drop_at: Option<Instant>,
    /// Count of drops within the current Dropping run. Reset on
    /// transition back to NotDropping.
    drop_count: u32,
    /// Most recent sojourn observation.
    last_sojourn: Option<Duration>,
}

impl Default for CodelQueue {
    fn default() -> Self {
        Self::new(CodelConfig::default())
    }
}

impl CodelQueue {
    /// Create a new CoDel state machine with the given config.
    #[must_use]
    pub fn new(config: CodelConfig) -> Self {
        Self {
            config,
            state: CodelState::NotDropping,
            above_target_since: None,
            last_drop_at: None,
            drop_count: 0,
            last_sojourn: None,
        }
    }

    /// Record a sojourn time (enqueue → dequeue duration). Updates
    /// the algorithm state. Returns the current state AFTER the
    /// update so caller can act on the new classification.
    pub fn record_sojourn(&mut self, sojourn: Duration, now: Instant) -> CodelState {
        self.last_sojourn = Some(sojourn);
        let target = self.config.target();
        if sojourn > target {
            // Above target — track when the streak started.
            if self.above_target_since.is_none() {
                self.above_target_since = Some(now);
            }
        } else {
            // Below or equal to target — reset the streak.
            self.above_target_since = None;
            // If we were dropping, exit.
            if self.state == CodelState::Dropping {
                self.state = CodelState::NotDropping;
                self.drop_count = 0;
                self.last_drop_at = None;
            }
        }

        // Promotion to Dropping: above-target streak has lasted at
        // least `interval`.
        if self.state == CodelState::NotDropping {
            if let Some(start) = self.above_target_since {
                if now.duration_since(start) >= self.config.interval() {
                    self.state = CodelState::Dropping;
                    self.drop_count = 0;
                    self.last_drop_at = None;
                }
            }
        }

        self.state
    }

    /// Should the current arriving workload be dropped/throttled?
    ///
    /// Returns `true` when CoDel is in the `Dropping` state AND the
    /// next-drop cadence has been reached. The cadence is
    /// `interval / sqrt(drop_count)` — drops accelerate as the
    /// drop count climbs, then taper as the queue drains.
    pub fn should_drop(&mut self, now: Instant) -> bool {
        if self.state != CodelState::Dropping {
            return false;
        }
        // Cadence: interval / sqrt(drop_count). Use drop_count.max(1)
        // for the very first drop (drop_count=0 → divide by 1 → fire
        // immediately).
        let count = self.drop_count.max(1) as f64;
        let cadence_ms = (self.config.interval_ms as f64 / count.sqrt()).max(1.0) as u64;
        let cadence = Duration::from_millis(cadence_ms);
        let due = self
            .last_drop_at
            .is_none_or(|t| now.duration_since(t) >= cadence);
        if due {
            self.drop_count = self.drop_count.saturating_add(1);
            self.last_drop_at = Some(now);
            true
        } else {
            false
        }
    }

    /// Current state machine state.
    #[must_use]
    pub fn state(&self) -> CodelState {
        self.state
    }

    /// Total drops in the current Dropping run (resets on exit).
    #[must_use]
    pub fn drop_count(&self) -> u32 {
        self.drop_count
    }

    /// Most recent sojourn observation, if any.
    #[must_use]
    pub fn last_sojourn(&self) -> Option<Duration> {
        self.last_sojourn
    }

    /// Snapshot for inclusion in capacity-doctor / runtime-telemetry
    /// reports.
    #[must_use]
    pub fn snapshot(&self) -> CodelSnapshot {
        CodelSnapshot {
            state: self.state,
            drop_count: self.drop_count,
            target_ms: self.config.target_ms,
            interval_ms: self.config.interval_ms,
            last_sojourn_ms: self.last_sojourn.map(|d| d.as_millis() as u64),
        }
    }
}

/// br-ft-codel: serde snapshot for telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodelSnapshot {
    pub state: CodelState,
    pub drop_count: u32,
    pub target_ms: u64,
    pub interval_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sojourn_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_queue_starts_not_dropping() {
        let q = CodelQueue::default();
        assert_eq!(q.state(), CodelState::NotDropping);
        assert_eq!(q.drop_count(), 0);
    }

    #[test]
    fn below_target_sojourn_keeps_state_not_dropping() {
        let mut q = CodelQueue::default();
        let t = Instant::now();
        for i in 0..50 {
            // 1ms sojourn — well below 5ms target.
            let state = q.record_sojourn(Duration::from_millis(1), t + Duration::from_millis(i));
            assert_eq!(state, CodelState::NotDropping);
        }
    }

    #[test]
    fn sustained_above_target_sojourn_promotes_to_dropping() {
        // br-ft-codel core invariant: above-target sojourn that
        // persists for ≥ interval transitions state to Dropping.
        let mut q = CodelQueue::default();
        let t0 = Instant::now();
        // First above-target observation at t=0.
        let s = q.record_sojourn(Duration::from_millis(20), t0);
        assert_eq!(
            s,
            CodelState::NotDropping,
            "first above-target = streak begins, no transition yet"
        );
        // Second above-target observation at t = interval (100ms).
        let s = q.record_sojourn(Duration::from_millis(20), t0 + Duration::from_millis(100));
        assert_eq!(
            s,
            CodelState::Dropping,
            "above-target streak ≥ interval → Dropping"
        );
    }

    #[test]
    fn below_target_after_dropping_returns_to_not_dropping() {
        let mut q = CodelQueue::default();
        let t0 = Instant::now();
        q.record_sojourn(Duration::from_millis(20), t0);
        q.record_sojourn(Duration::from_millis(20), t0 + Duration::from_millis(100));
        assert_eq!(q.state(), CodelState::Dropping);
        // A healthy sojourn (≤ target) returns the queue to NotDropping.
        let s = q.record_sojourn(Duration::from_millis(2), t0 + Duration::from_millis(150));
        assert_eq!(s, CodelState::NotDropping);
        assert_eq!(q.drop_count(), 0, "drop_count resets on exit from Dropping");
    }

    #[test]
    fn brief_spike_does_not_promote_to_dropping() {
        // Single above-target observation followed by below-target
        // must NOT transition (above_target_since is reset on any
        // below-target observation).
        let mut q = CodelQueue::default();
        let t0 = Instant::now();
        q.record_sojourn(Duration::from_millis(20), t0);
        let s = q.record_sojourn(Duration::from_millis(2), t0 + Duration::from_millis(50));
        assert_eq!(s, CodelState::NotDropping, "single spike must not promote");
        // Even if we later go above-target again, the streak restarts.
        q.record_sojourn(Duration::from_millis(20), t0 + Duration::from_millis(100));
        let s = q.record_sojourn(Duration::from_millis(20), t0 + Duration::from_millis(150));
        assert_eq!(
            s,
            CodelState::NotDropping,
            "streak restarted at t=100ms; only 50ms elapsed"
        );
    }

    #[test]
    fn should_drop_in_not_dropping_state_returns_false() {
        let mut q = CodelQueue::default();
        let t0 = Instant::now();
        // Healthy: no drop.
        assert!(!q.should_drop(t0));
    }

    #[test]
    fn should_drop_in_dropping_state_fires_at_cadence() {
        // br-ft-codel control law: drops fire at interval / sqrt(count).
        // For count=1, cadence = 100ms; for count=4, cadence = 50ms.
        let mut q = CodelQueue::default();
        let t0 = Instant::now();
        // Promote to Dropping.
        q.record_sojourn(Duration::from_millis(20), t0);
        q.record_sojourn(Duration::from_millis(20), t0 + Duration::from_millis(100));
        assert_eq!(q.state(), CodelState::Dropping);

        // First drop fires immediately (drop_count=0 → fires).
        assert!(q.should_drop(t0 + Duration::from_millis(100)));
        // Immediately after, no second drop yet (cadence not reached).
        assert!(!q.should_drop(t0 + Duration::from_millis(100)));
        // After 100ms more (cadence for count=1 ≈ 100ms), drops fire again.
        assert!(q.should_drop(t0 + Duration::from_millis(200)));
        assert!(q.drop_count() >= 2);
    }

    #[test]
    fn snapshot_carries_documented_fields() {
        let mut q = CodelQueue::default();
        let t0 = Instant::now();
        q.record_sojourn(Duration::from_millis(20), t0);
        q.record_sojourn(Duration::from_millis(20), t0 + Duration::from_millis(100));
        let snap = q.snapshot();
        assert_eq!(snap.state, CodelState::Dropping);
        assert_eq!(snap.target_ms, 5);
        assert_eq!(snap.interval_ms, 100);
        assert_eq!(snap.last_sojourn_ms, Some(20));
    }

    #[test]
    fn snapshot_serde_roundtrips() {
        let mut q = CodelQueue::default();
        q.record_sojourn(Duration::from_millis(2), Instant::now());
        let snap = q.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        let parsed: CodelSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, snap);
    }

    #[test]
    fn config_normalizes_minimums() {
        // target_ms=0 + interval_ms=0 should clamp to ≥ 1.
        let cfg = CodelConfig {
            target_ms: 0,
            interval_ms: 0,
        };
        assert_eq!(cfg.target(), Duration::from_millis(1));
        assert_eq!(cfg.interval(), Duration::from_millis(1));
        // interval_ms < target_ms should clamp interval ≥ target.
        let cfg = CodelConfig {
            target_ms: 50,
            interval_ms: 10,
        };
        assert_eq!(cfg.interval(), Duration::from_millis(50));
    }
}
