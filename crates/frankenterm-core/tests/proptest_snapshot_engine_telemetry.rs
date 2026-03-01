//! Property-based tests for snapshot engine telemetry counters (ft-3kxe.13).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. Snapshot serde roundtrip (arbitrary values)
//! 3. emit_trigger increments trigger counters
//! 4. Counter monotonicity across repeated emit_trigger calls
//! 5. Telemetry accessor is available on a fresh engine

use proptest::prelude::*;

use frankenterm_core::config::SnapshotConfig;
use frankenterm_core::snapshot_engine::{
    SnapshotEngine, SnapshotEngineTelemetrySnapshot, SnapshotTrigger,
};
use std::sync::Arc;

// =============================================================================
// Helpers
// =============================================================================

/// Create a snapshot engine with an in-memory temp path (operations that
/// don't touch SQLite still work for trigger counters).
fn test_engine() -> SnapshotEngine {
    SnapshotEngine::new(Arc::new(":memory:".to_string()), SnapshotConfig::default())
}

fn arb_trigger() -> impl Strategy<Value = SnapshotTrigger> {
    prop_oneof![
        Just(SnapshotTrigger::Periodic),
        Just(SnapshotTrigger::PeriodicFallback),
        Just(SnapshotTrigger::Manual),
        Just(SnapshotTrigger::Shutdown),
        Just(SnapshotTrigger::Startup),
        Just(SnapshotTrigger::Event),
        Just(SnapshotTrigger::WorkCompleted),
        Just(SnapshotTrigger::HazardThreshold),
        Just(SnapshotTrigger::StateTransition),
        Just(SnapshotTrigger::IdleWindow),
        Just(SnapshotTrigger::MemoryPressure),
    ]
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let engine = test_engine();
    let snap = engine.telemetry().snapshot();

    assert_eq!(snap.captures_attempted, 0);
    assert_eq!(snap.captures_succeeded, 0);
    assert_eq!(snap.dedup_skips, 0);
    assert_eq!(snap.capture_errors, 0);
    assert_eq!(snap.cleanup_runs, 0);
    assert_eq!(snap.cleanup_removed, 0);
    assert_eq!(snap.triggers_emitted, 0);
    assert_eq!(snap.triggers_accepted, 0);
    assert_eq!(snap.panes_captured, 0);
    assert_eq!(snap.bytes_persisted, 0);
}

#[test]
fn emit_trigger_increments_counters() {
    let engine = test_engine();

    // First trigger should succeed (channel has capacity)
    let accepted = engine.emit_trigger(SnapshotTrigger::Manual);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.triggers_emitted, 1);
    if accepted {
        assert_eq!(snap.triggers_accepted, 1);
    }
}

#[test]
fn multiple_triggers_accumulate() {
    let engine = test_engine();

    for _ in 0..10 {
        let _ = engine.emit_trigger(SnapshotTrigger::Event);
    }

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.triggers_emitted, 10);
    // All should be accepted since channel capacity is 512
    assert_eq!(snap.triggers_accepted, 10);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = SnapshotEngineTelemetrySnapshot {
        captures_attempted: 50,
        captures_succeeded: 45,
        dedup_skips: 3,
        capture_errors: 2,
        cleanup_runs: 5,
        cleanup_removed: 30,
        triggers_emitted: 100,
        triggers_accepted: 98,
        panes_captured: 500,
        bytes_persisted: 1_000_000,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: SnapshotEngineTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn triggers_emitted_equals_call_count(
        triggers in prop::collection::vec(arb_trigger(), 1..50),
    ) {
        let engine = test_engine();

        for trigger in &triggers {
            let _ = engine.emit_trigger(*trigger);
        }

        let snap = engine.telemetry().snapshot();
        prop_assert_eq!(snap.triggers_emitted, triggers.len() as u64);
        // With channel capacity of 512, all should be accepted
        prop_assert_eq!(snap.triggers_accepted, triggers.len() as u64);
    }

    #[test]
    fn trigger_counters_monotonic(
        counts in prop::collection::vec(1usize..5, 1..10),
    ) {
        let engine = test_engine();
        let mut prev_emitted = 0u64;
        let mut prev_accepted = 0u64;

        for count in &counts {
            for _ in 0..*count {
                let _ = engine.emit_trigger(SnapshotTrigger::Event);
            }

            let snap = engine.telemetry().snapshot();
            prop_assert!(
                snap.triggers_emitted >= prev_emitted,
                "triggers_emitted must not decrease: prev={}, cur={}",
                prev_emitted, snap.triggers_emitted
            );
            prop_assert!(
                snap.triggers_accepted >= prev_accepted,
                "triggers_accepted must not decrease: prev={}, cur={}",
                prev_accepted, snap.triggers_accepted
            );

            prev_emitted = snap.triggers_emitted;
            prev_accepted = snap.triggers_accepted;
        }
    }

    #[test]
    fn accepted_bounded_by_emitted(
        count in 1usize..100,
    ) {
        let engine = test_engine();

        for _ in 0..count {
            let _ = engine.emit_trigger(SnapshotTrigger::Manual);
        }

        let snap = engine.telemetry().snapshot();
        prop_assert!(
            snap.triggers_accepted <= snap.triggers_emitted,
            "triggers_accepted ({}) should be <= triggers_emitted ({})",
            snap.triggers_accepted, snap.triggers_emitted
        );
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        attempted in 0u64..10000,
        succeeded in 0u64..10000,
        dedup in 0u64..5000,
        errors in 0u64..5000,
        cleanup_r in 0u64..1000,
        cleanup_rm in 0u64..5000,
        emitted in 0u64..50000,
        accepted in 0u64..50000,
        panes in 0u64..100000,
        bytes in 0u64..10_000_000,
    ) {
        let snap = SnapshotEngineTelemetrySnapshot {
            captures_attempted: attempted,
            captures_succeeded: succeeded,
            dedup_skips: dedup,
            capture_errors: errors,
            cleanup_runs: cleanup_r,
            cleanup_removed: cleanup_rm,
            triggers_emitted: emitted,
            triggers_accepted: accepted,
            panes_captured: panes,
            bytes_persisted: bytes,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: SnapshotEngineTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
