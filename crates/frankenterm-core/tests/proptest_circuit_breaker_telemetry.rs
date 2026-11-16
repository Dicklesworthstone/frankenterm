//! Property-based tests for circuit breaker telemetry counters (ft-3kxe.15).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. failures_recorded / successes_recorded track call counts
//! 3. trips_total tracks Closed/HalfOpen → Open transitions
//! 4. resets_total tracks HalfOpen → Closed transitions
//! 5. half_open_probes tracks Open → HalfOpen transitions
//! 6. requests_rejected tracks blocked allow() calls
//! 7. Serde roundtrip for snapshot
//! 8. Counter monotonicity across repeated operations

use proptest::prelude::*;
use std::time::Duration;

use frankenterm_core::circuit_breaker::{
    CircuitBreaker, CircuitBreakerConfig, CircuitBreakerTelemetrySnapshot,
};

// =============================================================================
// Helpers
// =============================================================================

fn make_breaker(failure_threshold: u32, success_threshold: u32) -> CircuitBreaker {
    CircuitBreaker::new(CircuitBreakerConfig::new(
        failure_threshold,
        success_threshold,
        Duration::from_millis(0), // instant cooldown for tests
    ))
}

/// Operation to apply to a circuit breaker.
#[derive(Debug, Clone, Copy)]
enum Op {
    Allow,
    RecordSuccess,
    RecordFailure,
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::Allow),
        Just(Op::RecordSuccess),
        Just(Op::RecordFailure),
    ]
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let breaker = make_breaker(3, 1);
    let snap = breaker.telemetry().snapshot();

    assert_eq!(snap.trips_total, 0);
    assert_eq!(snap.resets_total, 0);
    assert_eq!(snap.half_open_probes, 0);
    assert_eq!(snap.successes_recorded, 0);
    assert_eq!(snap.failures_recorded, 0);
    assert_eq!(snap.requests_rejected, 0);
}

#[test]
fn failures_recorded_counts_all_calls() {
    let mut breaker = make_breaker(5, 1);

    for _ in 0..3 {
        breaker.record_failure();
    }

    let snap = breaker.telemetry().snapshot();
    assert_eq!(snap.failures_recorded, 3);
    assert_eq!(snap.trips_total, 0); // below threshold
}

#[test]
fn trip_on_threshold() {
    let mut breaker = make_breaker(2, 1);

    breaker.record_failure();
    assert_eq!(breaker.telemetry().snapshot().trips_total, 0);

    breaker.record_failure(); // triggers trip
    assert_eq!(breaker.telemetry().snapshot().trips_total, 1);
    assert_eq!(breaker.telemetry().snapshot().failures_recorded, 2);
}

#[test]
fn requests_rejected_while_open() {
    let mut breaker = CircuitBreaker::new(CircuitBreakerConfig::new(
        1,
        1,
        Duration::from_secs(60), // long cooldown so it stays open
    ));

    breaker.record_failure(); // opens
    assert!(!breaker.allow()); // rejected
    assert!(!breaker.allow()); // rejected

    let snap = breaker.telemetry().snapshot();
    assert_eq!(snap.requests_rejected, 2);
    assert_eq!(snap.trips_total, 1);
}

#[test]
fn half_open_probe_tracked() {
    let mut breaker = make_breaker(1, 1);

    breaker.record_failure(); // open
    breaker.allow(); // transitions to half-open (0ms cooldown)

    let snap = breaker.telemetry().snapshot();
    assert_eq!(snap.half_open_probes, 1);
}

#[test]
fn reset_tracked_on_close_from_half_open() {
    let mut breaker = make_breaker(1, 1);

    breaker.record_failure(); // open
    breaker.allow(); // half-open
    breaker.record_success(); // closes

    let snap = breaker.telemetry().snapshot();
    assert_eq!(snap.resets_total, 1);
    assert_eq!(snap.trips_total, 1);
}

#[test]
fn full_lifecycle_telemetry() {
    let mut breaker = make_breaker(1, 2);

    // Trip
    breaker.record_failure(); // open
    // Probe
    breaker.allow(); // half-open
    // Fail in half-open → re-trip
    breaker.record_failure(); // back to open
    // Probe again
    breaker.allow(); // half-open
    // Two successes to reset
    breaker.record_success();
    breaker.record_success(); // closes

    let snap = breaker.telemetry().snapshot();
    assert_eq!(snap.trips_total, 2); // initial trip + re-trip from half-open
    assert_eq!(snap.resets_total, 1); // one successful close
    assert_eq!(snap.half_open_probes, 2); // two transitions to half-open
    assert_eq!(snap.failures_recorded, 2);
    assert_eq!(snap.successes_recorded, 2);
}

#[test]
fn successes_in_closed_state_count() {
    let mut breaker = make_breaker(3, 1);

    breaker.record_success();
    breaker.record_success();
    breaker.record_success();

    let snap = breaker.telemetry().snapshot();
    assert_eq!(snap.successes_recorded, 3);
    assert_eq!(snap.trips_total, 0);
    assert_eq!(snap.resets_total, 0);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = CircuitBreakerTelemetrySnapshot {
        trips_total: 10,
        resets_total: 8,
        half_open_probes: 12,
        successes_recorded: 100,
        failures_recorded: 50,
        requests_rejected: 25,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: CircuitBreakerTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn failures_recorded_equals_call_count(
        count in 1usize..50,
    ) {
        let mut breaker = make_breaker(100, 1); // high threshold so never trips
        for _ in 0..count {
            breaker.record_failure();
        }
        let snap = breaker.telemetry().snapshot();
        prop_assert_eq!(snap.failures_recorded, count as u64);
    }

    #[test]
    fn successes_recorded_equals_call_count(
        count in 1usize..50,
    ) {
        let mut breaker = make_breaker(3, 1);
        for _ in 0..count {
            breaker.record_success();
        }
        let snap = breaker.telemetry().snapshot();
        prop_assert_eq!(snap.successes_recorded, count as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(arb_op(), 1..30),
    ) {
        let mut breaker = make_breaker(2, 1);
        let mut prev = breaker.telemetry().snapshot();

        for op in &ops {
            match op {
                Op::Allow => { breaker.allow(); }
                Op::RecordSuccess => { breaker.record_success(); }
                Op::RecordFailure => { breaker.record_failure(); }
            }

            let snap = breaker.telemetry().snapshot();
            prop_assert!(
                snap.trips_total >= prev.trips_total,
                "trips_total decreased: {} -> {}",
                prev.trips_total, snap.trips_total
            );
            prop_assert!(
                snap.resets_total >= prev.resets_total,
                "resets_total decreased: {} -> {}",
                prev.resets_total, snap.resets_total
            );
            prop_assert!(
                snap.failures_recorded >= prev.failures_recorded,
                "failures_recorded decreased: {} -> {}",
                prev.failures_recorded, snap.failures_recorded
            );
            prop_assert!(
                snap.successes_recorded >= prev.successes_recorded,
                "successes_recorded decreased: {} -> {}",
                prev.successes_recorded, snap.successes_recorded
            );
            prop_assert!(
                snap.requests_rejected >= prev.requests_rejected,
                "requests_rejected decreased: {} -> {}",
                prev.requests_rejected, snap.requests_rejected
            );
            prop_assert!(
                snap.half_open_probes >= prev.half_open_probes,
                "half_open_probes decreased: {} -> {}",
                prev.half_open_probes, snap.half_open_probes
            );

            prev = snap;
        }
    }

    #[test]
    fn trips_bounded_by_failures(
        failure_count in 1usize..30,
        threshold in 1u32..5,
    ) {
        let mut breaker = make_breaker(threshold, 1);
        for _ in 0..failure_count {
            breaker.record_failure();
            // Allow to enable half-open → re-trip cycles
            breaker.allow();
        }
        let snap = breaker.telemetry().snapshot();
        // Trips can't exceed failures (each trip needs at least one failure)
        prop_assert!(
            snap.trips_total <= snap.failures_recorded,
            "trips ({}) > failures ({})",
            snap.trips_total, snap.failures_recorded
        );
    }

    #[test]
    fn resets_bounded_by_successes(
        ops in prop::collection::vec(arb_op(), 1..50),
    ) {
        let mut breaker = make_breaker(1, 1);
        for op in &ops {
            match op {
                Op::Allow => { breaker.allow(); }
                Op::RecordSuccess => { breaker.record_success(); }
                Op::RecordFailure => { breaker.record_failure(); }
            }
        }
        let snap = breaker.telemetry().snapshot();
        // Resets can't exceed successes (each reset needs successes in half-open)
        prop_assert!(
            snap.resets_total <= snap.successes_recorded,
            "resets ({}) > successes ({})",
            snap.resets_total, snap.successes_recorded
        );
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        trips in 0u64..10000,
        resets in 0u64..10000,
        probes in 0u64..10000,
        successes in 0u64..50000,
        failures in 0u64..50000,
        rejected in 0u64..50000,
    ) {
        let snap = CircuitBreakerTelemetrySnapshot {
            trips_total: trips,
            resets_total: resets,
            half_open_probes: probes,
            successes_recorded: successes,
            failures_recorded: failures,
            requests_rejected: rejected,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: CircuitBreakerTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
