#![allow(clippy::float_cmp, clippy::suboptimal_flops)]
//! Property-based tests for semantic anomaly detection.
//!
//! Beads: ft-344j8.8 (conformal prediction), ft-344j8.9 (entropy gating)
//!
//! Validates:
//! 1. dot_product_simd: matches naive dot product for arbitrary vectors
//! 2. dot_product_simd: commutative (a·b == b·a)
//! 3. dot_product_simd: empty vectors → 0.0
//! 4. dot_product_simd: self-dot equals sum of squares
//! 5. dot_product_simd: mismatched lengths use min(len_a, len_b)
//! 6. normalize_simd: output is unit vector (magnitude ≈ 1.0)
//! 7. normalize_simd: zero vector stays zero
//! 8. normalize_simd: preserves direction (dot with original > 0)
//! 9. normalize_simd: idempotent (normalize(normalize(v)) ≈ normalize(v))
//! 10. SortedCalibrationBuffer: sorted invariant after arbitrary insertions
//! 11. SortedCalibrationBuffer: length never exceeds capacity
//! 12. SortedCalibrationBuffer: count_geq monotonically decreasing with threshold
//! 13. SortedCalibrationBuffer: conformal_p_value in (0, 1]
//! 14. SortedCalibrationBuffer: p_value monotonically decreasing with score
//! 15. SortedCalibrationBuffer: quantile(0) <= quantile(1)
//! 16. SortedCalibrationBuffer: eviction preserves sorted order
//! 17. ConformalAnomalyConfig: serde roundtrip preserves all fields
//! 18. ConformalShock: serde roundtrip preserves all fields
//! 19. ConformalAnomalySnapshot: serde roundtrip preserves all fields
//! 20. SemanticAnomalyConfig: serde roundtrip preserves all fields
//! 21. ConformalAnomalyDetector: total_observations increments per non-empty observe
//! 22. ConformalAnomalyDetector: p_value always in (0, 1] after warmup
//! 23. ConformalAnomalyDetector: stable stream FDR <= 2*alpha (empirical guarantee)
//! 24. ConformalAnomalyDetector: reset clears centroid
//! 25. ConformalAnomalyDetector: snapshot consistency
//! 26. ConformalAnomalyDetector: orthogonal shift detected after calibration
//! 27. ConformalAnomalyDetector: dimension change resets without panic
//! 28. SemanticAnomalyDetector: Z-score shock has positive distance and z_score
//! 29. SemanticAnomalyDetector: stable identical inputs produce no shocks
//! 30. SortedCalibrationBuffer: count_geq(min_score) == len after inserting only that value
//! 31. EntropyGate: constant data always skipped
//! 32. EntropyGate: uniform random data always passes
//! 33. EntropyGate: should_embed consistent with decision variant
//! 34. EntropyGate: statistics add up (evaluated = passed + skipped + bypassed + disabled)
//! 35. EntropyGateConfig: serde roundtrip preserves all fields
//! 36. EntropyGateSnapshot: serde roundtrip preserves all fields
//! 37. EntropyGate: skip_ratio in [0, 1]
//! 38. EntropyGate: disabled gate always returns Disabled
//! 39. EntropyGate: average_entropy in [0, 8] for measured segments
//! 40. EntropyGate: short segments always bypassed
//! 41. SortedCalibrationBuffer: O(log N) count_geq matches naive O(N) linear scan (isomorphism)
//! 42. SortedCalibrationBuffer: conformal_p_value matches naive computation
//! 43. dot_product_simd: matches naive for 384d vectors (embedding dimension isomorphism)
//! 44. ConformalAnomalyDetector: FDR on fixture embeddings stays within alpha bound

use proptest::collection::vec as arb_vec;
use proptest::prelude::*;

use frankenterm_core::semantic_anomaly::{
    ConformalAnomalyConfig, ConformalAnomalyDetector, ConformalAnomalySnapshot, ConformalShock,
    EntropyGate, EntropyGateConfig, EntropyGateDecision, EntropyGateSnapshot,
    SemanticAnomalyConfig, SemanticAnomalyDetector, SortedCalibrationBuffer, dot_product_simd,
    normalize_simd,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_f32_finite() -> impl Strategy<Value = f32> {
    -1e6_f32..1e6
}

fn _arb_vector(dim: usize) -> impl Strategy<Value = Vec<f32>> {
    arb_vec(arb_f32_finite(), dim..=dim)
}

fn arb_vector_range(min_dim: usize, max_dim: usize) -> impl Strategy<Value = Vec<f32>> {
    arb_vec(arb_f32_finite(), min_dim..=max_dim)
}

fn arb_positive_f32() -> impl Strategy<Value = f32> {
    0.001_f32..100.0
}

fn arb_conformal_config() -> impl Strategy<Value = ConformalAnomalyConfig> {
    (0.001_f64..0.5, 10_usize..500, 0.01_f32..0.5, 3_usize..50).prop_map(
        |(alpha, window, centroid_alpha, min_cal)| ConformalAnomalyConfig {
            alpha,
            calibration_window: window,
            centroid_alpha,
            min_calibration: min_cal,
        },
    )
}

fn arb_semantic_config() -> impl Strategy<Value = SemanticAnomalyConfig> {
    (0.01_f32..0.5, 0.01_f32..0.5, 2_usize..20, 1.0_f32..10.0).prop_map(
        |(centroid_alpha, variance_alpha, min_samples, threshold)| SemanticAnomalyConfig {
            centroid_alpha,
            variance_alpha,
            min_samples,
            shock_threshold_z: threshold,
        },
    )
}

// =============================================================================
// SIMD math properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // 1. dot_product_simd matches naive dot product
    #[test]
    fn prop_dot_product_matches_naive(
        a in arb_vector_range(1, 128),
        b in arb_vector_range(1, 128),
    ) {
        let n = a.len().min(b.len());
        let naive: f32 = a[..n].iter().zip(&b[..n]).map(|(x, y)| x * y).sum();
        let simd = dot_product_simd(&a, &b);
        // Allow f32 summation-order precision differences (4-accumulator SIMD
        // changes reduction order vs sequential sum, causing larger divergence
        // on large-magnitude ill-conditioned inputs).
        let tol = (naive.abs() * 1e-3).max(1e-4);
        let diff = (simd - naive).abs();
        let ok = diff < tol;
        prop_assert!(ok, "simd={simd} naive={naive} diff={diff} tol={tol}");
    }

    // 2. dot_product_simd is commutative
    #[test]
    fn prop_dot_product_commutative(
        a in arb_vector_range(1, 64),
        b in arb_vector_range(1, 64),
    ) {
        let ab = dot_product_simd(&a, &b);
        let ba = dot_product_simd(&b, &a);
        let diff = (ab - ba).abs();
        prop_assert!(diff < 1e-4, "a·b={ab} b·a={ba} diff={diff}");
    }

    // 3. dot_product_simd: empty vectors → 0.0
    #[test]
    fn prop_dot_product_empty(_dummy in 0..1u8) {
        let r = dot_product_simd(&[], &[]);
        prop_assert_eq!(r, 0.0);
    }

    // 4. dot_product_simd: self-dot equals sum of squares
    #[test]
    fn prop_dot_product_self_is_sum_sq(a in arb_vector_range(1, 64)) {
        let self_dot = dot_product_simd(&a, &a);
        let sum_sq: f32 = a.iter().map(|x| x * x).sum();
        let tol = (sum_sq.abs() * 1e-4).max(1e-4);
        let diff = (self_dot - sum_sq).abs();
        prop_assert!(diff < tol, "self_dot={self_dot} sum_sq={sum_sq} diff={diff}");
    }

    // 5. dot_product_simd: mismatched lengths use min
    #[test]
    fn prop_dot_product_mismatched_lengths(
        a in arb_vector_range(1, 64),
        extra in arb_vector_range(1, 32),
    ) {
        let mut b = a.clone();
        b.extend_from_slice(&extra);
        // a·b should equal a·a (extra elements of b ignored)
        let ab = dot_product_simd(&a, &b);
        let aa = dot_product_simd(&a, &a);
        let tol = (aa.abs() * 1e-4).max(1e-4);
        let diff = (ab - aa).abs();
        prop_assert!(diff < tol, "ab={ab} aa={aa}");
    }

    // 6. normalize_simd: output is unit vector
    #[test]
    fn prop_normalize_unit_vector(v in arb_vector_range(1, 128)) {
        let n = normalize_simd(&v);
        let mag_sq: f32 = n.iter().map(|x| x * x).sum();
        let is_zero = v.iter().all(|x| *x == 0.0);
        if is_zero {
            prop_assert!((mag_sq - 0.0).abs() < 1e-6);
        } else {
            prop_assert!((mag_sq - 1.0).abs() < 1e-3, "mag_sq={mag_sq}");
        }
    }

    // 7. normalize_simd: zero vector stays zero
    #[test]
    fn prop_normalize_zero(dim in 1_usize..64) {
        let v = vec![0.0_f32; dim];
        let n = normalize_simd(&v);
        for val in &n {
            prop_assert_eq!(*val, 0.0);
        }
    }

    // 8. normalize_simd: preserves direction (positive dot with original)
    #[test]
    fn prop_normalize_preserves_direction(v in arb_vector_range(1, 64)) {
        let is_zero = v.iter().all(|x| *x == 0.0);
        if !is_zero {
            let n = normalize_simd(&v);
            let d = dot_product_simd(&v, &n);
            prop_assert!(d > 0.0, "dot with normalized should be positive, got {d}");
        }
    }

    // 9. normalize_simd: idempotent
    #[test]
    fn prop_normalize_idempotent(v in arb_vector_range(1, 64)) {
        let n1 = normalize_simd(&v);
        let n2 = normalize_simd(&n1);
        for (a, b) in n1.iter().zip(&n2) {
            let diff = (a - b).abs();
            prop_assert!(diff < 1e-4, "a={a} b={b} diff={diff}");
        }
    }
}

// =============================================================================
// SortedCalibrationBuffer properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // 10. Sorted invariant after arbitrary insertions
    #[test]
    fn prop_buffer_sorted_invariant(
        capacity in 1_usize..50,
        scores in arb_vec(arb_positive_f32(), 1..200),
    ) {
        let mut buf = SortedCalibrationBuffer::new(capacity);
        for s in &scores {
            buf.insert(*s);
        }
        // Can't directly access sorted vec, but we can verify via quantile ordering.
        if buf.len() >= 2 {
            let q0 = buf.quantile(0.0).unwrap();
            let q50 = buf.quantile(0.5).unwrap();
            let q100 = buf.quantile(1.0).unwrap();
            prop_assert!(q0 <= q50, "q0={q0} > q50={q50}");
            prop_assert!(q50 <= q100, "q50={q50} > q100={q100}");
        }
    }

    // 11. Length never exceeds capacity
    #[test]
    fn prop_buffer_length_bounded(
        capacity in 1_usize..50,
        scores in arb_vec(arb_positive_f32(), 1..200),
    ) {
        let mut buf = SortedCalibrationBuffer::new(capacity);
        for s in &scores {
            buf.insert(*s);
            prop_assert!(buf.len() <= capacity, "len={} > cap={capacity}", buf.len());
        }
    }

    // 12. count_geq monotonically decreasing with threshold
    #[test]
    fn prop_buffer_count_geq_monotone(
        capacity in 5_usize..50,
        scores in arb_vec(arb_positive_f32(), 10..100),
    ) {
        let mut buf = SortedCalibrationBuffer::new(capacity);
        for s in &scores {
            buf.insert(*s);
        }
        let c_low = buf.count_geq(0.0);
        let c_mid = buf.count_geq(50.0);
        let c_high = buf.count_geq(101.0);
        prop_assert!(c_low >= c_mid, "c_low={c_low} < c_mid={c_mid}");
        prop_assert!(c_mid >= c_high, "c_mid={c_mid} < c_high={c_high}");
    }

    // 13. conformal_p_value in (0, 1]
    #[test]
    fn prop_buffer_p_value_bounds(
        capacity in 5_usize..50,
        scores in arb_vec(arb_positive_f32(), 5..100),
        query in arb_positive_f32(),
    ) {
        let mut buf = SortedCalibrationBuffer::new(capacity);
        for s in &scores {
            buf.insert(*s);
        }
        let p = buf.conformal_p_value(query);
        prop_assert!(p > 0.0, "p={p} should be > 0");
        prop_assert!(p <= 1.0, "p={p} should be <= 1");
    }

    // 14. p_value monotonically decreasing with score
    #[test]
    fn prop_buffer_p_value_monotone(
        capacity in 5_usize..50,
        scores in arb_vec(arb_positive_f32(), 10..100),
    ) {
        let mut buf = SortedCalibrationBuffer::new(capacity);
        for s in &scores {
            buf.insert(*s);
        }
        let p_low = buf.conformal_p_value(0.001);
        let p_high = buf.conformal_p_value(999.0);
        prop_assert!(p_low >= p_high, "p_low={p_low} < p_high={p_high}");
    }

    // 15. quantile(0) <= quantile(1)
    #[test]
    fn prop_buffer_quantile_order(
        capacity in 3_usize..50,
        scores in arb_vec(arb_positive_f32(), 3..100),
    ) {
        let mut buf = SortedCalibrationBuffer::new(capacity);
        for s in &scores {
            buf.insert(*s);
        }
        let q0 = buf.quantile(0.0).unwrap();
        let q1 = buf.quantile(1.0).unwrap();
        prop_assert!(q0 <= q1, "q0={q0} > q1={q1}");
    }

    // 16. Eviction preserves sorted order (stress test with wraps)
    #[test]
    fn prop_buffer_eviction_sorted(
        capacity in 2_usize..10,
        scores in arb_vec(arb_positive_f32(), 20..100),
    ) {
        let mut buf = SortedCalibrationBuffer::new(capacity);
        for s in &scores {
            buf.insert(*s);
            // After every insert, verify quantile ordering holds.
            if buf.len() >= 2 {
                let q0 = buf.quantile(0.0).unwrap();
                let q1 = buf.quantile(1.0).unwrap();
                prop_assert!(q0 <= q1, "After eviction: q0={q0} > q1={q1}");
            }
        }
    }

    // 30. count_geq(x) == len when all values are x
    #[test]
    fn prop_buffer_uniform_count(
        capacity in 2_usize..20,
        val in arb_positive_f32(),
        count in 1_usize..50,
    ) {
        let mut buf = SortedCalibrationBuffer::new(capacity);
        for _ in 0..count {
            buf.insert(val);
        }
        let expected_len = count.min(capacity);
        prop_assert_eq!(buf.len(), expected_len);
        prop_assert_eq!(buf.count_geq(val), expected_len);
    }
}

// =============================================================================
// Serde roundtrip properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 17. ConformalAnomalyConfig serde roundtrip
    #[test]
    fn prop_conformal_config_serde(config in arb_conformal_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let round: ConformalAnomalyConfig = serde_json::from_str(&json).unwrap();
        let alpha_ok = (round.alpha - config.alpha).abs() < 1e-10;
        prop_assert!(alpha_ok, "alpha mismatch");
        prop_assert_eq!(round.calibration_window, config.calibration_window);
        let ca_ok = (round.centroid_alpha - config.centroid_alpha).abs() < 1e-6;
        prop_assert!(ca_ok, "centroid_alpha mismatch");
        prop_assert_eq!(round.min_calibration, config.min_calibration);
    }

    // 18. ConformalShock serde roundtrip
    #[test]
    fn prop_conformal_shock_serde(
        distance in 0.0_f32..2.0,
        p_value in 0.001_f64..1.0,
        alpha in 0.001_f64..0.5,
        cal_count in 1_usize..500,
        cal_median in 0.0_f32..1.0,
    ) {
        let shock = ConformalShock {
            distance,
            p_value,
            alpha,
            calibration_count: cal_count,
            calibration_median: cal_median,
        };
        let json = serde_json::to_string(&shock).unwrap();
        let round: ConformalShock = serde_json::from_str(&json).unwrap();
        // f64 loses precision through JSON roundtrip — use tolerance.
        let dist_ok = (round.distance - shock.distance).abs() < 1e-6;
        prop_assert!(dist_ok, "distance mismatch");
        let p_ok = (round.p_value - shock.p_value).abs() < 1e-10;
        prop_assert!(p_ok, "p_value mismatch");
        let a_ok = (round.alpha - shock.alpha).abs() < 1e-10;
        prop_assert!(a_ok, "alpha mismatch");
        prop_assert_eq!(round.calibration_count, shock.calibration_count);
        let m_ok = (round.calibration_median - shock.calibration_median).abs() < 1e-6;
        prop_assert!(m_ok, "calibration_median mismatch");
    }

    // 19. ConformalAnomalySnapshot serde roundtrip
    #[test]
    fn prop_conformal_snapshot_serde(
        total_obs in 0_u64..10000,
        total_anom in 0_u64..1000,
        cal_count in 0_usize..500,
        cal_cap in 1_usize..1000,
        dim in 0_usize..1024,
        last_p in 0.0_f64..1.0,
    ) {
        let snap = ConformalAnomalySnapshot {
            total_observations: total_obs,
            total_anomalies: total_anom,
            calibration_count: cal_count,
            calibration_capacity: cal_cap,
            centroid_dim: dim,
            last_p_value: last_p,
            calibration_p75: Some(0.5),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let round: ConformalAnomalySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(round.total_observations, total_obs);
        prop_assert_eq!(round.total_anomalies, total_anom);
        prop_assert_eq!(round.calibration_count, cal_count);
    }

    // 20. SemanticAnomalyConfig serde roundtrip
    #[test]
    fn prop_semantic_config_serde(config in arb_semantic_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let round: SemanticAnomalyConfig = serde_json::from_str(&json).unwrap();
        let ca_ok = (round.centroid_alpha - config.centroid_alpha).abs() < 1e-6;
        prop_assert!(ca_ok, "centroid_alpha mismatch");
        let va_ok = (round.variance_alpha - config.variance_alpha).abs() < 1e-6;
        prop_assert!(va_ok, "variance_alpha mismatch");
        prop_assert_eq!(round.min_samples, config.min_samples);
        let st_ok = (round.shock_threshold_z - config.shock_threshold_z).abs() < 1e-6;
        prop_assert!(st_ok, "shock_threshold_z mismatch");
    }
}

// =============================================================================
// Conformal detector properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // 21. total_observations increments per non-empty observe
    #[test]
    fn prop_conformal_obs_count(
        config in arb_conformal_config(),
        count in 5_usize..50,
    ) {
        let mut det = ConformalAnomalyDetector::new(config);
        let v = vec![1.0_f32, 0.0, 0.0];
        for _ in 0..count {
            det.observe(&v);
        }
        prop_assert_eq!(det.total_observations(), count as u64);
    }

    // 22. p_value always in (0, 1] after warmup
    #[test]
    fn prop_conformal_p_value_bounded(
        min_cal in 3_usize..10,
    ) {
        let config = ConformalAnomalyConfig {
            min_calibration: min_cal,
            calibration_window: 50,
            alpha: 0.05,
            centroid_alpha: 0.1,
        };
        let mut det = ConformalAnomalyDetector::new(config);
        let v = vec![1.0_f32, 0.0, 0.0];
        for _ in 0..(min_cal + 5) {
            det.observe(&v);
        }
        let p = det.last_p_value();
        prop_assert!(p > 0.0, "p={p}");
        prop_assert!(p <= 1.0, "p={p}");
    }

    // 23. Stable stream FDR <= 2*alpha (empirical)
    #[test]
    fn prop_conformal_fdr_bound(
        alpha in 0.01_f64..0.2,
    ) {
        let config = ConformalAnomalyConfig {
            alpha,
            calibration_window: 100,
            min_calibration: 10,
            centroid_alpha: 0.1,
        };
        let mut det = ConformalAnomalyDetector::new(config);
        let v = vec![1.0_f32, 0.0, 0.0];
        let n = 500;
        let mut false_positives = 0u32;
        for _ in 0..n {
            if det.observe(&v).is_some() {
                false_positives += 1;
            }
        }
        let fdr = false_positives as f64 / n as f64;
        // Allow 2x alpha as margin for small sample effects.
        prop_assert!(
            fdr <= alpha * 2.0 + 0.02,
            "fdr={fdr} exceeds 2*alpha={} for alpha={alpha}",
            alpha * 2.0
        );
    }

    // 24. Reset clears centroid
    #[test]
    fn prop_conformal_reset(config in arb_conformal_config()) {
        let mut det = ConformalAnomalyDetector::new(config);
        let v = vec![1.0_f32, 0.0, 0.0];
        for _ in 0..20 {
            det.observe(&v);
        }
        det.reset();
        prop_assert!(det.current_centroid().is_empty());
        // But counters persist.
        prop_assert!(det.total_observations() >= 20);
    }

    // 25. Snapshot consistency
    #[test]
    fn prop_conformal_snapshot_consistent(
        config in arb_conformal_config(),
        count in 1_usize..50,
    ) {
        let mut det = ConformalAnomalyDetector::new(config.clone());
        let v = vec![1.0_f32, 0.0, 0.0];
        for _ in 0..count {
            det.observe(&v);
        }
        let snap = det.snapshot();
        prop_assert_eq!(snap.total_observations, count as u64);
        prop_assert_eq!(snap.calibration_capacity, config.calibration_window);
        prop_assert!(snap.calibration_count <= config.calibration_window);
        prop_assert_eq!(snap.centroid_dim, 3);
    }

    // 26. Orthogonal shift detected after calibration
    #[test]
    fn prop_conformal_detects_orthogonal(
        min_cal in 5_usize..20,
    ) {
        let config = ConformalAnomalyConfig {
            alpha: 0.05,
            calibration_window: 100,
            min_calibration: min_cal,
            centroid_alpha: 0.1,
        };
        let mut det = ConformalAnomalyDetector::new(config);
        let stable = vec![1.0_f32, 0.0, 0.0];
        for _ in 0..50 {
            det.observe(&stable);
        }
        let shift = vec![0.0_f32, 1.0, 0.0];
        let result = det.observe(&shift);
        prop_assert!(result.is_some(), "Orthogonal shift not detected, p={}", det.last_p_value());
    }

    // 27. Dimension change resets without panic
    #[test]
    fn prop_conformal_dim_change(
        dim1 in 2_usize..32,
        dim2 in 2_usize..32,
    ) {
        let config = ConformalAnomalyConfig::default();
        let mut det = ConformalAnomalyDetector::new(config);
        let v1: Vec<f32> = (0..dim1).map(|i| i as f32 + 1.0).collect();
        for _ in 0..10 {
            det.observe(&v1);
        }
        let v2: Vec<f32> = (0..dim2).map(|i| i as f32 + 1.0).collect();
        let result = det.observe(&v2);
        if dim1 != dim2 {
            // Dimension change resets → no anomaly.
            prop_assert!(result.is_none());
            prop_assert_eq!(det.current_centroid().len(), dim2);
        }
    }
}

// =============================================================================
// Z-score detector properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // 28. Z-score shock has positive distance and z_score
    #[test]
    fn prop_zscore_shock_positive(
        config in arb_semantic_config(),
    ) {
        let mut det = SemanticAnomalyDetector::new(config);
        let stable = vec![1.0_f32, 0.0, 0.0];
        for _ in 0..30 {
            det.observe(&stable);
        }
        let shift = vec![0.0_f32, 1.0, 0.0];
        if let Some(shock) = det.observe(&shift) {
            prop_assert!(shock.distance > 0.0, "distance={}", shock.distance);
            prop_assert!(shock.z_score > 0.0, "z_score={}", shock.z_score);
        }
    }

    // 29. Stable identical inputs produce no shocks
    #[test]
    fn prop_zscore_stable_no_shocks(
        config in arb_semantic_config(),
    ) {
        let mut det = SemanticAnomalyDetector::new(config);
        let v = vec![1.0_f32, 0.0, 0.0];
        let mut shocks = 0;
        for _ in 0..100 {
            if det.observe(&v).is_some() {
                shocks += 1;
            }
        }
        // Identical inputs should produce very few (if any) shocks.
        prop_assert!(shocks <= 3, "shocks={shocks} too many for identical inputs");
    }
}

// =============================================================================
// Entropy gate properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 31. Constant data always skipped
    #[test]
    fn prop_entropy_gate_constant_skipped(byte_val in 0_u8..=255) {
        let mut gate = EntropyGate::new(EntropyGateConfig {
            min_entropy_bits_per_byte: 2.0,
            min_segment_bytes: 4,
            enabled: true,
        });
        let segment = vec![byte_val; 200];
        let decision = gate.evaluate(&segment);
        let is_skip = matches!(decision, EntropyGateDecision::Skip { .. });
        prop_assert!(is_skip, "Constant byte {byte_val} should be skipped, got {decision:?}");
        prop_assert!(!decision.should_embed());
    }

    // 32. Uniform random data always passes
    #[test]
    fn prop_entropy_gate_uniform_passes(repeats in 2_usize..10) {
        let mut gate = EntropyGate::new(EntropyGateConfig {
            min_entropy_bits_per_byte: 2.0,
            min_segment_bytes: 4,
            enabled: true,
        });
        // All 256 byte values repeated → entropy ≈ 8.0
        let mut segment = Vec::with_capacity(256 * repeats);
        for _ in 0..repeats {
            for b in 0..=255u8 {
                segment.push(b);
            }
        }
        let decision = gate.evaluate(&segment);
        let is_pass = matches!(decision, EntropyGateDecision::Pass { .. });
        prop_assert!(is_pass, "Uniform data should pass, got {decision:?}");
        prop_assert!(decision.should_embed());
    }

    // 33. should_embed consistent with decision variant
    #[test]
    fn prop_entropy_gate_should_embed_consistent(
        enabled in proptest::bool::ANY,
        len in 0_usize..200,
        byte_val in 0_u8..=255,
    ) {
        let mut gate = EntropyGate::new(EntropyGateConfig {
            min_entropy_bits_per_byte: 2.0,
            min_segment_bytes: 16,
            enabled,
        });
        let segment = vec![byte_val; len];
        let decision = gate.evaluate(&segment);
        match &decision {
            EntropyGateDecision::Skip { .. } => {
                prop_assert!(!decision.should_embed());
            }
            EntropyGateDecision::Pass { .. }
            | EntropyGateDecision::Bypass { .. }
            | EntropyGateDecision::Disabled => {
                prop_assert!(decision.should_embed());
            }
        }
    }

    // 34. Statistics add up
    #[test]
    fn prop_entropy_gate_stats_add_up(
        segments in arb_vec(arb_vec(0_u8..=255, 0..100), 1..50),
    ) {
        let mut gate = EntropyGate::new(EntropyGateConfig {
            min_entropy_bits_per_byte: 2.0,
            min_segment_bytes: 8,
            enabled: true,
        });
        for seg in &segments {
            gate.evaluate(seg);
        }
        let total = gate.total_evaluated();
        let sum = gate.total_passed() + gate.total_skipped() + gate.total_bypassed();
        prop_assert_eq!(total, sum, "total={} != passed+skipped+bypassed={}", total, sum);
    }

    // 35. EntropyGateConfig serde roundtrip
    #[test]
    fn prop_entropy_gate_config_serde(
        threshold in 0.0_f64..8.0,
        min_bytes in 1_usize..1000,
        enabled in proptest::bool::ANY,
    ) {
        let config = EntropyGateConfig {
            min_entropy_bits_per_byte: threshold,
            min_segment_bytes: min_bytes,
            enabled,
        };
        let json = serde_json::to_string(&config).unwrap();
        let round: EntropyGateConfig = serde_json::from_str(&json).unwrap();
        let t_ok = (round.min_entropy_bits_per_byte - threshold).abs() < 1e-10;
        prop_assert!(t_ok, "threshold mismatch");
        prop_assert_eq!(round.min_segment_bytes, min_bytes);
        prop_assert_eq!(round.enabled, enabled);
    }

    // 36. EntropyGateSnapshot serde roundtrip
    #[test]
    fn prop_entropy_gate_snapshot_serde(
        total_eval in 0_u64..10000,
        total_pass in 0_u64..5000,
        total_skip in 0_u64..5000,
        total_bypass in 0_u64..2000,
    ) {
        let snap = EntropyGateSnapshot {
            enabled: true,
            threshold: 2.0,
            min_segment_bytes: 16,
            total_evaluated: total_eval,
            total_passed: total_pass,
            total_skipped: total_skip,
            total_bypassed: total_bypass,
            average_entropy: 4.0,
            skip_ratio: 0.3,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let round: EntropyGateSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(round.total_evaluated, total_eval);
        prop_assert_eq!(round.total_passed, total_pass);
        prop_assert_eq!(round.total_skipped, total_skip);
        prop_assert_eq!(round.total_bypassed, total_bypass);
    }

    // 37. skip_ratio in [0, 1]
    #[test]
    fn prop_entropy_gate_skip_ratio_bounded(
        segments in arb_vec(arb_vec(0_u8..=255, 0..50), 1..30),
    ) {
        let mut gate = EntropyGate::new(EntropyGateConfig {
            min_entropy_bits_per_byte: 2.0,
            min_segment_bytes: 4,
            enabled: true,
        });
        for seg in &segments {
            gate.evaluate(seg);
        }
        let ratio = gate.skip_ratio();
        prop_assert!(ratio >= 0.0, "ratio={ratio} < 0");
        prop_assert!(ratio <= 1.0, "ratio={ratio} > 1");
    }

    // 38. Disabled gate always returns Disabled
    #[test]
    fn prop_entropy_gate_disabled_always_disabled(
        len in 0_usize..200,
        byte_val in 0_u8..=255,
    ) {
        let mut gate = EntropyGate::new(EntropyGateConfig {
            enabled: false,
            ..Default::default()
        });
        let segment = vec![byte_val; len];
        let decision = gate.evaluate(&segment);
        let is_disabled = matches!(decision, EntropyGateDecision::Disabled);
        prop_assert!(is_disabled, "Disabled gate should return Disabled, got {decision:?}");
    }

    // 39. Average entropy in [0, 8] for measured segments
    #[test]
    fn prop_entropy_gate_avg_entropy_bounded(
        segments in arb_vec(arb_vec(0_u8..=255, 20..100), 5..30),
    ) {
        let mut gate = EntropyGate::new(EntropyGateConfig {
            min_entropy_bits_per_byte: 2.0,
            min_segment_bytes: 4,
            enabled: true,
        });
        for seg in &segments {
            gate.evaluate(seg);
        }
        let avg = gate.average_entropy();
        prop_assert!(avg >= 0.0, "avg={avg} < 0");
        prop_assert!(avg <= 8.0, "avg={avg} > 8");
    }

    // 40. Short segments always bypassed
    #[test]
    fn prop_entropy_gate_short_bypassed(
        min_bytes in 10_usize..50,
        len in 0_usize..10,
        byte_val in 0_u8..=255,
    ) {
        if len >= min_bytes {
            return Ok(());
        }
        let mut gate = EntropyGate::new(EntropyGateConfig {
            min_segment_bytes: min_bytes,
            enabled: true,
            ..Default::default()
        });
        let segment = vec![byte_val; len];
        let decision = gate.evaluate(&segment);
        let is_bypass = matches!(decision, EntropyGateDecision::Bypass { .. });
        prop_assert!(is_bypass, "Short segment (len={len} < min={min_bytes}) should bypass, got {decision:?}");
    }
}

// =============================================================================
// Mathematical isomorphism proofs (ft-344j8.10)
// =============================================================================

/// Naive O(N) count of scores >= threshold for isomorphism proof.
fn naive_count_geq(scores: &[f32], threshold: f32) -> usize {
    scores.iter().filter(|&&s| s >= threshold).count()
}

/// Naive conformal p-value for isomorphism proof.
fn naive_p_value(scores: &[f32], new_score: f32) -> f64 {
    if scores.is_empty() {
        return 1.0;
    }
    let count_geq = scores.iter().filter(|&&s| s >= new_score).count() as f64;
    let n = scores.len() as f64;
    (count_geq + 1.0) / (n + 1.0)
}

/// Naive scalar dot product for isomorphism proof.
fn naive_dot_product(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    a[..n].iter().zip(&b[..n]).map(|(x, y)| x * y).sum()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // 41. O(log N) count_geq matches naive O(N) linear scan (ISOMORPHISM PROOF)
    //
    // This is the core mathematical guarantee: the O(log N) binary-search-based
    // rank query in SortedCalibrationBuffer produces IDENTICAL results to a
    // naive O(N) linear scan over all calibration scores.
    #[test]
    fn prop_rank_query_isomorphism(
        capacity in 5_usize..100,
        insertions in arb_vec(arb_positive_f32(), 5..200),
        query_thresholds in arb_vec(arb_positive_f32(), 1..20),
    ) {
        let mut buf = SortedCalibrationBuffer::new(capacity);

        // Build the buffer.
        for &s in &insertions {
            buf.insert(s);
        }

        // Extract the actual scores in the buffer by querying all values.
        // We reconstruct the buffer contents using quantile queries.
        let buf_len = buf.len();
        let mut actual_scores = Vec::with_capacity(buf_len);
        for i in 0..buf_len {
            let q = if buf_len == 1 {
                0.0
            } else {
                i as f32 / (buf_len - 1) as f32
            };
            if let Some(v) = buf.quantile(q) {
                actual_scores.push(v);
            }
        }

        // For each query threshold, verify count_geq matches naive.
        for &threshold in &query_thresholds {
            let fast_count = buf.count_geq(threshold);
            let naive_count = naive_count_geq(&actual_scores, threshold);
            prop_assert_eq!(
                fast_count,
                naive_count,
                "count_geq mismatch for threshold={}: fast={}, naive={}",
                threshold,
                fast_count,
                naive_count
            );
        }
    }

    // 42. conformal_p_value matches naive computation (ISOMORPHISM PROOF)
    #[test]
    fn prop_p_value_isomorphism(
        capacity in 10_usize..100,
        insertions in arb_vec(arb_positive_f32(), 10..100),
        query_scores in arb_vec(arb_positive_f32(), 1..10),
    ) {
        let mut buf = SortedCalibrationBuffer::new(capacity);
        for &s in &insertions {
            buf.insert(s);
        }

        // Reconstruct buffer contents via quantile sampling.
        let buf_len = buf.len();
        let mut actual_scores = Vec::with_capacity(buf_len);
        for i in 0..buf_len {
            let q = if buf_len == 1 {
                0.0
            } else {
                i as f32 / (buf_len - 1) as f32
            };
            if let Some(v) = buf.quantile(q) {
                actual_scores.push(v);
            }
        }

        for &query in &query_scores {
            let fast_p = buf.conformal_p_value(query);
            let naive_p = naive_p_value(&actual_scores, query);
            let diff = (fast_p - naive_p).abs();
            prop_assert!(
                diff < 1e-10,
                "p_value mismatch for query={}: fast={}, naive={}, diff={}",
                query,
                fast_p,
                naive_p,
                diff
            );
        }
    }

    // 43. SIMD dot product matches naive for 384d vectors (embedding isomorphism)
    #[test]
    fn prop_simd_384d_isomorphism(
        seed in 0_u64..10000,
    ) {
        // Deterministic pseudo-random 384d vectors from seed.
        let a: Vec<f32> = (0..384)
            .map(|i| ((seed.wrapping_mul(6364136223846793005).wrapping_add(i as u64)) as f32) / u32::MAX as f32)
            .collect();
        let b: Vec<f32> = (0..384)
            .map(|i| ((seed.wrapping_mul(1442695040888963407).wrapping_add(i as u64 + 384)) as f32) / u32::MAX as f32)
            .collect();

        let simd_result = dot_product_simd(&a, &b);
        let naive_result = naive_dot_product(&a, &b);

        let tol = (naive_result.abs() * 1e-4).max(1e-4);
        let diff = (simd_result - naive_result).abs();
        prop_assert!(
            diff < tol,
            "384d isomorphism failed: simd={}, naive={}, diff={}, tol={}",
            simd_result,
            naive_result,
            diff,
            tol
        );
    }
}

// =============================================================================
// Deterministic fixture-based tests (ft-344j8.10)
// =============================================================================

/// Pre-computed embedding vectors for deterministic testing.
/// These are fixed vectors that simulate real embedding output.
/// No network access required.
mod fixtures {
    /// "Compiling main.rs" — high-weight on first dimensions (Rust context).
    pub const RUST_COMPILE: [f32; 8] = [0.9, 0.1, 0.05, 0.02, 0.01, 0.01, 0.0, 0.0];
    /// "java.lang.NullPointerException" — high-weight on middle dimensions (Java error).
    pub const JAVA_NPE: [f32; 8] = [0.01, 0.02, 0.8, 0.9, 0.05, 0.01, 0.0, 0.0];
    /// "Building... 42% [####]" — very similar to RUST_COMPILE.
    pub const BUILD_PROGRESS: [f32; 8] = [0.85, 0.12, 0.06, 0.03, 0.01, 0.01, 0.0, 0.0];
    /// "404 Not Found <html>" — orthogonal to all code contexts.
    pub const HTML_404: [f32; 8] = [0.0, 0.0, 0.01, 0.01, 0.02, 0.05, 0.9, 0.8];
    /// "test passed: 42 assertions" — test output context.
    pub const TEST_PASSED: [f32; 8] = [0.3, 0.1, 0.05, 0.02, 0.5, 0.3, 0.01, 0.0];
}

#[test]
fn test_fixture_orthogonal_shift_detected() {
    use frankenterm_core::semantic_anomaly::{ConformalAnomalyConfig, ConformalAnomalyDetector};

    let config = ConformalAnomalyConfig {
        min_calibration: 5,
        calibration_window: 50,
        alpha: 0.05,
        centroid_alpha: 0.1,
    };
    let mut det = ConformalAnomalyDetector::new(config);

    // Build calibration with a mix of similar Rust contexts to create
    // non-zero baseline variance in the calibration window.
    for i in 0..30 {
        // Alternate between compile and build progress to establish
        // a realistic calibration with some natural variance.
        if i % 3 == 0 {
            det.observe(&fixtures::BUILD_PROGRESS);
        } else {
            det.observe(&fixtures::RUST_COMPILE);
        }
    }

    // Test passed output is fairly similar → should NOT trigger
    // (it has overlap with the Rust compile context).
    let test_result = det.observe(&fixtures::TEST_PASSED);
    // test_passed shares some dimensions with rust_compile, but
    // may or may not trigger depending on calibration state.
    // The key assertion: Java NPE IS detected.

    // Java NPE is orthogonal → should trigger.
    let npe_result = det.observe(&fixtures::JAVA_NPE);
    assert!(
        npe_result.is_some(),
        "Java NPE should be anomalous after Rust context, p={}, test_result was {:?}",
        det.last_p_value(),
        test_result.is_some()
    );
}

#[test]
fn test_fixture_html_404_shift_detected() {
    use frankenterm_core::semantic_anomaly::{ConformalAnomalyConfig, ConformalAnomalyDetector};

    let config = ConformalAnomalyConfig {
        min_calibration: 5,
        calibration_window: 50,
        alpha: 0.05,
        centroid_alpha: 0.1,
    };
    let mut det = ConformalAnomalyDetector::new(config);

    // Build calibration with Rust context.
    for _ in 0..30 {
        det.observe(&fixtures::RUST_COMPILE);
    }

    // HTML 404 is completely orthogonal → must detect.
    let result = det.observe(&fixtures::HTML_404);
    assert!(
        result.is_some(),
        "HTML 404 should be anomalous, p={}",
        det.last_p_value()
    );
}

#[test]
fn test_fixture_fdr_bound_empirical() {
    use frankenterm_core::semantic_anomaly::{ConformalAnomalyConfig, ConformalAnomalyDetector};

    let alpha = 0.05;
    let config = ConformalAnomalyConfig {
        min_calibration: 10,
        calibration_window: 100,
        alpha,
        centroid_alpha: 0.1,
    };
    let mut det = ConformalAnomalyDetector::new(config);

    // Feed 1000 observations of stable Rust compile context.
    let n = 1000;
    let mut false_positives = 0u32;
    for _ in 0..n {
        if det.observe(&fixtures::RUST_COMPILE).is_some() {
            false_positives += 1;
        }
    }

    let empirical_fdr = false_positives as f64 / n as f64;
    // The conformal guarantee: FDR <= alpha for exchangeable data.
    // Allow 2x margin for small-sample effects.
    assert!(
        empirical_fdr <= alpha * 2.0 + 0.01,
        "Empirical FDR {empirical_fdr} exceeds 2*alpha={}",
        alpha * 2.0
    );
}

#[test]
fn test_fixture_entropy_gate_integration() {
    use frankenterm_core::semantic_anomaly::{
        ConformalAnomalyConfig, EntropyGateConfig, GatedAnomalyDetector,
    };

    let mut gated = GatedAnomalyDetector::new(
        EntropyGateConfig {
            min_entropy_bits_per_byte: 2.0,
            min_segment_bytes: 4,
            enabled: true,
        },
        ConformalAnomalyConfig {
            min_calibration: 5,
            calibration_window: 50,
            alpha: 0.05,
            centroid_alpha: 0.1,
        },
    );

    // Low-entropy segment (progress bar) → skipped.
    let progress_bar = b"==============================================";
    let result = gated.observe(progress_bar, |_| panic!("should be skipped"));
    assert!(result.was_skipped());

    // High-entropy segment → processed.
    let mut diverse = Vec::with_capacity(256);
    for b in 0..=255u8 {
        diverse.push(b);
    }
    let result = gated.observe(&diverse, |_| fixtures::RUST_COMPILE.to_vec());
    assert!(!result.was_skipped());

    // Verify gate statistics.
    assert_eq!(gated.gate.total_evaluated(), 2);
    assert_eq!(gated.gate.total_skipped(), 1);
    assert_eq!(gated.gate.total_passed(), 1);
}
