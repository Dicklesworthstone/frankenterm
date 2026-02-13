//! Property-based tests for protocol recovery invariants.
//!
//! Bead: wa-jpva
//!
//! Validates:
//! 1. classify_error_message: permanent patterns always classify Permanent
//! 2. classify_error_message: recoverable patterns always classify Recoverable
//! 3. classify_error_message: transient patterns always classify Transient
//! 4. classify_error_message: case insensitive — mixed case yields same result
//! 5. classify_error_message: unknown strings default to Recoverable
//! 6. classify_error_message: io error sub-classification (broken pipe → Recoverable)
//! 7. classify_error_message: io error sub-classification (would block → Transient)
//! 8. classify_error_message: io error fallback → Recoverable
//! 9. ProtocolErrorKind: Display roundtrip (display text matches variant)
//! 10. ProtocolErrorKind: serde roundtrip preserves identity
//! 11. RecoveryConfig: serde roundtrip preserves all fields
//! 12. RecoveryConfig: delay_for_attempt non-negative
//! 13. RecoveryConfig: delay_for_attempt approximately capped at max_delay
//! 14. RecoveryConfig: delay grows with attempt (zero jitter)
//! 15. RecoveryConfig: delay at attempt 0 ≈ initial_delay (zero jitter)
//! 16. RecoveryConfig: preset invariants (initial < max, max_retries > 0)
//! 17. ConnectionHealth: serde roundtrip preserves identity
//! 18. RecoveryStats: serde roundtrip preserves all fields
//! 19. FrameCorruptionDetector: starts not corrupted
//! 20. FrameCorruptionDetector: corruption at threshold
//! 21. FrameCorruptionDetector: transient errors don't count
//! 22. FrameCorruptionDetector: reset clears corruption
//! 23. FrameCorruptionDetector: window rotation halves counts
//! 24. ConnectionHealthTracker: starts Healthy
//! 25. ConnectionHealthTracker: permanent → Dead immediately
//! 26. ConnectionHealthTracker: 3+ transients → Degraded
//! 27. ConnectionHealthTracker: recovers after 5 successes from Degraded
//! 28. ConnectionHealthTracker: reset restores Healthy from any state
//! 29. RecoveryError: is_circuit_open / is_permanent classification
//! 30. classify_error_message: permanent patterns take priority over recoverable substrings

use proptest::prelude::*;

use frankenterm_core::protocol_recovery::{
    ConnectionHealth, ConnectionHealthTracker, FrameCorruptionDetector, ProtocolErrorKind,
    RecoveryConfig, RecoveryStats, classify_error_message,
};

// =============================================================================
// Strategies
// =============================================================================

/// Known permanent error substrings.
fn arb_permanent_pattern() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("codec version mismatch".to_string()),
        Just("incompatible".to_string()),
        Just("socket path not found".to_string()),
        Just("proxy command not supported".to_string()),
    ]
}

/// Known recoverable error substrings.
fn arb_recoverable_pattern() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("unexpected response".to_string()),
        Just("disconnected".to_string()),
        Just("codec error".to_string()),
        Just("frame exceeded max size".to_string()),
        Just("remote error".to_string()),
    ]
}

/// Known transient error substrings.
fn arb_transient_pattern() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("timed out".to_string()),
        Just("timeout".to_string()),
        Just("connection refused".to_string()),
    ]
}

/// Random prefix/suffix to wrap around patterns.
fn arb_noise() -> impl Strategy<Value = String> {
    "[a-z ]{0,20}"
}

fn arb_window_size() -> impl Strategy<Value = u32> {
    5_u32..200
}

fn arb_threshold() -> impl Strategy<Value = u32> {
    1_u32..20
}

// =============================================================================
// Property 1: Permanent patterns always classify Permanent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn permanent_patterns_classify_permanent(
        pattern in arb_permanent_pattern(),
        prefix in arb_noise(),
        suffix in arb_noise(),
    ) {
        let msg = format!("{} {} {}", prefix, pattern, suffix);
        let kind = classify_error_message(&msg);
        prop_assert_eq!(kind, ProtocolErrorKind::Permanent,
            "expected Permanent for message containing '{}', got {:?}", pattern, kind);
    }
}

// =============================================================================
// Property 2: Recoverable patterns classify Recoverable
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn recoverable_patterns_classify_recoverable(
        pattern in arb_recoverable_pattern(),
        prefix in arb_noise(),
        suffix in arb_noise(),
    ) {
        // Ensure no permanent patterns in noise
        let msg = format!("err {} {} {}", prefix, pattern, suffix);
        let lower = msg.to_lowercase();
        // Skip if noise accidentally contains a permanent or transient pattern
        prop_assume!(!lower.contains("incompatible"));
        prop_assume!(!lower.contains("codec version mismatch"));
        prop_assume!(!lower.contains("socket path not found"));
        prop_assume!(!lower.contains("proxy command not supported"));
        prop_assume!(!lower.contains("timed out"));
        prop_assume!(!lower.contains("timeout"));
        prop_assume!(!lower.contains("connection refused"));
        let kind = classify_error_message(&msg);
        prop_assert_eq!(kind, ProtocolErrorKind::Recoverable,
            "expected Recoverable for message containing '{}', got {:?}", pattern, kind);
    }
}

// =============================================================================
// Property 3: Transient patterns classify Transient
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn transient_patterns_classify_transient(
        pattern in arb_transient_pattern(),
        prefix in arb_noise(),
        suffix in arb_noise(),
    ) {
        let msg = format!("err {} {} {}", prefix, pattern, suffix);
        let lower = msg.to_lowercase();
        // Skip if noise accidentally contains a permanent pattern
        prop_assume!(!lower.contains("incompatible"));
        prop_assume!(!lower.contains("codec version mismatch"));
        prop_assume!(!lower.contains("socket path not found"));
        prop_assume!(!lower.contains("proxy command not supported"));
        // Skip if noise contains recoverable patterns that match first
        prop_assume!(!lower.contains("unexpected response"));
        prop_assume!(!lower.contains("disconnected"));
        prop_assume!(!lower.contains("codec error"));
        prop_assume!(!lower.contains("frame exceeded max size"));
        prop_assume!(!lower.contains("remote error"));
        let kind = classify_error_message(&msg);
        prop_assert_eq!(kind, ProtocolErrorKind::Transient,
            "expected Transient for message containing '{}', got {:?}", pattern, kind);
    }
}

// =============================================================================
// Property 4: Case insensitive classification
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn classify_case_insensitive(
        pattern in arb_permanent_pattern(),
    ) {
        let upper = pattern.to_uppercase();
        let lower = pattern.to_lowercase();
        let mixed: String = pattern.chars().enumerate()
            .map(|(i, c)| if i % 2 == 0 { c.to_uppercase().next().unwrap_or(c) } else { c })
            .collect();
        let k_upper = classify_error_message(&upper);
        let k_lower = classify_error_message(&lower);
        let k_mixed = classify_error_message(&mixed);
        prop_assert_eq!(k_upper, k_lower,
            "upper '{}' ({:?}) != lower '{}' ({:?})", upper, k_upper, lower, k_lower);
        prop_assert_eq!(k_lower, k_mixed,
            "lower '{}' ({:?}) != mixed '{}' ({:?})", lower, k_lower, mixed, k_mixed);
    }
}

// =============================================================================
// Property 5: Unknown strings default to Recoverable
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn unknown_defaults_to_recoverable(
        msg in "[0-9]{10,30}",
    ) {
        // Pure digit strings won't match any known patterns
        let kind = classify_error_message(&msg);
        prop_assert_eq!(kind, ProtocolErrorKind::Recoverable,
            "expected Recoverable for unknown msg '{}', got {:?}", msg, kind);
    }
}

// =============================================================================
// Property 6: IO error with broken pipe/reset/not connected → Recoverable
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn io_error_recoverable_subkinds(
        subkind in prop_oneof![
            Just("broken pipe"),
            Just("connection reset"),
            Just("not connected"),
        ],
    ) {
        let msg = format!("io error: {}", subkind);
        let kind = classify_error_message(&msg);
        prop_assert_eq!(kind, ProtocolErrorKind::Recoverable,
            "io error '{}' should be Recoverable, got {:?}", subkind, kind);
    }
}

// =============================================================================
// Property 7: IO error with would block/interrupted → Transient
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn io_error_transient_subkinds(
        subkind in prop_oneof![
            Just("would block"),
            Just("interrupted"),
            Just("timed out"),
        ],
    ) {
        let msg = format!("io error: {}", subkind);
        let kind = classify_error_message(&msg);
        prop_assert_eq!(kind, ProtocolErrorKind::Transient,
            "io error '{}' should be Transient, got {:?}", subkind, kind);
    }
}

// =============================================================================
// Property 8: IO error fallback → Recoverable
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn io_error_fallback_recoverable(
        detail in "[0-9]{5,20}",
    ) {
        // "io error: <digits>" — digits won't match any sub-pattern
        let msg = format!("io error: {}", detail);
        let kind = classify_error_message(&msg);
        prop_assert_eq!(kind, ProtocolErrorKind::Recoverable,
            "io error fallback '{}' should be Recoverable, got {:?}", detail, kind);
    }
}

// =============================================================================
// Property 9: ProtocolErrorKind Display matches variant name
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn error_kind_display_correct(_dummy in 0..1_u32) {
        prop_assert_eq!(ProtocolErrorKind::Recoverable.to_string(), "recoverable");
        prop_assert_eq!(ProtocolErrorKind::Transient.to_string(), "transient");
        prop_assert_eq!(ProtocolErrorKind::Permanent.to_string(), "permanent");
    }
}

// =============================================================================
// Property 10: ProtocolErrorKind serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn error_kind_serde_roundtrip(
        kind in prop_oneof![
            Just(ProtocolErrorKind::Recoverable),
            Just(ProtocolErrorKind::Transient),
            Just(ProtocolErrorKind::Permanent),
        ],
    ) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: ProtocolErrorKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, kind);
    }
}

// =============================================================================
// Property 11: RecoveryConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn recovery_config_serde_roundtrip(
        max_retries in 0_u32..20,
        init_ms in 1_u64..5000,
        max_ms in 5000_u64..60_000,
        backoff in 1.0_f64..5.0,
        jitter in 0.0_f64..0.5,
        perm_limit in 1_u32..20,
    ) {
        let config = RecoveryConfig {
            enabled: true,
            max_retries,
            initial_delay: std::time::Duration::from_millis(init_ms),
            max_delay: std::time::Duration::from_millis(max_ms),
            backoff_factor: backoff,
            jitter_fraction: jitter,
            circuit_failure_threshold: 5,
            circuit_success_threshold: 2,
            circuit_cooldown: std::time::Duration::from_secs(15),
            report_degradation: false,
            permanent_failure_limit: perm_limit,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: RecoveryConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_retries, config.max_retries);
        prop_assert_eq!(back.initial_delay, config.initial_delay);
        prop_assert_eq!(back.max_delay, config.max_delay);
        prop_assert!((back.backoff_factor - config.backoff_factor).abs() < 1e-10);
        prop_assert!((back.jitter_fraction - config.jitter_fraction).abs() < 1e-10);
        prop_assert_eq!(back.permanent_failure_limit, config.permanent_failure_limit);
        prop_assert_eq!(back.enabled, config.enabled);
    }
}

// =============================================================================
// Property 12: delay_for_attempt always non-negative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn delay_non_negative(
        init_ms in 1_u64..5000,
        max_ms in 5000_u64..60_000,
        backoff in 1.0_f64..5.0,
        jitter in 0.0_f64..0.5,
        attempt in 0_u32..50,
    ) {
        let config = RecoveryConfig {
            initial_delay: std::time::Duration::from_millis(init_ms),
            max_delay: std::time::Duration::from_millis(max_ms),
            backoff_factor: backoff,
            jitter_fraction: jitter,
            ..RecoveryConfig::default()
        };
        let delay = config.delay_for_attempt(attempt);
        prop_assert!(delay.as_millis() >= 1,
            "delay should be >= 1ms, got {:?} for attempt {}", delay, attempt);
    }
}

// =============================================================================
// Property 13: delay approximately capped at max_delay (with jitter margin)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn delay_approximately_capped(
        init_ms in 1_u64..1000,
        max_ms in 1000_u64..10_000,
        backoff in 1.0_f64..5.0,
        jitter in 0.0_f64..0.5,
        attempt in 0_u32..50,
    ) {
        let config = RecoveryConfig {
            initial_delay: std::time::Duration::from_millis(init_ms),
            max_delay: std::time::Duration::from_millis(max_ms),
            backoff_factor: backoff,
            jitter_fraction: jitter,
            ..RecoveryConfig::default()
        };
        let delay = config.delay_for_attempt(attempt);
        // Jitter can add up to jitter_fraction of max_delay
        let upper_bound_ms = max_ms as f64 * (1.0 + jitter) + 2.0;
        prop_assert!(delay.as_millis() as f64 <= upper_bound_ms,
            "delay {:?} exceeds approx cap {} for attempt {} (max={}, jitter={})",
            delay, upper_bound_ms, attempt, max_ms, jitter);
    }
}

// =============================================================================
// Property 14: delay grows with attempt (zero jitter, before cap)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn delay_monotonic_no_jitter(
        init_ms in 10_u64..500,
        backoff in 1.1_f64..3.0,
    ) {
        let config = RecoveryConfig {
            initial_delay: std::time::Duration::from_millis(init_ms),
            max_delay: std::time::Duration::from_secs(3600), // very high cap
            backoff_factor: backoff,
            jitter_fraction: 0.0,
            ..RecoveryConfig::default()
        };
        let mut prev = config.delay_for_attempt(0);
        for attempt in 1..8_u32 {
            let curr = config.delay_for_attempt(attempt);
            prop_assert!(curr >= prev,
                "delay not monotonic: attempt {} delay {:?} < attempt {} delay {:?}",
                attempt, curr, attempt - 1, prev);
            prev = curr;
        }
    }
}

// =============================================================================
// Property 15: delay at attempt 0 ≈ initial_delay (zero jitter)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn delay_at_attempt_zero(
        init_ms in 10_u64..5000,
        max_ms in 5000_u64..60_000,
        backoff in 1.0_f64..5.0,
    ) {
        let config = RecoveryConfig {
            initial_delay: std::time::Duration::from_millis(init_ms),
            max_delay: std::time::Duration::from_millis(max_ms),
            backoff_factor: backoff,
            jitter_fraction: 0.0,
            ..RecoveryConfig::default()
        };
        let delay = config.delay_for_attempt(0);
        // With zero jitter and attempt 0: base = init * factor^0 = init
        // jitter_seed at attempt 0 = sin(0) = 0, so jitter contribution = 0
        // But the code does: jitter_seed = (sin(0 * 7.13).abs()) * 2.0 - 1.0 = 0 * 2 - 1 = -1
        // jittered = capped + jitter_range * (-1) = capped - 0 = capped (jitter_fraction=0)
        // So delay should equal init_ms
        let diff = if delay.as_millis() > init_ms as u128 {
            delay.as_millis() - init_ms as u128
        } else {
            init_ms as u128 - delay.as_millis()
        };
        prop_assert!(diff <= 1,
            "delay at attempt 0 ({:?}) should be ~{}ms, diff={}",
            delay, init_ms, diff);
    }
}

// =============================================================================
// Property 16: Preset configs have sensible invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn preset_configs_sensible(_dummy in 0..1_u32) {
        let presets = [
            ("default", RecoveryConfig::default()),
            ("capture", RecoveryConfig::for_capture()),
            ("interactive", RecoveryConfig::for_interactive()),
        ];

        for (name, config) in &presets {
            prop_assert!(config.initial_delay <= config.max_delay,
                "{}: initial {:?} > max {:?}", name, config.initial_delay, config.max_delay);
            prop_assert!(config.max_retries > 0,
                "{}: max_retries should be > 0", name);
            prop_assert!(config.backoff_factor >= 1.0,
                "{}: backoff_factor {} < 1.0", name, config.backoff_factor);
            prop_assert!(config.jitter_fraction >= 0.0 && config.jitter_fraction <= 1.0,
                "{}: jitter_fraction {} out of [0,1]", name, config.jitter_fraction);
            prop_assert!(config.circuit_failure_threshold > 0,
                "{}: circuit_failure_threshold should be > 0", name);
            prop_assert!(config.circuit_success_threshold > 0,
                "{}: circuit_success_threshold should be > 0", name);
            prop_assert!(config.permanent_failure_limit > 0,
                "{}: permanent_failure_limit should be > 0", name);
            prop_assert!(config.enabled, "{}: should be enabled by default", name);
        }
    }
}

// =============================================================================
// Property 17: ConnectionHealth serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn connection_health_serde_roundtrip(
        health in prop_oneof![
            Just(ConnectionHealth::Healthy),
            Just(ConnectionHealth::Degraded),
            Just(ConnectionHealth::Corrupted),
            Just(ConnectionHealth::Dead),
        ],
    ) {
        let json = serde_json::to_string(&health).unwrap();
        let back: ConnectionHealth = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, health);
    }
}

// =============================================================================
// Property 18: RecoveryStats serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn recovery_stats_serde_roundtrip(
        total in 0_u64..10000,
        first_try in 0_u64..10000,
        retry_succ in 0_u64..1000,
        retries in 0_u64..5000,
        recoverable in 0_u64..1000,
        transient in 0_u64..1000,
        permanent in 0_u64..100,
        circuit_rej in 0_u64..100,
        consec_perm in 0_u64..10,
    ) {
        let stats = RecoveryStats {
            total_operations: total,
            first_try_successes: first_try,
            retry_successes: retry_succ,
            total_retries: retries,
            recoverable_failures: recoverable,
            transient_failures: transient,
            permanent_failures: permanent,
            circuit_rejections: circuit_rej,
            consecutive_permanent: consec_perm,
            circuit_state: "Closed".into(),
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: RecoveryStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_operations, stats.total_operations);
        prop_assert_eq!(back.first_try_successes, stats.first_try_successes);
        prop_assert_eq!(back.retry_successes, stats.retry_successes);
        prop_assert_eq!(back.total_retries, stats.total_retries);
        prop_assert_eq!(back.recoverable_failures, stats.recoverable_failures);
        prop_assert_eq!(back.transient_failures, stats.transient_failures);
        prop_assert_eq!(back.permanent_failures, stats.permanent_failures);
        prop_assert_eq!(back.circuit_rejections, stats.circuit_rejections);
        prop_assert_eq!(back.consecutive_permanent, stats.consecutive_permanent);
        prop_assert_eq!(back.circuit_state, stats.circuit_state);
    }
}

// =============================================================================
// Property 19: FrameCorruptionDetector starts not corrupted
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn detector_starts_clean(
        window in arb_window_size(),
        threshold in arb_threshold(),
    ) {
        let d = FrameCorruptionDetector::new(window, threshold);
        prop_assert!(!d.is_corrupted(),
            "new detector should not be corrupted (window={}, threshold={})", window, threshold);
        let (unexpected, codec) = d.error_counts();
        prop_assert_eq!(unexpected, 0);
        prop_assert_eq!(codec, 0);
    }
}

// =============================================================================
// Property 20: Corruption detected at threshold
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn detector_corruption_at_threshold(
        threshold in 1_u32..15,
    ) {
        // Use large window so rotation doesn't interfere
        let mut d = FrameCorruptionDetector::new(1000, threshold);

        // Record threshold-1 errors: not yet corrupted
        for i in 0..(threshold - 1) {
            d.record_error(ProtocolErrorKind::Recoverable,
                &format!("unexpected response: test {}", i));
        }
        prop_assert!(!d.is_corrupted(),
            "should not be corrupted with {} errors < threshold {}",
            threshold - 1, threshold);

        // One more error tips it over
        d.record_error(ProtocolErrorKind::Recoverable, "unexpected response: final");
        prop_assert!(d.is_corrupted(),
            "should be corrupted with {} errors = threshold {}", threshold, threshold);
    }
}

// =============================================================================
// Property 21: Transient errors don't contribute to corruption
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn detector_transient_no_corruption(
        n_transient in 1_u32..50,
        threshold in 1_u32..10,
    ) {
        let mut d = FrameCorruptionDetector::new(1000, threshold);
        for _ in 0..n_transient {
            d.record_error(ProtocolErrorKind::Transient, "timeout");
        }
        prop_assert!(!d.is_corrupted(),
            "transient errors should not cause corruption");
        let (unexpected, codec) = d.error_counts();
        prop_assert_eq!(unexpected, 0, "unexpected count should stay 0 for transient");
        prop_assert_eq!(codec, 0, "codec count should stay 0 for transient");
    }
}

// =============================================================================
// Property 22: Reset clears corruption
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn detector_reset_clears(
        threshold in 1_u32..10,
    ) {
        let mut d = FrameCorruptionDetector::new(1000, threshold);
        for i in 0..threshold {
            d.record_error(ProtocolErrorKind::Recoverable,
                &format!("unexpected response: {}", i));
        }
        prop_assert!(d.is_corrupted());
        d.reset();
        prop_assert!(!d.is_corrupted(),
            "reset should clear corruption");
        let (u, c) = d.error_counts();
        prop_assert_eq!(u, 0);
        prop_assert_eq!(c, 0);
    }
}

// =============================================================================
// Property 23: Window rotation halves counts
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn detector_window_rotation(
        window in 10_u32..50,
        n_errors in 2_u32..8,
    ) {
        // Use high threshold so corruption doesn't interfere
        let mut d = FrameCorruptionDetector::new(window, 1000);

        // Record some unexpected response errors
        for i in 0..n_errors {
            d.record_error(ProtocolErrorKind::Recoverable,
                &format!("unexpected response: {}", i));
        }
        let (before_unexpected, _) = d.error_counts();
        prop_assert_eq!(before_unexpected, n_errors);

        // Fill remaining window with successes to trigger rotation
        let remaining = window.saturating_sub(n_errors);
        for _ in 0..remaining {
            d.record_success();
        }

        // After rotation, counts should be halved
        let (after_unexpected, _) = d.error_counts();
        prop_assert_eq!(after_unexpected, n_errors / 2,
            "after rotation, {} errors should halve to {}, got {}",
            n_errors, n_errors / 2, after_unexpected);
    }
}

// =============================================================================
// Property 24: ConnectionHealthTracker starts Healthy
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn tracker_starts_healthy(_dummy in 0..1_u32) {
        let t = ConnectionHealthTracker::new();
        prop_assert_eq!(t.health(), ConnectionHealth::Healthy);
        let t2 = ConnectionHealthTracker::default();
        prop_assert_eq!(t2.health(), ConnectionHealth::Healthy);
    }
}

// =============================================================================
// Property 25: Permanent error → Dead immediately
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn tracker_permanent_means_dead(
        msg in "[a-z ]{5,30}",
    ) {
        let mut t = ConnectionHealthTracker::new();
        let h = t.record_error(ProtocolErrorKind::Permanent, &msg);
        prop_assert_eq!(h, ConnectionHealth::Dead,
            "permanent error should immediately yield Dead");
        prop_assert_eq!(t.health(), ConnectionHealth::Dead);
    }
}

// =============================================================================
// Property 26: 3+ consecutive transients → Degraded
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn tracker_transient_degrades_at_three(
        n_transient in 3_u32..20,
        msg in "[a-z ]{3,15}",
    ) {
        let mut t = ConnectionHealthTracker::new();
        for _ in 0..n_transient {
            t.record_error(ProtocolErrorKind::Transient, &msg);
        }
        prop_assert_eq!(t.health(), ConnectionHealth::Degraded,
            "3+ transient errors should yield Degraded, got {:?}", t.health());
    }
}

// =============================================================================
// Property 27: 5 successes recover from Degraded to Healthy
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn tracker_recovers_from_degraded(
        n_transient in 3_u32..10,
    ) {
        let mut t = ConnectionHealthTracker::new();
        // Degrade with transients
        for _ in 0..n_transient {
            t.record_error(ProtocolErrorKind::Transient, "timeout");
        }
        prop_assert_eq!(t.health(), ConnectionHealth::Degraded);

        // 4 successes: still Degraded
        for _ in 0..4 {
            t.record_success();
        }
        prop_assert_eq!(t.health(), ConnectionHealth::Degraded,
            "4 successes should not yet recover from Degraded");

        // 5th success: recovers
        t.record_success();
        prop_assert_eq!(t.health(), ConnectionHealth::Healthy,
            "5 successes should recover from Degraded to Healthy");
    }
}

// =============================================================================
// Property 28: Reset restores Healthy from any state
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn tracker_reset_restores_healthy(
        kind in prop_oneof![
            Just(ProtocolErrorKind::Permanent),
            Just(ProtocolErrorKind::Recoverable),
            Just(ProtocolErrorKind::Transient),
        ],
        n_errors in 1_u32..10,
    ) {
        let mut t = ConnectionHealthTracker::new();
        for _ in 0..n_errors {
            t.record_error(kind, "test error");
        }
        // Health may be Degraded, Corrupted, or Dead
        t.reset();
        prop_assert_eq!(t.health(), ConnectionHealth::Healthy,
            "reset should restore Healthy from any state");
    }
}

// =============================================================================
// Property 29: RecoveryError classification helpers
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn recovery_error_classification(_dummy in 0..1_u32) {
        use frankenterm_core::protocol_recovery::RecoveryError;

        // CircuitOpen
        let e = RecoveryError::CircuitOpen;
        prop_assert!(e.is_circuit_open());
        prop_assert!(!e.is_permanent());

        // Permanent
        let e = RecoveryError::Permanent("test".into());
        prop_assert!(!e.is_circuit_open());
        prop_assert!(e.is_permanent());

        // PermanentLimitReached
        let e = RecoveryError::PermanentLimitReached { limit: 3 };
        prop_assert!(!e.is_circuit_open());
        prop_assert!(e.is_permanent());

        // RetriesExhausted
        let e = RecoveryError::RetriesExhausted {
            attempts: 3,
            last_error: "test".into(),
            last_kind: ProtocolErrorKind::Recoverable,
        };
        prop_assert!(!e.is_circuit_open());
        prop_assert!(!e.is_permanent());

        // Disabled
        let e = RecoveryError::Disabled;
        prop_assert!(!e.is_circuit_open());
        prop_assert!(!e.is_permanent());
    }
}

// =============================================================================
// Property 30: Permanent patterns take priority over recoverable substrings
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn permanent_priority_over_recoverable(
        perm in arb_permanent_pattern(),
        rec in arb_recoverable_pattern(),
    ) {
        // Message contains both a permanent and recoverable pattern
        let msg = format!("{} and also {}", perm, rec);
        let kind = classify_error_message(&msg);
        prop_assert_eq!(kind, ProtocolErrorKind::Permanent,
            "permanent should take priority: msg='{}', got {:?}", msg, kind);
    }
}
