//! Property-based tests for ring buffer telemetry counters (ft-3kxe.22).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. pushes tracks push() calls
//! 3. overwrites tracks push() calls when buffer was full
//! 4. clears tracks clear() calls
//! 5. drains tracks drain() calls
//! 6. items_drained tracks total items drained
//! 7. Serde roundtrip for snapshot
//! 8. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::ring_buffer::{RingBuffer, RingBufferTelemetrySnapshot};

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let rb: RingBuffer<i32> = RingBuffer::new(5);
    let snap = rb.telemetry();

    assert_eq!(snap.pushes, 0);
    assert_eq!(snap.overwrites, 0);
    assert_eq!(snap.clears, 0);
    assert_eq!(snap.drains, 0);
    assert_eq!(snap.items_drained, 0);
}

#[test]
fn pushes_tracked() {
    let mut rb = RingBuffer::new(10);
    rb.push(1);
    rb.push(2);
    rb.push(3);

    let snap = rb.telemetry();
    assert_eq!(snap.pushes, 3);
    assert_eq!(snap.overwrites, 0);
}

#[test]
fn overwrites_tracked_when_full() {
    let mut rb = RingBuffer::new(2);
    rb.push(1);
    rb.push(2);
    // Buffer is now full
    rb.push(3); // overwrite
    rb.push(4); // overwrite

    let snap = rb.telemetry();
    assert_eq!(snap.pushes, 4);
    assert_eq!(snap.overwrites, 2);
}

#[test]
fn no_overwrites_when_not_full() {
    let mut rb = RingBuffer::new(10);
    for i in 0..5 {
        rb.push(i);
    }

    let snap = rb.telemetry();
    assert_eq!(snap.pushes, 5);
    assert_eq!(snap.overwrites, 0);
}

#[test]
fn clears_tracked() {
    let mut rb = RingBuffer::new(5);
    rb.push(1);
    rb.push(2);
    rb.clear();
    rb.push(3);
    rb.clear();

    let snap = rb.telemetry();
    assert_eq!(snap.clears, 2);
}

#[test]
fn drains_tracked() {
    let mut rb = RingBuffer::new(5);
    rb.push(1);
    rb.push(2);
    rb.push(3);
    let _ = rb.drain();

    let snap = rb.telemetry();
    assert_eq!(snap.drains, 1);
    assert_eq!(snap.items_drained, 3);
}

#[test]
fn drain_empty_buffer_counts() {
    let mut rb: RingBuffer<i32> = RingBuffer::new(5);
    let _ = rb.drain();

    let snap = rb.telemetry();
    assert_eq!(snap.drains, 1);
    assert_eq!(snap.items_drained, 0);
}

#[test]
fn multiple_drains_accumulate() {
    let mut rb = RingBuffer::new(5);
    rb.push(1);
    rb.push(2);
    let _ = rb.drain(); // drains 2

    rb.push(3);
    rb.push(4);
    rb.push(5);
    let _ = rb.drain(); // drains 3

    let snap = rb.telemetry();
    assert_eq!(snap.drains, 2);
    assert_eq!(snap.items_drained, 5);
}

#[test]
fn overwrites_exact_boundary() {
    let mut rb = RingBuffer::new(3);
    // Fill to capacity (no overwrites)
    rb.push(1);
    rb.push(2);
    rb.push(3);
    assert_eq!(rb.telemetry().overwrites, 0);

    // One more → first overwrite
    rb.push(4);
    assert_eq!(rb.telemetry().overwrites, 1);
}

#[test]
fn capacity_one_every_push_after_first_overwrites() {
    let mut rb = RingBuffer::new(1);
    rb.push(1);
    assert_eq!(rb.telemetry().overwrites, 0);

    rb.push(2);
    assert_eq!(rb.telemetry().overwrites, 1);

    rb.push(3);
    assert_eq!(rb.telemetry().overwrites, 2);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = RingBufferTelemetrySnapshot {
        pushes: 1000,
        overwrites: 500,
        clears: 10,
        drains: 5,
        items_drained: 200,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: RingBufferTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn mixed_operations() {
    let mut rb = RingBuffer::new(3);

    // Push without overwrite
    rb.push(1);
    rb.push(2);
    rb.push(3);

    // Push with overwrite
    rb.push(4);
    rb.push(5);

    // Clear
    rb.clear();

    // Push again
    rb.push(6);
    rb.push(7);

    // Drain
    let drained = rb.drain();
    assert_eq!(drained, vec![6, 7]);

    let snap = rb.telemetry();
    assert_eq!(snap.pushes, 7);
    assert_eq!(snap.overwrites, 2);
    assert_eq!(snap.clears, 1);
    assert_eq!(snap.drains, 1);
    assert_eq!(snap.items_drained, 2);
}

#[test]
fn clear_does_not_reset_telemetry() {
    let mut rb = RingBuffer::new(3);
    rb.push(1);
    rb.push(2);
    rb.clear();

    let snap = rb.telemetry();
    // pushes and clears are still tracked after clear
    assert_eq!(snap.pushes, 2);
    assert_eq!(snap.clears, 1);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn pushes_equals_push_calls(
        count in 1usize..50,
    ) {
        let mut rb = RingBuffer::new(100);
        for i in 0..count {
            rb.push(i);
        }
        let snap = rb.telemetry();
        prop_assert_eq!(snap.pushes, count as u64);
    }

    #[test]
    fn overwrites_equals_pushes_minus_capacity_when_exceeded(
        cap in 1usize..20,
        count in 1usize..100,
    ) {
        let mut rb = RingBuffer::new(cap);
        for i in 0..count {
            rb.push(i);
        }
        let snap = rb.telemetry();
        let expected_overwrites = if count > cap { (count - cap) as u64 } else { 0 };
        prop_assert_eq!(snap.overwrites, expected_overwrites,
            "cap={}, count={}, expected overwrites={}, got={}",
            cap, count, expected_overwrites, snap.overwrites);
        prop_assert_eq!(snap.pushes, count as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..4, 1..30),
    ) {
        let mut rb = RingBuffer::new(5);
        let mut prev = rb.telemetry();

        for op in &ops {
            match op {
                0 => { rb.push(42); }
                1 => { rb.clear(); }
                2 => { let _ = rb.drain(); }
                3 => {
                    // push to fill, then push to overwrite
                    rb.push(99);
                }
                _ => unreachable!(),
            }

            let snap = rb.telemetry();
            prop_assert!(snap.pushes >= prev.pushes,
                "pushes decreased: {} -> {}", prev.pushes, snap.pushes);
            prop_assert!(snap.overwrites >= prev.overwrites,
                "overwrites decreased: {} -> {}", prev.overwrites, snap.overwrites);
            prop_assert!(snap.clears >= prev.clears,
                "clears decreased: {} -> {}", prev.clears, snap.clears);
            prop_assert!(snap.drains >= prev.drains,
                "drains decreased: {} -> {}", prev.drains, snap.drains);
            prop_assert!(snap.items_drained >= prev.items_drained,
                "items_drained decreased: {} -> {}",
                prev.items_drained, snap.items_drained);

            prev = snap;
        }
    }

    #[test]
    fn items_drained_equals_sum_of_drain_sizes(
        chunks in prop::collection::vec(1usize..20, 1..10),
    ) {
        let mut rb = RingBuffer::new(100);
        let mut total_drained = 0usize;

        for chunk_size in &chunks {
            for i in 0..*chunk_size {
                rb.push(i);
            }
            let drained = rb.drain();
            total_drained += drained.len();
        }

        let snap = rb.telemetry();
        prop_assert_eq!(snap.items_drained, total_drained as u64,
            "expected items_drained={}, got={}",
            total_drained, snap.items_drained);
        prop_assert_eq!(snap.drains, chunks.len() as u64);
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        pushes in 0u64..100000,
        overwrites in 0u64..50000,
        clears in 0u64..10000,
        drains in 0u64..10000,
        items_drained in 0u64..100000,
    ) {
        let snap = RingBufferTelemetrySnapshot {
            pushes,
            overwrites,
            clears,
            drains,
            items_drained,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: RingBufferTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
