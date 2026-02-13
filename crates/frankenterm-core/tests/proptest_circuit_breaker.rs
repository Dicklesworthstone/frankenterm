//! Property-based tests for circuit_breaker module.
//!
//! Verifies the circuit breaker state machine invariants:
//! - Starts Closed, allow() returns true
//! - N consecutive failures → Open (allow returns false)
//! - Success resets failure counter in Closed state
//! - HalfOpen: M successes → Closed; any failure → Open
//! - Config normalizes thresholds to >= 1
//! - Status fields match circuit state
//! - CircuitBreakerStatus serde roundtrip
//! - CircuitStateKind serde roundtrip
//!
//! Note: Time-dependent transitions (Open → HalfOpen after cooldown) are
//! not tested here since proptest can't control Instant. Those are covered
//! by the unit tests in circuit_breaker.rs.

use proptest::prelude::*;
use std::time::Duration;

use frankenterm_core::circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitBreakerStatus, CircuitStateKind,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_config() -> impl Strategy<Value = CircuitBreakerConfig> {
    (1u32..=10, 1u32..=10, 1u64..=60_000).prop_map(|(fail_t, succ_t, cooldown_ms)| {
        CircuitBreakerConfig::new(fail_t, succ_t, Duration::from_millis(cooldown_ms))
    })
}

// ────────────────────────────────────────────────────────────────────
// Initial state: starts Closed
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// New circuit breaker is always in Closed state.
    #[test]
    fn prop_starts_closed(config in arb_config()) {
        let mut cb = CircuitBreaker::new(config);
        prop_assert!(cb.allow(), "new circuit should allow operations");
        let status = cb.status();
        prop_assert_eq!(status.state, CircuitStateKind::Closed);
        prop_assert_eq!(status.consecutive_failures, 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// Config normalization
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Config::new normalizes thresholds to be at least 1.
    #[test]
    fn prop_config_thresholds_at_least_one(
        fail_t in 0u32..=10,
        succ_t in 0u32..=10,
        cooldown_ms in 0u64..=60_000,
    ) {
        let config = CircuitBreakerConfig::new(
            fail_t,
            succ_t,
            Duration::from_millis(cooldown_ms),
        );
        prop_assert!(config.failure_threshold >= 1);
        prop_assert!(config.success_threshold >= 1);
    }
}

// ────────────────────────────────────────────────────────────────────
// Failure threshold → Open
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Exactly failure_threshold consecutive failures transitions to Open.
    #[test]
    fn prop_failures_open_circuit(
        fail_t in 1u32..=10,
    ) {
        let config = CircuitBreakerConfig::new(fail_t, 1, Duration::from_secs(3600));
        let mut cb = CircuitBreaker::new(config);

        // Record failure_threshold - 1 failures: should still be Closed
        for _ in 0..fail_t.saturating_sub(1) {
            cb.record_failure();
            let status = cb.status();
            prop_assert_eq!(
                status.state, CircuitStateKind::Closed,
                "should still be Closed after {} failures (threshold {})",
                status.consecutive_failures, fail_t
            );
        }

        // The failure_threshold-th failure opens the circuit
        cb.record_failure();
        let status = cb.status();
        prop_assert_eq!(
            status.state, CircuitStateKind::Open,
            "should be Open after {} failures", fail_t
        );
    }

    /// Fewer than threshold failures keeps circuit Closed.
    #[test]
    fn prop_fewer_failures_stays_closed(
        fail_t in 2u32..=10,
        n_failures in 0u32..=9,
    ) {
        let actual_failures = n_failures.min(fail_t - 1);
        let config = CircuitBreakerConfig::new(fail_t, 1, Duration::from_secs(3600));
        let mut cb = CircuitBreaker::new(config);

        for _ in 0..actual_failures {
            cb.record_failure();
        }

        let status = cb.status();
        prop_assert_eq!(status.state, CircuitStateKind::Closed);
        prop_assert_eq!(status.consecutive_failures, actual_failures);
    }
}

// ────────────────────────────────────────────────────────────────────
// Success resets failure counter
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// A success in Closed state resets consecutive_failures to 0.
    #[test]
    fn prop_success_resets_failures(
        fail_t in 2u32..=10,
        n_failures in 1u32..=9,
    ) {
        let actual_failures = n_failures.min(fail_t - 1);
        let config = CircuitBreakerConfig::new(fail_t, 1, Duration::from_secs(3600));
        let mut cb = CircuitBreaker::new(config);

        // Accumulate some failures (but not enough to open)
        for _ in 0..actual_failures {
            cb.record_failure();
        }
        prop_assert_eq!(cb.status().consecutive_failures, actual_failures);

        // A success resets
        cb.record_success();
        prop_assert_eq!(cb.status().consecutive_failures, 0);
        prop_assert_eq!(cb.status().state, CircuitStateKind::Closed);
    }

    /// Alternating success and failure keeps circuit Closed (never accumulates threshold).
    #[test]
    fn prop_alternating_stays_closed(
        fail_t in 2u32..=10,
        n_rounds in 1usize..=20,
    ) {
        let config = CircuitBreakerConfig::new(fail_t, 1, Duration::from_secs(3600));
        let mut cb = CircuitBreaker::new(config);

        for _ in 0..n_rounds {
            cb.record_failure();
            cb.record_success();
        }

        let status = cb.status();
        prop_assert_eq!(status.state, CircuitStateKind::Closed);
        prop_assert_eq!(status.consecutive_failures, 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// Open state: allow returns false (with long cooldown)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Once open (with long cooldown), allow() returns false.
    #[test]
    fn prop_open_blocks_operations(
        fail_t in 1u32..=5,
    ) {
        // Use a very long cooldown so it won't expire during the test
        let config = CircuitBreakerConfig::new(fail_t, 1, Duration::from_secs(3600));
        let mut cb = CircuitBreaker::new(config);

        for _ in 0..fail_t {
            cb.record_failure();
        }

        prop_assert_eq!(cb.status().state, CircuitStateKind::Open);
        prop_assert!(!cb.allow(), "Open circuit should block operations");
    }
}

// ────────────────────────────────────────────────────────────────────
// HalfOpen: successes close, failure re-opens
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// In HalfOpen, success_threshold successes → Closed.
    #[test]
    fn prop_half_open_successes_close(
        fail_t in 1u32..=5,
        succ_t in 1u32..=5,
    ) {
        // Use zero cooldown so open immediately transitions to half-open
        let config = CircuitBreakerConfig::new(fail_t, succ_t, Duration::ZERO);
        let mut cb = CircuitBreaker::new(config);

        // Open the circuit
        for _ in 0..fail_t {
            cb.record_failure();
        }
        prop_assert_eq!(cb.status().state, CircuitStateKind::Open);

        // With zero cooldown, allow() transitions to HalfOpen
        prop_assert!(cb.allow());
        prop_assert_eq!(cb.status().state, CircuitStateKind::HalfOpen);

        // Record success_threshold - 1 successes: still HalfOpen
        for i in 0..succ_t.saturating_sub(1) {
            cb.record_success();
            let status = cb.status();
            prop_assert_eq!(
                status.state, CircuitStateKind::HalfOpen,
                "should still be HalfOpen after {} successes (threshold {})",
                i + 1, succ_t
            );
        }

        // The final success closes it
        cb.record_success();
        prop_assert_eq!(cb.status().state, CircuitStateKind::Closed);
        prop_assert_eq!(cb.status().consecutive_failures, 0);
    }

    /// In HalfOpen, a single failure re-opens the circuit.
    #[test]
    fn prop_half_open_failure_reopens(
        fail_t in 1u32..=5,
        succ_t in 2u32..=5, // need >1 so we can be in HalfOpen with partial successes
    ) {
        let config = CircuitBreakerConfig::new(fail_t, succ_t, Duration::ZERO);
        let mut cb = CircuitBreaker::new(config);

        // Open the circuit
        for _ in 0..fail_t {
            cb.record_failure();
        }

        // Transition to HalfOpen
        cb.allow();
        prop_assert_eq!(cb.status().state, CircuitStateKind::HalfOpen);

        // One failure re-opens
        cb.record_failure();
        prop_assert_eq!(cb.status().state, CircuitStateKind::Open);
    }
}

// ────────────────────────────────────────────────────────────────────
// Status fields consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Status reflects config fields.
    #[test]
    fn prop_status_reflects_config(
        fail_t in 1u32..=10,
        succ_t in 1u32..=10,
        cooldown_ms in 1u64..=60_000,
    ) {
        let config = CircuitBreakerConfig::new(fail_t, succ_t, Duration::from_millis(cooldown_ms));
        let cb = CircuitBreaker::new(config.clone());

        let status = cb.status();
        prop_assert_eq!(status.failure_threshold, config.failure_threshold);
        prop_assert_eq!(status.success_threshold, config.success_threshold);
        prop_assert_eq!(status.open_cooldown_ms, cooldown_ms);
    }

    /// Closed status has no open_for_ms, no cooldown_remaining_ms, no half_open_successes.
    #[test]
    fn prop_closed_status_fields(config in arb_config()) {
        let cb = CircuitBreaker::new(config);
        let status = cb.status();

        prop_assert_eq!(status.state, CircuitStateKind::Closed);
        prop_assert!(status.open_for_ms.is_none());
        prop_assert!(status.cooldown_remaining_ms.is_none());
        prop_assert!(status.half_open_successes.is_none());
    }

    /// Open status has open_for_ms set.
    #[test]
    fn prop_open_status_fields(
        fail_t in 1u32..=5,
    ) {
        let config = CircuitBreakerConfig::new(fail_t, 1, Duration::from_secs(3600));
        let mut cb = CircuitBreaker::new(config);

        for _ in 0..fail_t {
            cb.record_failure();
        }

        let status = cb.status();
        prop_assert_eq!(status.state, CircuitStateKind::Open);
        prop_assert!(status.open_for_ms.is_some());
        prop_assert!(status.half_open_successes.is_none());
    }

    /// HalfOpen status has half_open_successes set.
    #[test]
    fn prop_half_open_status_fields(
        fail_t in 1u32..=5,
        succ_t in 2u32..=5,
    ) {
        let config = CircuitBreakerConfig::new(fail_t, succ_t, Duration::ZERO);
        let mut cb = CircuitBreaker::new(config);

        for _ in 0..fail_t {
            cb.record_failure();
        }
        cb.allow(); // transitions to HalfOpen

        let status = cb.status();
        prop_assert_eq!(status.state, CircuitStateKind::HalfOpen);
        prop_assert_eq!(status.half_open_successes, Some(0));
    }
}

// ────────────────────────────────────────────────────────────────────
// Serde roundtrips
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// CircuitStateKind serde roundtrip.
    #[test]
    fn prop_state_kind_serde(
        kind in prop_oneof![
            Just(CircuitStateKind::Closed),
            Just(CircuitStateKind::Open),
            Just(CircuitStateKind::HalfOpen),
        ],
    ) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: CircuitStateKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }

    /// CircuitBreakerStatus serde roundtrip.
    #[test]
    fn prop_status_serde_roundtrip(
        failures in 0u32..=100,
        fail_t in 1u32..=10,
        succ_t in 1u32..=10,
        cooldown_ms in 0u64..=60_000,
    ) {
        let status = CircuitBreakerStatus {
            state: CircuitStateKind::Closed,
            consecutive_failures: failures,
            failure_threshold: fail_t,
            success_threshold: succ_t,
            open_cooldown_ms: cooldown_ms,
            open_for_ms: None,
            cooldown_remaining_ms: None,
            half_open_successes: None,
        };

        let json = serde_json::to_string(&status).unwrap();
        let back: CircuitBreakerStatus = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(status.state, back.state);
        prop_assert_eq!(status.consecutive_failures, back.consecutive_failures);
        prop_assert_eq!(status.failure_threshold, back.failure_threshold);
        prop_assert_eq!(status.success_threshold, back.success_threshold);
        prop_assert_eq!(status.open_cooldown_ms, back.open_cooldown_ms);
    }
}

// ────────────────────────────────────────────────────────────────────
// State machine sequences
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Op {
    RecordSuccess,
    RecordFailure,
}

fn arb_ops(max_len: usize) -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![Just(Op::RecordSuccess), Just(Op::RecordFailure)],
        1..max_len,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// After any sequence of operations, status state is one of {Closed, Open, HalfOpen}.
    #[test]
    fn prop_state_always_valid(
        fail_t in 1u32..=5,
        succ_t in 1u32..=5,
        ops in arb_ops(50),
    ) {
        let config = CircuitBreakerConfig::new(fail_t, succ_t, Duration::ZERO);
        let mut cb = CircuitBreaker::new(config);

        for op in &ops {
            // Call allow to potentially transition Open → HalfOpen
            cb.allow();
            match op {
                Op::RecordSuccess => cb.record_success(),
                Op::RecordFailure => cb.record_failure(),
            }
        }

        let status = cb.status();
        let valid = matches!(
            status.state,
            CircuitStateKind::Closed | CircuitStateKind::Open | CircuitStateKind::HalfOpen
        );
        prop_assert!(valid, "invalid state: {:?}", status.state);
    }

    /// consecutive_failures never exceeds failure_threshold (once it hits threshold, circuit opens).
    #[test]
    fn prop_failures_bounded_by_threshold(
        fail_t in 1u32..=10,
        ops in arb_ops(30),
    ) {
        let config = CircuitBreakerConfig::new(fail_t, 1, Duration::from_secs(3600));
        let mut cb = CircuitBreaker::new(config);

        for op in &ops {
            match op {
                Op::RecordSuccess => cb.record_success(),
                Op::RecordFailure => cb.record_failure(),
            }
            let status = cb.status();
            // In Closed state, failures are bounded by threshold
            // (at threshold, it transitions to Open)
            if status.state == CircuitStateKind::Closed {
                prop_assert!(
                    status.consecutive_failures < fail_t,
                    "in Closed state, failures {} should be < threshold {}",
                    status.consecutive_failures, fail_t
                );
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Full cycle: Closed → Open → HalfOpen → Closed
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// A complete state machine cycle works correctly.
    #[test]
    fn prop_full_cycle(
        fail_t in 1u32..=5,
        succ_t in 1u32..=5,
    ) {
        let config = CircuitBreakerConfig::new(fail_t, succ_t, Duration::ZERO);
        let mut cb = CircuitBreaker::new(config);

        // 1. Start Closed
        prop_assert_eq!(cb.status().state, CircuitStateKind::Closed);

        // 2. Open via failures
        for _ in 0..fail_t {
            cb.record_failure();
        }
        prop_assert_eq!(cb.status().state, CircuitStateKind::Open);

        // 3. Transition to HalfOpen (zero cooldown)
        prop_assert!(cb.allow());
        prop_assert_eq!(cb.status().state, CircuitStateKind::HalfOpen);

        // 4. Close via successes
        for _ in 0..succ_t {
            cb.record_success();
        }
        prop_assert_eq!(cb.status().state, CircuitStateKind::Closed);
        prop_assert_eq!(cb.status().consecutive_failures, 0);
    }
}
