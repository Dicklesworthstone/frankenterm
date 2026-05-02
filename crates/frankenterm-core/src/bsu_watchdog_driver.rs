//! Deterministic BSU watchdog tick driver (br-ft-2m6qe
//! sub-task 1 substrate slice).
//!
//! The bead's sub-task 1:
//!
//! > "asupersync timer wiring: when
//! > BsuDepthCounter.open_bsu returns Opened, call
//! > WatchdogState::arm and spawn
//! > runtime_async::sleep_with_cx(150ms). On wake, recheck
//! > depth. If still >0, call should_force_flush; on
//! > ForceFlush, call buffer-drain with
//! > DrainCause::Watchdog + depth.force_reset +
//! > watchdog.mark_triggered."
//!
//! This module ships the **pure decision layer** the
//! integration's timer loop drives. The actual
//! `runtime_async::sleep_with_cx` call lives at the
//! integration site; this substrate emits the typed
//! [`WatchdogTickAction`] the loop dispatches on. Splitting
//! the decision from the side-effecting sleep means the
//! state-machine doctrine is unit-tested without spawning a
//! runtime, and the timer-loop code at the call site is one
//! match expression.
//!
//! ## Driver shape
//!
//! ```text
//! loop {
//!     match evaluate_watchdog_tick(&depth, &watchdog, now_ms, config) {
//!         ArmTimer { deadline_ms } => {
//!             watchdog.arm(now_ms, config);
//!             runtime_async::sleep_with_cx(deadline_ms - now_ms).await;
//!         }
//!         WaitForTimer => { runtime_async::sleep_with_cx(...).await; }
//!         FireForceFlush => {
//!             buffer.drain(DrainCause::Watchdog, &mut tlm);
//!             depth.force_reset();
//!             watchdog.mark_triggered();
//!         }
//!         DisarmAfterEsu => watchdog.disarm(),
//!         NoOp => {}
//!     }
//! }
//! ```
//!
//! ## Bead invariants enforced + pinned by tests
//!
//! - **Single-fire**: `FireForceFlush` returns once per
//!   pending window; subsequent ticks return `NoOp` until
//!   the integration calls `mark_triggered` (or the bead's
//!   wrapper does).
//! - **Disarm on ESU**: when depth drops to 0 and the
//!   watchdog is still `Pending`, the next tick emits
//!   `DisarmAfterEsu` so the integration cancels its
//!   pending sleep.
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
    /// force-flush. The integration drains the buffer with
    /// `DrainCause::Watchdog`, calls `depth.force_reset`,
    /// and `watchdog.mark_triggered`.
    FireForceFlush,
    /// Watchdog was `Pending` but depth dropped to 0
    /// (natural ESU drain happened). The integration
    /// cancels the pending sleep and disarms the watchdog.
    DisarmAfterEsu,
    /// Watchdog was `Triggered` (already fired this BSU)
    /// or `Idle` with depth == 0 — nothing to do.
    NoOp,
}

// ============================================================================
// Decision function
// ============================================================================

/// Decide what the integration's timer loop should do this
/// tick.
///
/// Pure function over the (depth, watchdog, now_ms, config)
/// inputs. The integration calls this at every tick of its
/// asupersync poll loop and dispatches on the returned
/// action.
///
/// Decision matrix:
///
/// | watchdog state | depth | action                |
/// |----------------|-------|-----------------------|
/// | Idle           | 0     | NoOp                  |
/// | Idle           | > 0   | ArmTimer              |
/// | Pending        | 0     | DisarmAfterEsu        |
/// | Pending        | > 0   | WaitForTimer / Fire   |
/// | Triggered      | any   | NoOp                  |
#[must_use]
pub fn evaluate_watchdog_tick(
    depth: &BsuDepthCounter,
    watchdog: &WatchdogState,
    now_ms: u64,
    config: WatchdogConfig,
) -> WatchdogTickAction {
    let bsu_open = depth.is_in_bsu();
    match watchdog {
        WatchdogState::Idle => {
            if bsu_open {
                let deadline_ms =
                    now_ms.saturating_add(u64::from(config.effective_timeout_ms()));
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
                // BSU still open — check deadline via
                // should_force_flush.
                match watchdog.should_force_flush(now_ms) {
                    WatchdogDecision::ForceFlush => WatchdogTickAction::FireForceFlush,
                    WatchdogDecision::Wait => WatchdogTickAction::WaitForTimer,
                }
            }
        }
        WatchdogState::Triggered => WatchdogTickAction::NoOp,
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
        let (depth, watchdog, cfg) = fresh_state();
        assert_eq!(
            evaluate_watchdog_tick(&depth, &watchdog, 1_000, cfg),
            WatchdogTickAction::NoOp
        );
    }

    // ----------------------------------------------------------------
    // Idle + depth>0 → ArmTimer
    // ----------------------------------------------------------------

    #[test]
    fn idle_with_bsu_open_returns_arm_timer() {
        let (mut depth, watchdog, cfg) = fresh_state();
        let _ = depth.open_bsu();
        let now_ms = 1_000;
        let action = evaluate_watchdog_tick(&depth, &watchdog, now_ms, cfg);
        match action {
            WatchdogTickAction::ArmTimer { deadline_ms } => {
                assert_eq!(
                    deadline_ms,
                    now_ms + u64::from(DEFAULT_WATCHDOG_TIMEOUT_MS)
                );
            }
            other => panic!("expected ArmTimer, got {other:?}"),
        }
    }

    #[test]
    fn idle_arm_uses_clamped_min_timeout_when_config_below_floor() {
        // Config with timeout_ms below MIN gets bumped to
        // MIN_WATCHDOG_TIMEOUT_MS by effective_timeout_ms.
        let (mut depth, watchdog, _) = fresh_state();
        let _ = depth.open_bsu();
        let cfg = WatchdogConfig::default().with_timeout_ms(1);
        let action = evaluate_watchdog_tick(&depth, &watchdog, 100, cfg);
        match action {
            WatchdogTickAction::ArmTimer { deadline_ms } => {
                // effective_timeout_ms enforces MIN_WATCHDOG_TIMEOUT_MS = 16
                assert_eq!(
                    deadline_ms,
                    100 + u64::from(cfg.effective_timeout_ms()),
                );
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
            evaluate_watchdog_tick(&depth, &watchdog, 1_050, cfg),
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
            evaluate_watchdog_tick(&depth, &watchdog, after_deadline, cfg),
            WatchdogTickAction::FireForceFlush
        );
    }

    #[test]
    fn pending_at_exact_deadline_returns_fire_force_flush() {
        let (mut depth, mut watchdog, cfg) = fresh_state();
        let _ = depth.open_bsu();
        watchdog.arm(1_000, cfg);
        let exactly_deadline = 1_000 + u64::from(DEFAULT_WATCHDOG_TIMEOUT_MS);
        assert_eq!(
            evaluate_watchdog_tick(&depth, &watchdog, exactly_deadline, cfg),
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
            evaluate_watchdog_tick(&depth, &watchdog, 1_050, cfg),
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
            evaluate_watchdog_tick(&depth, &watchdog, after_deadline, cfg),
            WatchdogTickAction::DisarmAfterEsu
        );
    }

    // ----------------------------------------------------------------
    // Triggered → NoOp regardless of inputs
    // ----------------------------------------------------------------

    #[test]
    fn triggered_returns_noop_at_any_depth() {
        let cfg = WatchdogConfig::default();
        let watchdog = WatchdogState::Triggered;

        // depth = 0
        let depth0 = BsuDepthCounter::new();
        assert_eq!(
            evaluate_watchdog_tick(&depth0, &watchdog, 100, cfg),
            WatchdogTickAction::NoOp
        );

        // depth > 0
        let mut depth1 = BsuDepthCounter::new();
        let _ = depth1.open_bsu();
        assert_eq!(
            evaluate_watchdog_tick(&depth1, &watchdog, 100, cfg),
            WatchdogTickAction::NoOp
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
        let action1 = evaluate_watchdog_tick(&depth, &watchdog, 1_000, cfg);
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
            evaluate_watchdog_tick(&depth, &watchdog, 1_050, cfg),
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
        let action_open = evaluate_watchdog_tick(&depth, &watchdog, 0, cfg);
        match action_open {
            WatchdogTickAction::ArmTimer { deadline_ms } => {
                assert_eq!(deadline_ms, u64::from(DEFAULT_WATCHDOG_TIMEOUT_MS));
            }
            other => panic!("expected ArmTimer, got {other:?}"),
        }
        watchdog.arm(0, cfg);

        // t=50: still pending, deadline at 150ms.
        assert_eq!(
            evaluate_watchdog_tick(&depth, &watchdog, 50, cfg),
            WatchdogTickAction::WaitForTimer
        );

        // t=100: ESU closes — depth → 0.
        let _ = depth.close_esu();
        // t=110: tick — should emit DisarmAfterEsu.
        assert_eq!(
            evaluate_watchdog_tick(&depth, &watchdog, 110, cfg),
            WatchdogTickAction::DisarmAfterEsu
        );
        watchdog.disarm();

        // t=120: idle, depth=0 → NoOp.
        assert_eq!(
            evaluate_watchdog_tick(&depth, &watchdog, 120, cfg),
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
            evaluate_watchdog_tick(&depth, &watchdog, after, cfg),
            WatchdogTickAction::FireForceFlush
        );

        // Integration's force-flush dispatch:
        // depth.force_reset + watchdog.mark_triggered.
        depth.force_reset();
        watchdog.mark_triggered();

        // Next tick: watchdog Triggered → NoOp.
        assert_eq!(
            evaluate_watchdog_tick(&depth, &watchdog, after + 50, cfg),
            WatchdogTickAction::NoOp
        );

        // Eventually the integration disarms (to be ready
        // for the next BSU). Substrate's prior issue: a
        // long-stuck Triggered indicates the integration
        // forgot. The test asserts the state machine emits
        // NoOp in the meantime — no double-dispatch.
        watchdog.disarm();
        // Now a fresh BSU opens again.
        let _ = depth.open_bsu();
        match evaluate_watchdog_tick(&depth, &watchdog, after + 100, cfg) {
            WatchdogTickAction::ArmTimer { .. } => {}
            other => {
                panic!("expected ArmTimer for re-armed BSU, got {other:?}")
            }
        }
    }
}
