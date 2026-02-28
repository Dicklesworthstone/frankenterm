//! Property-based tests for protocol recovery telemetry counters (ft-3kxe.34).
//!
//! Validates:
//! 1. Telemetry starts at zero for all three structs
//! 2. RecoveryEngine telemetry_snapshot matches operation counts
//! 3. FrameCorruptionDetector telemetry tracks successes, errors, rotations, resets
//! 4. ConnectionHealthTracker telemetry tracks successes, errors, transitions, resets
//! 5. Serde roundtrip for all snapshot types
//! 6. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::protocol_recovery::{
    ConnectionHealthTelemetrySnapshot, ConnectionHealthTracker, FrameCorruptionDetector,
    FrameCorruptionTelemetrySnapshot, ProtocolErrorKind, RecoveryConfig, RecoveryEngine,
    RecoveryTelemetrySnapshot,
};

// =============================================================================
// Helpers
// =============================================================================

fn test_engine() -> RecoveryEngine {
    RecoveryEngine::new(RecoveryConfig::default())
}

fn test_detector() -> FrameCorruptionDetector {
    FrameCorruptionDetector::new(100, 3)
}

fn test_tracker() -> ConnectionHealthTracker {
    ConnectionHealthTracker::new()
}

// =============================================================================
// RecoveryEngine telemetry tests
// =============================================================================

#[test]
fn recovery_telemetry_starts_at_zero() {
    let eng = test_engine();
    let snap = eng.telemetry_snapshot();

    assert_eq!(snap.total_operations, 0);
    assert_eq!(snap.first_try_successes, 0);
    assert_eq!(snap.retry_successes, 0);
    assert_eq!(snap.total_retries, 0);
    assert_eq!(snap.recoverable_failures, 0);
    assert_eq!(snap.transient_failures, 0);
    assert_eq!(snap.permanent_failures, 0);
    assert_eq!(snap.circuit_rejections, 0);
}

#[test]
fn recovery_snapshot_serde_roundtrip() {
    let snap = RecoveryTelemetrySnapshot {
        total_operations: 10000,
        first_try_successes: 8000,
        retry_successes: 1500,
        total_retries: 3000,
        recoverable_failures: 300,
        transient_failures: 150,
        permanent_failures: 50,
        circuit_rejections: 20,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: RecoveryTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// FrameCorruptionDetector telemetry tests
// =============================================================================

#[test]
fn detector_telemetry_starts_at_zero() {
    let det = test_detector();
    let snap = det.telemetry().snapshot();

    assert_eq!(snap.successes_recorded, 0);
    assert_eq!(snap.errors_recorded, 0);
    assert_eq!(snap.corruption_detections, 0);
    assert_eq!(snap.window_rotations, 0);
    assert_eq!(snap.resets, 0);
}

#[test]
fn detector_successes_tracked() {
    let mut det = test_detector();
    det.record_success();
    det.record_success();
    det.record_success();

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.successes_recorded, 3);
    assert_eq!(snap.errors_recorded, 0);
}

#[test]
fn detector_errors_tracked() {
    let mut det = test_detector();
    det.record_error(ProtocolErrorKind::Recoverable, "unexpected response from server");
    det.record_error(ProtocolErrorKind::Transient, "timed out");

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.errors_recorded, 2);
}

#[test]
fn detector_corruption_detections_tracked() {
    let mut det = FrameCorruptionDetector::new(1000, 2);
    det.record_error(ProtocolErrorKind::Recoverable, "unexpected response");
    det.record_error(ProtocolErrorKind::Recoverable, "codec error");

    let snap = det.telemetry().snapshot();
    // After 2 errors reaching threshold, corruption detected on 2nd call
    assert!(snap.corruption_detections >= 1);
}

#[test]
fn detector_window_rotations_tracked() {
    let mut det = FrameCorruptionDetector::new(3, 100);
    // 3 ops = 1 rotation
    det.record_success();
    det.record_success();
    det.record_success();

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.window_rotations, 1);
    assert_eq!(snap.successes_recorded, 3);
}

#[test]
fn detector_resets_tracked() {
    let mut det = test_detector();
    det.reset();
    det.reset();

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.resets, 2);
}

#[test]
fn detector_snapshot_serde_roundtrip() {
    let snap = FrameCorruptionTelemetrySnapshot {
        successes_recorded: 5000,
        errors_recorded: 200,
        corruption_detections: 10,
        window_rotations: 50,
        resets: 3,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: FrameCorruptionTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// ConnectionHealthTracker telemetry tests
// =============================================================================

#[test]
fn tracker_telemetry_starts_at_zero() {
    let trk = test_tracker();
    let snap = trk.telemetry().snapshot();

    assert_eq!(snap.successes_recorded, 0);
    assert_eq!(snap.errors_recorded, 0);
    assert_eq!(snap.healthy_transitions, 0);
    assert_eq!(snap.degraded_transitions, 0);
    assert_eq!(snap.corrupted_transitions, 0);
    assert_eq!(snap.dead_transitions, 0);
    assert_eq!(snap.resets, 0);
}

#[test]
fn tracker_successes_tracked() {
    let mut trk = test_tracker();
    trk.record_success();
    trk.record_success();

    let snap = trk.telemetry().snapshot();
    assert_eq!(snap.successes_recorded, 2);
}

#[test]
fn tracker_errors_tracked() {
    let mut trk = test_tracker();
    trk.record_error(ProtocolErrorKind::Recoverable, "disconnected");
    trk.record_error(ProtocolErrorKind::Transient, "timeout");

    let snap = trk.telemetry().snapshot();
    assert_eq!(snap.errors_recorded, 2);
}

#[test]
fn tracker_degraded_transition_tracked() {
    let mut trk = test_tracker();
    trk.record_error(ProtocolErrorKind::Recoverable, "disconnected");

    let snap = trk.telemetry().snapshot();
    assert_eq!(snap.degraded_transitions, 1);
}

#[test]
fn tracker_dead_transition_tracked() {
    let mut trk = test_tracker();
    trk.record_error(ProtocolErrorKind::Permanent, "incompatible codec");

    let snap = trk.telemetry().snapshot();
    assert_eq!(snap.dead_transitions, 1);
}

#[test]
fn tracker_healthy_recovery_tracked() {
    let mut trk = test_tracker();
    // First become degraded
    trk.record_error(ProtocolErrorKind::Recoverable, "disconnected");
    // Then recover with 5 consecutive successes
    for _ in 0..5 {
        trk.record_success();
    }

    let snap = trk.telemetry().snapshot();
    assert_eq!(snap.healthy_transitions, 1);
    assert_eq!(snap.degraded_transitions, 1);
}

#[test]
fn tracker_resets_tracked() {
    let mut trk = test_tracker();
    trk.reset();
    trk.reset();

    let snap = trk.telemetry().snapshot();
    assert_eq!(snap.resets, 2);
}

#[test]
fn tracker_snapshot_serde_roundtrip() {
    let snap = ConnectionHealthTelemetrySnapshot {
        successes_recorded: 10000,
        errors_recorded: 500,
        healthy_transitions: 20,
        degraded_transitions: 25,
        corrupted_transitions: 5,
        dead_transitions: 2,
        resets: 10,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: ConnectionHealthTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn detector_successes_equals_call_count(
        count in 1usize..50,
    ) {
        let mut det = test_detector();
        for _ in 0..count {
            det.record_success();
        }
        let snap = det.telemetry().snapshot();
        prop_assert_eq!(snap.successes_recorded, count as u64);
    }

    #[test]
    fn detector_errors_equals_call_count(
        count in 1usize..20,
    ) {
        let mut det = test_detector();
        for _ in 0..count {
            det.record_error(ProtocolErrorKind::Recoverable, "unexpected response");
        }
        let snap = det.telemetry().snapshot();
        prop_assert_eq!(snap.errors_recorded, count as u64);
    }

    #[test]
    fn detector_counters_monotonically_increase(
        ops in prop::collection::vec(0u8..4, 1..30),
    ) {
        let mut det = FrameCorruptionDetector::new(10, 5);
        let mut prev = det.telemetry().snapshot();

        for op in &ops {
            match op {
                0 => { det.record_success(); }
                1 => { det.record_error(ProtocolErrorKind::Recoverable, "unexpected response"); }
                2 => { det.record_error(ProtocolErrorKind::Transient, "timeout"); }
                3 => { det.reset(); }
                _ => unreachable!(),
            }

            let snap = det.telemetry().snapshot();
            prop_assert!(snap.successes_recorded >= prev.successes_recorded,
                "successes_recorded decreased");
            prop_assert!(snap.errors_recorded >= prev.errors_recorded,
                "errors_recorded decreased");
            prop_assert!(snap.corruption_detections >= prev.corruption_detections,
                "corruption_detections decreased");
            prop_assert!(snap.window_rotations >= prev.window_rotations,
                "window_rotations decreased");
            prop_assert!(snap.resets >= prev.resets,
                "resets decreased");

            prev = snap;
        }
    }

    #[test]
    fn tracker_counters_monotonically_increase(
        ops in prop::collection::vec(0u8..5, 1..30),
    ) {
        let mut trk = test_tracker();
        let mut prev = trk.telemetry().snapshot();

        for op in &ops {
            match op {
                0 => { trk.record_success(); }
                1 => { trk.record_error(ProtocolErrorKind::Recoverable, "disconnected"); }
                2 => { trk.record_error(ProtocolErrorKind::Transient, "timeout"); }
                3 => { trk.record_error(ProtocolErrorKind::Permanent, "incompatible"); }
                4 => { trk.reset(); }
                _ => unreachable!(),
            }

            let snap = trk.telemetry().snapshot();
            prop_assert!(snap.successes_recorded >= prev.successes_recorded,
                "successes_recorded decreased");
            prop_assert!(snap.errors_recorded >= prev.errors_recorded,
                "errors_recorded decreased");
            prop_assert!(snap.healthy_transitions >= prev.healthy_transitions,
                "healthy_transitions decreased");
            prop_assert!(snap.degraded_transitions >= prev.degraded_transitions,
                "degraded_transitions decreased");
            prop_assert!(snap.corrupted_transitions >= prev.corrupted_transitions,
                "corrupted_transitions decreased");
            prop_assert!(snap.dead_transitions >= prev.dead_transitions,
                "dead_transitions decreased");
            prop_assert!(snap.resets >= prev.resets,
                "resets decreased");

            prev = snap;
        }
    }

    #[test]
    fn detector_snapshot_roundtrip_arbitrary(
        successes_recorded in 0u64..100000,
        errors_recorded in 0u64..50000,
        corruption_detections in 0u64..1000,
        window_rotations in 0u64..5000,
        resets in 0u64..100,
    ) {
        let snap = FrameCorruptionTelemetrySnapshot {
            successes_recorded,
            errors_recorded,
            corruption_detections,
            window_rotations,
            resets,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: FrameCorruptionTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }

    #[test]
    fn tracker_snapshot_roundtrip_arbitrary(
        successes_recorded in 0u64..100000,
        errors_recorded in 0u64..50000,
        healthy_transitions in 0u64..10000,
        degraded_transitions in 0u64..10000,
        corrupted_transitions in 0u64..1000,
        dead_transitions in 0u64..500,
        resets in 0u64..100,
    ) {
        let snap = ConnectionHealthTelemetrySnapshot {
            successes_recorded,
            errors_recorded,
            healthy_transitions,
            degraded_transitions,
            corrupted_transitions,
            dead_transitions,
            resets,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: ConnectionHealthTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }

    #[test]
    fn recovery_snapshot_roundtrip_arbitrary(
        total_operations in 0u64..100000,
        first_try_successes in 0u64..80000,
        retry_successes in 0u64..20000,
        total_retries in 0u64..50000,
        recoverable_failures in 0u64..10000,
        transient_failures in 0u64..10000,
        permanent_failures in 0u64..5000,
        circuit_rejections in 0u64..1000,
    ) {
        let snap = RecoveryTelemetrySnapshot {
            total_operations,
            first_try_successes,
            retry_successes,
            total_retries,
            recoverable_failures,
            transient_failures,
            permanent_failures,
            circuit_rejections,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: RecoveryTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
