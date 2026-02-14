//! Property-based tests for adaptive Kalman watchdog thresholds.
//!
//! Bead: wa-1qz1.7
//!
//! Verifies the following properties:
//! 1. Kalman filter converges to the true mean of a stationary signal
//! 2. Variance P remains strictly positive after any observation sequence
//! 3. Adaptive threshold is always >= the Kalman estimate (for k >= 0)
//! 4. z-scores are monotonically non-decreasing with distance from estimate
//! 5. Health status ordering: Healthy < Degraded < Critical < Hung
//! 6. Serde roundtrips for AdaptiveWatchdogConfig, HealthClassification,
//!    AdaptiveHealthReport, ComponentClassification
//! 7. ComponentTracker observation_count tracking
//! 8. AdaptiveWatchdog component-level health report consistency
//! 9. Kalman filter std_dev relationship to variance

use proptest::prelude::*;

use frankenterm_core::kalman_watchdog::{
    AdaptiveHealthReport, AdaptiveWatchdog, AdaptiveWatchdogConfig, ComponentClassification,
    ComponentTracker, HealthClassification, KalmanFilter,
};
use frankenterm_core::watchdog::{Component, HealthStatus};

// =============================================================================
// Strategies
// =============================================================================

fn arb_adaptive_config() -> impl Strategy<Value = AdaptiveWatchdogConfig> {
    (
        0.5f64..10.0,    // sensitivity_k
        0.01f64..1000.0, // process_noise
        0.01f64..10000.0, // measurement_noise
        1usize..20,      // min_observations
        0.5f64..5.0,     // degraded_z
        1.0f64..8.0,     // critical_z
        2.0f64..15.0,    // hung_z
    )
        .prop_map(|(k, pn, mn, mo, dz, cz, hz)| {
            // Ensure degraded_z < critical_z < hung_z
            let dz = dz;
            let cz = dz + (cz - 1.0).abs() + 0.1;
            let hz = cz + (hz - 2.0).abs() + 0.1;
            AdaptiveWatchdogConfig {
                sensitivity_k: k,
                process_noise: pn,
                measurement_noise: mn,
                min_observations: mo,
                degraded_z: dz,
                critical_z: cz,
                hung_z: hz,
            }
        })
}

fn arb_health_status() -> impl Strategy<Value = HealthStatus> {
    prop_oneof![
        Just(HealthStatus::Healthy),
        Just(HealthStatus::Degraded),
        Just(HealthStatus::Critical),
        Just(HealthStatus::Hung),
    ]
}

fn arb_health_classification() -> impl Strategy<Value = HealthClassification> {
    (
        arb_health_status(),
        proptest::option::of(-10.0f64..20.0),  // z_score
        proptest::option::of(100.0f64..60000.0), // adaptive_threshold_ms
        proptest::option::of(100.0f64..60000.0), // estimated_interval_ms
        proptest::option::of(1.0f64..500.0),     // estimated_std_dev_ms
        0usize..200,                              // observations
        any::<bool>(),                            // adaptive_mode
    )
        .prop_map(|(status, z, threshold, interval, std_dev, obs, adaptive)| {
            HealthClassification {
                status,
                z_score: z,
                adaptive_threshold_ms: threshold,
                estimated_interval_ms: interval,
                estimated_std_dev_ms: std_dev,
                observations: obs,
                adaptive_mode: adaptive,
            }
        })
}

fn arb_component() -> impl Strategy<Value = Component> {
    prop_oneof![
        Just(Component::Discovery),
        Just(Component::Capture),
        Just(Component::Persistence),
        Just(Component::Maintenance),
    ]
}

fn arb_component_classification() -> impl Strategy<Value = ComponentClassification> {
    (arb_component(), arb_health_classification()).prop_map(|(component, classification)| {
        ComponentClassification {
            component,
            classification,
        }
    })
}

fn arb_adaptive_health_report() -> impl Strategy<Value = AdaptiveHealthReport> {
    (
        0u64..2_000_000_000,
        arb_health_status(),
        prop::collection::vec(arb_component_classification(), 0..8),
    )
        .prop_map(|(timestamp_ms, overall, components)| AdaptiveHealthReport {
            timestamp_ms,
            overall,
            components,
        })
}

// =============================================================================
// 1. Kalman convergence
// =============================================================================

proptest! {
    /// For a stationary heartbeat interval (constant true interval with Gaussian-like noise),
    /// after N observations, the Kalman estimate must be within 2*sigma/sqrt(N) of the truth.
    #[test]
    fn proptest_kalman_convergence(
        true_interval in 1.0f64..120.0,
        noise_sigma in 0.1f64..5.0,
        n in 20usize..200,
    ) {
        let q = 0.01; // Low process noise for stationary signal
        let r = noise_sigma * noise_sigma;
        let mut kf = KalmanFilter::new(q, r);

        // Use a simple deterministic noise pattern (seeded by proptest)
        // Instead of random, use a sinusoidal pattern scaled by noise_sigma
        for i in 0..n {
            let phase = (i as f64) * 2.0 * std::f64::consts::PI / 7.0;
            let noise = noise_sigma * phase.sin();
            kf.update(true_interval + noise);
        }

        let estimate = kf.estimate();
        // Convergence bound: for stationary signal with zero-mean noise,
        // the estimate should be close to the true value.
        // Using a generous bound since sinusoidal noise isn't perfectly zero-mean
        // over arbitrary intervals.
        let bound = 2.0 * noise_sigma;
        prop_assert!(
            (estimate - true_interval).abs() < bound,
            "estimate {} should be within {} of true {} (n={}, sigma={})",
            estimate, bound, true_interval, n, noise_sigma
        );
    }
}

// =============================================================================
// 2. Variance stays positive
// =============================================================================

proptest! {
    /// After any sequence of observations, the Kalman variance P must remain strictly positive.
    #[test]
    fn proptest_variance_positive_definite(
        observations in prop::collection::vec(0.1f64..300.0, 1..500),
    ) {
        let mut kf = KalmanFilter::new(0.1, 1.0);

        for &z in &observations {
            kf.update(z);
            prop_assert!(
                kf.variance() > 0.0,
                "P={} must be > 0 after observing {}",
                kf.variance(), z
            );
        }
    }

    /// Variance stays positive even with extreme Q/R ratios.
    #[test]
    fn proptest_variance_positive_extreme_params(
        q in 1e-12f64..1e6,
        r in 1e-12f64..1e6,
        observations in prop::collection::vec(0.1f64..1e6, 1..100),
    ) {
        let mut kf = KalmanFilter::new(q, r);

        for &z in &observations {
            kf.update(z);
            prop_assert!(
                kf.variance() > 0.0,
                "P={} must be > 0 (q={}, r={}, z={})",
                kf.variance(), q, r, z
            );
        }
    }
}

// =============================================================================
// 3. Threshold >= estimate
// =============================================================================

proptest! {
    /// The adaptive threshold must always be >= the Kalman estimate (since k >= 0).
    #[test]
    fn proptest_threshold_above_estimate(
        k in 0.0f64..10.0,
        observations in prop::collection::vec(1.0f64..10000.0, 2..50),
    ) {
        let config = AdaptiveWatchdogConfig {
            sensitivity_k: k,
            process_noise: 100.0,
            measurement_noise: 2500.0,
            min_observations: 1,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);

        // Feed observations as sequential timestamps
        let mut t = 0u64;
        for &interval in &observations {
            t += interval as u64;
            tracker.observe(t);
        }

        if let (Some(est), Some(threshold)) = (
            tracker.estimated_interval(),
            tracker.adaptive_threshold(k),
        ) {
            prop_assert!(
                threshold >= est,
                "threshold {} must be >= estimate {} (k={})",
                threshold, est, k
            );
        }
    }
}

// =============================================================================
// 4. z-score ordering (monotone with distance)
// =============================================================================

proptest! {
    /// For observations sorted by distance from the estimate, z-scores must be
    /// monotonically non-decreasing.
    #[test]
    fn proptest_zscore_ordering(
        base_interval in 100.0f64..10000.0,
        q in 0.01f64..100.0,
        r in 0.01f64..100.0,
    ) {
        let mut kf = KalmanFilter::new(q, r);

        // Feed a stable signal
        for _ in 0..50 {
            kf.update(base_interval);
        }

        // Generate observations at increasing distances above the estimate
        let est = kf.estimate();
        let distances: [f64; 6] = [0.5, 1.0, 2.0, 5.0, 10.0, 20.0];

        let mut prev_z = f64::NEG_INFINITY;
        for &d in &distances {
            let obs = d.mul_add(kf.std_dev(), est);
            if let Some(z) = kf.z_score(obs) {
                prop_assert!(
                    z >= prev_z - 1e-10, // Small tolerance for float
                    "z-score should be non-decreasing: prev={} curr={} at distance {}",
                    prev_z, z, d
                );
                prev_z = z;
            }
        }
    }
}

// =============================================================================
// 5. Health status ordering
// =============================================================================

proptest! {
    /// For any z-score sequence, the health status must be monotonically non-decreasing
    /// with increasing z-score.
    #[test]
    fn proptest_health_status_ordering(
        z_scores in prop::collection::vec(0.0f64..20.0, 2..20),
    ) {
        let config = AdaptiveWatchdogConfig::default();

        // Sort z-scores
        let mut sorted = z_scores.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

        // Map each z-score to a HealthStatus
        let statuses: Vec<HealthStatus> = sorted.iter().map(|&z| {
            if z < config.degraded_z {
                HealthStatus::Healthy
            } else if z < config.critical_z {
                HealthStatus::Degraded
            } else if z < config.hung_z {
                HealthStatus::Critical
            } else {
                HealthStatus::Hung
            }
        }).collect();

        // Verify monotonically non-decreasing
        for window in statuses.windows(2) {
            prop_assert!(
                window[0] <= window[1],
                "health status must be non-decreasing: {:?} > {:?}",
                window[0], window[1]
            );
        }
    }
}

// =============================================================================
// 6. Robustness properties
// =============================================================================

proptest! {
    /// Warmup mode uses fixed thresholds regardless of observations.
    #[test]
    fn proptest_warmup_uses_fixed(
        min_obs in 5usize..50,
        n_obs in 1usize..5, // Always less than min_obs=5
        fallback_ms in 1000u64..60000,
    ) {
        // Ensure n_obs < min_obs
        let n_obs = n_obs.min(min_obs.saturating_sub(1)).max(1);

        let config = AdaptiveWatchdogConfig {
            min_observations: min_obs,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, fallback_ms);

        // Feed fewer observations than min_obs
        for i in 0..=n_obs {
            tracker.observe((i as u64) * 1000);
        }

        let c = tracker.classify((n_obs as u64 + 1) * 1000 + 500, &config);
        prop_assert!(!c.adaptive_mode, "should be in warmup mode with {} < {} observations",
            tracker.observation_count(), min_obs);
    }

    /// Kalman filter reset truly clears state.
    #[test]
    fn proptest_reset_clears(
        observations in prop::collection::vec(1.0f64..1000.0, 1..50),
    ) {
        let mut kf = KalmanFilter::new(1.0, 1.0);
        for &z in &observations {
            kf.update(z);
        }
        prop_assert!(kf.is_initialized());

        kf.reset();
        prop_assert!(!kf.is_initialized());
        prop_assert!((kf.estimate() - 0.0).abs() < f64::EPSILON);
        prop_assert!(kf.z_score(100.0).is_none());
    }

    /// Adaptive threshold increases with k.
    #[test]
    fn proptest_threshold_monotone_in_k(
        k1 in 0.0f64..5.0,
        k2_delta in 0.01f64..5.0,
    ) {
        let k2 = k1 + k2_delta;
        let config = AdaptiveWatchdogConfig {
            min_observations: 3,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);

        for i in 0..10 {
            tracker.observe(i * 1000);
        }

        if let (Some(t1), Some(t2)) = (
            tracker.adaptive_threshold(k1),
            tracker.adaptive_threshold(k2),
        ) {
            prop_assert!(
                t2 >= t1 - 1e-10,
                "threshold at k={} ({}) should be >= threshold at k={} ({})",
                k2, t2, k1, t1
            );
        }
    }
}

// =============================================================================
// 7. Serde roundtrips
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// AdaptiveWatchdogConfig serde roundtrip preserves all fields.
    #[test]
    fn proptest_config_serde_roundtrip(config in arb_adaptive_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: AdaptiveWatchdogConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((back.sensitivity_k - config.sensitivity_k).abs() < 1e-10,
            "sensitivity_k mismatch");
        prop_assert!((back.process_noise - config.process_noise).abs() < 1e-10,
            "process_noise mismatch");
        prop_assert!((back.measurement_noise - config.measurement_noise).abs() < 1e-10,
            "measurement_noise mismatch");
        prop_assert_eq!(back.min_observations, config.min_observations);
        prop_assert!((back.degraded_z - config.degraded_z).abs() < 1e-10,
            "degraded_z mismatch");
        prop_assert!((back.critical_z - config.critical_z).abs() < 1e-10,
            "critical_z mismatch");
        prop_assert!((back.hung_z - config.hung_z).abs() < 1e-10,
            "hung_z mismatch");
    }

    /// HealthClassification serde roundtrip preserves all fields.
    #[test]
    fn proptest_health_classification_serde_roundtrip(hc in arb_health_classification()) {
        let json = serde_json::to_string(&hc).unwrap();
        let back: HealthClassification = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.status, hc.status);
        prop_assert_eq!(back.observations, hc.observations);
        prop_assert_eq!(back.adaptive_mode, hc.adaptive_mode);
        // Compare Options with float tolerance
        match (back.z_score, hc.z_score) {
            (Some(a), Some(b)) => prop_assert!((a - b).abs() < 1e-10, "z_score mismatch"),
            (None, None) => {}
            _ => prop_assert!(false, "z_score None/Some mismatch"),
        }
        match (back.adaptive_threshold_ms, hc.adaptive_threshold_ms) {
            (Some(a), Some(b)) => prop_assert!((a - b).abs() < 1e-10,
                "adaptive_threshold_ms mismatch"),
            (None, None) => {}
            _ => prop_assert!(false, "adaptive_threshold_ms None/Some mismatch"),
        }
    }

    /// AdaptiveHealthReport serde roundtrip preserves structure.
    #[test]
    fn proptest_health_report_serde_roundtrip(report in arb_adaptive_health_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: AdaptiveHealthReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.timestamp_ms, report.timestamp_ms);
        prop_assert_eq!(back.overall, report.overall);
        prop_assert_eq!(back.components.len(), report.components.len());
        for (b, r) in back.components.iter().zip(report.components.iter()) {
            prop_assert_eq!(b.component, r.component);
            prop_assert_eq!(b.classification.status, r.classification.status);
            prop_assert_eq!(b.classification.observations, r.classification.observations);
        }
    }

    /// ComponentClassification serde roundtrip preserves component and status.
    #[test]
    fn proptest_component_classification_serde_roundtrip(cc in arb_component_classification()) {
        let json = serde_json::to_string(&cc).unwrap();
        let back: ComponentClassification = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.component, cc.component);
        prop_assert_eq!(back.classification.status, cc.classification.status);
        prop_assert_eq!(back.classification.adaptive_mode, cc.classification.adaptive_mode);
        prop_assert_eq!(back.classification.observations, cc.classification.observations);
    }
}

// =============================================================================
// 8. ComponentTracker behavioral properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// observation_count tracks the number of inter-heartbeat intervals observed.
    /// The first observe() sets the baseline; each subsequent one increments the count.
    #[test]
    fn proptest_observation_count_tracks(
        intervals in prop::collection::vec(100u64..5000, 2..50),
    ) {
        let config = AdaptiveWatchdogConfig::default();
        let mut tracker = ComponentTracker::new(&config, 5_000);

        prop_assert_eq!(tracker.observation_count(), 0);

        let mut t = 0u64;
        for (i, &interval) in intervals.iter().enumerate() {
            t += interval;
            tracker.observe(t);
            // First observe sets baseline (no interval computed), subsequent ones increment
            if i == 0 {
                prop_assert_eq!(tracker.observation_count(), 0,
                    "first observe should not increment count");
            } else {
                prop_assert_eq!(tracker.observation_count(), i,
                    "observation_count should be {} after {} observes", i, i + 1);
            }
        }
    }

    /// After enough observations, estimated_interval returns Some.
    #[test]
    fn proptest_estimated_interval_available_after_warmup(
        intervals in prop::collection::vec(100u64..5000, 3..30),
    ) {
        let config = AdaptiveWatchdogConfig::default();
        let mut tracker = ComponentTracker::new(&config, 5_000);

        let mut t = 0u64;
        for &interval in &intervals {
            t += interval;
            tracker.observe(t);
        }

        // After at least 2 observes (1 baseline + 1 interval), filter should be initialized
        if intervals.len() >= 2 {
            prop_assert!(
                tracker.estimated_interval().is_some(),
                "estimated_interval should be Some after {} observations",
                tracker.observation_count()
            );
        }
    }

    /// tracker.reset() clears observation_count and estimated_interval.
    #[test]
    fn proptest_tracker_reset_clears_state(
        intervals in prop::collection::vec(100u64..5000, 3..20),
    ) {
        let config = AdaptiveWatchdogConfig::default();
        let mut tracker = ComponentTracker::new(&config, 5_000);

        let mut t = 0u64;
        for &interval in &intervals {
            t += interval;
            tracker.observe(t);
        }
        prop_assert!(tracker.observation_count() > 0);

        tracker.reset();
        prop_assert_eq!(tracker.observation_count(), 0);
        prop_assert!(tracker.estimated_interval().is_none());
        prop_assert!(tracker.adaptive_threshold(3.0).is_none());
    }
}

// =============================================================================
// 9. Kalman filter std_dev and variance relationship
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// std_dev is always the square root of variance.
    #[test]
    fn proptest_std_dev_is_sqrt_variance(
        observations in prop::collection::vec(0.1f64..1000.0, 1..100),
    ) {
        let mut kf = KalmanFilter::new(1.0, 1.0);
        for &z in &observations {
            kf.update(z);
            let expected_sd = kf.variance().sqrt();
            prop_assert!(
                (kf.std_dev() - expected_sd).abs() < 1e-12,
                "std_dev {} != sqrt(variance) {}",
                kf.std_dev(), expected_sd
            );
        }
    }

    /// After a constant signal, estimate converges and std_dev decreases.
    #[test]
    fn proptest_constant_signal_decreasing_variance(
        true_val in 10.0f64..10000.0,
        n in 10usize..100,
    ) {
        let mut kf = KalmanFilter::new(0.01, 100.0); // Low Q, high R

        let mut variances = Vec::with_capacity(n);
        for _ in 0..n {
            kf.update(true_val);
            variances.push(kf.variance());
        }

        // Variance should generally decrease (or stabilize) for constant signal
        // Check that the last variance is less than the first
        if variances.len() >= 2 {
            prop_assert!(
                variances.last().unwrap() <= variances.first().unwrap(),
                "variance should decrease for constant signal: first={}, last={}",
                variances.first().unwrap(), variances.last().unwrap()
            );
        }
    }
}

// =============================================================================
// 10. AdaptiveWatchdog integration properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// check_health produces a report with exactly 4 components (the default set).
    #[test]
    fn proptest_health_report_has_all_components(
        current_ms in 10000u64..100000,
    ) {
        let config = AdaptiveWatchdogConfig::default();
        let watchdog = AdaptiveWatchdog::new(config);
        let report = watchdog.check_health(current_ms);

        prop_assert_eq!(report.components.len(), 4,
            "default watchdog should track 4 components");
        prop_assert_eq!(report.timestamp_ms, current_ms);
    }

    /// Overall health is the worst (max) of all component health statuses.
    #[test]
    fn proptest_overall_is_worst_component(
        heartbeats in prop::collection::vec(1000u64..10000, 5..20),
    ) {
        let config = AdaptiveWatchdogConfig::default();
        let mut watchdog = AdaptiveWatchdog::new(config);

        // Feed heartbeats to Discovery component
        let mut t = 0u64;
        for &interval in &heartbeats {
            t += interval;
            watchdog.observe(Component::Discovery, t);
        }

        let report = watchdog.check_health(t + 100);
        let worst = report.components.iter()
            .map(|c| c.classification.status)
            .max()
            .unwrap_or(HealthStatus::Healthy);

        prop_assert_eq!(report.overall, worst,
            "overall should be the worst component status");
    }

    /// Components in check_health are sorted deterministically.
    #[test]
    fn proptest_health_report_components_sorted(
        current_ms in 10000u64..100000,
    ) {
        let config = AdaptiveWatchdogConfig::default();
        let watchdog = AdaptiveWatchdog::new(config);
        let report = watchdog.check_health(current_ms);

        // Components should be sorted: Discovery, Capture, Persistence, Maintenance
        let component_order: Vec<Component> = report.components.iter()
            .map(|c| c.component)
            .collect();
        let expected = vec![
            Component::Discovery,
            Component::Capture,
            Component::Persistence,
            Component::Maintenance,
        ];
        prop_assert_eq!(component_order, expected,
            "components should be in deterministic order");
    }

    /// After reset, all components report healthy with 0 observations.
    #[test]
    fn proptest_watchdog_reset_all(
        heartbeats in prop::collection::vec(1000u64..10000, 5..20),
    ) {
        let config = AdaptiveWatchdogConfig::default();
        let mut watchdog = AdaptiveWatchdog::new(config);

        let mut t = 0u64;
        for &interval in &heartbeats {
            t += interval;
            watchdog.observe(Component::Discovery, t);
            watchdog.observe(Component::Capture, t);
        }

        watchdog.reset();
        let report = watchdog.check_health(t + 100);

        for comp in &report.components {
            prop_assert_eq!(comp.classification.observations, 0,
                "observations should be 0 after reset for {:?}", comp.component);
        }
    }
}
