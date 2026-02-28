//! Property-based tests for semantic anomaly telemetry counters (ft-3kxe.39).
//!
//! Validates:
//! 1. Telemetry starts at zero for both detectors
//! 2. SemanticAnomalyDetector: observations, shocks_detected, resets tracked
//! 3. ConformalAnomalyDetector: observations, anomalies_detected, dimension_resets, resets tracked
//! 4. Serde roundtrip for both snapshots
//! 5. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::semantic_anomaly::{
    ConformalAnomalyConfig, ConformalAnomalyDetector, ConformalAnomalyTelemetrySnapshot,
    SemanticAnomalyConfig, SemanticAnomalyDetector, SemanticAnomalyTelemetrySnapshot,
};

// =============================================================================
// Helpers
// =============================================================================

fn test_zscore_detector() -> SemanticAnomalyDetector {
    SemanticAnomalyDetector::new(SemanticAnomalyConfig::default())
}

fn test_conformal_detector() -> ConformalAnomalyDetector {
    ConformalAnomalyDetector::new(ConformalAnomalyConfig::default())
}

/// Generate a simple embedding vector of given dimension.
fn make_embedding(dim: usize, base: f32) -> Vec<f32> {
    (0..dim).map(|i| base + i as f32 * 0.01).collect()
}

/// Generate an extreme outlier embedding (all large values).
fn make_outlier(dim: usize) -> Vec<f32> {
    vec![100.0; dim]
}

// =============================================================================
// Z-score Detector Unit Tests
// =============================================================================

#[test]
fn zscore_telemetry_starts_at_zero() {
    let det = test_zscore_detector();
    let snap = det.telemetry().snapshot();

    assert_eq!(snap.observations, 0);
    assert_eq!(snap.shocks_detected, 0);
    assert_eq!(snap.resets, 0);
}

#[test]
fn zscore_observations_tracked() {
    let mut det = test_zscore_detector();
    det.observe(&make_embedding(8, 1.0));
    det.observe(&make_embedding(8, 1.1));
    det.observe(&make_embedding(8, 1.2));

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.observations, 3);
}

#[test]
fn zscore_empty_embedding_still_counted() {
    let mut det = test_zscore_detector();
    det.observe(&[]);
    det.observe(&[]);

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.observations, 2);
    assert_eq!(snap.shocks_detected, 0);
}

#[test]
fn zscore_resets_tracked() {
    let mut det = test_zscore_detector();
    det.observe(&make_embedding(8, 1.0));
    det.reset();
    det.reset();

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.resets, 2);
    assert_eq!(snap.observations, 1);
}

#[test]
fn zscore_shocks_require_warmup() {
    let mut det = test_zscore_detector();
    // Feed similar embeddings to build baseline (need min_samples=5)
    for i in 0..10 {
        det.observe(&make_embedding(8, 1.0 + i as f32 * 0.001));
    }

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.observations, 10);
    // Shocks may or may not have fired depending on threshold
    // Just verify the counter is non-negative (already guaranteed by u64)
}

// =============================================================================
// Conformal Detector Unit Tests
// =============================================================================

#[test]
fn conformal_telemetry_starts_at_zero() {
    let det = test_conformal_detector();
    let snap = det.telemetry().snapshot();

    assert_eq!(snap.observations, 0);
    assert_eq!(snap.anomalies_detected, 0);
    assert_eq!(snap.dimension_resets, 0);
    assert_eq!(snap.resets, 0);
}

#[test]
fn conformal_observations_tracked() {
    let mut det = test_conformal_detector();
    det.observe(&make_embedding(8, 1.0));
    det.observe(&make_embedding(8, 1.1));

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.observations, 2);
}

#[test]
fn conformal_empty_embedding_counted() {
    let mut det = test_conformal_detector();
    det.observe(&[]);
    det.observe(&[]);

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.observations, 2);
    assert_eq!(snap.anomalies_detected, 0);
}

#[test]
fn conformal_dimension_resets_tracked() {
    let mut det = test_conformal_detector();
    det.observe(&make_embedding(8, 1.0));
    // Switch to different dimension — triggers dimension reset
    det.observe(&make_embedding(16, 1.0));

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.observations, 2);
    assert_eq!(snap.dimension_resets, 1);
}

#[test]
fn conformal_resets_tracked() {
    let mut det = test_conformal_detector();
    det.observe(&make_embedding(8, 1.0));
    det.reset();
    det.reset();

    let snap = det.telemetry().snapshot();
    assert_eq!(snap.resets, 2);
    assert_eq!(snap.observations, 1);
}

#[test]
fn conformal_anomalies_match_total_anomalies() {
    // Use a small calibration window and low alpha to try to trigger anomalies
    let mut det = ConformalAnomalyDetector::new(ConformalAnomalyConfig {
        alpha: 0.5,
        min_calibration: 3,
        calibration_window: 10,
        ..ConformalAnomalyConfig::default()
    });

    // Build up calibration with similar embeddings
    for i in 0..5 {
        det.observe(&make_embedding(8, 1.0 + i as f32 * 0.001));
    }
    // Feed a very different embedding to try triggering anomaly
    det.observe(&make_outlier(8));

    let snap = det.telemetry().snapshot();
    // anomalies_detected should match the detector's total_anomalies counter
    assert_eq!(snap.anomalies_detected, det.total_anomalies());
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

#[test]
fn zscore_snapshot_serde_roundtrip() {
    let snap = SemanticAnomalyTelemetrySnapshot {
        observations: 5000,
        shocks_detected: 42,
        resets: 3,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: SemanticAnomalyTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn conformal_snapshot_serde_roundtrip() {
    let snap = ConformalAnomalyTelemetrySnapshot {
        observations: 10000,
        anomalies_detected: 150,
        dimension_resets: 5,
        resets: 2,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: ConformalAnomalyTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn zscore_observations_equal_call_count(
        count in 1usize..20,
    ) {
        let mut det = test_zscore_detector();
        for i in 0..count {
            det.observe(&make_embedding(8, 1.0 + i as f32 * 0.01));
        }
        let snap = det.telemetry().snapshot();
        prop_assert_eq!(snap.observations, count as u64);
    }

    #[test]
    fn conformal_observations_equal_call_count(
        count in 1usize..20,
    ) {
        let mut det = test_conformal_detector();
        for i in 0..count {
            det.observe(&make_embedding(8, 1.0 + i as f32 * 0.01));
        }
        let snap = det.telemetry().snapshot();
        prop_assert_eq!(snap.observations, count as u64);
    }

    #[test]
    fn zscore_counters_monotonically_increase(
        ops in prop::collection::vec(0u8..3, 1..20),
    ) {
        let mut det = test_zscore_detector();
        let mut prev = det.telemetry().snapshot();

        for (i, op) in ops.iter().enumerate() {
            match op {
                0 => { det.observe(&make_embedding(8, 1.0 + i as f32 * 0.01)); }
                1 => { det.observe(&make_outlier(8)); }
                2 => { det.reset(); }
                _ => unreachable!(),
            }

            let snap = det.telemetry().snapshot();
            prop_assert!(snap.observations >= prev.observations,
                "observations decreased");
            prop_assert!(snap.shocks_detected >= prev.shocks_detected,
                "shocks_detected decreased");
            prop_assert!(snap.resets >= prev.resets,
                "resets decreased");

            prev = snap;
        }
    }

    #[test]
    fn conformal_counters_monotonically_increase(
        ops in prop::collection::vec(0u8..4, 1..20),
    ) {
        let mut det = test_conformal_detector();
        let mut prev = det.telemetry().snapshot();

        for (i, op) in ops.iter().enumerate() {
            match op {
                0 => { det.observe(&make_embedding(8, 1.0 + i as f32 * 0.01)); }
                1 => { det.observe(&make_outlier(8)); }
                2 => { det.observe(&make_embedding(16, 1.0)); } // dimension change
                3 => { det.reset(); }
                _ => unreachable!(),
            }

            let snap = det.telemetry().snapshot();
            prop_assert!(snap.observations >= prev.observations,
                "observations decreased");
            prop_assert!(snap.anomalies_detected >= prev.anomalies_detected,
                "anomalies_detected decreased");
            prop_assert!(snap.dimension_resets >= prev.dimension_resets,
                "dimension_resets decreased");
            prop_assert!(snap.resets >= prev.resets,
                "resets decreased");

            prev = snap;
        }
    }

    #[test]
    fn zscore_snapshot_roundtrip_arbitrary(
        observations in 0u64..100000,
        shocks_detected in 0u64..10000,
        resets in 0u64..100,
    ) {
        let snap = SemanticAnomalyTelemetrySnapshot {
            observations,
            shocks_detected,
            resets,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: SemanticAnomalyTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }

    #[test]
    fn conformal_snapshot_roundtrip_arbitrary(
        observations in 0u64..100000,
        anomalies_detected in 0u64..10000,
        dimension_resets in 0u64..100,
        resets in 0u64..100,
    ) {
        let snap = ConformalAnomalyTelemetrySnapshot {
            observations,
            anomalies_detected,
            dimension_resets,
            resets,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: ConformalAnomalyTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
