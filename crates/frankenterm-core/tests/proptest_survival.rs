//! Property-based tests for survival/hazard model invariants.
//!
//! Bead: wa-wiwt
//!
//! Validates:
//! 1. Baseline hazard non-negative: h₀(t) ≥ 0 for all t, k, λ
//! 2. Baseline hazard zero for t ≤ 0: h₀(t) = 0 when t ≤ 0
//! 3. Increasing hazard for k > 1: h₀(t₁) < h₀(t₂) when t₁ < t₂
//! 4. Constant hazard for k = 1: h₀(t) = 1/λ for all t > 0
//! 5. Survival probability in [0, 1]: S(t) ∈ [0, 1]
//! 6. S + F = 1: survival_probability + failure_probability = 1
//! 7. Survival decreases with time: S(t₁) ≥ S(t₂) when t₁ < t₂
//! 8. Cumulative hazard non-negative: H(t) ≥ 0
//! 9. Cumulative hazard monotonic: H(t₁) ≤ H(t₂) when t₁ < t₂
//! 10. Positive covariates increase hazard: exp(β·X) > 1 when β·X > 0
//! 11. Warmup behavior: hazard=0, survival=1 during warmup
//! 12. Observation count tracks: n observations → count = n
//! 13. Action thresholds ordered: None < IncreaseSnapshot < Immediate < Alert
//! 14. Covariate dot product: X·β consistency

use proptest::prelude::*;

use frankenterm_core::survival::{
    Covariates, HazardAction, Observation, SurvivalConfig, SurvivalModel, WeibullParams,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_shape() -> impl Strategy<Value = f64> {
    0.1_f64..10.0
}

fn arb_scale() -> impl Strategy<Value = f64> {
    1.0_f64..10_000.0
}

fn arb_time() -> impl Strategy<Value = f64> {
    0.001_f64..1000.0
}

fn arb_beta() -> impl Strategy<Value = [f64; Covariates::COUNT]> {
    proptest::array::uniform5(-2.0_f64..2.0)
}

fn arb_params() -> impl Strategy<Value = WeibullParams> {
    (arb_shape(), arb_scale(), arb_beta()).prop_map(|(shape, scale, beta)| WeibullParams {
        shape,
        scale,
        beta,
    })
}

fn arb_covariates() -> impl Strategy<Value = Covariates> {
    (
        0.0_f64..32.0,  // rss_gb
        0.0_f64..100.0, // pane_count
        0.0_f64..10.0,  // output_rate_mbps
        0.0_f64..720.0, // uptime_hours
        0.0_f64..10.0,  // conn_error_rate
    )
        .prop_map(|(rss, panes, output, uptime, errors)| Covariates {
            rss_gb: rss,
            pane_count: panes,
            output_rate_mbps: output,
            uptime_hours: uptime,
            conn_error_rate: errors,
        })
}

// =============================================================================
// Property: Baseline hazard non-negative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn baseline_hazard_nonnegative(
        params in arb_params(),
        t in -10.0_f64..1000.0,
    ) {
        let h = params.baseline_hazard(t);
        prop_assert!(h >= 0.0,
            "baseline hazard should be >= 0, got {} for t={}, k={}, lambda={}",
            h, t, params.shape, params.scale);
        prop_assert!(h.is_finite() || h == 0.0,
            "baseline hazard should be finite, got {}", h);
    }
}

// =============================================================================
// Property: Baseline hazard zero for t ≤ 0
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn baseline_hazard_zero_for_nonpositive_time(
        params in arb_params(),
        t in -100.0_f64..=0.0,
    ) {
        let h = params.baseline_hazard(t);
        prop_assert!((h - 0.0).abs() < 1e-10,
            "h₀(t) should be 0 for t={}, got {}", t, h);
    }
}

// =============================================================================
// Property: Increasing hazard for k > 1
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn baseline_hazard_increasing_for_k_gt_1(
        shape in 1.01_f64..10.0,
        scale in arb_scale(),
        t1 in 0.1_f64..100.0,
        t_delta in 0.1_f64..100.0,
    ) {
        let t2 = t1 + t_delta;
        let params = WeibullParams {
            shape,
            scale,
            beta: [0.0; Covariates::COUNT],
        };
        let h1 = params.baseline_hazard(t1);
        let h2 = params.baseline_hazard(t2);

        prop_assert!(h2 >= h1 - 1e-10,
            "for k={} > 1, h₀({}) = {} should be >= h₀({}) = {}",
            shape, t2, h2, t1, h1);
    }
}

// =============================================================================
// Property: Constant hazard for k = 1 (exponential)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn baseline_hazard_constant_for_k_1(
        scale in arb_scale(),
        t1 in 0.1_f64..500.0,
        t2 in 0.1_f64..500.0,
    ) {
        let params = WeibullParams {
            shape: 1.0,
            scale,
            beta: [0.0; Covariates::COUNT],
        };
        let h1 = params.baseline_hazard(t1);
        let h2 = params.baseline_hazard(t2);

        prop_assert!((h1 - h2).abs() < 1e-10,
            "for k=1, h₀({}) = {} should equal h₀({}) = {}",
            t1, h1, t2, h2);

        // h₀(t) = 1/λ for k=1
        let expected = 1.0 / scale;
        prop_assert!((h1 - expected).abs() < 1e-10,
            "for k=1, h₀ should be 1/λ = {}, got {}", expected, h1);
    }
}

// =============================================================================
// Property: Survival probability in [0, 1]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn survival_in_unit_interval(
        params in arb_params(),
        t in arb_time(),
        cov in arb_covariates(),
    ) {
        let s = params.survival_probability(t, &cov);
        prop_assert!(s >= 0.0, "survival should be >= 0, got {}", s);
        prop_assert!(s <= 1.0, "survival should be <= 1, got {}", s);
    }
}

// =============================================================================
// Property: S + F = 1
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn survival_plus_failure_equals_one(
        params in arb_params(),
        t in arb_time(),
        cov in arb_covariates(),
    ) {
        let s = params.survival_probability(t, &cov);
        let f = params.failure_probability(t, &cov);
        prop_assert!((s + f - 1.0).abs() < 1e-10,
            "S + F should equal 1.0, got S={} + F={} = {}", s, f, s + f);
    }
}

// =============================================================================
// Property: Survival decreases with time
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn survival_decreases_with_time(
        params in arb_params(),
        cov in arb_covariates(),
        t1 in 0.01_f64..100.0,
        t_delta in 0.01_f64..100.0,
    ) {
        let t2 = t1 + t_delta;
        let s1 = params.survival_probability(t1, &cov);
        let s2 = params.survival_probability(t2, &cov);

        prop_assert!(s2 <= s1 + 1e-10,
            "S({}) = {} should be <= S({}) = {}",
            t2, s2, t1, s1);
    }
}

// =============================================================================
// Property: Cumulative hazard non-negative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn cumulative_hazard_nonnegative(
        params in arb_params(),
        t in -10.0_f64..1000.0,
        cov in arb_covariates(),
    ) {
        let h_cum = params.cumulative_hazard(t, &cov);
        prop_assert!(h_cum >= 0.0,
            "cumulative hazard should be >= 0, got {}", h_cum);
    }
}

// =============================================================================
// Property: Cumulative hazard monotonic
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn cumulative_hazard_monotonic(
        params in arb_params(),
        cov in arb_covariates(),
        t1 in 0.01_f64..100.0,
        t_delta in 0.01_f64..100.0,
    ) {
        let t2 = t1 + t_delta;
        let h1 = params.cumulative_hazard(t1, &cov);
        let h2 = params.cumulative_hazard(t2, &cov);

        prop_assert!(h2 >= h1 - 1e-10,
            "H({}) = {} should be >= H({}) = {}",
            t2, h2, t1, h1);
    }
}

// =============================================================================
// Property: Positive covariates increase hazard
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn positive_covariate_effect_increases_hazard(
        shape in arb_shape(),
        scale in arb_scale(),
        t in 1.0_f64..100.0,
    ) {
        let beta_pos = [0.5, 0.1, 0.2, 0.05, 0.3];
        let params = WeibullParams {
            shape,
            scale,
            beta: beta_pos,
        };

        let zero_cov = Covariates::default();
        let risky_cov = Covariates {
            rss_gb: 10.0,
            pane_count: 50.0,
            output_rate_mbps: 5.0,
            uptime_hours: 100.0,
            conn_error_rate: 2.0,
        };

        let h_zero = params.hazard(t, &zero_cov);
        let h_risky = params.hazard(t, &risky_cov);

        // With positive betas and positive covariates, exp(β·X) > 1.
        prop_assert!(h_risky >= h_zero - 1e-10,
            "risky hazard ({}) should be >= baseline ({})", h_risky, h_zero);
    }
}

// =============================================================================
// Property: Warmup behavior
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn warmup_returns_safe_defaults(
        cov in arb_covariates(),
        t in arb_time(),
        warmup_obs in 5_usize..20,
    ) {
        let config = SurvivalConfig {
            warmup_observations: warmup_obs,
            ..SurvivalConfig::default()
        };
        let model = SurvivalModel::new(config);

        // During warmup: hazard = 0, survival = 1.
        prop_assert!((model.hazard_rate(t, &cov) - 0.0).abs() < 1e-10,
            "warmup hazard should be 0");
        prop_assert!((model.survival_probability(t, &cov) - 1.0).abs() < 1e-10,
            "warmup survival should be 1");
        prop_assert!(model.in_warmup());
    }
}

// =============================================================================
// Property: Observation count tracks correctly
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn observation_count_tracks(
        n in 1_usize..30,
    ) {
        let config = SurvivalConfig {
            warmup_observations: 100, // Keep in warmup to avoid parameter updates
            ..SurvivalConfig::default()
        };
        let model = SurvivalModel::new(config);

        for i in 0..n {
            model.observe(Observation {
                time: (i + 1) as f64,
                event_observed: false,
                covariates: Covariates::default(),
                timestamp_secs: 1_000_000 + i as u64,
            });
        }

        prop_assert_eq!(model.observation_count(), n as u64,
            "observation count should be {}", n);
    }
}

// =============================================================================
// Property: Action thresholds ordered
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn action_thresholds_ordered(
        low in 0.0_f64..0.499,
        mid in 0.5_f64..0.799,
        high in 0.8_f64..0.949,
        critical in 0.95_f64..10.0,
    ) {
        // Test action ordering by checking the enum ordering.
        prop_assert!(HazardAction::None < HazardAction::IncreaseSnapshotFrequency);
        prop_assert!(HazardAction::IncreaseSnapshotFrequency < HazardAction::ImmediateSnapshot);
        prop_assert!(HazardAction::ImmediateSnapshot < HazardAction::AlertAndPrepareRestart);

        // Suppress unused variable warnings.
        let _ = (low, mid, high, critical);
    }
}

// =============================================================================
// Property: Covariate dot product correct
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn covariate_dot_product(
        cov in arb_covariates(),
        beta in arb_beta(),
    ) {
        let result = cov.dot(&beta);
        let arr = cov.to_array();
        let expected: f64 = arr.iter().zip(beta.iter()).map(|(x, b)| x * b).sum();

        prop_assert!((result - expected).abs() < 1e-10,
            "dot product should be {}, got {}", expected, result);
    }
}

// =============================================================================
// Property: Full hazard rate non-negative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn full_hazard_nonnegative(
        params in arb_params(),
        t in arb_time(),
        cov in arb_covariates(),
    ) {
        let h = params.hazard(t, &cov);
        prop_assert!(h >= 0.0,
            "hazard should be >= 0, got {} for t={}", h, t);
    }
}

// =============================================================================
// Property: Log-likelihood well-defined for moderate inputs
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn log_likelihood_finite(
        shape in arb_shape(),
        scale in arb_scale(),
        // Small betas to avoid exp(β·X) overflow (β·X stays well under 700).
        beta in proptest::array::uniform5(-0.1_f64..0.1),
        t in 0.1_f64..100.0,
        event in any::<bool>(),
        cov in arb_covariates(),
    ) {
        let params = WeibullParams { shape, scale, beta };
        let obs = Observation {
            time: t,
            event_observed: event,
            covariates: cov,
            timestamp_secs: 1_000_000,
        };
        let ll = params.log_likelihood_single(&obs);

        // With constrained betas, log-likelihood should be finite or -inf, never NaN or +inf.
        prop_assert!(!ll.is_nan(),
            "log-likelihood should not be NaN for t={}, event={}", t, event);
        prop_assert!(ll <= 0.0 || ll.is_finite(),
            "log-likelihood should not be +inf, got {}", ll);
    }
}

// =============================================================================
// Property: Failure probability in [0, 1]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn failure_probability_bounded(
        params in arb_params(),
        t in arb_time(),
        cov in arb_covariates(),
    ) {
        let f = params.failure_probability(t, &cov);
        prop_assert!(f >= 0.0, "failure prob should be >= 0, got {}", f);
        prop_assert!(f <= 1.0, "failure prob should be <= 1, got {}", f);
    }
}
