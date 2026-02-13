//! Property-based tests for BOCPD (Bayesian Online Change-Point Detection).
//!
//! Verifies core invariants:
//! - Posterior distributions always sum to ~1.0
//! - Hazard rate within valid range produces valid model behavior
//! - Feature computation is well-defined for arbitrary text
//! - Change-point detection monotonically counts
//!
//! Bead: wa-1qz1.2

use std::time::Duration;

use proptest::prelude::*;

use frankenterm_core::bocpd::{BocpdConfig, BocpdManager, BocpdModel, OutputFeatures};

// =============================================================================
// Proptest: Hazard rate robustness
// =============================================================================

proptest! {
    /// Any valid hazard rate should produce a model that doesn't panic or NaN.
    #[test]
    fn hazard_rate_robustness(
        hazard_rate in 0.001f64..0.1,
        offset in -50.0f64..50.0,
    ) {
        let config = BocpdConfig {
            hazard_rate,
            detection_threshold: 0.7,
            min_observations: 5,
            max_run_length: 50,
        };
        let mut model = BocpdModel::new(config);

        // Feed 40 observations at varying offsets
        for i in 0..40 {
            let x = (i as f64).mul_add(0.3, offset);
            let _cp = model.update(x);
        }

        // Posterior must sum to ~1.0
        let post_sum = model.posterior_sum();
        prop_assert!(
            (post_sum - 1.0).abs() < 0.01,
            "posterior sum {} too far from 1.0 (hazard_rate={}, offset={})",
            post_sum, hazard_rate, offset,
        );

        // Observation count must match
        prop_assert_eq!(model.observation_count(), 40);

        // MAP run length must be within bounds
        let map_rl = model.map_run_length();
        prop_assert!(
            map_rl <= 50,
            "MAP run length {} exceeds max_run_length 50",
            map_rl,
        );
    }

    // =========================================================================
    // Proptest: Conjugate prior consistency via model
    // =========================================================================

    /// Feeding a constant value should result in a stable model (no spurious
    /// change-points after warmup).
    #[test]
    fn conjugate_prior_stability(
        value in -100.0f64..100.0,
        n_obs in 30usize..200,
    ) {
        let config = BocpdConfig {
            hazard_rate: 0.005,
            detection_threshold: 0.7,
            min_observations: 20,
            max_run_length: 200,
        };
        let mut model = BocpdModel::new(config);

        let mut change_points = 0u64;
        for _ in 0..n_obs {
            if model.update(value).is_some() {
                change_points += 1;
            }
        }

        // A constant signal should trigger very few (ideally zero) change-points.
        // Allow up to 2 due to warmup edge effects.
        prop_assert!(
            change_points <= 2,
            "constant value {} produced {} change-points over {} observations",
            value, change_points, n_obs,
        );

        // Posterior must remain valid
        let post_sum = model.posterior_sum();
        prop_assert!(
            (post_sum - 1.0).abs() < 0.01,
            "posterior sum {} diverged from 1.0",
            post_sum,
        );
    }

    // =========================================================================
    // Proptest: Run length normalization
    // =========================================================================

    /// After arbitrary observation sequences, the run length posterior must
    /// remain a valid probability distribution.
    #[test]
    fn run_length_normalization(
        observations in prop::collection::vec(-1000.0f64..1000.0, 1..300),
    ) {
        let config = BocpdConfig {
            hazard_rate: 0.01,
            detection_threshold: 0.5,
            min_observations: 5,
            max_run_length: 100,
        };
        let mut model = BocpdModel::new(config);

        for &x in &observations {
            let _cp = model.update(x);
        }

        // Posterior must always be a valid distribution
        let posterior = model.run_length_posterior();
        let total: f64 = posterior.iter().sum();
        prop_assert!(
            (total - 1.0).abs() < 0.01,
            "posterior total {} not ~1.0 after {} observations",
            total, observations.len(),
        );

        // All probabilities must be non-negative
        for (i, &p) in posterior.iter().enumerate() {
            prop_assert!(
                p >= 0.0 && p.is_finite(),
                "posterior[{}] = {} is invalid",
                i, p,
            );
        }

        // Observation count must match
        prop_assert_eq!(model.observation_count(), observations.len() as u64);
    }

    // =========================================================================
    // Proptest: Feature computation validity
    // =========================================================================

    /// Feature computation must never produce NaN or Inf for any input.
    #[test]
    fn feature_computation_valid(
        text in "[a-zA-Z0-9 \n\t]{0,2000}",
        elapsed_ms in 1u64..60_000,
    ) {
        let elapsed = Duration::from_millis(elapsed_ms);
        let features = OutputFeatures::compute(&text, elapsed);

        // All fields must be finite
        prop_assert!(features.output_rate.is_finite(), "output_rate is not finite");
        prop_assert!(features.byte_rate.is_finite(), "byte_rate is not finite");
        prop_assert!(features.entropy.is_finite(), "entropy is not finite");
        prop_assert!(features.unique_line_ratio.is_finite(), "unique_line_ratio is not finite");
        prop_assert!(features.ansi_density.is_finite(), "ansi_density is not finite");

        // Ranges
        prop_assert!(features.output_rate >= 0.0, "output_rate negative");
        prop_assert!(features.byte_rate >= 0.0, "byte_rate negative");
        prop_assert!((0.0..=8.0).contains(&features.entropy),
            "entropy {} out of [0, 8]", features.entropy);
        prop_assert!((0.0..=1.0).contains(&features.unique_line_ratio),
            "unique_line_ratio {} out of [0, 1]", features.unique_line_ratio);
        prop_assert!((0.0..=1.0).contains(&features.ansi_density),
            "ansi_density {} out of [0, 1]", features.ansi_density);

        // primary_metric must be the output_rate
        prop_assert!(
            (features.primary_metric() - features.output_rate).abs() < f64::EPSILON,
            "primary_metric {} != output_rate {}",
            features.primary_metric(), features.output_rate,
        );
    }

    // =========================================================================
    // Proptest: Manager snapshot roundtrip
    // =========================================================================

    /// Manager snapshots must serialize/deserialize without loss.
    #[test]
    fn manager_snapshot_roundtrip(
        pane_count in 1usize..20,
        obs_per_pane in 5usize..50,
    ) {
        let config = BocpdConfig::default();
        let mut manager = BocpdManager::new(config);

        for pane_id in 0..pane_count as u64 {
            manager.register_pane(pane_id);
            for i in 0..obs_per_pane {
                let features = OutputFeatures {
                    output_rate: (i as f64).mul_add(0.5, 10.0),
                    byte_rate: (i as f64).mul_add(10.0, 500.0),
                    entropy: 4.0,
                    unique_line_ratio: 0.8,
                    ansi_density: 0.05,
                };
                manager.observe(pane_id, features);
            }
        }

        let snapshot = manager.snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let roundtrip: frankenterm_core::bocpd::BocpdSnapshot =
            serde_json::from_str(&json).unwrap();

        prop_assert_eq!(roundtrip.pane_count, snapshot.pane_count);
        prop_assert_eq!(roundtrip.total_change_points, snapshot.total_change_points);
        prop_assert_eq!(roundtrip.panes.len(), snapshot.panes.len());
    }

    // =========================================================================
    // Proptest: Change-point count monotonicity
    // =========================================================================

    /// Change-point count must be monotonically non-decreasing.
    #[test]
    fn change_point_count_monotonic(
        observations in prop::collection::vec(-500.0f64..500.0, 10..200),
    ) {
        let config = BocpdConfig {
            hazard_rate: 0.01,
            detection_threshold: 0.5,
            min_observations: 5,
            max_run_length: 100,
        };
        let mut model = BocpdModel::new(config);
        let mut prev_count = 0u64;

        for &x in &observations {
            let _cp = model.update(x);
            let current_count = model.change_point_count();
            prop_assert!(
                current_count >= prev_count,
                "change_point_count decreased from {} to {}",
                prev_count, current_count,
            );
            prev_count = current_count;
        }
    }
}
