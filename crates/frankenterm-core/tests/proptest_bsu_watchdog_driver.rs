use proptest::prelude::*;

use frankenterm_core::bsu_watchdog_driver::{evaluate_watchdog_tick, WatchdogTickAction};
use frankenterm_core::sync_output_watchdog::{BsuDepthCounter, WatchdogConfig, WatchdogState};

fn arb_watchdog_config() -> impl Strategy<Value = WatchdogConfig> {
    (0u32..=1_000, 0u32..=1_000).prop_map(|(timeout_ms, min_timeout_ms)| {
        WatchdogConfig::default()
            .with_timeout_ms(timeout_ms)
            .with_min_timeout_ms(min_timeout_ms)
    })
}

fn arb_nonzero_unfloored_config() -> impl Strategy<Value = WatchdogConfig> {
    (1u32..=1_000, 0u32..=1_000)
        .prop_filter(
            "minimum timeout must not exceed configured timeout",
            |(timeout_ms, min_timeout_ms)| min_timeout_ms <= timeout_ms,
        )
        .prop_map(|(timeout_ms, min_timeout_ms)| {
            WatchdogConfig::default()
                .with_timeout_ms(timeout_ms)
                .with_min_timeout_ms(min_timeout_ms)
        })
}

fn open_depth(open_count: u8) -> BsuDepthCounter {
    let mut depth = BsuDepthCounter::new();
    for _ in 0..open_count {
        let _ = depth.open_bsu();
    }
    depth
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_bsu_watchdog_driver_idle_without_bsu_is_noop(
        now_ms in any::<u64>(),
        config in arb_watchdog_config(),
    ) {
        let depth = BsuDepthCounter::new();
        let mut watchdog = WatchdogState::Idle;

        prop_assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, now_ms, config),
            WatchdogTickAction::NoOp,
        );
        prop_assert!(watchdog.is_idle());
    }

    #[test]
    fn proptest_bsu_watchdog_driver_idle_with_open_bsu_arms_saturating_deadline(
        open_count in 1u8..=32,
        now_ms in any::<u64>(),
        config in arb_watchdog_config(),
    ) {
        let depth = open_depth(open_count);
        let mut watchdog = WatchdogState::Idle;
        let expected_deadline =
            now_ms.saturating_add(u64::from(config.effective_timeout_ms()));

        prop_assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, now_ms, config),
            WatchdogTickAction::ArmTimer {
                deadline_ms: expected_deadline,
            },
        );
        prop_assert!(watchdog.is_idle());
    }

    #[test]
    fn proptest_bsu_watchdog_driver_pending_open_bsu_waits_before_deadline(
        open_count in 1u8..=32,
        start_ms in 0u64..=u64::MAX - 1_001,
        config in arb_nonzero_unfloored_config(),
    ) {
        let depth = open_depth(open_count);
        let mut watchdog = WatchdogState::Idle;
        watchdog.arm(start_ms, config);
        let before_deadline = start_ms + u64::from(config.effective_timeout_ms()) - 1;

        prop_assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, before_deadline, config),
            WatchdogTickAction::WaitForTimer,
        );
        prop_assert!(watchdog.is_pending());
    }

    #[test]
    fn proptest_bsu_watchdog_driver_pending_open_bsu_fires_once_at_or_after_deadline(
        open_count in 1u8..=32,
        start_ms in 0u64..=u64::MAX - 2_001,
        config in arb_nonzero_unfloored_config(),
        extra_ms in 0u64..=1_000,
    ) {
        let depth = open_depth(open_count);
        let mut watchdog = WatchdogState::Idle;
        watchdog.arm(start_ms, config);
        let at_or_after_deadline =
            start_ms + u64::from(config.effective_timeout_ms()) + extra_ms;

        prop_assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, at_or_after_deadline, config),
            WatchdogTickAction::FireForceFlush,
        );
        prop_assert!(watchdog.is_triggered());
        prop_assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, at_or_after_deadline, config),
            WatchdogTickAction::NoOp,
        );
        prop_assert!(watchdog.is_triggered());
    }

    #[test]
    fn proptest_bsu_watchdog_driver_pending_depth_zero_disarms_instead_of_firing(
        start_ms in 0u64..=u64::MAX - 2_001,
        config in arb_nonzero_unfloored_config(),
        extra_ms in 0u64..=1_000,
    ) {
        let depth = BsuDepthCounter::new();
        let mut watchdog = WatchdogState::Idle;
        watchdog.arm(start_ms, config);
        let at_or_after_deadline =
            start_ms + u64::from(config.effective_timeout_ms()) + extra_ms;

        prop_assert_eq!(
            evaluate_watchdog_tick(&depth, &mut watchdog, at_or_after_deadline, config),
            WatchdogTickAction::DisarmAfterEsu,
        );
        prop_assert!(watchdog.is_pending());
    }
}
