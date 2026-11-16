//! Property-based tests for completion tracker telemetry counters (ft-3kxe.21).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. tokens_requested / tokens_created / tokens_rejected track begin() calls
//! 3. advances tracks advance() calls
//! 4. completions / failures / timeouts / partial_failures track terminal states
//! 5. evictions tracks evict_completed() results
//! 6. Serde roundtrip for snapshot
//! 7. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::completion_token::{
    CompletionBoundary, CompletionTracker, CompletionTrackerConfig,
    CompletionTrackerTelemetrySnapshot, StepOutcome,
};

// =============================================================================
// Helpers
// =============================================================================

fn make_tracker() -> CompletionTracker {
    CompletionTracker::new(CompletionTrackerConfig {
        default_timeout_ms: 0, // no timeout
        max_active_tokens: 100,
        retention_ms: 0, // immediate eviction for testing
    })
}

fn make_tiny_tracker() -> CompletionTracker {
    CompletionTracker::new(CompletionTrackerConfig {
        default_timeout_ms: 0,
        max_active_tokens: 2,
        retention_ms: 0,
    })
}

fn simple_boundary() -> CompletionBoundary {
    CompletionBoundary::new(&["step_a"])
}

fn two_step_boundary() -> CompletionBoundary {
    CompletionBoundary::new(&["step_a", "step_b"])
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let tracker = make_tracker();
    let snap = tracker.telemetry().snapshot();

    assert_eq!(snap.tokens_requested, 0);
    assert_eq!(snap.tokens_created, 0);
    assert_eq!(snap.tokens_rejected, 0);
    assert_eq!(snap.advances, 0);
    assert_eq!(snap.completions, 0);
    assert_eq!(snap.failures, 0);
    assert_eq!(snap.timeouts, 0);
    assert_eq!(snap.partial_failures, 0);
    assert_eq!(snap.evictions, 0);
}

#[test]
fn begin_tracks_requests_and_creates() {
    let mut tracker = make_tracker();

    tracker.begin("op1", simple_boundary());
    tracker.begin("op2", simple_boundary());

    let snap = tracker.telemetry().snapshot();
    assert_eq!(snap.tokens_requested, 2);
    assert_eq!(snap.tokens_created, 2);
    assert_eq!(snap.tokens_rejected, 0);
}

#[test]
fn begin_rejected_at_capacity() {
    let mut tracker = make_tiny_tracker(); // max 2

    let t1 = tracker.begin("op1", simple_boundary());
    let t2 = tracker.begin("op2", simple_boundary());
    let t3 = tracker.begin("op3", simple_boundary());

    assert!(t1.is_some());
    assert!(t2.is_some());
    assert!(t3.is_none()); // rejected

    let snap = tracker.telemetry().snapshot();
    assert_eq!(snap.tokens_requested, 3);
    assert_eq!(snap.tokens_created, 2);
    assert_eq!(snap.tokens_rejected, 1);
}

#[test]
fn advance_counted() {
    let mut tracker = make_tracker();
    let tid = tracker.begin("op", two_step_boundary()).unwrap();

    tracker.advance(&tid, "step_a", StepOutcome::Ok, "done");
    tracker.advance(&tid, "step_b", StepOutcome::Ok, "done");

    let snap = tracker.telemetry().snapshot();
    assert_eq!(snap.advances, 2);
}

#[test]
fn completion_tracked() {
    let mut tracker = make_tracker();
    let tid = tracker.begin("op", simple_boundary()).unwrap();

    tracker.advance(&tid, "step_a", StepOutcome::Ok, "done");

    let snap = tracker.telemetry().snapshot();
    assert_eq!(snap.completions, 1);
    assert_eq!(snap.failures, 0);
}

#[test]
fn failure_tracked() {
    let mut tracker = make_tracker();
    let tid = tracker.begin("op", simple_boundary()).unwrap();

    tracker.advance(&tid, "step_a", StepOutcome::Error, "boom");

    let snap = tracker.telemetry().snapshot();
    assert_eq!(snap.failures, 1);
    assert_eq!(snap.completions, 0);
}

#[test]
fn partial_failure_tracked() {
    let mut tracker = make_tracker();
    let tid = tracker.begin("op", two_step_boundary()).unwrap();

    // First step succeeds
    tracker.advance(&tid, "step_a", StepOutcome::Ok, "ok");
    // Second step fails → partial failure (has both ok and error)
    tracker.advance(&tid, "step_b", StepOutcome::Error, "boom");

    let snap = tracker.telemetry().snapshot();
    assert_eq!(snap.partial_failures, 1);
    assert_eq!(snap.failures, 0);
    assert_eq!(snap.completions, 0);
}

#[test]
fn timeout_tracked() {
    let mut tracker = make_tracker();
    let tid = tracker.begin("op", simple_boundary()).unwrap();

    tracker.timeout(&tid);

    let snap = tracker.telemetry().snapshot();
    assert_eq!(snap.timeouts, 1);
}

#[test]
fn eviction_tracked() {
    let mut tracker = make_tracker();
    let tid = tracker.begin("op", simple_boundary()).unwrap();

    // Complete the token
    tracker.advance(&tid, "step_a", StepOutcome::Ok, "done");

    // Evict it (retention_ms=0 so it's immediately eligible)
    let evicted = tracker.evict_completed();
    assert_eq!(evicted, 1);

    let snap = tracker.telemetry().snapshot();
    assert_eq!(snap.evictions, 1);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = CompletionTrackerTelemetrySnapshot {
        tokens_requested: 100,
        tokens_created: 90,
        tokens_rejected: 10,
        advances: 500,
        completions: 60,
        failures: 15,
        timeouts: 10,
        partial_failures: 5,
        evictions: 50,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: CompletionTrackerTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn mixed_operations() {
    let mut tracker = make_tracker();

    // Success path
    let t1 = tracker.begin("op1", simple_boundary()).unwrap();
    tracker.advance(&t1, "step_a", StepOutcome::Ok, "done");

    // Failure path
    let t2 = tracker.begin("op2", simple_boundary()).unwrap();
    tracker.advance(&t2, "step_a", StepOutcome::Error, "fail");

    // Timeout path
    let t3 = tracker.begin("op3", simple_boundary()).unwrap();
    tracker.timeout(&t3);

    let snap = tracker.telemetry().snapshot();
    assert_eq!(snap.tokens_requested, 3);
    assert_eq!(snap.tokens_created, 3);
    assert_eq!(snap.advances, 2); // 1 + 1 + 0: timeout doesn't go through advance
    assert_eq!(snap.completions, 1);
    assert_eq!(snap.failures, 1);
    assert_eq!(snap.timeouts, 1);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn tokens_requested_equals_begin_calls(
        count in 1usize..30,
    ) {
        let mut tracker = make_tracker();
        for _ in 0..count {
            tracker.begin("op", simple_boundary());
        }
        let snap = tracker.telemetry().snapshot();
        prop_assert_eq!(snap.tokens_requested, count as u64);
        prop_assert_eq!(snap.tokens_created, count as u64);
    }

    #[test]
    fn created_plus_rejected_equals_requested(
        count in 1usize..10,
    ) {
        let mut tracker = make_tiny_tracker(); // max 2
        for _ in 0..count {
            tracker.begin("op", simple_boundary());
        }
        let snap = tracker.telemetry().snapshot();
        prop_assert_eq!(
            snap.tokens_created + snap.tokens_rejected,
            snap.tokens_requested,
            "created ({}) + rejected ({}) != requested ({})",
            snap.tokens_created, snap.tokens_rejected, snap.tokens_requested
        );
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..3, 1..20),
    ) {
        let mut tracker = make_tracker();

        // Pre-create a token for advance operations
        let tid = tracker.begin("op", simple_boundary()).unwrap();
        let mut prev = tracker.telemetry().snapshot();

        for op in &ops {
            match op {
                0 => { tracker.begin("op", simple_boundary()); }
                1 => { tracker.advance(&tid, "step_a", StepOutcome::Ok, "ok"); }
                2 => {
                    let t = tracker.begin("op", simple_boundary());
                    if let Some(t) = t {
                        tracker.timeout(&t);
                    }
                }
                _ => unreachable!(),
            }

            let snap = tracker.telemetry().snapshot();
            prop_assert!(snap.tokens_requested >= prev.tokens_requested,
                "tokens_requested decreased");
            prop_assert!(snap.tokens_created >= prev.tokens_created,
                "tokens_created decreased");
            prop_assert!(snap.advances >= prev.advances,
                "advances decreased");
            prop_assert!(snap.completions >= prev.completions,
                "completions decreased");
            prop_assert!(snap.failures >= prev.failures,
                "failures decreased");
            prop_assert!(snap.timeouts >= prev.timeouts,
                "timeouts decreased");

            prev = snap;
        }
    }

    #[test]
    fn terminal_states_bounded_by_created(
        success_count in 0usize..10,
        failure_count in 0usize..10,
    ) {
        let mut tracker = make_tracker();

        for _ in 0..success_count {
            let tid = tracker.begin("op", simple_boundary()).unwrap();
            tracker.advance(&tid, "step_a", StepOutcome::Ok, "done");
        }
        for _ in 0..failure_count {
            let tid = tracker.begin("op", simple_boundary()).unwrap();
            tracker.advance(&tid, "step_a", StepOutcome::Error, "fail");
        }

        let snap = tracker.telemetry().snapshot();
        let total_terminal = snap.completions + snap.failures
            + snap.timeouts + snap.partial_failures;
        prop_assert!(
            total_terminal <= snap.tokens_created,
            "terminal ({}) > created ({})",
            total_terminal, snap.tokens_created
        );
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        req in 0u64..100000,
        created in 0u64..100000,
        rejected in 0u64..10000,
        advances in 0u64..100000,
        completions in 0u64..50000,
        failures in 0u64..20000,
        timeouts in 0u64..10000,
        partials in 0u64..10000,
    ) {
        let snap = CompletionTrackerTelemetrySnapshot {
            tokens_requested: req,
            tokens_created: created,
            tokens_rejected: rejected,
            advances,
            completions,
            failures,
            timeouts,
            partial_failures: partials,
            evictions: 0,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: CompletionTrackerTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
