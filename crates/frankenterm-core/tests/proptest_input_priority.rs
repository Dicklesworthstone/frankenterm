use proptest::prelude::*;

use frankenterm_core::input_priority::{
    negotiate_priority, record_priority_outcome, safe_qos_class, safe_sched_fifo_priority,
    safe_windows_thread_priority, InputPriorityClass, MacOsQosClass, OsPriorityHint, Platform,
    PriorityFallbackReason, PriorityOutcomeStats, WindowsThreadPriority,
    DEFAULT_SCHED_FIFO_PRIORITY, SAFE_SCHED_FIFO_MAX,
};

fn arb_priority_class() -> impl Strategy<Value = InputPriorityClass> {
    prop_oneof![
        Just(InputPriorityClass::LowLatency),
        Just(InputPriorityClass::Normal),
    ]
}

fn arb_platform() -> impl Strategy<Value = Platform> {
    prop_oneof![
        Just(Platform::Linux),
        Just(Platform::MacOs),
        Just(Platform::Windows),
        Just(Platform::Other),
    ]
}

fn arb_macos_qos() -> impl Strategy<Value = MacOsQosClass> {
    prop_oneof![
        Just(MacOsQosClass::UserInteractive),
        Just(MacOsQosClass::UserInitiated),
        Just(MacOsQosClass::Default),
        Just(MacOsQosClass::Utility),
        Just(MacOsQosClass::Background),
    ]
}

fn arb_windows_priority() -> impl Strategy<Value = WindowsThreadPriority> {
    prop_oneof![
        Just(WindowsThreadPriority::TimeCritical),
        Just(WindowsThreadPriority::Highest),
        Just(WindowsThreadPriority::AboveNormal),
        Just(WindowsThreadPriority::Normal),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_input_priority_negotiation_matches_platform_policy(
        class in arb_priority_class(),
        platform in arb_platform(),
    ) {
        let negotiated = negotiate_priority(class, platform);

        match (class, platform, negotiated.hint) {
            (InputPriorityClass::Normal, _, OsPriorityHint::Default) => {
                prop_assert_eq!(
                    negotiated.fallback_reason,
                    Some(PriorityFallbackReason::NormalRequested),
                );
                prop_assert!(!negotiated.is_low_latency());
            }
            (InputPriorityClass::LowLatency, Platform::Linux, OsPriorityHint::Linux(hint)) => {
                prop_assert_eq!(hint.priority, DEFAULT_SCHED_FIFO_PRIORITY);
                prop_assert!((1..=SAFE_SCHED_FIFO_MAX).contains(&hint.priority));
                prop_assert_eq!(negotiated.fallback_reason, None);
                prop_assert!(negotiated.is_low_latency());
            }
            (InputPriorityClass::LowLatency, Platform::MacOs, OsPriorityHint::MacOs(class)) => {
                prop_assert_eq!(class, MacOsQosClass::UserInteractive);
                prop_assert_eq!(negotiated.fallback_reason, None);
                prop_assert!(negotiated.is_low_latency());
            }
            (InputPriorityClass::LowLatency, Platform::Windows, OsPriorityHint::Windows(priority)) => {
                prop_assert_eq!(priority, WindowsThreadPriority::TimeCritical);
                prop_assert_eq!(negotiated.fallback_reason, None);
                prop_assert!(negotiated.is_low_latency());
            }
            (InputPriorityClass::LowLatency, Platform::Other, OsPriorityHint::Default) => {
                prop_assert_eq!(
                    negotiated.fallback_reason,
                    Some(PriorityFallbackReason::UnsupportedPlatform),
                );
                prop_assert!(!negotiated.is_low_latency());
            }
            other => prop_assert!(false, "unexpected negotiation shape: {other:?}"),
        }
    }

    #[test]
    fn proptest_input_priority_sched_fifo_clamp_is_bounded_and_idempotent(
        requested in any::<u8>(),
    ) {
        let safe = safe_sched_fifo_priority(requested);
        prop_assert!((1..=SAFE_SCHED_FIFO_MAX).contains(&safe.priority));
        prop_assert_eq!(safe.clamped, requested == 0 || requested > SAFE_SCHED_FIFO_MAX);

        let second = safe_sched_fifo_priority(safe.priority);
        prop_assert_eq!(second.priority, safe.priority);
        prop_assert!(!second.clamped);
    }

    #[test]
    fn proptest_input_priority_qos_and_windows_clamps_are_safe_and_idempotent(
        qos in arb_macos_qos(),
        windows in arb_windows_priority(),
    ) {
        let safe_qos = safe_qos_class(qos);
        prop_assert!(matches!(
            safe_qos.class,
            MacOsQosClass::UserInteractive | MacOsQosClass::UserInitiated
        ));
        prop_assert_eq!(
            safe_qos.clamped,
            !matches!(qos, MacOsQosClass::UserInteractive | MacOsQosClass::UserInitiated),
        );
        prop_assert_eq!(safe_qos_class(safe_qos.class).clamped, false);

        let safe_windows = safe_windows_thread_priority(windows);
        prop_assert!(matches!(
            safe_windows.priority,
            WindowsThreadPriority::TimeCritical
                | WindowsThreadPriority::Highest
                | WindowsThreadPriority::AboveNormal
        ));
        prop_assert_eq!(safe_windows.clamped, windows == WindowsThreadPriority::Normal);
        prop_assert_eq!(
            safe_windows_thread_priority(safe_windows.priority).clamped,
            false,
        );
    }

    #[test]
    fn proptest_input_priority_record_grant_saturates_and_returns_running_total(
        start in any::<u64>(),
        grant_count in 0usize..256,
    ) {
        let mut stats = PriorityOutcomeStats {
            low_latency_grants_total: start,
            ..PriorityOutcomeStats::default()
        };

        let mut expected = start;
        for _ in 0..grant_count {
            expected = expected.saturating_add(1);
            prop_assert_eq!(stats.record_grant(), expected);
        }
        prop_assert_eq!(stats.low_latency_grants_total, expected);
        prop_assert_eq!(
            stats.is_healthy(),
            expected > 0 && stats.fallback_os_call_rejected_total == 0,
        );
    }

    #[test]
    fn proptest_input_priority_record_outcome_routes_negotiation_results(
        events in prop::collection::vec((arb_priority_class(), arb_platform(), any::<bool>()), 0..128),
    ) {
        let mut stats = PriorityOutcomeStats::default();
        let mut expected_grants = 0u64;
        let mut expected_normal = 0u64;
        let mut expected_unsupported = 0u64;
        let mut expected_rejected = 0u64;

        for (class, platform, applied) in events {
            let negotiated = negotiate_priority(class, platform);
            record_priority_outcome(&mut stats, negotiated, applied);

            match negotiated.fallback_reason {
                Some(PriorityFallbackReason::NormalRequested) => {
                    expected_normal = expected_normal.saturating_add(1);
                }
                Some(PriorityFallbackReason::UnsupportedPlatform) => {
                    expected_unsupported = expected_unsupported.saturating_add(1);
                }
                Some(PriorityFallbackReason::OsCallRejected) => {
                    expected_rejected = expected_rejected.saturating_add(1);
                }
                None if applied => {
                    expected_grants = expected_grants.saturating_add(1);
                }
                None => {
                    expected_rejected = expected_rejected.saturating_add(1);
                }
            }
        }

        prop_assert_eq!(stats.low_latency_grants_total, expected_grants);
        prop_assert_eq!(stats.normal_requests_total, expected_normal);
        prop_assert_eq!(stats.fallback_unsupported_platform_total, expected_unsupported);
        prop_assert_eq!(stats.fallback_os_call_rejected_total, expected_rejected);
        prop_assert_eq!(
            stats.is_healthy(),
            expected_grants > 0 && expected_rejected == 0,
        );
    }
}
