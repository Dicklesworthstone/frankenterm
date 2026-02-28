//! Property-based tests for token bucket telemetry counters (ft-3kxe.23).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. acquires tracks try_acquire() calls
//! 3. acquires_granted + acquires_denied = acquires
//! 4. tokens_consumed matches total tokens consumed
//! 5. refills tracks time-advancing refill computations
//! 6. resets tracks reset() calls
//! 7. rate_changes tracks set_refill_rate() calls
//! 8. Serde roundtrip for snapshot
//! 9. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::token_bucket::{TokenBucket, TokenBucketTelemetrySnapshot};

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let b = TokenBucket::new(10.0, 5.0);
    let snap = b.telemetry();

    assert_eq!(snap.acquires, 0);
    assert_eq!(snap.acquires_granted, 0);
    assert_eq!(snap.acquires_denied, 0);
    assert_eq!(snap.tokens_consumed, 0);
    assert_eq!(snap.refills, 0);
    assert_eq!(snap.resets, 0);
    assert_eq!(snap.rate_changes, 0);
}

#[test]
fn acquires_tracked() {
    let mut b = TokenBucket::with_time(10.0, 5.0, 0);
    b.try_acquire(1, 0);
    b.try_acquire(1, 0);
    b.try_acquire(1, 0);

    let snap = b.telemetry();
    assert_eq!(snap.acquires, 3);
    assert_eq!(snap.acquires_granted, 3);
    assert_eq!(snap.acquires_denied, 0);
}

#[test]
fn denied_acquires_tracked() {
    let mut b = TokenBucket::with_time(2.0, 1.0, 0);
    b.try_acquire(1, 0); // granted
    b.try_acquire(1, 0); // granted
    b.try_acquire(1, 0); // denied (empty)
    b.try_acquire(1, 0); // denied

    let snap = b.telemetry();
    assert_eq!(snap.acquires, 4);
    assert_eq!(snap.acquires_granted, 2);
    assert_eq!(snap.acquires_denied, 2);
}

#[test]
fn granted_plus_denied_equals_acquires() {
    let mut b = TokenBucket::with_time(3.0, 1.0, 0);
    for _ in 0..10 {
        b.try_acquire(1, 0);
    }

    let snap = b.telemetry();
    assert_eq!(snap.acquires_granted + snap.acquires_denied, snap.acquires);
}

#[test]
fn tokens_consumed_tracks_cost() {
    let mut b = TokenBucket::with_time(100.0, 10.0, 0);
    b.try_acquire(5, 0);
    b.try_acquire(10, 0);
    b.try_acquire(3, 0);

    let snap = b.telemetry();
    assert_eq!(snap.tokens_consumed, 18);
}

#[test]
fn refills_tracked_on_time_advance() {
    let mut b = TokenBucket::with_time(10.0, 5.0, 0);
    // Acquire at same time — no refill (first call refills from t=0)
    b.try_acquire(1, 0);
    // Acquire at later time — refill
    b.try_acquire(1, 100);
    // Acquire at same time as previous — no refill
    b.try_acquire(1, 100);
    // Acquire at later time — refill
    b.try_acquire(1, 200);

    let snap = b.telemetry();
    // Refill at t=100 and t=200 (t=0 has no advance since last_refill_ms starts at 0)
    assert_eq!(snap.refills, 2);
}

#[test]
fn resets_tracked() {
    let mut b = TokenBucket::with_time(10.0, 5.0, 0);
    b.try_acquire(10, 0);
    b.reset(100);
    b.try_acquire(5, 100);
    b.reset(200);

    let snap = b.telemetry();
    assert_eq!(snap.resets, 2);
}

#[test]
fn rate_changes_tracked() {
    let mut b = TokenBucket::new(10.0, 5.0);
    b.set_refill_rate(10.0);
    b.set_refill_rate(2.0);
    b.set_refill_rate(7.5);

    let snap = b.telemetry();
    assert_eq!(snap.rate_changes, 3);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = TokenBucketTelemetrySnapshot {
        acquires: 1000,
        acquires_granted: 800,
        acquires_denied: 200,
        tokens_consumed: 5000,
        refills: 500,
        resets: 10,
        rate_changes: 5,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: TokenBucketTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn mixed_operations() {
    let mut b = TokenBucket::with_time(5.0, 5.0, 0);

    // Grant 5
    for _ in 0..5 {
        b.try_acquire(1, 0);
    }
    // Deny 2
    b.try_acquire(1, 0);
    b.try_acquire(1, 0);

    // Reset
    b.reset(100);

    // Grant 3 more with time advance
    b.try_acquire(1, 200);
    b.try_acquire(1, 300);
    b.try_acquire(1, 400);

    // Change rate
    b.set_refill_rate(10.0);

    let snap = b.telemetry();
    assert_eq!(snap.acquires, 10);
    assert_eq!(snap.acquires_granted, 8);
    assert_eq!(snap.acquires_denied, 2);
    assert_eq!(snap.tokens_consumed, 8);
    assert_eq!(snap.resets, 1);
    assert_eq!(snap.rate_changes, 1);
}

#[test]
fn telemetry_preserved_across_reset() {
    let mut b = TokenBucket::with_time(5.0, 5.0, 0);
    b.try_acquire(3, 0);
    b.reset(0);

    let snap = b.telemetry();
    // Reset doesn't clear telemetry counters
    assert_eq!(snap.acquires, 1);
    assert_eq!(snap.tokens_consumed, 3);
    assert_eq!(snap.resets, 1);
}

#[test]
fn try_acquire_one_counted() {
    let mut b = TokenBucket::with_time(10.0, 5.0, 0);
    b.try_acquire_one(0);
    b.try_acquire_one(0);

    let snap = b.telemetry();
    assert_eq!(snap.acquires, 2);
    assert_eq!(snap.tokens_consumed, 2);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn acquires_equals_call_count(
        count in 1usize..50,
    ) {
        let mut b = TokenBucket::with_time(1000.0, 100.0, 0);
        for _ in 0..count {
            b.try_acquire(1, 0);
        }
        let snap = b.telemetry();
        prop_assert_eq!(snap.acquires, count as u64);
    }

    #[test]
    fn granted_plus_denied_equals_acquires_prop(
        cap in 1u32..20,
        count in 1usize..50,
    ) {
        let mut b = TokenBucket::with_time(cap as f64, 1.0, 0);
        for _ in 0..count {
            b.try_acquire(1, 0);
        }
        let snap = b.telemetry();
        prop_assert_eq!(
            snap.acquires_granted + snap.acquires_denied,
            snap.acquires,
            "granted ({}) + denied ({}) != acquires ({})",
            snap.acquires_granted, snap.acquires_denied, snap.acquires
        );
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..5, 1..30),
    ) {
        let mut b = TokenBucket::with_time(10.0, 5.0, 0);
        let mut prev = b.telemetry();
        let mut time = 0u64;

        for op in &ops {
            match op {
                0 => { b.try_acquire(1, time); }
                1 => { time += 100; b.try_acquire(1, time); }
                2 => { b.reset(time); }
                3 => { b.set_refill_rate(5.0); }
                4 => { time += 50; b.try_acquire(3, time); }
                _ => unreachable!(),
            }

            let snap = b.telemetry();
            prop_assert!(snap.acquires >= prev.acquires,
                "acquires decreased: {} -> {}", prev.acquires, snap.acquires);
            prop_assert!(snap.acquires_denied >= prev.acquires_denied,
                "acquires_denied decreased: {} -> {}",
                prev.acquires_denied, snap.acquires_denied);
            prop_assert!(snap.tokens_consumed >= prev.tokens_consumed,
                "tokens_consumed decreased: {} -> {}",
                prev.tokens_consumed, snap.tokens_consumed);
            prop_assert!(snap.refills >= prev.refills,
                "refills decreased: {} -> {}", prev.refills, snap.refills);
            prop_assert!(snap.resets >= prev.resets,
                "resets decreased: {} -> {}", prev.resets, snap.resets);
            prop_assert!(snap.rate_changes >= prev.rate_changes,
                "rate_changes decreased: {} -> {}",
                prev.rate_changes, snap.rate_changes);

            prev = snap;
        }
    }

    #[test]
    fn tokens_consumed_bounded_by_capacity_times_acquires(
        cap in 1u32..50,
        count in 1usize..30,
    ) {
        let mut b = TokenBucket::with_time(cap as f64, 100.0, 0);
        for i in 0..count {
            b.try_acquire(1, (i * 1000) as u64);
        }
        let snap = b.telemetry();
        // tokens_consumed can't exceed total acquires * max_cost_per_acquire (which is 1)
        prop_assert!(snap.tokens_consumed <= snap.acquires,
            "tokens_consumed ({}) > acquires ({})",
            snap.tokens_consumed, snap.acquires);
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        acquires in 0u64..100000,
        granted in 0u64..50000,
        denied in 0u64..50000,
        consumed in 0u64..100000,
        refills in 0u64..50000,
        resets in 0u64..10000,
        rate_changes in 0u64..10000,
    ) {
        let snap = TokenBucketTelemetrySnapshot {
            acquires,
            acquires_granted: granted,
            acquires_denied: denied,
            tokens_consumed: consumed,
            refills,
            resets,
            rate_changes,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: TokenBucketTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
