//! Property-based tests for retry backoff invariants.
//!
//! Bead: wa-obj7
//!
//! Validates:
//! 1. Delay non-negative: delay_for_attempt always ≥ 0
//! 2. Delay exponential growth (no jitter): delay(n+1) ≥ delay(n)
//! 3. Delay capped at max_delay (no jitter): delay ≤ max_delay
//! 4. Delay at attempt 0 = initial_delay (no jitter)
//! 5. Delay deterministic with zero jitter: same inputs → same output
//! 6. Delay jitter bounded: within ±jitter_percent of base
//! 7. Delay exponent capped at 31: attempt ≥ 31 same as 31
//! 8. RetryPolicy::new clamps backoff_factor ≥ 1.0
//! 9. RetryPolicy::new clamps jitter_percent to [0, 1]
//! 10. is_retryable: IO errors retryable
//! 11. is_retryable: Runtime errors retryable
//! 12. is_retryable: Policy errors not retryable
//! 13. is_retryable: Config errors not retryable
//! 14. Preset policies: all have max_attempts > 0
//! 15. Preset policies: initial_delay < max_delay

use std::time::Duration;

use proptest::prelude::*;

use frankenterm_core::retry::{RetryPolicy, is_retryable};

// =============================================================================
// Strategies
// =============================================================================

fn arb_initial_delay_ms() -> impl Strategy<Value = u64> {
    1_u64..10_000
}

fn arb_max_delay_ms() -> impl Strategy<Value = u64> {
    1000_u64..100_000
}

fn arb_backoff_factor() -> impl Strategy<Value = f64> {
    1.0_f64..5.0
}

fn arb_attempt() -> impl Strategy<Value = u32> {
    0_u32..50
}

/// Generate a policy with zero jitter for deterministic delay tests.
fn arb_no_jitter_policy() -> impl Strategy<Value = RetryPolicy> {
    (
        arb_initial_delay_ms(),
        arb_max_delay_ms(),
        arb_backoff_factor(),
    )
        .prop_filter("max >= initial", |(init, max, _)| max >= init)
        .prop_map(|(init, max, factor)| RetryPolicy {
            initial_delay: Duration::from_millis(init),
            max_delay: Duration::from_millis(max),
            backoff_factor: factor,
            jitter_percent: 0.0,
            max_attempts: Some(10),
        })
}

/// Generate a policy with jitter for bounded-jitter tests.
fn arb_jitter_policy() -> impl Strategy<Value = RetryPolicy> {
    (
        arb_initial_delay_ms(),
        arb_max_delay_ms(),
        arb_backoff_factor(),
        0.01_f64..0.5,
    )
        .prop_filter("max >= initial", |(init, max, _, _)| max >= init)
        .prop_map(|(init, max, factor, jitter)| RetryPolicy {
            initial_delay: Duration::from_millis(init),
            max_delay: Duration::from_millis(max),
            backoff_factor: factor,
            jitter_percent: jitter,
            max_attempts: Some(10),
        })
}

// =============================================================================
// Property: Delay non-negative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn delay_non_negative(
        policy in arb_jitter_policy(),
        attempt in arb_attempt(),
    ) {
        let delay = policy.delay_for_attempt(attempt);
        // Duration is always non-negative by construction, but verify
        prop_assert!(delay.as_millis() < u128::MAX);
    }
}

// =============================================================================
// Property: Delay monotonic growth (no jitter)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn delay_monotonic_no_jitter(
        policy in arb_no_jitter_policy(),
    ) {
        let mut prev = policy.delay_for_attempt(0);
        for attempt in 1..15_u32 {
            let curr = policy.delay_for_attempt(attempt);
            prop_assert!(curr >= prev,
                "delay not monotonic: attempt {} delay {:?} < attempt {} delay {:?}",
                attempt, curr, attempt - 1, prev);
            prev = curr;
        }
    }
}

// =============================================================================
// Property: Delay capped at max_delay (no jitter)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn delay_capped_no_jitter(
        policy in arb_no_jitter_policy(),
        attempt in arb_attempt(),
    ) {
        let delay = policy.delay_for_attempt(attempt);
        prop_assert!(delay <= policy.max_delay,
            "delay {:?} exceeds max {:?} at attempt {}", delay, policy.max_delay, attempt);
    }
}

// =============================================================================
// Property: Delay at attempt 0 = initial_delay (no jitter)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn delay_at_attempt_zero(
        policy in arb_no_jitter_policy(),
    ) {
        let delay = policy.delay_for_attempt(0);
        // Attempt 0: initial_delay * factor^0 = initial_delay * 1 = initial_delay
        // But capped at max_delay
        let expected = policy.initial_delay.min(policy.max_delay);
        prop_assert_eq!(delay, expected,
            "delay at attempt 0 should be min(initial, max), got {:?}", delay);
    }
}

// =============================================================================
// Property: Delay deterministic with zero jitter
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn delay_deterministic_no_jitter(
        policy in arb_no_jitter_policy(),
        attempt in arb_attempt(),
    ) {
        let d1 = policy.delay_for_attempt(attempt);
        let d2 = policy.delay_for_attempt(attempt);
        prop_assert_eq!(d1, d2,
            "zero-jitter delay not deterministic: {:?} vs {:?}", d1, d2);
    }
}

// =============================================================================
// Property: Jitter bounded within ±jitter_percent of base
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn jitter_bounded(
        init_ms in 100_u64..5000,
        max_ms in 5000_u64..50_000,
        factor in arb_backoff_factor(),
        jitter in 0.01_f64..0.5,
        attempt in 0_u32..10,
    ) {
        let no_jitter_policy = RetryPolicy {
            initial_delay: Duration::from_millis(init_ms),
            max_delay: Duration::from_millis(max_ms),
            backoff_factor: factor,
            jitter_percent: 0.0,
            max_attempts: Some(10),
        };
        let jitter_policy = RetryPolicy {
            initial_delay: Duration::from_millis(init_ms),
            max_delay: Duration::from_millis(max_ms),
            backoff_factor: factor,
            jitter_percent: jitter,
            max_attempts: Some(10),
        };

        let base = no_jitter_policy.delay_for_attempt(attempt).as_millis() as f64;
        let jittered = jitter_policy.delay_for_attempt(attempt).as_millis() as f64;

        // Jitter should be within ±jitter_percent of base (plus rounding margin)
        let margin = base.mul_add(jitter, 2.0); // +2ms for rounding
        prop_assert!(jittered >= (base - margin).max(0.0),
            "jittered {} below base-margin {} (base={}, jitter={})",
            jittered, (base - margin).max(0.0), base, jitter);
        prop_assert!(jittered <= base + margin,
            "jittered {} above base+margin {} (base={}, jitter={})",
            jittered, base + margin, base, jitter);
    }
}

// =============================================================================
// Property: Exponent capped at 31
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn exponent_capped_at_31(
        policy in arb_no_jitter_policy(),
    ) {
        let at_31 = policy.delay_for_attempt(31);
        let at_50 = policy.delay_for_attempt(50);
        let at_100 = policy.delay_for_attempt(100);
        let at_u32_max = policy.delay_for_attempt(u32::MAX);
        prop_assert_eq!(at_31, at_50,
            "attempt 31 ({:?}) != attempt 50 ({:?})", at_31, at_50);
        prop_assert_eq!(at_31, at_100);
        prop_assert_eq!(at_31, at_u32_max);
    }
}

// =============================================================================
// Property: RetryPolicy::new clamps backoff_factor ≥ 1.0
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn new_clamps_backoff_factor(
        raw_factor in -10.0_f64..10.0,
    ) {
        let policy = RetryPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            raw_factor,
            0.1,
            Some(3),
        );
        prop_assert!(policy.backoff_factor >= 1.0,
            "backoff_factor {} < 1.0 for raw {}", policy.backoff_factor, raw_factor);
    }
}

// =============================================================================
// Property: RetryPolicy::new clamps jitter_percent to [0, 1]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn new_clamps_jitter_percent(
        raw_jitter in -5.0_f64..5.0,
    ) {
        let policy = RetryPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            2.0,
            raw_jitter,
            Some(3),
        );
        prop_assert!(policy.jitter_percent >= 0.0,
            "jitter_percent {} < 0 for raw {}", policy.jitter_percent, raw_jitter);
        prop_assert!(policy.jitter_percent <= 1.0,
            "jitter_percent {} > 1 for raw {}", policy.jitter_percent, raw_jitter);
    }
}

// =============================================================================
// Property: is_retryable — IO errors are retryable
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn io_errors_retryable(
        kind in prop_oneof![
            Just(std::io::ErrorKind::TimedOut),
            Just(std::io::ErrorKind::ConnectionRefused),
            Just(std::io::ErrorKind::BrokenPipe),
            Just(std::io::ErrorKind::ConnectionReset),
        ],
    ) {
        let err = frankenterm_core::Error::Io(std::io::Error::new(kind, "test"));
        prop_assert!(is_retryable(&err), "IO error {:?} should be retryable", kind);
    }
}

// =============================================================================
// Property: is_retryable — Runtime errors are retryable
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn runtime_errors_retryable(
        msg in "[a-z ]{5,50}",
    ) {
        let err = frankenterm_core::Error::Runtime(msg.clone());
        prop_assert!(is_retryable(&err), "Runtime('{}') should be retryable", msg);
    }
}

// =============================================================================
// Property: is_retryable — Policy errors not retryable
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn policy_errors_not_retryable(
        msg in "[a-z ]{5,50}",
    ) {
        let err = frankenterm_core::Error::Policy(msg.clone());
        prop_assert!(!is_retryable(&err), "Policy('{}') should not be retryable", msg);
    }
}

// =============================================================================
// Property: is_retryable — Config errors not retryable
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_errors_not_retryable(
        path in "[a-z.]{3,20}",
    ) {
        let err = frankenterm_core::Error::Config(
            frankenterm_core::error::ConfigError::FileNotFound(path.clone()),
        );
        prop_assert!(!is_retryable(&err), "Config(FileNotFound('{}')) should not be retryable", path);
    }
}

// =============================================================================
// Property: Preset policies have sensible invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn preset_policies_sensible(_dummy in 0..1_u32) {
        let presets = [
            RetryPolicy::default(),
            RetryPolicy::wezterm_cli(),
            RetryPolicy::db_write(),
            RetryPolicy::webhook(),
            RetryPolicy::browser(),
        ];

        for (i, policy) in presets.iter().enumerate() {
            prop_assert!(policy.max_attempts.unwrap_or(1) > 0,
                "preset {} has 0 max_attempts", i);
            prop_assert!(policy.initial_delay <= policy.max_delay,
                "preset {} initial {:?} > max {:?}", i, policy.initial_delay, policy.max_delay);
            prop_assert!(policy.backoff_factor >= 1.0,
                "preset {} backoff_factor {} < 1.0", i, policy.backoff_factor);
            prop_assert!(policy.jitter_percent >= 0.0 && policy.jitter_percent <= 1.0,
                "preset {} jitter {} out of [0,1]", i, policy.jitter_percent);
        }
    }
}

// =============================================================================
// Property: Exponential delay formula correctness (no jitter)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn delay_formula_correct(
        init_ms in 10_u64..1000,
        factor in 1.0_f64..3.0,
        attempt in 0_u32..10,
    ) {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(init_ms),
            max_delay: Duration::from_secs(3600), // very high cap
            backoff_factor: factor,
            jitter_percent: 0.0,
            max_attempts: Some(20),
        };

        let delay = policy.delay_for_attempt(attempt);
        let expected_ms = (init_ms as f64) * factor.powi(attempt as i32);
        // Cap at max_delay, matching the implementation's behavior.
        let expected = Duration::from_millis(expected_ms as u64).min(policy.max_delay);

        // Allow 1ms tolerance for f64→u64 truncation
        let diff = delay.abs_diff(expected);
        prop_assert!(diff <= Duration::from_millis(1),
            "delay {:?} ≠ expected {:?} (diff {:?}) for init={}, factor={}, attempt={}",
            delay, expected, diff, init_ms, factor, attempt);
    }
}

// =============================================================================
// Property: Backoff ratio between consecutive attempts
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn backoff_ratio_correct(
        init_ms in 100_u64..1000,
        factor in 1.5_f64..3.0,
    ) {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(init_ms),
            max_delay: Duration::from_secs(3600),
            backoff_factor: factor,
            jitter_percent: 0.0,
            max_attempts: Some(20),
        };

        // For attempts that don't hit the cap, ratio should be ≈ factor
        for attempt in 0..5_u32 {
            let d_n = policy.delay_for_attempt(attempt).as_millis() as f64;
            let d_n1 = policy.delay_for_attempt(attempt + 1).as_millis() as f64;
            if d_n > 0.0 && d_n1 < policy.max_delay.as_millis() as f64 {
                let ratio = d_n1 / d_n;
                prop_assert!((ratio - factor).abs() < 0.1,
                    "ratio {} ≠ factor {} at attempt {}", ratio, factor, attempt);
            }
        }
    }
}
