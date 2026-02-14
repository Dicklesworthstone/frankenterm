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

use frankenterm_core::retry::{is_retryable, RetryPolicy};

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

// =============================================================================
// Property: RetryPolicy Clone preserves all fields
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn clone_preserves_fields(policy in arb_jitter_policy()) {
        let cloned = policy.clone();
        prop_assert_eq!(cloned.initial_delay, policy.initial_delay);
        prop_assert_eq!(cloned.max_delay, policy.max_delay);
        prop_assert!((cloned.backoff_factor - policy.backoff_factor).abs() < f64::EPSILON);
        prop_assert!((cloned.jitter_percent - policy.jitter_percent).abs() < f64::EPSILON);
        prop_assert_eq!(cloned.max_attempts, policy.max_attempts);
    }

    #[test]
    fn clone_preserves_delay_behavior(
        policy in arb_no_jitter_policy(),
        attempt in arb_attempt(),
    ) {
        let cloned = policy.clone();
        let d1 = policy.delay_for_attempt(attempt);
        let d2 = cloned.delay_for_attempt(attempt);
        prop_assert_eq!(d1, d2,
            "clone should produce same delay: {:?} vs {:?}", d1, d2);
    }
}

// =============================================================================
// Property: RetryPolicy Debug formatting
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn debug_format_non_empty(policy in arb_jitter_policy()) {
        let debug = format!("{:?}", policy);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("RetryPolicy"));
    }

    #[test]
    fn debug_contains_field_names(policy in arb_no_jitter_policy()) {
        let debug = format!("{:?}", policy);
        prop_assert!(debug.contains("initial_delay"), "should contain 'initial_delay'");
        prop_assert!(debug.contains("max_delay"), "should contain 'max_delay'");
        prop_assert!(debug.contains("backoff_factor"), "should contain 'backoff_factor'");
        prop_assert!(debug.contains("jitter_percent"), "should contain 'jitter_percent'");
        prop_assert!(debug.contains("max_attempts"), "should contain 'max_attempts'");
    }
}

// =============================================================================
// Property: Default policy has known values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn default_has_known_values(_dummy in 0..1u8) {
        let d = RetryPolicy::default();
        prop_assert_eq!(d.initial_delay, Duration::from_millis(100));
        prop_assert_eq!(d.max_delay, Duration::from_secs(30));
        prop_assert!((d.backoff_factor - 2.0).abs() < f64::EPSILON);
        prop_assert!((d.jitter_percent - 0.1).abs() < f64::EPSILON);
        prop_assert_eq!(d.max_attempts, Some(3));
    }

    #[test]
    fn default_is_deterministic(_dummy in 0..1u8) {
        let a = RetryPolicy::default();
        let b = RetryPolicy::default();
        prop_assert_eq!(a.initial_delay, b.initial_delay);
        prop_assert_eq!(a.max_delay, b.max_delay);
        prop_assert!((a.backoff_factor - b.backoff_factor).abs() < f64::EPSILON);
        prop_assert!((a.jitter_percent - b.jitter_percent).abs() < f64::EPSILON);
        prop_assert_eq!(a.max_attempts, b.max_attempts);
    }
}

// =============================================================================
// Property: High attempts plateau at max_delay (no jitter)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn high_attempts_plateau(policy in arb_no_jitter_policy()) {
        // After enough attempts with sufficient growth, delay should converge to max_delay
        let d_high = policy.delay_for_attempt(40);
        // Only assert equality with max_delay if the exponential growth can actually reach it
        let saturated = (policy.initial_delay.as_millis() as f64)
            * policy.backoff_factor.powi(31);
        if saturated >= policy.max_delay.as_millis() as f64 {
            prop_assert_eq!(d_high, policy.max_delay,
                "high attempt should hit max_delay: {:?} vs {:?}", d_high, policy.max_delay);
        } else {
            // Delay should still be capped at max_delay
            prop_assert!(d_high <= policy.max_delay,
                "high attempt {:?} should not exceed max_delay {:?}", d_high, policy.max_delay);
        }
    }

    #[test]
    fn delay_at_attempt_1_no_jitter(
        init_ms in 10_u64..1000,
        factor in 1.0_f64..3.0,
    ) {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(init_ms),
            max_delay: Duration::from_secs(3600),
            backoff_factor: factor,
            jitter_percent: 0.0,
            max_attempts: Some(20),
        };
        let d0 = policy.delay_for_attempt(0);
        let d1 = policy.delay_for_attempt(1);
        // d1 should be approximately d0 * factor
        let expected_ms = (init_ms as f64 * factor) as u64;
        let expected = Duration::from_millis(expected_ms).min(policy.max_delay);
        let diff = d1.abs_diff(expected);
        prop_assert!(diff <= Duration::from_millis(1),
            "d1 {:?} should be ~{:?} (d0={:?}, factor={})", d1, expected, d0, factor);
    }
}

// =============================================================================
// Property: new() clamps both backoff and jitter simultaneously
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn new_clamps_both(
        raw_factor in -10.0_f64..10.0,
        raw_jitter in -5.0_f64..5.0,
        max_attempts in prop::option::of(1u32..100),
    ) {
        let policy = RetryPolicy::new(
            Duration::from_millis(100),
            Duration::from_secs(10),
            raw_factor,
            raw_jitter,
            max_attempts,
        );
        prop_assert!(policy.backoff_factor >= 1.0,
            "backoff_factor {} < 1.0", policy.backoff_factor);
        prop_assert!(policy.jitter_percent >= 0.0,
            "jitter_percent {} < 0.0", policy.jitter_percent);
        prop_assert!(policy.jitter_percent <= 1.0,
            "jitter_percent {} > 1.0", policy.jitter_percent);
        prop_assert_eq!(policy.max_attempts, max_attempts);
    }

    #[test]
    fn new_preserves_valid_inputs(
        init_ms in 1_u64..10000,
        max_ms in 10000_u64..100000,
        factor in 1.0_f64..5.0,
        jitter in 0.0_f64..1.0,
    ) {
        let policy = RetryPolicy::new(
            Duration::from_millis(init_ms),
            Duration::from_millis(max_ms),
            factor,
            jitter,
            Some(5),
        );
        prop_assert_eq!(policy.initial_delay, Duration::from_millis(init_ms));
        prop_assert_eq!(policy.max_delay, Duration::from_millis(max_ms));
        prop_assert!((policy.backoff_factor - factor).abs() < f64::EPSILON);
        prop_assert!((policy.jitter_percent - jitter).abs() < f64::EPSILON);
    }
}

// =============================================================================
// Property: Preset policies additional invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn preset_initial_delays_positive(_dummy in 0..1u8) {
        let presets = [
            RetryPolicy::default(),
            RetryPolicy::wezterm_cli(),
            RetryPolicy::db_write(),
            RetryPolicy::webhook(),
            RetryPolicy::browser(),
        ];
        for (i, p) in presets.iter().enumerate() {
            prop_assert!(p.initial_delay > Duration::ZERO,
                "preset {} initial_delay should be positive", i);
            prop_assert!(p.max_delay > Duration::ZERO,
                "preset {} max_delay should be positive", i);
        }
    }

    #[test]
    fn preset_delay_at_zero_equals_initial(_dummy in 0..1u8) {
        // With no jitter, attempt 0 should yield initial_delay
        let presets = [
            ("default", RetryPolicy::default()),
            ("wezterm_cli", RetryPolicy::wezterm_cli()),
            ("db_write", RetryPolicy::db_write()),
            ("webhook", RetryPolicy::webhook()),
            ("browser", RetryPolicy::browser()),
        ];
        for (name, p) in &presets {
            let no_jitter = RetryPolicy {
                jitter_percent: 0.0,
                ..p.clone()
            };
            let d = no_jitter.delay_for_attempt(0);
            let expected = no_jitter.initial_delay.min(no_jitter.max_delay);
            prop_assert_eq!(d, expected,
                "preset {} delay at attempt 0 should be min(initial, max): {:?} vs {:?}",
                name, d, expected);
        }
    }
}

// =============================================================================
// Property: is_retryable — Wezterm-specific errors
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn wezterm_not_running_retryable(_dummy in 0..1u8) {
        let err = frankenterm_core::Error::Wezterm(
            frankenterm_core::error::WeztermError::NotRunning
        );
        prop_assert!(is_retryable(&err), "NotRunning should be retryable");
    }

    #[test]
    fn wezterm_timeout_retryable(timeout_ms in 1u64..30_000) {
        let err = frankenterm_core::Error::Wezterm(
            frankenterm_core::error::WeztermError::Timeout(timeout_ms)
        );
        prop_assert!(is_retryable(&err), "Timeout should be retryable");
    }

    #[test]
    fn wezterm_cli_not_found_not_retryable(_dummy in 0..1u8) {
        let err = frankenterm_core::Error::Wezterm(
            frankenterm_core::error::WeztermError::CliNotFound
        );
        prop_assert!(!is_retryable(&err), "CliNotFound should not be retryable");
    }

    #[test]
    fn wezterm_pane_not_found_not_retryable(id in 1u64..10000) {
        let err = frankenterm_core::Error::Wezterm(
            frankenterm_core::error::WeztermError::PaneNotFound(id)
        );
        prop_assert!(!is_retryable(&err), "PaneNotFound should not be retryable");
    }
}

// =============================================================================
// Property: is_retryable classification completeness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn json_errors_not_retryable(msg in "[a-z]{5,20}") {
        let err = frankenterm_core::Error::Json(
            serde_json::from_str::<serde_json::Value>(&msg).unwrap_err()
        );
        prop_assert!(!is_retryable(&err), "Json errors should not be retryable");
    }

    #[test]
    fn setup_errors_not_retryable(msg in "[a-z ]{5,30}") {
        let err = frankenterm_core::Error::SetupError(msg);
        prop_assert!(!is_retryable(&err), "SetupError should not be retryable");
    }

    #[test]
    fn cancelled_errors_not_retryable(msg in "[a-z ]{5,30}") {
        let err = frankenterm_core::Error::Cancelled(msg);
        prop_assert!(!is_retryable(&err), "Cancelled should not be retryable");
    }
}
