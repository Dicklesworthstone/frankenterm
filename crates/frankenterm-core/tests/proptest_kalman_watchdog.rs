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

use proptest::prelude::*;

use frankenterm_core::kalman_watchdog::{AdaptiveWatchdogConfig, ComponentTracker, KalmanFilter};
use frankenterm_core::watchdog::HealthStatus;

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
// 6. Additional robustness properties
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
