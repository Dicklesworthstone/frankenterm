//! Deterministic BSU watchdog tick driver (br-ft-2m6qe
//! sub-task 1 substrate slice).
//!
//! The bead's sub-task 1:
//!
//! > "asupersync timer wiring: when
//! > BsuDepthCounter.open_bsu returns Opened, call
//! > WatchdogState::arm and spawn
//! > runtime_async::sleep_with_cx(150ms). On wake, recheck
//! > depth. If still >0, consume force-flush; on
//! > ForceFlush, call buffer-drain with
//! > DrainCause::Watchdog + depth.force_reset."
//!
//! This module ships the **deterministic decision/state-transition
//! layer** the integration's timer loop drives. The actual
//! `runtime_async::sleep_with_cx` call lives at the
//! integration site; this substrate emits the typed
//! [`WatchdogTickAction`] the loop dispatches on. Splitting
//! the timer/sleep side effects from the watchdog transition
//! means the state-machine doctrine is unit-tested without
//! spawning a runtime, and the timer-loop code at the call site
//! is one match expression.
//!
//! ## Driver shape
//!
//! ```text
//! loop {
//!     match evaluate_watchdog_tick(&depth, &mut watchdog, now_ms, config) {
//!         ArmTimer { deadline_ms } => {
//!             watchdog.arm(now_ms, config);
//!             runtime_async::sleep_with_cx(deadline_ms - now_ms).await;
//!         }
//!         WaitForTimer => { runtime_async::sleep_with_cx(...).await; }
//!         FireForceFlush => {
//!             buffer.drain(DrainCause::Watchdog, &mut tlm);
//!             depth.force_reset();
//!         }
//!         DisarmAfterEsu => watchdog.disarm(),
//!         NoOp => {}
//!     }
//! }
//! ```
//!
//! ## Bead invariants enforced + pinned by tests
//!
//! - **Single-fire**: `FireForceFlush` consumes the
//!   pending watchdog transition and returns once per
//!   pending window; subsequent ticks return `NoOp`.
//! - **Disarm after drain**: when depth drops to 0 and the
//!   watchdog is still active (`Pending` after natural ESU, or
//!   `Triggered` after force-flush reset), the next tick emits
//!   `DisarmAfterEsu` so the integration cancels its pending
//!   sleep and returns the watchdog to `Idle`.
//! - **Re-arm on nested BSU close-then-reopen**: after a
//!   `DisarmAfterEsu`, a fresh `open_bsu` should produce
//!   `ArmTimer` again.

use crate::sync_output_watchdog::{
    BsuDepthCounter, WatchdogConfig, WatchdogDecision, WatchdogState,
};

// ============================================================================
// WatchdogTickAction
// ============================================================================

/// Action the integration's timer loop takes per tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogTickAction {
    /// Watchdog state is `Idle` and depth > 0 (a BSU just
    /// opened) — arm the timer with the given deadline.
    /// The integration calls `WatchdogState::arm(now_ms,
    /// config)` then sleeps until `deadline_ms`.
    ArmTimer { deadline_ms: u64 },
    /// Watchdog is `Pending` and the deadline hasn't
    /// elapsed — keep waiting. The integration's outer
    /// loop continues sleeping.
    WaitForTimer,
    /// Deadline elapsed AND depth > 0 → fire the
    /// force-flush. The driver has already consumed the
    /// watchdog transition; the integration drains the buffer
    /// with `DrainCause::Watchdog` and calls
    /// `depth.force_reset`.
    FireForceFlush,
    /// Watchdog was active but depth dropped to 0 (natural ESU
    /// drain or post-watchdog force reset happened). The
    /// integration cancels the pending sleep and disarms the
    /// watchdog.
    DisarmAfterEsu,
    /// Watchdog was `Triggered` with depth still > 0 (already
    /// fired this BSU) or `Idle` with depth == 0 — nothing to do.
    NoOp,
}

// ============================================================================
// Decision function
// ============================================================================

/// Decide what the integration's timer loop should do this
/// tick.
///
/// Decision function over the (depth, watchdog, now_ms,
/// config) inputs. The integration calls this at every tick
/// of its asupersync poll loop and dispatches on the returned
/// action. `FireForceFlush` is intentionally consuming: the
/// watchdog state is transitioned to `Triggered` before the
/// action is returned, so repeated ticks cannot double-fire
/// the same pending window.
///
/// Decision matrix:
///
/// | watchdog state | depth | action                |
/// |----------------|-------|-----------------------|
/// | Idle           | 0     | NoOp                  |
/// | Idle           | > 0   | ArmTimer              |
/// | Pending        | 0     | DisarmAfterEsu        |
/// | Pending        | > 0   | WaitForTimer / Fire   |
/// | Triggered      | 0     | DisarmAfterEsu        |
/// | Triggered      | > 0   | NoOp                  |
#[must_use]
pub fn evaluate_watchdog_tick(
    depth: &BsuDepthCounter,
    watchdog: &mut WatchdogState,
    now_ms: u64,
    config: WatchdogConfig,
) -> WatchdogTickAction {
    let bsu_open = depth.is_in_bsu();
    match watchdog {
        WatchdogState::Idle => {
            if bsu_open {
                let deadline_ms = now_ms.saturating_add(u64::from(config.effective_timeout_ms()));
                WatchdogTickAction::ArmTimer { deadline_ms }
            } else {
                WatchdogTickAction::NoOp
            }
        }
        WatchdogState::Pending { .. } => {
            if !bsu_open {
                // Natural ESU drain — disarm.
                WatchdogTickAction::DisarmAfterEsu
            } else {
                // BSU still open — consume the deadline
                // transition atomically so repeated ticks
                // cannot dispatch duplicate force-flushes.
                match watchdog.consume_force_flush(now_ms) {
                    WatchdogDecision::ForceFlush => WatchdogTickAction::FireForceFlush,
                    WatchdogDecision::Wait => WatchdogTickAction::WaitForTimer,
                }
            }
        }
        WatchdogState::Triggered => {
            if bsu_open {
                WatchdogTickAction::NoOp
            } else {
                WatchdogTickAction::DisarmAfterEsu
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_output_watchdog::DEFAULT_WATCHDOG_TIMEOUT_MS;

    fn fresh_state() -> (BsuDepthCounter, WatchdogState, WatchdogConfig) {
        (
            BsuDepthCounter::new(),
            WatchdogState::Idle,
            WatchdogConfig::default(),
        )
    }

    // ----------------------------------------------------------------
    // Idle + depth=0 → NoOp
    // ----------------------------------------------------------------

    #[test]
    fn idle_with_no_bsu_returns_noop() {
        let (depth, mut watchdog, cfg) = fresh_state();
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, 1_000, cfg),
            WatchdogTickAction::NoOp
        );
    }

    // ----------------------------------------------------------------
    // Idle + depth>0 → ArmTimer
    // ----------------------------------------------------------------

    #[test]
    fn idle_with_bsu_open_returns_arm_timer() {
        let (mut depth, mut watchdog, cfg) = fresh_state();
        let _ = depth.open_bsu();
        let now_ms = 1_000;
        let action = evaluate_watchdog_tick(&depth, &mut watchdog, now_ms, cfg);
        match action {
            WatchdogTickAction::ArmTimer { deadline_ms } => {
                assert_eq!(deadline_ms, now_ms + u64::from(DEFAULT_WATCHDOG_TIMEOUT_MS));
            }
            other => panic!("expected ArmTimer, got {other:?}"),
        }
    }

    #[test]
    fn idle_arm_uses_clamped_min_timeout_when_config_below_floor() {
        // Config with timeout_ms below MIN gets bumped to
        // MIN_WATCHDOG_TIMEOUT_MS by effective_timeout_ms.
        let (mut depth, mut watchdog, _) = fresh_state();
        let _ = depth.open_bsu();
        let cfg = WatchdogConfig::default().with_timeout_ms(1);
        let action = evaluate_watchdog_tick(&depth, &mut watchdog, 100, cfg);
        match action {
            WatchdogTickAction::ArmTimer { deadline_ms } => {
                // effective_timeout_ms enforces MIN_WATCHDOG_TIMEOUT_MS = 16
                assert_eq!(deadline_ms, 100 + u64::from(cfg.effective_timeout_ms()),);
            }
            other => panic!("expected ArmTimer, got {other:?}"),
        }
    }

    // ----------------------------------------------------------------
    // Pending + deadline not elapsed → WaitForTimer
    // ----------------------------------------------------------------

    #[test]
    fn pending_with_deadline_in_future_returns_wait() {
        let (mut depth, mut watchdog, cfg) = fresh_state();
        let _ = depth.open_bsu();
        watchdog.arm(1_000, cfg);
        // Tick at 1_050 ms — well before the 150ms deadline.
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, 1_050, cfg),
            WatchdogTickAction::WaitForTimer
        );
    }

    // ----------------------------------------------------------------
    // Pending + deadline elapsed → FireForceFlush
    // ----------------------------------------------------------------

    #[test]
    fn pending_with_deadline_elapsed_returns_fire_force_flush() {
        let (mut depth, mut watchdog, cfg) = fresh_state();
        let _ = depth.open_bsu();
        watchdog.arm(1_000, cfg);
        let after_deadline = 1_000 + u64::from(DEFAULT_WATCHDOG_TIMEOUT_MS) + 1;
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, after_deadline, cfg),
            WatchdogTickAction::FireForceFlush
        );
        assert_eq!(watchdog, WatchdogState::Triggered);
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, after_deadline + 1, cfg),
            WatchdogTickAction::NoOp
        );
    }

    #[test]
    fn pending_at_exact_deadline_returns_fire_force_flush() {
        let (mut depth, mut watchdog, cfg) = fresh_state();
        let _ = depth.open_bsu();
        watchdog.arm(1_000, cfg);
        let exactly_deadline = 1_000 + u64::from(DEFAULT_WATCHDOG_TIMEOUT_MS);
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, exactly_deadline, cfg),
            WatchdogTickAction::FireForceFlush
        );
    }

    // ----------------------------------------------------------------
    // Pending + depth dropped to 0 → DisarmAfterEsu
    // ----------------------------------------------------------------

    #[test]
    fn pending_with_depth_zero_returns_disarm_after_esu() {
        // Open a BSU, arm the watchdog, then close the ESU
        // (depth → 0 via close_esu). The next tick should
        // emit DisarmAfterEsu so the integration cancels
        // its pending sleep.
        let (mut depth, mut watchdog, cfg) = fresh_state();
        let _ = depth.open_bsu();
        watchdog.arm(1_000, cfg);
        let _ = depth.close_esu(); // depth back to 0
        // Even before the deadline elapses, the watchdog
        // should disarm.
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, 1_050, cfg),
            WatchdogTickAction::DisarmAfterEsu
        );
    }

    #[test]
    fn pending_disarm_takes_priority_over_force_flush_when_depth_zero() {
        // Edge case: if both the deadline elapsed AND depth
        // dropped to 0 simultaneously, the integration should
        // disarm rather than force-flush — there's nothing
        // left in the buffer.
        let (mut depth, mut watchdog, cfg) = fresh_state();
        let _ = depth.open_bsu();
        watchdog.arm(1_000, cfg);
        let _ = depth.close_esu();
        let after_deadline = 1_000 + u64::from(DEFAULT_WATCHDOG_TIMEOUT_MS) + 100;
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, after_deadline, cfg),
            WatchdogTickAction::DisarmAfterEsu
        );
    }

    // ----------------------------------------------------------------
    // Triggered → NoOp regardless of inputs
    // ----------------------------------------------------------------

    #[test]
    fn triggered_with_open_bsu_returns_noop() {
        let cfg = WatchdogConfig::default();
        let mut watchdog = WatchdogState::Triggered;

        let mut depth1 = BsuDepthCounter::new();
        let _ = depth1.open_bsu();
        assert_eq!(
            evaluate_watchdog_tick(&depth1, &mut watchdog, 100, cfg),
            WatchdogTickAction::NoOp
        );
    }

    #[test]
    fn triggered_with_reset_depth_returns_disarm() {
        let cfg = WatchdogConfig::default();
        let mut watchdog = WatchdogState::Triggered;
        let depth = BsuDepthCounter::new();

        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, 100, cfg),
            WatchdogTickAction::DisarmAfterEsu
        );
    }

    // ----------------------------------------------------------------
    // Nested BSU: open + open returns arm only the first
    // time; subsequent ticks are WaitForTimer
    // ----------------------------------------------------------------

    #[test]
    fn nested_bsu_does_not_re_arm_existing_pending_state() {
        // Outer BSU opens at depth=0→1, watchdog arms.
        // Inner BSU at depth=1→2 should NOT re-arm; the
        // watchdog is already Pending.
        let (mut depth, mut watchdog, cfg) = fresh_state();
        let _ = depth.open_bsu(); // depth = 1
        let action1 = evaluate_watchdog_tick(&depth, &mut watchdog, 1_000, cfg);
        match action1 {
            WatchdogTickAction::ArmTimer { .. } => {}
            other => panic!("expected ArmTimer, got {other:?}"),
        }
        // Integration arms the watchdog.
        watchdog.arm(1_000, cfg);

        let _ = depth.open_bsu(); // depth = 2 (nested)
        // Tick again — watchdog already pending, depth>0,
        // deadline not elapsed.
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, 1_050, cfg),
            WatchdogTickAction::WaitForTimer
        );
    }

    // ----------------------------------------------------------------
    // End-to-end: clean BSU lifecycle
    // ----------------------------------------------------------------

    #[test]
    fn scenario_clean_open_close_disarm_sequence() {
        let (mut depth, mut watchdog, cfg) = fresh_state();
        // t=0: BSU opens.
        let _ = depth.open_bsu();
        let action_open = evaluate_watchdog_tick(&depth, &mut watchdog, 0, cfg);
        match action_open {
            WatchdogTickAction::ArmTimer { deadline_ms } => {
                assert_eq!(deadline_ms, u64::from(DEFAULT_WATCHDOG_TIMEOUT_MS));
            }
            other => panic!("expected ArmTimer, got {other:?}"),
        }
        watchdog.arm(0, cfg);

        // t=50: still pending, deadline at 150ms.
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, 50, cfg),
            WatchdogTickAction::WaitForTimer
        );

        // t=100: ESU closes — depth → 0.
        let _ = depth.close_esu();
        // t=110: tick — should emit DisarmAfterEsu.
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, 110, cfg),
            WatchdogTickAction::DisarmAfterEsu
        );
        watchdog.disarm();

        // t=120: idle, depth=0 → NoOp.
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, 120, cfg),
            WatchdogTickAction::NoOp
        );
    }

    #[test]
    fn scenario_force_flush_then_reset_then_reopen() {
        let (mut depth, mut watchdog, cfg) = fresh_state();
        // BSU opens, watchdog arms, deadline elapses.
        let _ = depth.open_bsu();
        watchdog.arm(0, cfg);
        let after = u64::from(DEFAULT_WATCHDOG_TIMEOUT_MS) + 1;
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, after, cfg),
            WatchdogTickAction::FireForceFlush
        );

        // Integration's force-flush dispatch:
        // depth.force_reset. The driver already consumed
        // the watchdog transition.
        depth.force_reset();

        // Next tick: depth has been reset, so the driver asks
        // the integration to disarm before the next BSU.
        assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, after + 50, cfg),
            WatchdogTickAction::DisarmAfterEsu
        );

        watchdog.disarm();
        // Now a fresh BSU opens again.
        let _ = depth.open_bsu();
        match evaluate_watchdog_tick(&depth, &mut watchdog, after + 100, cfg) {
            WatchdogTickAction::ArmTimer { .. } => {}
            other => {
                panic!("expected ArmTimer for re-armed BSU, got {other:?}")
            }
        }
    }
}
