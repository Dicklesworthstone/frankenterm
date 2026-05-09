//! Watchdog timer for the TripleBuffer reader-slot lifecycle
//! (ft-2okh0.3 subset for the bead's "reader holds slot too long"
//! failure-mode mitigation).
//!
//! Pane 3 shipped the core `TripleBuffer<T>` in
//! `crates/frankenterm-core/src/triple_buffer.rs` (ft-d0ol8) and
//! its module docs explicitly defer the watchdog wiring:
//! > "If a render thread crashes mid-frame holding a slot, that
//! > slot is leaked. Watchdog: if a slot hasn't been released in
//! > 5 seconds, force-recycle. Watchdog wiring is the integration
//! > bead's job; this module exposes the API."
//!
//! This module ships the policy substrate the integration bead
//! drives:
//!
//! - `TripleBufferWatchdog` — single-acquire state machine
//!   tracking the most recent outstanding `acquire`. Pure-logic
//!   so it's testable without threads or timers.
//! - `WatchdogDecision` — `NoOp` / `Warn` / `ForceRecycle`. The
//!   integration's periodic-tick handler matches on this and
//!   either logs a warning or invokes
//!   `TripleBuffer::force_recycle`.
//! - `WatchdogConfig` — `warn_after` and `force_recycle_after`
//!   thresholds, defaulted to the bead's 5 s and a 1 s warning
//!   horizon.
//! - `WatchdogStats` — lifetime `acquires_total` / `releases_total`
//!   / `warnings_emitted` / `force_recycle_invocations` counters
//!   for `ft doctor`.
//!
//! ## What is deferred (continuation, see follow-up bead)
//!
//! - Wiring `TripleBufferWatchdog::record_acquire` /
//!   `record_release` into the actual renderer — likely via a
//!   drop guard returned from a thin wrapper around
//!   `TripleBuffer::acquire`.
//! - The periodic `poll(now)` cadence — driven by the same
//!   timer that runs `IdleDetector::poll` from ft-mpc9b.5.3.
//! - Per-pane scaling: today the watchdog assumes one outstanding
//!   acquire at a time (matches the TripleBuffer single-reader
//!   contract). Multi-pane fleets need one watchdog per pane;
//!   the integration bead either spins up N independent
//!   watchdogs or upgrades this to a slot-keyed map.
//! - `ft doctor` surface for `WatchdogStats`.

#![allow(dead_code)]

use std::time::{Duration, Instant};

/// Default warning horizon — log a `Warn` decision when an
/// acquire has been outstanding this long. The bead doesn't
/// pin a specific number; 1 s is a reasonable "the renderer
/// might still be working but it's getting suspicious" line.
pub const DEFAULT_WARN_AFTER: Duration = Duration::from_secs(1);

/// Default force-recycle threshold per the bead: "if a slot
/// hasn't been released in 5 seconds, force-recycle".
pub const DEFAULT_FORCE_RECYCLE_AFTER: Duration = Duration::from_secs(5);

/// Operator-tunable thresholds.
///
/// Fields are `pub(crate)` because `force_recycle_after` is the
/// stuck-reader recovery cap — arbitrary code paths must not be
/// able to silently set it to `Duration::MAX` disabling the
/// protection. Use the [`Self::with_warn_after`] /
/// [`Self::with_force_recycle_after`] builder API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogConfig {
    pub(crate) warn_after: Duration,
    pub(crate) force_recycle_after: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            warn_after: DEFAULT_WARN_AFTER,
            force_recycle_after: DEFAULT_FORCE_RECYCLE_AFTER,
        }
    }
}

impl WatchdogConfig {
    #[must_use]
    pub const fn warn_after(self) -> Duration {
        self.warn_after
    }

    #[must_use]
    pub const fn force_recycle_after(self) -> Duration {
        self.force_recycle_after
    }

    #[must_use]
    pub const fn with_warn_after(mut self, warn_after: Duration) -> Self {
        self.warn_after = warn_after;
        self
    }

    #[must_use]
    pub const fn with_force_recycle_after(mut self, force_recycle_after: Duration) -> Self {
        self.force_recycle_after = force_recycle_after;
        self
    }
}

/// What the watchdog wants the integration to do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogDecision {
    /// No outstanding acquire, or the acquire is well within
    /// the warn horizon — caller does nothing.
    NoOp,
    /// An acquire has been outstanding past `warn_after` but
    /// hasn't yet hit `force_recycle_after`. The integration
    /// should emit a structured-log warning. The watchdog
    /// records the warning in its lifetime counter so duplicate
    /// warnings on subsequent ticks don't re-bump.
    Warn { outstanding_for: Duration },
    /// The acquire has been outstanding past
    /// `force_recycle_after`. The integration should call
    /// `TripleBuffer::force_recycle` and treat this as a
    /// fatal-recoverable event (the bead's classification).
    /// After the integration calls force_recycle it should call
    /// `record_release` to clear the watchdog.
    ForceRecycle { outstanding_for: Duration },
}

/// Lifetime counters surfaced via `ft doctor`.
///
/// Counters are `pub(crate)` so external code can't silently
/// zero `force_recycle_invocations` (the operator alarm signal
/// for stuck-reader recoveries). Use the read accessors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchdogStats {
    pub(crate) acquires_total: u64,
    pub(crate) releases_total: u64,
    pub(crate) warnings_emitted: u64,
    pub(crate) force_recycle_invocations: u64,
    /// Counter for `record_acquire` calls that overwrote a
    /// previous Active/Warned state without a matching
    /// `record_release` (integration-bug signal — usually
    /// indicates a forgotten release path or a panic-unwind
    /// gap).
    pub(crate) double_acquires_total: u64,
}

impl WatchdogStats {
    #[must_use]
    pub const fn acquires_total(&self) -> u64 {
        self.acquires_total
    }

    #[must_use]
    pub const fn releases_total(&self) -> u64 {
        self.releases_total
    }

    #[must_use]
    pub const fn warnings_emitted(&self) -> u64 {
        self.warnings_emitted
    }

    #[must_use]
    pub const fn force_recycle_invocations(&self) -> u64 {
        self.force_recycle_invocations
    }

    #[must_use]
    pub const fn double_acquires_total(&self) -> u64 {
        self.double_acquires_total
    }
}

/// Internal state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchdogState {
    Idle,
    /// An acquire is outstanding since this `Instant`.
    Active {
        acquired_at: Instant,
    },
    /// Past the warn threshold; we've already emitted one warning
    /// at the `warned_at` timestamp. Used to dedupe warnings on
    /// subsequent ticks before force-recycle fires.
    Warned {
        acquired_at: Instant,
        warned_at: Instant,
    },
    /// `ForceRecycle` already fired for this acquire. The
    /// integration's force_recycle dispatcher saw the decision;
    /// subsequent polls return `NoOp` until `record_release`
    /// (which the integration must call after invoking
    /// `TripleBuffer::force_recycle`). Without this state the
    /// poll would re-fire `ForceRecycle` on every tick,
    /// inflating the counter and causing duplicate
    /// `TripleBuffer::force_recycle()` calls.
    Triggered {
        acquired_at: Instant,
    },
}

/// Tracks the most-recent outstanding `TripleBuffer::acquire` and
/// emits warning / force-recycle decisions per the configured
/// thresholds. Designed for single-acquire-at-a-time use; multi-
/// pane fleets instantiate one watchdog per pane.
#[derive(Debug, Clone)]
pub struct TripleBufferWatchdog {
    state: WatchdogState,
    config: WatchdogConfig,
    stats: WatchdogStats,
}

impl TripleBufferWatchdog {
    pub fn new() -> Self {
        Self::with_config(WatchdogConfig::default())
    }

    pub fn with_config(config: WatchdogConfig) -> Self {
        Self {
            state: WatchdogState::Idle,
            config,
            stats: WatchdogStats::default(),
        }
    }

    /// Notify the watchdog that the renderer just called
    /// `TripleBuffer::acquire`. If a previous acquire was already
    /// outstanding (caller didn't pair it with a release — bug or
    /// the watchdog already force-recycled it), the new acquire
    /// supersedes it; the previous acquire's outstanding-time is
    /// effectively forgotten because force_recycle would have
    /// already rotated the slot away.
    pub fn record_acquire(&mut self, now: Instant) {
        self.stats.acquires_total = self.stats.acquires_total.saturating_add(1);
        // Detect a forgotten release: if state isn't Idle, the
        // previous acquire wasn't paired. Bump the
        // double-acquires counter so the integration's audit
        // can spot the bug.
        if !matches!(self.state, WatchdogState::Idle) {
            self.stats.double_acquires_total = self.stats.double_acquires_total.saturating_add(1);
        }
        self.state = WatchdogState::Active { acquired_at: now };
    }

    /// Notify the watchdog that the renderer dropped its
    /// `Arc<T>`. Resets to `Idle`. Idempotent — calling release
    /// when already idle is a no-op (the integration may be
    /// defensive about this after a force_recycle).
    pub fn record_release(&mut self) {
        if !matches!(self.state, WatchdogState::Idle) {
            self.stats.releases_total = self.stats.releases_total.saturating_add(1);
        }
        self.state = WatchdogState::Idle;
    }

    /// Periodic tick. Caller invokes from the same scheduler
    /// that drives `IdleDetector::poll` (or whichever periodic
    /// timer the integration installs). Returns the decision the
    /// integration must act on.
    ///
    /// Side effects: if the decision is `Warn`, the warning
    /// counter bumps and the state advances to `Warned` so the
    /// next tick (still under force_recycle_after) returns
    /// `NoOp` rather than re-warning. If the decision is
    /// `ForceRecycle`, the force_recycle counter bumps. The state
    /// remains `Active`/`Warned` until the integration calls
    /// `record_release` after invoking
    /// `TripleBuffer::force_recycle`.
    pub fn poll(&mut self, now: Instant) -> WatchdogDecision {
        match self.state {
            WatchdogState::Idle => WatchdogDecision::NoOp,
            WatchdogState::Active { acquired_at } => {
                let outstanding = now.saturating_duration_since(acquired_at);
                if outstanding >= self.config.force_recycle_after {
                    self.stats.force_recycle_invocations =
                        self.stats.force_recycle_invocations.saturating_add(1);
                    self.state = WatchdogState::Triggered { acquired_at };
                    WatchdogDecision::ForceRecycle {
                        outstanding_for: outstanding,
                    }
                } else if outstanding >= self.config.warn_after {
                    self.stats.warnings_emitted = self.stats.warnings_emitted.saturating_add(1);
                    self.state = WatchdogState::Warned {
                        acquired_at,
                        warned_at: now,
                    };
                    WatchdogDecision::Warn {
                        outstanding_for: outstanding,
                    }
                } else {
                    WatchdogDecision::NoOp
                }
            }
            WatchdogState::Warned {
                acquired_at,
                warned_at: _,
            } => {
                let outstanding = now.saturating_duration_since(acquired_at);
                if outstanding >= self.config.force_recycle_after {
                    self.stats.force_recycle_invocations =
                        self.stats.force_recycle_invocations.saturating_add(1);
                    self.state = WatchdogState::Triggered { acquired_at };
                    WatchdogDecision::ForceRecycle {
                        outstanding_for: outstanding,
                    }
                } else {
                    // Already warned for this acquire; don't
                    // re-warn on every tick.
                    WatchdogDecision::NoOp
                }
            }
            WatchdogState::Triggered { .. } => {
                // ForceRecycle already fired for this acquire.
                // Subsequent polls are NoOp until
                // record_release transitions back to Idle.
                // This prevents the prior bug where every
                // post-trigger poll re-fired ForceRecycle,
                // inflating force_recycle_invocations and
                // causing duplicate TripleBuffer::force_recycle
                // calls.
                WatchdogDecision::NoOp
            }
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, WatchdogState::Idle)
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.state,
            WatchdogState::Active { .. }
                | WatchdogState::Warned { .. }
                | WatchdogState::Triggered { .. }
        )
    }

    /// True iff the watchdog has fired ForceRecycle for the
    /// current acquire and is waiting for the integration to
    /// call `record_release`. A long-stuck `Triggered` state
    /// indicates the integration's release-after-force-recycle
    /// wiring is broken.
    #[must_use]
    pub fn is_triggered(&self) -> bool {
        matches!(self.state, WatchdogState::Triggered { .. })
    }

    pub fn config(&self) -> &WatchdogConfig {
        &self.config
    }

    pub fn stats(&self) -> &WatchdogStats {
        &self.stats
    }
}

impl Default for TripleBufferWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn fresh_watchdog_is_idle() {
        let w = TripleBufferWatchdog::new();
        assert!(w.is_idle());
        assert!(!w.is_active());
        assert_eq!(w.stats().acquires_total, 0);
        assert_eq!(w.stats().releases_total, 0);
    }

    #[test]
    fn idle_watchdog_polls_to_noop() {
        let mut w = TripleBufferWatchdog::new();
        let decision = w.poll(t0() + Duration::from_secs(60));
        assert_eq!(decision, WatchdogDecision::NoOp);
    }

    #[test]
    fn record_acquire_transitions_to_active() {
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        assert!(w.is_active());
        assert_eq!(w.stats().acquires_total, 1);
    }

    #[test]
    fn record_release_returns_to_idle() {
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        w.record_release();
        assert!(w.is_idle());
        assert_eq!(w.stats().releases_total, 1);
    }

    #[test]
    fn release_when_idle_is_idempotent_no_counter_bump() {
        let mut w = TripleBufferWatchdog::new();
        w.record_release();
        w.record_release();
        assert_eq!(w.stats().releases_total, 0);
        assert!(w.is_idle());
    }

    #[test]
    fn poll_within_warn_horizon_is_noop() {
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        let decision = w.poll(now + Duration::from_millis(500));
        assert_eq!(decision, WatchdogDecision::NoOp);
        assert_eq!(w.stats().warnings_emitted, 0);
    }

    #[test]
    fn poll_past_warn_horizon_emits_warn() {
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        let decision = w.poll(now + Duration::from_millis(1500));
        assert!(matches!(decision, WatchdogDecision::Warn { .. }));
        assert_eq!(w.stats().warnings_emitted, 1);
    }

    #[test]
    fn warn_only_fires_once_per_acquire() {
        // The bead's invariant: don't spam warnings every tick.
        // The state machine moves to Warned and subsequent ticks
        // (still under force_recycle_after) return NoOp.
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        let _ = w.poll(now + Duration::from_millis(1500));
        let d2 = w.poll(now + Duration::from_secs(2));
        let d3 = w.poll(now + Duration::from_secs(3));
        assert_eq!(d2, WatchdogDecision::NoOp);
        assert_eq!(d3, WatchdogDecision::NoOp);
        assert_eq!(w.stats().warnings_emitted, 1);
    }

    #[test]
    fn poll_past_force_recycle_threshold_returns_force_recycle() {
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        let decision = w.poll(now + Duration::from_secs(6));
        assert!(matches!(decision, WatchdogDecision::ForceRecycle { .. }));
        assert_eq!(w.stats().force_recycle_invocations, 1);
    }

    #[test]
    fn force_recycle_outstanding_for_reflects_elapsed() {
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        let decision = w.poll(now + Duration::from_secs(7));
        match decision {
            WatchdogDecision::ForceRecycle { outstanding_for } => {
                assert!(outstanding_for >= Duration::from_secs(6));
                assert!(outstanding_for <= Duration::from_secs(8));
            }
            other => panic!("expected ForceRecycle, got {:?}", other),
        }
    }

    #[test]
    fn force_recycle_fires_from_warned_state_too() {
        // The warn-then-recycle path: warn fired at 1.5s, then
        // force-recycle at 5s+ from the same acquire.
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        let _ = w.poll(now + Duration::from_millis(1500));
        let decision = w.poll(now + Duration::from_secs(6));
        assert!(matches!(decision, WatchdogDecision::ForceRecycle { .. }));
        assert_eq!(w.stats().warnings_emitted, 1);
        assert_eq!(w.stats().force_recycle_invocations, 1);
    }

    #[test]
    fn force_recycle_fires_exactly_once_until_release() {
        // Regression: previously, ForceRecycle re-fired on every
        // poll until record_release. Each repeat bumped the
        // counter AND triggered a duplicate
        // TripleBuffer::force_recycle() via the wrapper. Now the
        // post-trigger state is Triggered → subsequent polls
        // return NoOp until record_release.
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        let d1 = w.poll(now + Duration::from_secs(6));
        assert!(matches!(d1, WatchdogDecision::ForceRecycle { .. }));
        assert_eq!(w.stats().force_recycle_invocations, 1);
        assert!(w.is_triggered());

        // Subsequent polls: NoOp; counter stays at 1.
        let d2 = w.poll(now + Duration::from_secs(7));
        assert_eq!(d2, WatchdogDecision::NoOp);
        let d3 = w.poll(now + Duration::from_secs(10));
        assert_eq!(d3, WatchdogDecision::NoOp);
        assert_eq!(w.stats().force_recycle_invocations, 1);

        // After record_release, state goes back to Idle.
        w.record_release();
        assert!(w.is_idle());
        assert!(!w.is_triggered());
    }

    #[test]
    fn double_acquire_bumps_double_acquires_counter() {
        // Regression: previously, record_acquire silently
        // overwrote prior Active/Warned state with no signal —
        // integration bugs that forgot a release path were
        // invisible. Now the double_acquires_total counter
        // surfaces them.
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        assert_eq!(w.stats().double_acquires_total, 0);
        // Forgot a release; another acquire arrives.
        w.record_acquire(now + Duration::from_millis(100));
        assert_eq!(w.stats().double_acquires_total, 1);
        // Another forgotten release.
        w.record_acquire(now + Duration::from_millis(200));
        assert_eq!(w.stats().double_acquires_total, 2);
    }

    #[test]
    fn watchdog_config_builder_round_trips() {
        let cfg = WatchdogConfig::default()
            .with_warn_after(Duration::from_millis(500))
            .with_force_recycle_after(Duration::from_secs(2));
        assert_eq!(cfg.warn_after(), Duration::from_millis(500));
        assert_eq!(cfg.force_recycle_after(), Duration::from_secs(2));
    }

    #[test]
    fn watchdog_stats_accessors_round_trip() {
        let s = WatchdogStats::default();
        assert_eq!(s.acquires_total(), 0);
        assert_eq!(s.releases_total(), 0);
        assert_eq!(s.warnings_emitted(), 0);
        assert_eq!(s.force_recycle_invocations(), 0);
        assert_eq!(s.double_acquires_total(), 0);
    }

    #[test]
    fn release_after_warn_resets_for_next_acquire() {
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        let _ = w.poll(now + Duration::from_millis(1500));
        // Renderer finished — release.
        w.record_release();
        assert!(w.is_idle());
        // Next acquire: warn counter starts fresh from the new
        // acquired_at.
        w.record_acquire(now + Duration::from_secs(2));
        let d = w.poll(now + Duration::from_secs(2) + Duration::from_millis(500));
        assert_eq!(d, WatchdogDecision::NoOp);
    }

    #[test]
    fn record_acquire_supersedes_outstanding_acquire() {
        // If acquire is called twice without intervening release,
        // the new acquire supersedes the old. The outstanding-
        // since clock resets.
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        let _ = w.poll(now + Duration::from_secs(3)); // would be approaching force-recycle
        // Renderer finishes its first acquire and starts a new
        // one — without an explicit release call (defensive).
        w.record_acquire(now + Duration::from_secs(3));
        let d = w.poll(now + Duration::from_secs(3) + Duration::from_millis(500));
        // The new acquire is only 500ms old → NoOp.
        assert_eq!(d, WatchdogDecision::NoOp);
        assert_eq!(w.stats().acquires_total, 2);
    }

    #[test]
    fn custom_thresholds_apply() {
        // Mobile-style aggressive thresholds: warn at 100ms,
        // force-recycle at 500ms.
        let cfg = WatchdogConfig {
            warn_after: Duration::from_millis(100),
            force_recycle_after: Duration::from_millis(500),
        };
        let now = t0();
        let mut w = TripleBufferWatchdog::with_config(cfg);
        w.record_acquire(now);
        assert_eq!(
            w.poll(now + Duration::from_millis(50)),
            WatchdogDecision::NoOp
        );
        let warn = w.poll(now + Duration::from_millis(150));
        assert!(matches!(warn, WatchdogDecision::Warn { .. }));
        let force = w.poll(now + Duration::from_millis(600));
        assert!(matches!(force, WatchdogDecision::ForceRecycle { .. }));
    }

    #[test]
    fn typical_render_pattern_drives_zero_warnings() {
        // Models a healthy renderer: acquire → render in 5ms →
        // release, repeated 100 times. Watchdog never warns.
        let mut w = TripleBufferWatchdog::new();
        let mut clock = t0();
        for _ in 0..100 {
            w.record_acquire(clock);
            let _ = w.poll(clock + Duration::from_millis(2));
            clock += Duration::from_millis(5);
            w.record_release();
        }
        assert_eq!(w.stats().acquires_total, 100);
        assert_eq!(w.stats().releases_total, 100);
        assert_eq!(w.stats().warnings_emitted, 0);
        assert_eq!(w.stats().force_recycle_invocations, 0);
        assert!(w.is_idle());
    }

    #[test]
    fn crashed_renderer_pattern_triggers_force_recycle() {
        // Models the bead's failure-mode scenario: render thread
        // crashed mid-frame holding a slot.
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);

        // Periodic ticks every second.
        for sec in 1..=4 {
            let d = w.poll(now + Duration::from_secs(sec));
            // 1s warning, 2/3/4s NoOp (already warned).
            if sec == 1 {
                assert!(matches!(d, WatchdogDecision::Warn { .. }));
            } else {
                assert_eq!(d, WatchdogDecision::NoOp);
            }
        }
        // 5s tick → force_recycle.
        let d5 = w.poll(now + Duration::from_secs(5));
        assert!(matches!(d5, WatchdogDecision::ForceRecycle { .. }));
        assert_eq!(w.stats().force_recycle_invocations, 1);
    }

    #[test]
    fn stats_are_independent_telemetry_axes() {
        // The four counters (acquires / releases / warnings /
        // force_recycles) are separate axes; a single acquire
        // can drive at most 1 warning and 1 force_recycle but
        // those don't double-count as releases.
        let now = t0();
        let mut w = TripleBufferWatchdog::new();
        w.record_acquire(now);
        let _ = w.poll(now + Duration::from_millis(1500));
        let _ = w.poll(now + Duration::from_secs(6));
        // No release yet.
        assert_eq!(w.stats().acquires_total, 1);
        assert_eq!(w.stats().releases_total, 0);
        assert_eq!(w.stats().warnings_emitted, 1);
        assert_eq!(w.stats().force_recycle_invocations, 1);
    }

    #[test]
    fn config_thresholds_round_trip() {
        // Sanity check: the constants match the bead's stated 5s
        // force-recycle threshold.
        let cfg = WatchdogConfig::default();
        assert_eq!(cfg.force_recycle_after, Duration::from_secs(5));
        assert_eq!(cfg.warn_after, Duration::from_secs(1));
    }
}
