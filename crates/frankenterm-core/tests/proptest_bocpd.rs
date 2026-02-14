//! Property-based tests for BOCPD (Bayesian Online Change-Point Detection).
//!
//! Verifies core invariants:
//! - Posterior distributions always sum to ~1.0
//! - Hazard rate within valid range produces valid model behavior
//! - Feature computation is well-defined for arbitrary text
//! - Change-point detection monotonically counts
//! - Serde roundtrips for all serializable types
//! - Manager register/unregister/auto-registration
//! - Warmup phase prevents false detections
//!
//! Bead: wa-1qz1.2, wa-1u90p.7.1

use std::time::Duration;

use proptest::prelude::*;

use frankenterm_core::bocpd::{
    BocpdConfig, BocpdManager, BocpdModel, BocpdSnapshot, ChangePoint, OutputFeatures,
    PaneBocpdSummary, PaneChangePoint,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_bocpd_config() -> impl Strategy<Value = BocpdConfig> {
    (
        0.001f64..0.1, // hazard_rate
        0.1f64..0.99,  // detection_threshold
        5usize..50,    // min_observations
        50usize..500,  // max_run_length
    )
        .prop_map(
            |(hazard_rate, detection_threshold, min_observations, max_run_length)| BocpdConfig {
                hazard_rate,
                detection_threshold,
                min_observations,
                max_run_length,
            },
        )
}

fn arb_change_point() -> impl Strategy<Value = ChangePoint> {
    (
        0u64..10_000, // observation_index
        0.0f64..1.0,  // posterior_probability
        0usize..500,  // map_run_length
    )
        .prop_map(
            |(observation_index, posterior_probability, map_run_length)| ChangePoint {
                observation_index,
                posterior_probability,
                map_run_length,
            },
        )
}

fn arb_output_features() -> impl Strategy<Value = OutputFeatures> {
    (
        0.0f64..1000.0,    // output_rate
        0.0f64..100_000.0, // byte_rate
        0.0f64..8.0,       // entropy
        0.0f64..1.0,       // unique_line_ratio
        0.0f64..1.0,       // ansi_density
    )
        .prop_map(
            |(output_rate, byte_rate, entropy, unique_line_ratio, ansi_density)| OutputFeatures {
                output_rate,
                byte_rate,
                entropy,
                unique_line_ratio,
                ansi_density,
            },
        )
}

fn arb_pane_change_point() -> impl Strategy<Value = PaneChangePoint> {
    (
        0u64..1000,                                  // pane_id
        0u64..10_000,                                // observation_index
        0.0f64..1.0,                                 // posterior_probability
        proptest::option::of(arb_output_features()), // features_at_change
        0u64..2_000_000_000,                         // timestamp_secs
    )
        .prop_map(
            |(
                pane_id,
                observation_index,
                posterior_probability,
                features_at_change,
                timestamp_secs,
            )| {
                PaneChangePoint {
                    pane_id,
                    observation_index,
                    posterior_probability,
                    features_at_change,
                    timestamp_secs,
                }
            },
        )
}

fn arb_pane_summary() -> impl Strategy<Value = PaneBocpdSummary> {
    (
        0u64..1000,
        0u64..10_000,
        0u64..100,
        0.0f64..1.0,
        0u64..500,
        proptest::bool::ANY,
    )
        .prop_map(
            |(
                pane_id,
                observation_count,
                change_point_count,
                current_change_prob,
                map_run_length,
                in_warmup,
            )| {
                PaneBocpdSummary {
                    pane_id,
                    observation_count,
                    change_point_count,
                    current_change_prob,
                    map_run_length,
                    in_warmup,
                }
            },
        )
}

fn arb_bocpd_snapshot() -> impl Strategy<Value = BocpdSnapshot> {
    (
        0u64..100,
        0u64..500,
        prop::collection::vec(arb_pane_summary(), 0..10),
    )
        .prop_map(|(pane_count, total_change_points, panes)| BocpdSnapshot {
            pane_count,
            total_change_points,
            panes,
        })
}

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
        let roundtrip: BocpdSnapshot =
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

// =============================================================================
// Serde roundtrip properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// BocpdConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_config_serde_roundtrip(config in arb_bocpd_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: BocpdConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((back.hazard_rate - config.hazard_rate).abs() < 1e-10,
            "hazard_rate mismatch");
        prop_assert!((back.detection_threshold - config.detection_threshold).abs() < 1e-10,
            "detection_threshold mismatch");
        prop_assert_eq!(back.min_observations, config.min_observations);
        prop_assert_eq!(back.max_run_length, config.max_run_length);
    }

    /// ChangePoint serde roundtrip preserves all fields.
    #[test]
    fn prop_change_point_serde_roundtrip(cp in arb_change_point()) {
        let json = serde_json::to_string(&cp).unwrap();
        let back: ChangePoint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.observation_index, cp.observation_index);
        prop_assert!((back.posterior_probability - cp.posterior_probability).abs() < 1e-10);
        prop_assert_eq!(back.map_run_length, cp.map_run_length);
    }

    /// OutputFeatures serde roundtrip preserves all fields.
    #[test]
    fn prop_output_features_serde_roundtrip(f in arb_output_features()) {
        let json = serde_json::to_string(&f).unwrap();
        let back: OutputFeatures = serde_json::from_str(&json).unwrap();
        prop_assert!((back.output_rate - f.output_rate).abs() < 1e-10);
        prop_assert!((back.byte_rate - f.byte_rate).abs() < 1e-10);
        prop_assert!((back.entropy - f.entropy).abs() < 1e-10);
        prop_assert!((back.unique_line_ratio - f.unique_line_ratio).abs() < 1e-10);
        prop_assert!((back.ansi_density - f.ansi_density).abs() < 1e-10);
    }

    /// PaneChangePoint serde roundtrip preserves all fields.
    #[test]
    fn prop_pane_change_point_serde_roundtrip(pcp in arb_pane_change_point()) {
        let json = serde_json::to_string(&pcp).unwrap();
        let back: PaneChangePoint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, pcp.pane_id);
        prop_assert_eq!(back.observation_index, pcp.observation_index);
        prop_assert!((back.posterior_probability - pcp.posterior_probability).abs() < 1e-10);
        prop_assert_eq!(back.timestamp_secs, pcp.timestamp_secs);
        prop_assert_eq!(back.features_at_change.is_some(), pcp.features_at_change.is_some());
    }

    /// PaneBocpdSummary serde roundtrip preserves all fields.
    #[test]
    fn prop_pane_summary_serde_roundtrip(s in arb_pane_summary()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: PaneBocpdSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, s.pane_id);
        prop_assert_eq!(back.observation_count, s.observation_count);
        prop_assert_eq!(back.change_point_count, s.change_point_count);
        prop_assert!((back.current_change_prob - s.current_change_prob).abs() < 1e-10);
        prop_assert_eq!(back.map_run_length, s.map_run_length);
        prop_assert_eq!(back.in_warmup, s.in_warmup);
    }

    /// BocpdSnapshot serde roundtrip preserves structure.
    #[test]
    fn prop_snapshot_serde_roundtrip(snap in arb_bocpd_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: BocpdSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_count, snap.pane_count);
        prop_assert_eq!(back.total_change_points, snap.total_change_points);
        prop_assert_eq!(back.panes.len(), snap.panes.len());
        for (b, s) in back.panes.iter().zip(snap.panes.iter()) {
            prop_assert_eq!(b.pane_id, s.pane_id);
            prop_assert_eq!(b.observation_count, s.observation_count);
        }
    }
}

// =============================================================================
// Model behavioral properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// During warmup, no change-points are detected.
    #[test]
    fn prop_warmup_no_detection(
        min_obs in 5usize..30,
        observations in prop::collection::vec(-100.0f64..100.0, 30..60),
    ) {
        let config = BocpdConfig {
            hazard_rate: 0.01,
            detection_threshold: 0.5,
            min_observations: min_obs,
            max_run_length: 100,
        };
        let mut model = BocpdModel::new(config);

        for (i, &x) in observations.iter().enumerate() {
            if i < min_obs {
                let cp = model.update(x);
                prop_assert!(
                    cp.is_none(),
                    "change-point detected during warmup at obs {}",
                    i,
                );
                prop_assert!(model.in_warmup() || i + 1 == min_obs);
            } else {
                let _ = model.update(x);
            }
        }
    }

    /// observation_count always equals the number of update() calls.
    #[test]
    fn prop_observation_count_tracks_updates(
        observations in prop::collection::vec(-500.0f64..500.0, 1..200),
    ) {
        let config = BocpdConfig::default();
        let mut model = BocpdModel::new(config);

        for (i, &x) in observations.iter().enumerate() {
            let _ = model.update(x);
            prop_assert_eq!(
                model.observation_count(), (i + 1) as u64,
                "observation_count mismatch at step {}", i,
            );
        }
    }

    /// change_point_probability is always in [0.0, 1.0].
    #[test]
    fn prop_change_point_probability_bounded(
        observations in prop::collection::vec(-500.0f64..500.0, 1..100),
    ) {
        let config = BocpdConfig {
            hazard_rate: 0.01,
            detection_threshold: 0.5,
            min_observations: 5,
            max_run_length: 100,
        };
        let mut model = BocpdModel::new(config);

        for &x in &observations {
            let _ = model.update(x);
            let p = model.change_point_probability();
            prop_assert!(
                (0.0..=1.0).contains(&p) && p.is_finite(),
                "change_point_probability {} out of [0, 1]", p,
            );
        }
    }

    /// MAP run length never exceeds max_run_length.
    #[test]
    fn prop_map_run_length_bounded(
        max_run in 20usize..200,
        observations in prop::collection::vec(-200.0f64..200.0, 10..100),
    ) {
        let config = BocpdConfig {
            hazard_rate: 0.01,
            detection_threshold: 0.5,
            min_observations: 5,
            max_run_length: max_run,
        };
        let mut model = BocpdModel::new(config);

        for &x in &observations {
            let _ = model.update(x);
            let rl = model.map_run_length();
            prop_assert!(
                rl <= max_run,
                "MAP run length {} exceeds max_run_length {}",
                rl, max_run,
            );
        }
    }
}

// =============================================================================
// Manager behavioral properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Register/unregister tracks pane_count correctly.
    #[test]
    fn prop_manager_register_unregister(
        pane_ids in prop::collection::vec(0u64..1000, 1..30),
    ) {
        let config = BocpdConfig::default();
        let mut manager = BocpdManager::new(config);

        // Deduplicate pane_ids for predictable counts
        let unique: std::collections::HashSet<u64> = pane_ids.iter().copied().collect();
        for &id in &unique {
            manager.register_pane(id);
        }
        prop_assert_eq!(manager.pane_count(), unique.len());

        // Remove half
        let remove: Vec<u64> = unique.iter().copied().take(unique.len() / 2).collect();
        for &id in &remove {
            manager.unregister_pane(id);
        }
        prop_assert_eq!(manager.pane_count(), unique.len() - remove.len());
    }

    /// Observe auto-registers unknown panes.
    #[test]
    fn prop_manager_auto_registers(
        pane_ids in prop::collection::vec(0u64..500, 1..20),
    ) {
        let config = BocpdConfig::default();
        let mut manager = BocpdManager::new(config);

        let unique: std::collections::HashSet<u64> = pane_ids.iter().copied().collect();
        for &id in &unique {
            let features = OutputFeatures {
                output_rate: 5.0,
                byte_rate: 200.0,
                entropy: 4.0,
                unique_line_ratio: 0.8,
                ansi_density: 0.05,
            };
            manager.observe(id, features);
        }
        prop_assert_eq!(manager.pane_count(), unique.len());
    }

    /// Manager total_change_points is monotonically non-decreasing.
    #[test]
    fn prop_manager_total_cps_monotonic(
        pane_count in 1usize..5,
        obs_count in 10usize..80,
    ) {
        let config = BocpdConfig {
            hazard_rate: 0.01,
            detection_threshold: 0.5,
            min_observations: 5,
            max_run_length: 100,
        };
        let mut manager = BocpdManager::new(config);
        let mut prev_total = 0u64;

        for pane_id in 0..pane_count as u64 {
            manager.register_pane(pane_id);
        }

        for i in 0..obs_count {
            let pane_id = (i as u64) % (pane_count as u64);
            let features = OutputFeatures {
                output_rate: if i < obs_count / 2 { 5.0 } else { 500.0 },
                byte_rate: 200.0,
                entropy: 4.0,
                unique_line_ratio: 0.8,
                ansi_density: 0.05,
            };
            manager.observe(pane_id, features);
            let current = manager.total_change_points();
            prop_assert!(
                current >= prev_total,
                "total_change_points decreased from {} to {}",
                prev_total, current,
            );
            prev_total = current;
        }
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn config_default_serde_roundtrip() {
    let config = BocpdConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    let back: BocpdConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.max_run_length, 200);
    assert!((back.hazard_rate - 0.005).abs() < 1e-10);
}

#[test]
fn empty_manager_snapshot() {
    let manager = BocpdManager::new(BocpdConfig::default());
    let snap = manager.snapshot();
    assert_eq!(snap.pane_count, 0);
    assert_eq!(snap.total_change_points, 0);
    assert!(snap.panes.is_empty());
}

#[test]
fn features_primary_metric_is_output_rate() {
    let f = OutputFeatures {
        output_rate: 42.0,
        byte_rate: 100.0,
        entropy: 3.0,
        unique_line_ratio: 0.5,
        ansi_density: 0.1,
    };
    assert!((f.primary_metric() - 42.0).abs() < f64::EPSILON);
}
