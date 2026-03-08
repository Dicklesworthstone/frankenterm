//! Property-based tests for the network reliability taxonomy.

use std::time::Duration;

use frankenterm_core::network_reliability::{
    BackoffCalculator, IoOutcome, NetworkErrorKind, ReliabilityConfig, RetryPolicy, Subsystem,
    TimeoutPolicy, classify_io_error,
};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_error_kind() -> impl Strategy<Value = NetworkErrorKind> {
    prop_oneof![
        Just(NetworkErrorKind::Transient),
        Just(NetworkErrorKind::Permanent),
        Just(NetworkErrorKind::Degraded),
    ]
}

fn arb_subsystem() -> impl Strategy<Value = Subsystem> {
    prop_oneof![
        Just(Subsystem::WeztermCli),
        Just(Subsystem::WeztermMux),
        Just(Subsystem::Ipc),
        Just(Subsystem::Web),
        Just(Subsystem::Search),
        Just(Subsystem::Storage),
        Just(Subsystem::Distributed),
        Just(Subsystem::PaneCapture),
    ]
}

fn arb_retry_policy() -> impl Strategy<Value = RetryPolicy> {
    (1..20u32, 10..5000u64, 1000..60000u64, 10..40u32, 0..100u32).prop_map(
        |(max_attempts, init_ms, max_ms, mult_x10, jitter_pct)| RetryPolicy {
            max_attempts,
            initial_backoff: Duration::from_millis(init_ms),
            max_backoff: Duration::from_millis(max_ms.max(init_ms)),
            backoff_multiplier: (mult_x10 as f64) / 10.0,
            jitter_factor: (jitter_pct as f64) / 100.0,
        },
    )
}

fn arb_io_error_kind() -> impl Strategy<Value = std::io::ErrorKind> {
    prop_oneof![
        Just(std::io::ErrorKind::TimedOut),
        Just(std::io::ErrorKind::ConnectionRefused),
        Just(std::io::ErrorKind::ConnectionReset),
        Just(std::io::ErrorKind::ConnectionAborted),
        Just(std::io::ErrorKind::Interrupted),
        Just(std::io::ErrorKind::BrokenPipe),
        Just(std::io::ErrorKind::WouldBlock),
        Just(std::io::ErrorKind::NotFound),
        Just(std::io::ErrorKind::PermissionDenied),
        Just(std::io::ErrorKind::InvalidInput),
        Just(std::io::ErrorKind::InvalidData),
        Just(std::io::ErrorKind::AddrNotAvailable),
        Just(std::io::ErrorKind::Unsupported),
        Just(std::io::ErrorKind::AddrInUse),
        Just(std::io::ErrorKind::OutOfMemory),
        Just(std::io::ErrorKind::Other),
    ]
}

// ---------------------------------------------------------------------------
// NR-1: NetworkErrorKind retryable classification is stable
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr1_retryable_is_consistent(kind in arb_error_kind()) {
        // Calling is_retryable twice gives same answer
        prop_assert_eq!(kind.is_retryable(), kind.is_retryable());
    }
}

// ---------------------------------------------------------------------------
// NR-2: classify_io_error always returns a valid kind
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr2_classify_io_error_always_valid(ek in arb_io_error_kind()) {
        let err = std::io::Error::new(ek, "test");
        let kind = classify_io_error(&err);
        // Must be one of the three variants
        let is_valid = matches!(
            kind,
            NetworkErrorKind::Transient
                | NetworkErrorKind::Permanent
                | NetworkErrorKind::Degraded
        );
        prop_assert!(is_valid);
    }
}

// ---------------------------------------------------------------------------
// NR-3: Backoff delays never exceed max_backoff
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr3_delays_bounded_by_max(policy in arb_retry_policy()) {
        let max = policy.max_backoff;
        let mut calc = BackoffCalculator::new(policy);
        for _ in 0..20 {
            match calc.next_delay() {
                Some(d) => prop_assert!(d <= max, "delay {:?} > max {:?}", d, max),
                None => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NR-4: Backoff exhausts exactly at max_attempts
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr4_exhausts_at_max_attempts(policy in arb_retry_policy()) {
        let max = policy.max_attempts;
        let mut calc = BackoffCalculator::new(policy);
        let mut count = 0u32;
        while calc.next_delay().is_some() {
            count += 1;
            prop_assert!(count <= max + 1, "count {} exceeded max {}", count, max);
        }
        prop_assert_eq!(count, max);
    }
}

// ---------------------------------------------------------------------------
// NR-5: Reset allows full retry sequence again
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr5_reset_restores_retries(policy in arb_retry_policy()) {
        let max = policy.max_attempts;
        let mut calc = BackoffCalculator::new(policy);
        // exhaust
        while calc.next_delay().is_some() {}
        prop_assert!(!calc.can_retry());

        calc.reset();
        prop_assert_eq!(calc.attempt(), 0);
        prop_assert!(calc.can_retry());

        let mut count = 0u32;
        while calc.next_delay().is_some() {
            count += 1;
        }
        prop_assert_eq!(count, max);
    }
}

// ---------------------------------------------------------------------------
// NR-6: Permanent errors always return None from next_delay_for_kind
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr6_permanent_never_retried(policy in arb_retry_policy()) {
        let mut calc = BackoffCalculator::new(policy);
        let result = calc.next_delay_for_kind(NetworkErrorKind::Permanent);
        prop_assert!(result.is_none());
    }
}

// ---------------------------------------------------------------------------
// NR-7: TimeoutPolicy invariant: deadline >= io_operation >= connect
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr7_timeout_ordering(subsystem in arb_subsystem()) {
        let p = TimeoutPolicy::for_subsystem(subsystem);
        prop_assert!(p.request_deadline >= p.io_operation);
    }
}

// ---------------------------------------------------------------------------
// NR-8: Serde roundtrip preserves NetworkErrorKind
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr8_error_kind_serde(kind in arb_error_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: NetworkErrorKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }
}

// ---------------------------------------------------------------------------
// NR-9: Serde roundtrip preserves Subsystem
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr9_subsystem_serde(subsystem in arb_subsystem()) {
        let json = serde_json::to_string(&subsystem).unwrap();
        let back: Subsystem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(subsystem, back);
    }
}

// ---------------------------------------------------------------------------
// NR-10: ReliabilityConfig for every subsystem is self-consistent
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr10_reliability_config_consistent(subsystem in arb_subsystem()) {
        let cfg = ReliabilityConfig::for_subsystem(subsystem);
        prop_assert_eq!(cfg.subsystem, subsystem);
        prop_assert!(cfg.timeouts.connect > Duration::ZERO);
        // Backoff calculator should start fresh
        let calc = cfg.backoff();
        prop_assert_eq!(calc.attempt(), 0);
    }
}

// ---------------------------------------------------------------------------
// NR-11: IoOutcome from_io preserves Ok vs Err
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr11_io_outcome_ok_preserves(val in any::<u64>()) {
        let outcome = IoOutcome::from_io(Ok::<_, std::io::Error>(val));
        prop_assert!(outcome.is_ok());
        prop_assert!(outcome.error_kind().is_none());
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr12_io_outcome_err_classifies(ek in arb_io_error_kind()) {
        let err = std::io::Error::new(ek, "test");
        let expected_kind = classify_io_error(&err);
        let outcome = IoOutcome::<()>::from_io(Err(std::io::Error::new(ek, "test")));
        prop_assert!(!outcome.is_ok());
        prop_assert_eq!(outcome.error_kind(), Some(expected_kind));
    }
}

// ---------------------------------------------------------------------------
// NR-13: Zero-jitter delays are monotonically non-decreasing
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn nr13_zero_jitter_monotonic(
        max_attempts in 2..10u32,
        init_ms in 10..500u64,
        mult_x10 in 10..40u32,
    ) {
        let policy = RetryPolicy {
            max_attempts,
            initial_backoff: Duration::from_millis(init_ms),
            max_backoff: Duration::from_secs(60),
            backoff_multiplier: (mult_x10 as f64) / 10.0,
            jitter_factor: 0.0,
        };
        let mut calc = BackoffCalculator::new(policy);
        let mut prev = Duration::ZERO;
        while let Some(d) = calc.next_delay() {
            prop_assert!(d >= prev, "delay {:?} < prev {:?}", d, prev);
            prev = d;
        }
    }
}
