//! Property-based tests for the PAC-Bayes `GaussianPosterior` math in
//! [`frankenterm_core::aegis_backpressure`].
//!
//! The higher-level PacBayes engine is covered by
//! `proptest_pac_bayes_telemetry.rs`, but the underlying Gaussian
//! posterior's mathematical invariants were untested. These pin them:
//!
//! 1. **KL self-divergence is zero** — KL(p ‖ p) = 0.
//! 2. **KL non-negativity** — KL(p ‖ q) ≥ 0 (Gibbs' inequality).
//! 3. **Conjugate update shrinks variance** — incorporating an
//!    observation strictly reduces posterior variance.
//! 4. **Update mean is convex** — the new mean lies between the old mean
//!    and the observation (precision-weighted average).
//! 5. **Confidence bounds bracket the mean and are symmetric** —
//!    lower ≤ mean ≤ upper and (lower+upper)/2 = mean.
//! 6. **std_dev = sqrt(variance)**.

use proptest::prelude::*;

use frankenterm_core::aegis_backpressure::{GaussianPosterior, PaneSnapshot};

fn arb_mean() -> impl Strategy<Value = f64> {
    -1_000.0_f64..1_000.0
}

/// Variance bounded away from the 1e-12 floor and from huge values so the
/// conjugate-update variance decrease is well above the float-noise floor.
fn arb_variance() -> impl Strategy<Value = f64> {
    0.1_f64..100.0
}

fn arb_delta() -> impl Strategy<Value = f64> {
    0.01_f64..0.5
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// KL(p ‖ p) is zero.
    #[test]
    fn kl_self_divergence_is_zero(mean in arb_mean(), variance in arb_variance()) {
        let p = GaussianPosterior::new(mean, variance);
        let kl = p.kl_divergence(&p.clone());
        prop_assert!(kl.abs() <= 1e-9, "KL(p||p) must be ~0, got {}", kl);
    }

    /// KL divergence between any two Gaussians is non-negative.
    #[test]
    fn kl_divergence_is_non_negative(
        m1 in arb_mean(), v1 in arb_variance(),
        m2 in arb_mean(), v2 in arb_variance(),
    ) {
        let p = GaussianPosterior::new(m1, v1);
        let q = GaussianPosterior::new(m2, v2);
        let kl = p.kl_divergence(&q);
        prop_assert!(kl >= -1e-9, "KL must be >= 0, got {}", kl);
    }

    /// A conjugate Bayesian update strictly reduces posterior variance.
    #[test]
    fn update_shrinks_variance(
        mean in arb_mean(), variance in arb_variance(),
        obs in arb_mean(), obs_var in arb_variance(),
    ) {
        let mut p = GaussianPosterior::new(mean, variance);
        let prior_var = p.variance;
        p.update(obs, obs_var);
        prop_assert!(p.variance < prior_var,
            "update must shrink variance: {} -> {}", prior_var, p.variance);
    }

    /// The updated mean is a convex combination of the prior mean and the
    /// observation, so it lies within their range.
    #[test]
    fn update_mean_is_convex(
        mean in arb_mean(), variance in arb_variance(),
        obs in arb_mean(), obs_var in arb_variance(),
    ) {
        let mut p = GaussianPosterior::new(mean, variance);
        let lo = mean.min(obs);
        let hi = mean.max(obs);
        p.update(obs, obs_var);
        prop_assert!(p.mean >= lo - 1e-9 && p.mean <= hi + 1e-9,
            "updated mean {} must lie in [{}, {}]", p.mean, lo, hi);
    }

    /// Confidence bounds bracket the mean and are symmetric about it.
    #[test]
    fn bounds_bracket_and_are_symmetric(
        mean in arb_mean(), variance in arb_variance(), delta in arb_delta(),
    ) {
        let p = GaussianPosterior::new(mean, variance);
        let upper = p.upper_bound(delta);
        let lower = p.lower_bound(delta);
        prop_assert!(lower <= p.mean + 1e-9, "lower {} must be <= mean {}", lower, p.mean);
        prop_assert!(upper >= p.mean - 1e-9, "upper {} must be >= mean {}", upper, p.mean);
        let mid = (upper + lower) / 2.0;
        prop_assert!((mid - p.mean).abs() <= 1e-6 * p.mean.abs().max(1.0),
            "(upper+lower)/2 {} must equal mean {}", mid, p.mean);
    }

    /// std_dev is the square root of the variance.
    #[test]
    fn std_dev_is_sqrt_variance(mean in arb_mean(), variance in arb_variance()) {
        let p = GaussianPosterior::new(mean, variance);
        let sd = p.std_dev();
        prop_assert!((sd * sd - p.variance).abs() <= 1e-9 * p.variance.max(1.0),
            "std_dev^2 {} must equal variance {}", sd * sd, p.variance);
    }

    /// PaneSnapshot (per-pane PAC-Bayes telemetry DTO) serde round-trips.
    /// All-finite fields keep the JSON round-trip exact.
    #[test]
    fn pane_snapshot_serde_roundtrip(
        pane_id in any::<u64>(),
        observations in any::<usize>(),
        frame_drops in any::<usize>(),
        drop_rate in 0.0_f64..1.0,
        smoothed_ratio in 0.0_f64..1.0,
        threshold_mean in -1e6_f64..1e6,
        threshold_variance in 0.0_f64..1e6,
        throttled in any::<bool>(),
    ) {
        let snap = PaneSnapshot {
            pane_id,
            observations,
            frame_drops,
            drop_rate,
            smoothed_ratio,
            threshold_mean,
            threshold_variance,
            throttled,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: PaneSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.pane_id, back.pane_id);
        prop_assert_eq!(snap.observations, back.observations);
        prop_assert_eq!(snap.frame_drops, back.frame_drops);
        prop_assert_eq!(snap.throttled, back.throttled);
        prop_assert!((snap.drop_rate - back.drop_rate).abs() < 1e-12);
        prop_assert!((snap.threshold_mean - back.threshold_mean).abs() < 1e-9);
        prop_assert!((snap.threshold_variance - back.threshold_variance).abs() < 1e-9);
    }
}
