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
    ActivityProfile, Covariates, HazardAction, HazardForecastPoint, HazardReport, Observation,
    RestartMode, RestartRecommendation, RestartScheduler, RestartSchedulerConfig,
    RestartScoreBreakdown, RiskFactor, SurvivalConfig, SurvivalModel, WeibullParams,
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

// =============================================================================
// NEW: WeibullParams Default has valid fields
// =============================================================================

proptest! {
    #[test]
    fn weibull_params_default_valid(_dummy in 0..1u8) {
        let p = WeibullParams::default();
        prop_assert!(p.shape > 0.0, "default shape should be positive");
        prop_assert!(p.scale > 0.0, "default scale should be positive");
        prop_assert_eq!(p.beta.len(), Covariates::COUNT);
    }
}

// =============================================================================
// NEW: WeibullParams Clone preserves all fields
// =============================================================================

proptest! {
    #[test]
    fn weibull_params_clone_preserves(params in arb_params()) {
        let cloned = params.clone();
        prop_assert_eq!(params.shape.to_bits(), cloned.shape.to_bits());
        prop_assert_eq!(params.scale.to_bits(), cloned.scale.to_bits());
        for i in 0..Covariates::COUNT {
            prop_assert_eq!(params.beta[i].to_bits(), cloned.beta[i].to_bits());
        }
    }
}

// =============================================================================
// NEW: WeibullParams serde roundtrip
// =============================================================================

proptest! {
    #[test]
    fn weibull_params_serde_roundtrip(params in arb_params()) {
        let json = serde_json::to_string(&params).unwrap();
        let back: WeibullParams = serde_json::from_str(&json).unwrap();
        prop_assert!((params.shape - back.shape).abs() < 1e-10,
            "shape mismatch: {} vs {}", params.shape, back.shape);
        prop_assert!((params.scale - back.scale).abs() < 1e-10,
            "scale mismatch: {} vs {}", params.scale, back.scale);
        for i in 0..Covariates::COUNT {
            prop_assert!((params.beta[i] - back.beta[i]).abs() < 1e-10,
                "beta[{}] mismatch: {} vs {}", i, params.beta[i], back.beta[i]);
        }
    }
}

// =============================================================================
// NEW: SurvivalConfig Default has valid fields
// =============================================================================

proptest! {
    #[test]
    fn survival_config_default_valid(_dummy in 0..1u8) {
        let cfg = SurvivalConfig::default();
        prop_assert!(cfg.warmup_observations >= 1, "warmup should be >= 1");
        prop_assert!(cfg.learning_rate > 0.0, "learning_rate should be > 0");
        prop_assert!(cfg.learning_rate <= 1.0, "learning_rate should be <= 1");
        prop_assert!(cfg.max_observations >= 1, "max_observations should be >= 1");
        prop_assert!(cfg.snapshot_frequency_threshold < cfg.immediate_snapshot_threshold,
            "snapshot threshold should be < immediate threshold");
        prop_assert!(cfg.immediate_snapshot_threshold < cfg.alert_threshold,
            "immediate threshold should be < alert threshold");
    }
}

// =============================================================================
// NEW: SurvivalConfig serde roundtrip
// =============================================================================

proptest! {
    #[test]
    fn survival_config_serde_roundtrip(
        warmup in 1usize..100,
        lr in 0.001_f64..1.0,
    ) {
        let mut cfg = SurvivalConfig::default();
        cfg.warmup_observations = warmup;
        cfg.learning_rate = lr;
        let json = serde_json::to_string(&cfg).unwrap();
        let back: SurvivalConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.warmup_observations, warmup);
        prop_assert!((back.learning_rate - lr).abs() < 1e-10,
            "learning_rate mismatch: {} vs {}", lr, back.learning_rate);
    }
}

// =============================================================================
// NEW: Covariates Default is all zeros
// =============================================================================

proptest! {
    #[test]
    fn covariates_default_all_zeros(_dummy in 0..1u8) {
        let cov = Covariates::default();
        let arr = cov.to_array();
        for (i, &val) in arr.iter().enumerate() {
            prop_assert!((val - 0.0).abs() < 1e-15,
                "default covariate[{}] should be 0.0, got {}", i, val);
        }
    }
}

// =============================================================================
// NEW: Covariates::names() returns COUNT elements
// =============================================================================

proptest! {
    #[test]
    fn covariates_names_count(_dummy in 0..1u8) {
        let names = Covariates::names();
        prop_assert_eq!(names.len(), Covariates::COUNT,
            "names() should return {} elements", Covariates::COUNT);
        for name in &names {
            prop_assert!(!name.is_empty(), "covariate name should not be empty");
        }
    }
}

// =============================================================================
// NEW: Covariates::to_array length equals COUNT
// =============================================================================

proptest! {
    #[test]
    fn covariates_to_array_length(cov in arb_covariates()) {
        let arr = cov.to_array();
        prop_assert_eq!(arr.len(), Covariates::COUNT);
    }
}

// =============================================================================
// NEW: Covariates serde roundtrip
// =============================================================================

proptest! {
    #[test]
    fn covariates_serde_roundtrip(cov in arb_covariates()) {
        let json = serde_json::to_string(&cov).unwrap();
        let back: Covariates = serde_json::from_str(&json).unwrap();
        let a1 = cov.to_array();
        let a2 = back.to_array();
        for i in 0..Covariates::COUNT {
            prop_assert!((a1[i] - a2[i]).abs() < 1e-10,
                "covariate[{}] mismatch after serde: {} vs {}", i, a1[i], a2[i]);
        }
    }
}

// =============================================================================
// NEW: Observation serde roundtrip
// =============================================================================

proptest! {
    #[test]
    fn observation_serde_roundtrip(
        t in arb_time(),
        event in any::<bool>(),
        cov in arb_covariates(),
        ts in any::<u64>(),
    ) {
        let obs = Observation {
            time: t,
            event_observed: event,
            covariates: cov,
            timestamp_secs: ts,
        };
        let json = serde_json::to_string(&obs).unwrap();
        let back: Observation = serde_json::from_str(&json).unwrap();
        prop_assert!((obs.time - back.time).abs() < 1e-10,
            "time mismatch: {} vs {}", obs.time, back.time);
        prop_assert_eq!(obs.event_observed, back.event_observed);
        prop_assert_eq!(obs.timestamp_secs, back.timestamp_secs);
    }
}

// =============================================================================
// NEW: HazardAction serde snake_case roundtrip
// =============================================================================

proptest! {
    #[test]
    fn hazard_action_serde_roundtrip(
        idx in 0u8..4,
    ) {
        let action = match idx {
            0 => HazardAction::None,
            1 => HazardAction::IncreaseSnapshotFrequency,
            2 => HazardAction::ImmediateSnapshot,
            _ => HazardAction::AlertAndPrepareRestart,
        };
        let json = serde_json::to_string(&action).unwrap();
        let back: HazardAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(action, back);
        // Check snake_case serialization
        let s = json.trim_matches('"');
        prop_assert!(s.chars().all(|c| c.is_lowercase() || c == '_'),
            "serialized form should be snake_case, got {}", s);
    }
}

// =============================================================================
// NEW: HazardAction Display non-empty
// =============================================================================

proptest! {
    #[test]
    fn hazard_action_display_nonempty(idx in 0u8..4) {
        let action = match idx {
            0 => HazardAction::None,
            1 => HazardAction::IncreaseSnapshotFrequency,
            2 => HazardAction::ImmediateSnapshot,
            _ => HazardAction::AlertAndPrepareRestart,
        };
        let display = format!("{}", action);
        prop_assert!(!display.is_empty(), "Display should be non-empty");
    }
}

// =============================================================================
// NEW: HazardAction total ordering is transitive
// =============================================================================

proptest! {
    #[test]
    fn hazard_action_ordering_transitive(
        a_idx in 0u8..4,
        b_idx in 0u8..4,
        c_idx in 0u8..4,
    ) {
        let actions = [
            HazardAction::None,
            HazardAction::IncreaseSnapshotFrequency,
            HazardAction::ImmediateSnapshot,
            HazardAction::AlertAndPrepareRestart,
        ];
        let a = actions[a_idx as usize];
        let b = actions[b_idx as usize];
        let c = actions[c_idx as usize];
        if a <= b && b <= c {
            prop_assert!(a <= c, "ordering should be transitive: {:?} <= {:?} <= {:?}", a, b, c);
        }
    }
}

// =============================================================================
// NEW: SurvivalModel with_params preserves params
// =============================================================================

proptest! {
    #[test]
    fn survival_model_with_params_preserves(params in arb_params()) {
        let config = SurvivalConfig::default();
        let model = SurvivalModel::with_params(config, params.clone());
        let got = model.params();
        prop_assert_eq!(params.shape.to_bits(), got.shape.to_bits());
        prop_assert_eq!(params.scale.to_bits(), got.scale.to_bits());
    }
}

// =============================================================================
// NEW: SurvivalModel evaluate_action returns None during warmup
// =============================================================================

proptest! {
    #[test]
    fn evaluate_action_warmup_is_none(
        cov in arb_covariates(),
        t in arb_time(),
    ) {
        let config = SurvivalConfig {
            warmup_observations: 100,
            ..SurvivalConfig::default()
        };
        let model = SurvivalModel::new(config);
        let action = model.evaluate_action(t, &cov);
        prop_assert_eq!(action, HazardAction::None,
            "during warmup, action should be None, got {:?}", action);
    }
}

// =============================================================================
// NEW: Zero covariates dot product with any beta is zero
// =============================================================================

proptest! {
    #[test]
    fn zero_covariates_dot_is_zero(beta in arb_beta()) {
        let cov = Covariates::default();
        let result = cov.dot(&beta);
        prop_assert!((result - 0.0).abs() < 1e-15,
            "dot product of zero covariates should be 0, got {}", result);
    }
}

// =============================================================================
// NEW: S(t) = exp(-H(t)) relationship
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn survival_equals_exp_neg_cumulative_hazard(
        params in arb_params(),
        t in arb_time(),
        cov in arb_covariates(),
    ) {
        let s = params.survival_probability(t, &cov);
        let h_cum = params.cumulative_hazard(t, &cov);
        let expected = (-h_cum).exp();
        prop_assert!((s - expected).abs() < 1e-10,
            "S(t) = exp(-H(t)): got S={}, exp(-H)={}", s, expected);
    }
}

// =============================================================================
// NEW: Decreasing hazard for k < 1
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn baseline_hazard_decreasing_for_k_lt_1(
        shape in 0.1_f64..0.99,
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
        prop_assert!(h2 <= h1 + 1e-10,
            "for k={} < 1, h₀({}) = {} should be <= h₀({}) = {}",
            shape, t2, h2, t1, h1);
    }
}

// =============================================================================
// NEW: WeibullParams Debug contains type name
// =============================================================================

proptest! {
    #[test]
    fn weibull_params_debug_nonempty(params in arb_params()) {
        let dbg = format!("{:?}", params);
        prop_assert!(dbg.contains("WeibullParams"), "Debug should contain type name, got: {}", dbg);
    }
}

// =============================================================================
// NEW: SurvivalModel observation_count starts at zero
// =============================================================================

proptest! {
    #[test]
    fn model_observation_count_starts_zero(_dummy in 0..1u8) {
        let model = SurvivalModel::new(SurvivalConfig::default());
        prop_assert_eq!(model.observation_count(), 0);
        prop_assert!(model.in_warmup());
    }
}

// =============================================================================
// NEW: Hazard with zero beta equals baseline hazard
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn hazard_zero_beta_equals_baseline(
        shape in arb_shape(),
        scale in arb_scale(),
        t in arb_time(),
        cov in arb_covariates(),
    ) {
        let params = WeibullParams {
            shape,
            scale,
            beta: [0.0; Covariates::COUNT],
        };
        let baseline = params.baseline_hazard(t);
        let full = params.hazard(t, &cov);
        // With zero betas, exp(β·X) = exp(0) = 1, so hazard = baseline.
        prop_assert!((full - baseline).abs() < 1e-10,
            "zero-beta hazard should equal baseline: {} vs {}", full, baseline);
    }
}

// =============================================================================
// Strategies for restart scheduling types
// =============================================================================

fn arb_risk_factor() -> impl Strategy<Value = RiskFactor> {
    (
        prop_oneof![
            Just("rss_gb".to_string()),
            Just("pane_count".to_string()),
            Just("output_rate_mbps".to_string()),
            Just("uptime_hours".to_string()),
            Just("conn_error_rate".to_string()),
        ],
        0.0_f64..100.0,
        -2.0_f64..2.0,
        -5.0_f64..5.0,
        0.0_f64..1.0,
    )
        .prop_map(
            |(name, value, coefficient, contribution, risk_fraction)| RiskFactor {
                name,
                value,
                coefficient,
                contribution,
                risk_fraction,
            },
        )
}

fn arb_hazard_action() -> impl Strategy<Value = HazardAction> {
    prop_oneof![
        Just(HazardAction::None),
        Just(HazardAction::IncreaseSnapshotFrequency),
        Just(HazardAction::ImmediateSnapshot),
        Just(HazardAction::AlertAndPrepareRestart),
    ]
}

fn arb_hazard_report() -> impl Strategy<Value = HazardReport> {
    (
        any::<u64>(),
        0.0_f64..10.0,
        0.0_f64..1.0,
        0.0_f64..1.0,
        arb_hazard_action(),
        prop::collection::vec(arb_risk_factor(), 0..6),
        arb_params(),
        any::<bool>(),
        0_usize..1000,
    )
        .prop_map(
            |(ts, hazard, surv, fail, action, factors, params, warmup, obs)| HazardReport {
                timestamp_secs: ts,
                hazard_rate: hazard,
                survival_probability: surv,
                failure_probability: fail,
                action,
                risk_factors: factors,
                params,
                in_warmup: warmup,
                observation_count: obs,
            },
        )
}

fn arb_restart_mode() -> impl Strategy<Value = RestartMode> {
    prop_oneof![
        (0.0_f64..1.0).prop_map(|s| RestartMode::Automatic { min_score: s }),
        Just(RestartMode::Advisory),
        Just(RestartMode::Manual),
    ]
}

fn arb_restart_config() -> impl Strategy<Value = RestartSchedulerConfig> {
    (
        arb_restart_mode(),
        0.1_f64..2.0,
        1.0_f64..20.0,
        1.0_f64..48.0,
        60_u32..2880,
        0.01_f64..0.99,
        0.0_f64..1.0,
        any::<bool>(),
        5_u32..120,
    )
        .prop_map(
            |(
                mode,
                threshold,
                steepness,
                cooldown,
                horizon,
                alpha,
                default_act,
                snapshot,
                warning,
            )| {
                RestartSchedulerConfig {
                    mode,
                    hazard_threshold: threshold,
                    urgency_steepness: steepness,
                    cooldown_hours: cooldown,
                    schedule_horizon_minutes: horizon,
                    activity_ewma_alpha: alpha,
                    default_activity: default_act,
                    pre_restart_snapshot: snapshot,
                    advance_warning_minutes: warning,
                }
            },
        )
}

fn arb_forecast_point() -> impl Strategy<Value = HazardForecastPoint> {
    (
        0_u32..2880,
        0.0_f64..10.0,
        proptest::option::of(0.0_f64..1.0),
    )
        .prop_map(|(offset, hazard, activity)| HazardForecastPoint {
            offset_minutes: offset,
            hazard_rate: hazard,
            predicted_activity: activity,
        })
}

fn arb_score_breakdown() -> impl Strategy<Value = RestartScoreBreakdown> {
    (0.0_f64..1.0, 0.0_f64..1.0, 0.0_f64..1.0, 0.0_f64..1.0).prop_map(
        |(urgency, activity, recency, score)| RestartScoreBreakdown {
            hazard_urgency: urgency,
            activity_minimum: activity,
            recency_penalty: recency,
            score,
        },
    )
}

fn arb_recommendation() -> impl Strategy<Value = RestartRecommendation> {
    (
        any::<u64>(),
        0_u32..2880,
        0.0_f64..10.0,
        0.0_f64..1.0,
        arb_score_breakdown(),
        any::<bool>(),
        proptest::option::of(any::<u64>()),
        proptest::option::of(any::<u64>()),
    )
        .prop_map(
            |(epoch, offset, hazard, activity, breakdown, auto_exec, warning, snapshot)| {
                RestartRecommendation {
                    scheduled_for_epoch_secs: epoch,
                    offset_minutes: offset,
                    hazard_rate: hazard,
                    predicted_activity: activity,
                    breakdown,
                    should_execute_automatically: auto_exec,
                    warning_epoch_secs: warning,
                    snapshot_epoch_secs: snapshot,
                }
            },
        )
}

// =============================================================================
// RiskFactor serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_risk_factor_serde_roundtrip(rf in arb_risk_factor()) {
        let json = serde_json::to_string(&rf).unwrap();
        let back: RiskFactor = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &rf.name);
        prop_assert!((back.value - rf.value).abs() < 1e-10, "value mismatch");
        prop_assert!((back.coefficient - rf.coefficient).abs() < 1e-10, "coefficient mismatch");
    }

    #[test]
    fn prop_risk_factor_json_has_name(rf in arb_risk_factor()) {
        let json = serde_json::to_string(&rf).unwrap();
        prop_assert!(json.contains("\"name\""));
    }
}

// =============================================================================
// HazardReport serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_hazard_report_serde_roundtrip(report in arb_hazard_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: HazardReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.timestamp_secs, report.timestamp_secs);
        prop_assert_eq!(back.action, report.action);
        prop_assert_eq!(back.in_warmup, report.in_warmup);
        prop_assert_eq!(back.observation_count, report.observation_count);
        prop_assert_eq!(back.risk_factors.len(), report.risk_factors.len());
    }

    #[test]
    fn prop_hazard_report_serde_deterministic(report in arb_hazard_report()) {
        let j1 = serde_json::to_string(&report).unwrap();
        let j2 = serde_json::to_string(&report).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    #[test]
    fn prop_hazard_report_json_contains_action(report in arb_hazard_report()) {
        let json = serde_json::to_string(&report).unwrap();
        prop_assert!(json.contains("\"action\""), "JSON should contain action field");
    }
}

// =============================================================================
// RestartMode serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_restart_mode_serde_roundtrip(mode in arb_restart_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let back: RestartMode = serde_json::from_str(&json).unwrap();
        match (&back, &mode) {
            (RestartMode::Automatic { min_score: a }, RestartMode::Automatic { min_score: b }) => {
                prop_assert!((a - b).abs() < 1e-10, "min_score mismatch: {} vs {}", a, b);
            }
            (RestartMode::Advisory, RestartMode::Advisory) => {}
            (RestartMode::Manual, RestartMode::Manual) => {}
            _ => prop_assert!(false, "mode variant mismatch: {:?} vs {:?}", back, mode),
        }
    }

    #[test]
    fn prop_restart_mode_default_is_advisory(_dummy in 0..1u8) {
        let mode = RestartMode::default();
        let is_advisory = matches!(mode, RestartMode::Advisory);
        prop_assert!(is_advisory, "default mode should be Advisory");
    }

    #[test]
    fn prop_restart_mode_json_has_mode_tag(mode in arb_restart_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        prop_assert!(json.contains("\"mode\""), "JSON should contain mode tag");
    }
}

// =============================================================================
// RestartSchedulerConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_restart_config_serde_roundtrip(cfg in arb_restart_config()) {
        let json = serde_json::to_string(&cfg).unwrap();
        let back: RestartSchedulerConfig = serde_json::from_str(&json).unwrap();
        // Float fields need approximate comparison
        prop_assert!((back.hazard_threshold - cfg.hazard_threshold).abs() < 1e-10);
        prop_assert!((back.urgency_steepness - cfg.urgency_steepness).abs() < 1e-10);
        prop_assert!((back.cooldown_hours - cfg.cooldown_hours).abs() < 1e-10);
        prop_assert!((back.activity_ewma_alpha - cfg.activity_ewma_alpha).abs() < 1e-10);
        prop_assert!((back.default_activity - cfg.default_activity).abs() < 1e-10);
        // Non-float fields are exact
        prop_assert_eq!(back.schedule_horizon_minutes, cfg.schedule_horizon_minutes);
        prop_assert_eq!(back.pre_restart_snapshot, cfg.pre_restart_snapshot);
        prop_assert_eq!(back.advance_warning_minutes, cfg.advance_warning_minutes);
    }

    #[test]
    fn prop_restart_config_default_valid(_dummy in 0..1u8) {
        let cfg = RestartSchedulerConfig::default();
        prop_assert!(cfg.hazard_threshold > 0.0);
        prop_assert!(cfg.cooldown_hours > 0.0);
        prop_assert!(cfg.schedule_horizon_minutes > 0);
        prop_assert!(cfg.activity_ewma_alpha > 0.0 && cfg.activity_ewma_alpha < 1.0);
    }

    #[test]
    fn prop_restart_config_serde_deterministic(cfg in arb_restart_config()) {
        let j1 = serde_json::to_string(&cfg).unwrap();
        let j2 = serde_json::to_string(&cfg).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =============================================================================
// ActivityProfile property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_activity_profile_serde_roundtrip(
        alpha in 0.01_f64..0.99,
        default_act in 0.0_f64..1.0,
    ) {
        let profile = ActivityProfile::new(alpha, default_act);
        let json = serde_json::to_string(&profile).unwrap();
        let back: ActivityProfile = serde_json::from_str(&json).unwrap();
        // Float precision: compare hourly snapshots approximately
        let orig_snap = profile.hourly_snapshot();
        let back_snap = back.hourly_snapshot();
        for h in 0..24 {
            prop_assert!((orig_snap[h] - back_snap[h]).abs() < 1e-10,
                "hour {} mismatch: {} vs {}", h, orig_snap[h], back_snap[h]);
        }
    }

    #[test]
    fn prop_activity_profile_new_all_hours_default(
        alpha in 0.01_f64..0.99,
        default_act in 0.0_f64..1.0,
    ) {
        let profile = ActivityProfile::new(alpha, default_act);
        let snapshot = profile.hourly_snapshot();
        let clamped = default_act.clamp(0.0, 1.0);
        for (h, &snap_val) in snapshot.iter().enumerate() {
            prop_assert!((snap_val - clamped).abs() < 1e-10,
                "hour {} should be {}, got {}", h, clamped, snap_val);
            prop_assert_eq!(profile.sample_count(h as u8), 0);
        }
    }

    #[test]
    fn prop_activity_profile_update_hour_increments_count(
        alpha in 0.01_f64..0.99,
        hour in 0_u8..24,
        activity in 0.0_f64..1.0,
    ) {
        let mut profile = ActivityProfile::new(alpha, 0.5);
        prop_assert_eq!(profile.sample_count(hour), 0);
        profile.update_hour(hour, activity);
        prop_assert_eq!(profile.sample_count(hour), 1);
    }

    #[test]
    fn prop_activity_profile_predict_in_range(
        alpha in 0.01_f64..0.99,
        default_act in 0.0_f64..1.0,
        hour in 0_u8..24,
    ) {
        let profile = ActivityProfile::new(alpha, default_act);
        let prediction = profile.predict_hour(hour);
        prop_assert!((0.0..=1.0).contains(&prediction),
            "prediction should be in [0, 1], got {}", prediction);
    }

    #[test]
    fn prop_activity_profile_first_update_sets_value(
        alpha in 0.01_f64..0.99,
        hour in 0_u8..24,
        activity in 0.0_f64..1.0,
    ) {
        let mut profile = ActivityProfile::new(alpha, 0.5);
        profile.update_hour(hour, activity);
        let predicted = profile.predict_hour(hour);
        // First sample should set the value directly (not EWMA blend)
        let clamped = activity.clamp(0.0, 1.0);
        prop_assert!((predicted - clamped).abs() < 1e-10,
            "first update should set value to {}, got {}", clamped, predicted);
    }
}

// =============================================================================
// HazardForecastPoint serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_forecast_point_serde_roundtrip(fp in arb_forecast_point()) {
        let json = serde_json::to_string(&fp).unwrap();
        let back: HazardForecastPoint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.offset_minutes, fp.offset_minutes);
        prop_assert!((back.hazard_rate - fp.hazard_rate).abs() < 1e-10);
        prop_assert_eq!(back.predicted_activity.is_some(), fp.predicted_activity.is_some());
    }

    #[test]
    fn prop_forecast_point_serde_deterministic(fp in arb_forecast_point()) {
        let j1 = serde_json::to_string(&fp).unwrap();
        let j2 = serde_json::to_string(&fp).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =============================================================================
// RestartScoreBreakdown serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_score_breakdown_serde_roundtrip(sb in arb_score_breakdown()) {
        let json = serde_json::to_string(&sb).unwrap();
        let back: RestartScoreBreakdown = serde_json::from_str(&json).unwrap();
        prop_assert!((back.hazard_urgency - sb.hazard_urgency).abs() < 1e-10);
        prop_assert!((back.activity_minimum - sb.activity_minimum).abs() < 1e-10);
        prop_assert!((back.recency_penalty - sb.recency_penalty).abs() < 1e-10);
        prop_assert!((back.score - sb.score).abs() < 1e-10);
    }

    #[test]
    fn prop_score_breakdown_json_has_score(sb in arb_score_breakdown()) {
        let json = serde_json::to_string(&sb).unwrap();
        prop_assert!(json.contains("\"score\""));
    }
}

// =============================================================================
// RestartRecommendation serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_recommendation_serde_roundtrip(rec in arb_recommendation()) {
        let json = serde_json::to_string(&rec).unwrap();
        let back: RestartRecommendation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.scheduled_for_epoch_secs, rec.scheduled_for_epoch_secs);
        prop_assert_eq!(back.offset_minutes, rec.offset_minutes);
        prop_assert_eq!(back.should_execute_automatically, rec.should_execute_automatically);
        prop_assert_eq!(back.warning_epoch_secs, rec.warning_epoch_secs);
        prop_assert_eq!(back.snapshot_epoch_secs, rec.snapshot_epoch_secs);
    }

    #[test]
    fn prop_recommendation_serde_deterministic(rec in arb_recommendation()) {
        let j1 = serde_json::to_string(&rec).unwrap();
        let j2 = serde_json::to_string(&rec).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    #[test]
    fn prop_recommendation_json_structure(rec in arb_recommendation()) {
        let json = serde_json::to_string(&rec).unwrap();
        prop_assert!(json.contains("\"scheduled_for_epoch_secs\""));
        prop_assert!(json.contains("\"breakdown\""));
    }
}

// =============================================================================
// RestartScheduler construction and basic properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_restart_scheduler_new_config_preserved(cfg in arb_restart_config()) {
        let scheduler = RestartScheduler::new(cfg.clone());
        prop_assert_eq!(scheduler.config(), &cfg);
    }

    #[test]
    fn prop_restart_scheduler_default_config_valid(_dummy in 0..1u8) {
        let scheduler = RestartScheduler::new(RestartSchedulerConfig::default());
        let cfg = scheduler.config();
        let is_advisory = matches!(cfg.mode, RestartMode::Advisory);
        prop_assert!(is_advisory, "default scheduler mode should be Advisory");
    }

    #[test]
    fn prop_restart_scheduler_activity_predict_in_range(
        hour in 0_u8..24,
    ) {
        let scheduler = RestartScheduler::new(RestartSchedulerConfig::default());
        let activity = scheduler.activity_profile().predict_hour(hour);
        prop_assert!((0.0..=1.0).contains(&activity),
            "predicted activity should be in [0, 1], got {}", activity);
    }

    #[test]
    fn prop_restart_scheduler_record_activity_updates_profile(
        hour in 0_u8..24,
        activity in 0.0_f64..1.0,
    ) {
        let mut scheduler = RestartScheduler::new(RestartSchedulerConfig::default());
        let before = scheduler.activity_profile().sample_count(hour);
        scheduler.activity_profile_mut().update_hour(hour, activity);
        let after = scheduler.activity_profile().sample_count(hour);
        prop_assert_eq!(after, before + 1);
    }

    #[test]
    fn prop_restart_scheduler_no_restart_initially(_dummy in 0..1u8) {
        let scheduler = RestartScheduler::new(RestartSchedulerConfig::default());
        prop_assert!(scheduler.last_restart_at().is_none(),
            "fresh scheduler should have no last restart");
    }
}
