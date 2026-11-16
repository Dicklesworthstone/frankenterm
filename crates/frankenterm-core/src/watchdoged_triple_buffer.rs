//! `WatchdogedTripleBuffer<T>` — composes `TripleBuffer<T>` with
//! `TripleBufferWatchdog` (ft-ipau0 sub-bead of the ft-2okh0.3
//! TripleBuffer integration epic).
//!
//! Pane 3 shipped `TripleBuffer<T>` (br-ft-d0ol8) with a
//! lock-free `acquire() -> Arc<T>`. The watchdog substrate
//! (`triple_buffer_watchdog.rs`, ft-2okh0.3) ships the
//! `Idle / Active / Warned` policy state machine that decides when an
//! orphan slot needs `force_recycle`. The integration bead (ft-ipau0)
//! has to:
//!
//! 1. Wrap `TripleBuffer::acquire()` in a drop guard so the renderer
//!    can't forget to call `record_release`.
//! 2. Drive periodic `poll(now)` from the GUI's frame timer.
//! 3. On `WatchdogDecision::ForceRecycle`, actually invoke
//!    `TripleBuffer::force_recycle()`.
//! 4. Surface combined health (slot state + watchdog stats) through
//!    `ft doctor`.
//!
//! This module ships steps 1-3 as pure-logic plumbing in core. Step 4
//! lives in the gui crate; the `WatchdogedHealth` type here is the
//! `ft doctor` payload it serialises.
//!
//! ## What is deferred to the gui-crate continuation
//!
//! - Replacing the renderer's existing single-buffer mutex-protected
//!   path with `WatchdogedTripleBuffer<TerminalState>`.
//! - Wiring the GUI's frame timer to call `WatchdogedTripleBuffer::poll`
//!   at the same cadence that runs `IdleDetector::poll`
//!   (ft-mpc9b.5.3).
//! - Surfacing `WatchdogedHealth` through `ft doctor`.
//! - Per-pane scaling: today this wrapper assumes one outstanding
//!   acquire at a time (matches the TripleBuffer single-reader
//!   contract). For 200-pane fleets, the gui crate spins up one
//!   `WatchdogedTripleBuffer` per pane.
//! - Persistent-rope composition (sub-task 3.3 of ft-2okh0.3) — gated
//!   on br-ft-mpc9b.2.5 which currently lands a negative-result
//!   decision; revisit if/when that substrate ships.

#![allow(dead_code)]

use std::ops::Deref;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crate::triple_buffer::{PublishOutcome, TripleBuffer, TripleBufferHealth};
use crate::triple_buffer_watchdog::{
    TripleBufferWatchdog, WatchdogConfig, WatchdogDecision, WatchdogStats,
};

fn watchdog_guard(lock: &Mutex<TripleBufferWatchdog>) -> MutexGuard<'_, TripleBufferWatchdog> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            lock.clear_poison();
            poisoned.into_inner()
        }
    }
}

// ============================================================================
// Combined health payload
// ============================================================================

/// `ft doctor` payload combining slot health (from `TripleBuffer`) and
/// watchdog telemetry (from `TripleBufferWatchdog`). Plain data —
/// the gui crate can serialize / render this however it likes.
///
/// Fields are `pub(crate)` so external code can't replace
/// `triple_buffer` or `watchdog` wholesale (which would zero the
/// recovered-event signal `force_recycles_total` /
/// `force_recycle_invocations`). Use the read accessors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogedHealth {
    pub(crate) triple_buffer: TripleBufferHealth,
    pub(crate) watchdog: WatchdogStats,
    /// Snapshot of whether the watchdog currently has an outstanding
    /// acquire (i.e. a renderer holds a slot). Plain copy of
    /// `TripleBufferWatchdog::is_active()` taken under the same
    /// lock as `watchdog`.
    pub(crate) watchdog_active: bool,
}

impl WatchdogedHealth {
    #[must_use]
    pub fn triple_buffer(&self) -> &TripleBufferHealth {
        &self.triple_buffer
    }

    #[must_use]
    pub fn watchdog(&self) -> &WatchdogStats {
        &self.watchdog
    }

    #[must_use]
    pub const fn watchdog_active(&self) -> bool {
        self.watchdog_active
    }
}

// ============================================================================
// Acquire guard (RAII for record_release)
// ============================================================================

/// RAII guard returned by `WatchdogedTripleBuffer::acquire`. Holds
/// the `Arc<T>` snapshot the renderer reads from; on `Drop` calls
/// `record_release` on the watchdog so the policy state machine
/// returns to `Idle` and stops counting toward the warn / force-
/// recycle thresholds.
///
/// `WatchdogedAcquireGuard<T>` deref's to `T`, so the renderer can
/// treat it as a transparent `&T` reader.
pub struct WatchdogedAcquireGuard<T: Send + Sync + 'static> {
    snapshot: Arc<T>,
    watchdog: Arc<Mutex<TripleBufferWatchdog>>,
    released: bool,
}

impl<T: Send + Sync + 'static> WatchdogedAcquireGuard<T> {
    /// Explicitly release the guard before drop. After this call,
    /// `release()` is idempotent and the eventual drop is a no-op.
    /// Useful when the renderer wants the watchdog state machine to
    /// transition to `Idle` *before* the guard's lifetime expires
    /// (e.g. early exit from the paint loop).
    pub fn release(&mut self) {
        if !self.released {
            watchdog_guard(&self.watchdog).record_release();
            self.released = true;
        }
    }

    /// Borrow the underlying `Arc<T>` without consuming the guard.
    /// Mirrors `Arc::clone`-style ergonomics for callers that need to
    /// stash the snapshot in a parallel data structure (e.g. an
    /// accessibility query handler).
    #[must_use]
    pub fn snapshot_arc(&self) -> Arc<T> {
        Arc::clone(&self.snapshot)
    }

    /// Consume the guard, returning the inner `Arc<T>`. Calls
    /// `record_release` first so the watchdog returns to `Idle`.
    /// Useful when the snapshot needs to outlive the guard's RAII
    /// scope (rare; usually the renderer just drops the guard).
    #[must_use]
    pub fn into_snapshot(mut self) -> Arc<T> {
        self.release();
        Arc::clone(&self.snapshot)
    }

    /// Whether `release()` (or `into_snapshot()`) has already been
    /// called. After the first explicit release, subsequent drops
    /// are no-ops.
    #[must_use]
    pub fn is_released(&self) -> bool {
        self.released
    }
}

impl<T: Send + Sync + 'static> Deref for WatchdogedAcquireGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.snapshot
    }
}

impl<T: Send + Sync + 'static> Drop for WatchdogedAcquireGuard<T> {
    fn drop(&mut self) {
        // Drop is the universal release path; `release()` short-
        // circuits this when the caller already released explicitly.
        if !self.released {
            watchdog_guard(&self.watchdog).record_release();
        }
    }
}

// ============================================================================
// Polled decision
// ============================================================================

/// What the periodic poll did this tick. Carries the watchdog's
/// raw decision plus a flag indicating whether the wrapper actually
/// invoked `TripleBuffer::force_recycle` in response. Returned to
/// the gui caller so it can log / increment its own per-pane
/// counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolledDecision {
    pub(crate) watchdog: WatchdogDecision,
    /// `true` when this poll caused the wrapper to call
    /// `TripleBuffer::force_recycle()`. Each `ForceRecycle` decision
    /// triggers exactly one call (the watchdog's own
    /// `force_recycle_invocations` counter and this flag are kept in
    /// lock-step).
    pub(crate) force_recycled: bool,
}

impl PolledDecision {
    #[must_use]
    pub const fn no_op() -> Self {
        Self {
            watchdog: WatchdogDecision::NoOp,
            force_recycled: false,
        }
    }

    #[must_use]
    pub const fn is_no_op(&self) -> bool {
        matches!(self.watchdog, WatchdogDecision::NoOp) && !self.force_recycled
    }

    #[must_use]
    pub const fn watchdog(&self) -> WatchdogDecision {
        self.watchdog
    }

    #[must_use]
    pub const fn force_recycled(&self) -> bool {
        self.force_recycled
    }
}

// ============================================================================
// WatchdogedTripleBuffer
// ============================================================================

/// `TripleBuffer<T>` + `TripleBufferWatchdog` composed behind a
/// thread-safe handle. Cheap to clone (everything's behind an `Arc`).
/// One instance per pane; the gui crate stores them in a per-pane
/// map.
pub struct WatchdogedTripleBuffer<T: Send + Sync + 'static> {
    inner: Arc<TripleBuffer<T>>,
    watchdog: Arc<Mutex<TripleBufferWatchdog>>,
}

impl<T: Send + Sync + 'static> Clone for WatchdogedTripleBuffer<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            watchdog: Arc::clone(&self.watchdog),
        }
    }
}

impl<T: Send + Sync + 'static> WatchdogedTripleBuffer<T> {
    /// Build a wrapper with the watchdog's default thresholds
    /// (`DEFAULT_WARN_AFTER` = 1 s, `DEFAULT_FORCE_RECYCLE_AFTER` = 5 s).
    pub fn new(initial: T) -> Self {
        Self::with_watchdog_config(initial, WatchdogConfig::default())
    }

    /// Build a wrapper with operator-tuned watchdog thresholds.
    pub fn with_watchdog_config(initial: T, config: WatchdogConfig) -> Self {
        Self {
            inner: Arc::new(TripleBuffer::new(initial)),
            watchdog: Arc::new(Mutex::new(TripleBufferWatchdog::with_config(config))),
        }
    }

    /// Acquire a snapshot. Returns an RAII guard that calls
    /// `record_release` on drop. The renderer derefs the guard to
    /// read `T`.
    ///
    /// `now` is the timestamp the renderer uses for "frame start";
    /// the watchdog measures outstanding-time against this. The
    /// renderer's frame-clock is the source of truth (not
    /// `Instant::now()` here) so tests can drive deterministic
    /// time.
    pub fn acquire(&self, now: Instant) -> WatchdogedAcquireGuard<T> {
        let snapshot = self.inner.acquire();
        watchdog_guard(&self.watchdog).record_acquire(now);
        WatchdogedAcquireGuard {
            snapshot,
            watchdog: Arc::clone(&self.watchdog),
            released: false,
        }
    }

    /// Publish a new value (input thread side). Delegates to
    /// `TripleBuffer::publish`.
    pub fn publish(&self, value: T) -> PublishOutcome {
        self.inner.publish(value)
    }

    /// Periodic poll. Drives the watchdog's tick and, on a
    /// `ForceRecycle` decision, invokes `TripleBuffer::force_recycle`
    /// before returning the combined `PolledDecision`. The wrapper
    /// owns the "decision → action" coupling so the gui caller's
    /// timer thread is just `let _ = wtb.poll(now);` — no chance of
    /// the integration forgetting to wire the force-recycle action.
    pub fn poll(&self, now: Instant) -> PolledDecision {
        let decision = watchdog_guard(&self.watchdog).poll(now);
        let force_recycled = matches!(decision, WatchdogDecision::ForceRecycle { .. });
        if force_recycled {
            self.inner.force_recycle();
        }
        PolledDecision {
            watchdog: decision,
            force_recycled,
        }
    }

    /// Force-recycle the slot regardless of watchdog state. Useful
    /// for the gui's "operator pressed `ft flush`" path. Bumps the
    /// underlying `TripleBuffer`'s `force_recycle` counter; the
    /// watchdog stats are *not* touched (this is an operator action,
    /// not a watchdog reaction).
    pub fn force_recycle(&self) {
        self.inner.force_recycle();
    }

    /// Combined health snapshot for `ft doctor`.
    pub fn health(&self) -> WatchdogedHealth {
        let triple_buffer = self.inner.health();
        let wd = watchdog_guard(&self.watchdog);
        let watchdog_stats = wd.stats().clone();
        let watchdog_active = wd.is_active();
        WatchdogedHealth {
            triple_buffer,
            watchdog: watchdog_stats,
            watchdog_active,
        }
    }

    /// Operator hook: re-tune the watchdog thresholds at runtime
    /// without rebuilding the wrapper. Returns the previous config
    /// AND prior stats atomically (under the same lock acquisition
    /// as the swap), so the integration layer can log a complete
    /// config-change audit record without a TOCTOU window.
    ///
    /// The reconfigure resets all stats. Capturing them in the
    /// returned tuple is the only audit-safe way to observe the
    /// pre-reset values.
    pub fn reconfigure_watchdog(
        &self,
        new_config: WatchdogConfig,
    ) -> (WatchdogConfig, WatchdogStats) {
        let mut wd = watchdog_guard(&self.watchdog);
        let prev_config = *wd.config();
        let prev_stats = wd.stats().clone();
        // Reconfigures rebuild the state machine with the new
        // thresholds. Stats are reset; the integration layer
        // captures the pre-reset stats from the returned tuple
        // (no separate health() call needed — TOCTOU-free).
        *wd = TripleBufferWatchdog::with_config(new_config);
        (prev_config, prev_stats)
    }

    /// Read-only handle to the underlying `TripleBuffer`. Mostly for
    /// tests; integration code should go through `acquire` /
    /// `publish` / `poll` so the watchdog state stays consistent.
    #[must_use]
    pub fn inner(&self) -> &Arc<TripleBuffer<T>> {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t0() -> Instant {
        Instant::now()
    }

    fn poison_watchdog_mutex<T: Send + Sync + 'static>(wtb: &WatchdogedTripleBuffer<T>) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = wtb
                .watchdog
                .lock()
                .expect("test watchdog mutex should start unpoisoned");
            panic!("poison watchdog mutex");
        }));
        assert!(result.is_err());
    }

    // ----------------------------------------------------------------
    // Acquire / drop / release lifecycle
    // ----------------------------------------------------------------

    #[test]
    fn fresh_wrapper_is_idle_and_inactive() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(42);
        let h = wtb.health();
        assert!(!h.watchdog_active);
        assert_eq!(h.watchdog.acquires_total, 0);
        assert_eq!(h.watchdog.releases_total, 0);
    }

    #[test]
    fn watchdoged_health_accessors_round_trip() {
        // Pin the read-only accessor surface — pub(crate)
        // fields can't be replaced wholesale from outside
        // the crate; only the accessors expose them.
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(0);
        let h = wtb.health();
        assert!(!h.watchdog_active());
        assert_eq!(h.triple_buffer().publishes_total(), 0);
        assert_eq!(h.watchdog().acquires_total, 0);
    }

    #[test]
    fn polled_decision_accessors_round_trip() {
        let d = PolledDecision::no_op();
        assert!(matches!(d.watchdog(), WatchdogDecision::NoOp));
        assert!(!d.force_recycled());
        assert!(d.is_no_op());
    }

    #[test]
    fn acquire_returns_snapshot_and_marks_active() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(42);
        let g = wtb.acquire(t0());
        assert_eq!(*g, 42);
        assert!(wtb.health().watchdog_active);
        assert_eq!(wtb.health().watchdog.acquires_total, 1);
    }

    #[test]
    fn drop_guard_releases_watchdog() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(42);
        {
            let _g = wtb.acquire(t0());
            assert!(wtb.health().watchdog_active);
        }
        let h = wtb.health();
        assert!(!h.watchdog_active);
        assert_eq!(h.watchdog.releases_total, 1);
    }

    #[test]
    fn watchdog_mutex_recovers_for_acquire_and_release() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(42);
        poison_watchdog_mutex(&wtb);

        let g = wtb.acquire(t0());
        let h = wtb.health();
        assert!(h.watchdog_active);
        assert_eq!(h.watchdog.acquires_total, 1);

        drop(g);
        let h = wtb.health();
        assert!(!h.watchdog_active);
        assert_eq!(h.watchdog.releases_total, 1);
    }

    #[test]
    fn explicit_release_idempotent() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(42);
        let mut g = wtb.acquire(t0());
        assert!(!g.is_released());
        g.release();
        assert!(g.is_released());
        assert_eq!(wtb.health().watchdog.releases_total, 1);
        // Second release is a no-op — counter stays at 1.
        g.release();
        assert!(g.is_released());
        assert_eq!(wtb.health().watchdog.releases_total, 1);
        // Drop after explicit release should also be a no-op.
        drop(g);
        assert_eq!(wtb.health().watchdog.releases_total, 1);
    }

    #[test]
    fn into_snapshot_releases_then_returns_arc() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(99);
        let g = wtb.acquire(t0());
        let snap = g.into_snapshot();
        assert_eq!(*snap, 99);
        assert!(!wtb.health().watchdog_active);
        assert_eq!(wtb.health().watchdog.releases_total, 1);
    }

    #[test]
    fn snapshot_arc_does_not_release() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(7);
        let g = wtb.acquire(t0());
        let extra = g.snapshot_arc();
        assert_eq!(*extra, 7);
        assert!(
            wtb.health().watchdog_active,
            "snapshot_arc must not release"
        );
        drop(g);
        assert!(!wtb.health().watchdog_active);
    }

    // ----------------------------------------------------------------
    // Publish path
    // ----------------------------------------------------------------

    #[test]
    fn publish_updates_subsequent_acquire() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(1);
        let outcome = wtb.publish(2);
        let _ = outcome; // PublishOutcome carries triple-buffer stats; we just want it not to panic
        let g = wtb.acquire(t0());
        assert_eq!(*g, 2);
    }

    // ----------------------------------------------------------------
    // Poll / watchdog interactions
    // ----------------------------------------------------------------

    #[test]
    fn poll_idle_returns_no_op() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(0);
        let d = wtb.poll(t0());
        assert!(d.is_no_op());
        assert!(matches!(d.watchdog, WatchdogDecision::NoOp));
        assert!(!d.force_recycled);
    }

    #[test]
    fn poll_within_warn_horizon_is_no_op() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(0);
        let start = t0();
        let _g = wtb.acquire(start);
        let d = wtb.poll(start + Duration::from_millis(500));
        assert!(d.is_no_op());
    }

    #[test]
    fn poll_past_warn_emits_warn() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::with_watchdog_config(
            0,
            WatchdogConfig {
                warn_after: Duration::from_millis(100),
                force_recycle_after: Duration::from_secs(5),
            },
        );
        let start = t0();
        let _g = wtb.acquire(start);
        let d = wtb.poll(start + Duration::from_millis(150));
        assert!(matches!(d.watchdog, WatchdogDecision::Warn { .. }));
        assert!(!d.force_recycled);
        assert_eq!(wtb.health().watchdog.warnings_emitted, 1);
    }

    #[test]
    fn poll_past_force_recycle_triggers_force_recycle() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::with_watchdog_config(
            0,
            WatchdogConfig {
                warn_after: Duration::from_millis(100),
                force_recycle_after: Duration::from_millis(500),
            },
        );
        let start = t0();
        let _g = wtb.acquire(start);
        let d = wtb.poll(start + Duration::from_millis(600));
        assert!(matches!(d.watchdog, WatchdogDecision::ForceRecycle { .. }));
        assert!(d.force_recycled, "wrapper should auto-invoke force_recycle");
        // Watchdog counter bumped.
        assert_eq!(wtb.health().watchdog.force_recycle_invocations, 1);
        // Underlying TripleBuffer's force_recycle counter also bumped.
        assert!(wtb.health().triple_buffer.has_force_recycled());
    }

    #[test]
    fn poll_recovers_poisoned_watchdog_mutex() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::with_watchdog_config(
            0,
            WatchdogConfig {
                warn_after: Duration::from_millis(100),
                force_recycle_after: Duration::from_millis(500),
            },
        );
        let start = t0();
        let _g = wtb.acquire(start);
        poison_watchdog_mutex(&wtb);

        let d = wtb.poll(start + Duration::from_millis(600));
        assert!(matches!(d.watchdog, WatchdogDecision::ForceRecycle { .. }));
        assert!(d.force_recycled);
        let h = wtb.health();
        assert_eq!(h.watchdog.force_recycle_invocations, 1);
        assert!(h.triple_buffer.has_force_recycled());
    }

    #[test]
    fn poll_warn_only_fires_once_per_acquire() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::with_watchdog_config(
            0,
            WatchdogConfig {
                warn_after: Duration::from_millis(100),
                force_recycle_after: Duration::from_secs(5),
            },
        );
        let start = t0();
        let _g = wtb.acquire(start);
        let d1 = wtb.poll(start + Duration::from_millis(150));
        let d2 = wtb.poll(start + Duration::from_millis(200));
        let d3 = wtb.poll(start + Duration::from_millis(250));
        assert!(matches!(d1.watchdog, WatchdogDecision::Warn { .. }));
        assert!(
            d2.is_no_op(),
            "second poll within force-recycle horizon must not re-warn"
        );
        assert!(d3.is_no_op());
        assert_eq!(wtb.health().watchdog.warnings_emitted, 1);
    }

    #[test]
    fn release_after_warn_resets_for_next_acquire() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::with_watchdog_config(
            0,
            WatchdogConfig {
                warn_after: Duration::from_millis(100),
                force_recycle_after: Duration::from_secs(5),
            },
        );
        let start = t0();
        {
            let _g = wtb.acquire(start);
            let d = wtb.poll(start + Duration::from_millis(150));
            assert!(matches!(d.watchdog, WatchdogDecision::Warn { .. }));
        }
        // Drop released. New acquire should start fresh.
        let _g = wtb.acquire(start + Duration::from_millis(200));
        let d = wtb.poll(start + Duration::from_millis(250));
        // Within warn horizon (50ms < 100ms) of the new acquire.
        assert!(
            d.is_no_op(),
            "post-release new acquire must reset warn state"
        );
        // Older warning is still counted in lifetime stats.
        assert_eq!(wtb.health().watchdog.warnings_emitted, 1);
    }

    // ----------------------------------------------------------------
    // Crashed-renderer pattern
    // ----------------------------------------------------------------

    #[test]
    fn crashed_renderer_pattern_warns_then_force_recycles() {
        // Default config: 1s warn, 5s force-recycle.
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(0);
        let start = t0();
        // Renderer "crashes" — guard never drops. Simulate by leaking
        // a guard via std::mem::forget.
        let g = wtb.acquire(start);
        std::mem::forget(g);

        // Tick 1: 0.5s — no-op
        let d = wtb.poll(start + Duration::from_millis(500));
        assert!(d.is_no_op());

        // Tick 2: 1.5s — warn fires
        let d = wtb.poll(start + Duration::from_millis(1500));
        assert!(matches!(d.watchdog, WatchdogDecision::Warn { .. }));
        assert!(!d.force_recycled);

        // Tick 3: 3s — still warned, no-op
        let d = wtb.poll(start + Duration::from_secs(3));
        assert!(d.is_no_op());

        // Tick 4: 5s — force-recycle fires + auto-invokes inner.force_recycle
        let d = wtb.poll(start + Duration::from_secs(5));
        assert!(matches!(d.watchdog, WatchdogDecision::ForceRecycle { .. }));
        assert!(d.force_recycled);
        let h = wtb.health();
        assert_eq!(h.watchdog.warnings_emitted, 1);
        assert_eq!(h.watchdog.force_recycle_invocations, 1);
        assert!(h.triple_buffer.has_force_recycled());
    }

    // ----------------------------------------------------------------
    // operator-driven force_recycle
    // ----------------------------------------------------------------

    #[test]
    fn operator_force_recycle_does_not_bump_watchdog() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(0);
        wtb.force_recycle();
        let h = wtb.health();
        assert_eq!(
            h.watchdog.force_recycle_invocations, 0,
            "operator-driven force_recycle is not a watchdog reaction"
        );
        assert!(h.triple_buffer.has_force_recycled());
    }

    // ----------------------------------------------------------------
    // Reconfigure
    // ----------------------------------------------------------------

    #[test]
    fn reconfigure_replaces_thresholds() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(0);
        let new_cfg = WatchdogConfig {
            warn_after: Duration::from_millis(50),
            force_recycle_after: Duration::from_millis(200),
        };
        let prev = wtb.reconfigure_watchdog(new_cfg);
        // Previous config was the default (1s / 5s); confirm the
        // returned previous config matches that. Tuple now also
        // carries prior stats — TOCTOU-free audit capture.
        let (prev_config, prev_stats) = prev;
        assert_eq!(prev_config.warn_after, Duration::from_secs(1));
        assert_eq!(prev_config.force_recycle_after, Duration::from_secs(5));
        // Default stats: nothing happened yet, all counters zero.
        assert_eq!(prev_stats.acquires_total, 0);
        assert_eq!(prev_stats.releases_total, 0);
        assert_eq!(prev_stats.force_recycle_invocations, 0);
        // New thresholds in effect: 60ms-old acquire should fire warn.
        let start = t0();
        let _g = wtb.acquire(start);
        let d = wtb.poll(start + Duration::from_millis(60));
        assert!(
            matches!(d.watchdog, WatchdogDecision::Warn { .. }),
            "after reconfigure, 60ms acquire should breach the new 50ms warn threshold"
        );
    }

    #[test]
    fn reconfigure_recovers_poisoned_watchdog_mutex() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(0);
        poison_watchdog_mutex(&wtb);

        let new_cfg = WatchdogConfig {
            warn_after: Duration::from_millis(10),
            force_recycle_after: Duration::from_millis(20),
        };
        let (prev_config, prev_stats) = wtb.reconfigure_watchdog(new_cfg);
        assert_eq!(prev_config.warn_after, Duration::from_secs(1));
        assert_eq!(prev_config.force_recycle_after, Duration::from_secs(5));
        assert_eq!(prev_stats.acquires_total, 0);

        let start = t0();
        let _g = wtb.acquire(start);
        let d = wtb.poll(start + Duration::from_millis(15));
        assert!(matches!(d.watchdog, WatchdogDecision::Warn { .. }));
    }

    // ----------------------------------------------------------------
    // Clone semantics
    // ----------------------------------------------------------------

    #[test]
    fn clone_shares_watchdog_state() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(0);
        let cloned = wtb.clone();
        let _g = cloned.acquire(t0());
        // Both handles see the same watchdog state.
        assert!(wtb.health().watchdog_active);
        assert!(cloned.health().watchdog_active);
        drop(_g);
        assert!(!wtb.health().watchdog_active);
        assert!(!cloned.health().watchdog_active);
    }

    #[test]
    fn clone_shares_publish_path() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(1);
        let cloned = wtb.clone();
        cloned.publish(99);
        let g = wtb.acquire(t0());
        assert_eq!(*g, 99, "publish via clone is visible via original handle");
    }

    // ----------------------------------------------------------------
    // Health combined view
    // ----------------------------------------------------------------

    #[test]
    fn health_combines_triple_buffer_and_watchdog() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(0);
        // Drive some activity.
        wtb.publish(1);
        wtb.publish(2);
        let _g = wtb.acquire(t0());

        let h = wtb.health();
        // TripleBufferHealth has its own internal counters; just
        // confirm we're getting both halves filled in.
        assert!(h.watchdog_active);
        assert_eq!(h.watchdog.acquires_total, 1);
    }

    #[test]
    fn polled_decision_is_no_op_helper() {
        let n = PolledDecision::no_op();
        assert!(n.is_no_op());
        assert!(matches!(n.watchdog, WatchdogDecision::NoOp));
        assert!(!n.force_recycled);
    }

    // ----------------------------------------------------------------
    // Cross-cut: typical 100-frame render pattern
    // ----------------------------------------------------------------

    #[test]
    fn typical_render_loop_zero_warnings_zero_force_recycles() {
        let wtb: WatchdogedTripleBuffer<u32> = WatchdogedTripleBuffer::new(0);
        let mut t = t0();
        for frame in 0..100u32 {
            wtb.publish(frame);
            let g = wtb.acquire(t);
            // Simulated render takes 2ms.
            t += Duration::from_millis(2);
            // Periodic poll fires at frame end.
            let d = wtb.poll(t);
            assert!(
                d.is_no_op(),
                "no-op expected during normal render frame {frame}"
            );
            drop(g);
            // Frame budget: 16.6ms total at 60Hz; we have ~14ms idle.
            t += Duration::from_millis(14);
        }
        let h = wtb.health();
        assert_eq!(h.watchdog.warnings_emitted, 0);
        assert_eq!(h.watchdog.force_recycle_invocations, 0);
        assert_eq!(h.watchdog.acquires_total, 100);
        assert_eq!(h.watchdog.releases_total, 100);
        assert!(!h.watchdog_active);
    }
}
