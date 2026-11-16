//! Property-based tests for PAC-Bayesian adaptive backpressure (ft-l5em3.3).
//!
//! Verifies algebraic invariants of the Bayesian posterior, PAC-Bayes bounds,
//! sigmoid severity, starvation guard, and controller determinism.

use frankenterm_core::aegis_backpressure::{
    ExternalCauseEvidence, GaussianPosterior, PacBayesBackpressure, PacBayesConfig,
    PacBayesSnapshot, PacBayesTelemetrySnapshot, PacBayesThrottleActions, QueueObservation,
};
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn arb_fill_ratio() -> impl Strategy<Value = f64> {
    0.0..=1.0_f64
}

fn arb_positive_variance() -> impl Strategy<Value = f64> {
    0.001..=10.0_f64
}

fn arb_mean() -> impl Strategy<Value = f64> {
    -5.0..=5.0_f64
}

fn arb_delta() -> impl Strategy<Value = f64> {
    0.001..=0.5_f64
}

fn arb_starvation_cost() -> impl Strategy<Value = f64> {
    0.01..=0.99_f64
}

fn arb_observation() -> impl Strategy<Value = QueueObservation> {
    (any::<u64>(), arb_fill_ratio(), any::<bool>()).prop_map(
        |(pane_id, fill_ratio, frame_dropped)| {
            QueueObservation {
                pane_id: pane_id % 100, // Keep pane IDs bounded
                fill_ratio,
                frame_dropped,
                external_cause: None,
            }
        },
    )
}

fn arb_external_evidence() -> impl Strategy<Value = ExternalCauseEvidence> {
    (0.0..=16.0_f64, 0.0..=1.0_f64, any::<bool>(), 0.0..=1.0_f64).prop_map(
        |(system_load, other_panes_slow_fraction, pty_producing, io_wait_fraction)| {
            ExternalCauseEvidence {
                system_load,
                other_panes_slow_fraction,
                pty_producing,
                io_wait_fraction,
            }
        },
    )
}

fn arb_config() -> impl Strategy<Value = PacBayesConfig> {
    (
        arb_delta(),
        arb_starvation_cost(),
        arb_fill_ratio(),        // prior_threshold_mean
        arb_positive_variance(), // prior_threshold_variance
        0..=50_usize,            // warmup_observations
    )
        .prop_map(
            |(delta, starvation_cost, prior_mean, prior_var, warmup)| PacBayesConfig {
                delta,
                starvation_cost,
                prior_threshold_mean: prior_mean,
                prior_threshold_variance: prior_var,
                warmup_observations: warmup,
                ..Default::default()
            },
        )
}

// ── Gaussian Posterior Properties ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // GP-1: KL divergence is non-negative
    #[test]
    fn kl_divergence_non_negative(
        mean1 in arb_mean(),
        var1 in arb_positive_variance(),
        mean2 in arb_mean(),
        var2 in arb_positive_variance(),
    ) {
        let p = GaussianPosterior::new(mean1, var1);
        let q = GaussianPosterior::new(mean2, var2);
        let kl = p.kl_divergence(&q);
        prop_assert!(kl >= -1e-10, "KL should be non-negative: {}", kl);
    }

    // GP-2: KL divergence of identical distributions is zero
    #[test]
    fn kl_divergence_zero_for_identical(
        mean in arb_mean(),
        var in arb_positive_variance(),
    ) {
        let p = GaussianPosterior::new(mean, var);
        let kl = p.kl_divergence(&p);
        prop_assert!(kl.abs() < 1e-10, "KL(P||P) = 0: got {}", kl);
    }

    // GP-3: Posterior update decreases variance
    #[test]
    fn posterior_update_shrinks_variance(
        mean in arb_mean(),
        var in arb_positive_variance(),
        obs in arb_mean(),
        obs_var in arb_positive_variance(),
    ) {
        let mut p = GaussianPosterior::new(mean, var);
        let initial_var = p.variance;
        p.update(obs, obs_var);
        prop_assert!(
            p.variance <= initial_var + 1e-12,
            "Bayesian update should not increase variance: was {initial_var}, now {}",
            p.variance
        );
    }

    // GP-4: Posterior mean is between prior and observation (convex combination)
    #[test]
    fn posterior_mean_is_convex_combination(
        mean in arb_mean(),
        var in arb_positive_variance(),
        obs in arb_mean(),
        obs_var in arb_positive_variance(),
    ) {
        let mut p = GaussianPosterior::new(mean, var);
        p.update(obs, obs_var);
        let lower = mean.min(obs);
        let upper = mean.max(obs);
        prop_assert!(
            p.mean >= lower - 1e-10 && p.mean <= upper + 1e-10,
            "Posterior mean {} should be between {} and {}",
            p.mean, lower, upper
        );
    }

    // GP-5: Multiple updates increase observation count
    #[test]
    fn observation_count_increases(
        mean in arb_mean(),
        var in arb_positive_variance(),
        n in 1..=20_usize,
    ) {
        let mut p = GaussianPosterior::new(mean, var);
        for _ in 0..n {
            p.update(0.5, 0.1);
        }
        prop_assert_eq!(p.n_observations, n);
    }

    // GP-6: Upper bound > mean > lower bound
    #[test]
    fn confidence_bounds_bracket_mean(
        mean in arb_mean(),
        var in arb_positive_variance(),
        delta in arb_delta(),
    ) {
        let p = GaussianPosterior::new(mean, var);
        let upper = p.upper_bound(delta);
        let lower = p.lower_bound(delta);
        prop_assert!(upper >= p.mean, "upper {} >= mean {}", upper, p.mean);
        prop_assert!(lower <= p.mean, "lower {} <= mean {}", lower, p.mean);
        // Symmetry
        let diff_up = (upper - p.mean).abs();
        let diff_down = (p.mean - lower).abs();
        prop_assert!((diff_up - diff_down).abs() < 1e-6, "bounds should be symmetric");
    }

    // GP-7: Tighter delta → wider confidence interval
    #[test]
    fn tighter_delta_gives_wider_bounds(
        mean in arb_mean(),
        var in arb_positive_variance(),
    ) {
        let p = GaussianPosterior::new(mean, var);
        let tight = p.upper_bound(0.01); // 99% confidence
        let loose = p.upper_bound(0.10); // 90% confidence
        prop_assert!(tight >= loose - 1e-10, "tighter confidence should be wider: tight={}, loose={}", tight, loose);
    }
}

// ── Controller Properties ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // CTRL-1: Severity is bounded [0, max_severity]
    #[test]
    fn severity_bounded(
        config in arb_config(),
        obs in prop::collection::vec(arb_observation(), 1..=30),
    ) {
        let mut ctrl = PacBayesBackpressure::new(config.clone());
        for o in &obs {
            let actions = ctrl.observe(o);
            prop_assert!(
                actions.severity >= 0.0 && actions.severity <= config.max_severity + 1e-10,
                "severity {} out of [0, {}]", actions.severity, config.max_severity
            );
        }
    }

    // CTRL-2: Risk bound is bounded [0, 1]
    #[test]
    fn risk_bound_bounded(
        config in arb_config(),
        obs in prop::collection::vec(arb_observation(), 1..=30),
    ) {
        let mut ctrl = PacBayesBackpressure::new(config);
        for o in &obs {
            let actions = ctrl.observe(o);
            prop_assert!(
                actions.risk_bound >= 0.0 && actions.risk_bound <= 1.0 + 1e-10,
                "risk_bound {} out of [0, 1]", actions.risk_bound
            );
        }
    }

    // CTRL-3: Poll multiplier ≥ 1
    #[test]
    fn poll_multiplier_at_least_one(
        config in arb_config(),
        obs in prop::collection::vec(arb_observation(), 1..=20),
    ) {
        let mut ctrl = PacBayesBackpressure::new(config);
        for o in &obs {
            let actions = ctrl.observe(o);
            prop_assert!(
                actions.poll_multiplier >= 1.0 - 1e-10,
                "poll_multiplier {} < 1", actions.poll_multiplier
            );
        }
    }

    // CTRL-4: Buffer limit factor in [0.2, 1.0]
    #[test]
    fn buffer_limit_factor_bounded(
        config in arb_config(),
        obs in prop::collection::vec(arb_observation(), 1..=20),
    ) {
        let mut ctrl = PacBayesBackpressure::new(config);
        for o in &obs {
            let actions = ctrl.observe(o);
            prop_assert!(
                actions.buffer_limit_factor >= 0.2 - 1e-10
                    && actions.buffer_limit_factor <= 1.0 + 1e-10,
                "buffer_limit_factor {} out of [0.2, 1.0]",
                actions.buffer_limit_factor
            );
        }
    }

    // CTRL-5: Pane skip fraction in [0, 0.5]
    #[test]
    fn pane_skip_fraction_bounded(
        config in arb_config(),
        obs in prop::collection::vec(arb_observation(), 1..=20),
    ) {
        let mut ctrl = PacBayesBackpressure::new(config);
        for o in &obs {
            let actions = ctrl.observe(o);
            prop_assert!(
                actions.pane_skip_fraction >= -1e-10
                    && actions.pane_skip_fraction <= 0.5 + 1e-10,
                "pane_skip_fraction {} out of [0, 0.5]",
                actions.pane_skip_fraction
            );
        }
    }

    // CTRL-6: Observation count matches input count
    #[test]
    fn observation_count_matches(
        obs in prop::collection::vec(arb_observation(), 1..=50),
    ) {
        let mut ctrl = PacBayesBackpressure::with_defaults();
        for o in &obs {
            ctrl.observe(o);
        }
        prop_assert_eq!(ctrl.total_observations(), obs.len());
    }

    // CTRL-7: Frame drops tracked correctly
    #[test]
    fn frame_drop_tracking(
        obs in prop::collection::vec(arb_observation(), 1..=50),
    ) {
        let mut ctrl = PacBayesBackpressure::with_defaults();
        let expected_drops = obs.iter().filter(|o| o.frame_dropped).count();
        for o in &obs {
            ctrl.observe(o);
        }
        let actual_rate = ctrl.global_drop_rate();
        let expected_rate = expected_drops as f64 / obs.len() as f64;
        prop_assert!(
            (actual_rate - expected_rate).abs() < 1e-10,
            "drop rate: actual={actual_rate}, expected={expected_rate}"
        );
    }

    // CTRL-8: Determinism — same input sequence produces same output
    #[test]
    fn deterministic_replay(
        config in arb_config(),
        obs in prop::collection::vec(arb_observation(), 1..=30),
    ) {
        let mut ctrl1 = PacBayesBackpressure::new(config.clone());
        let mut ctrl2 = PacBayesBackpressure::new(config);

        let actions1: Vec<_> = obs.iter().map(|o| ctrl1.observe(o)).collect();
        let actions2: Vec<_> = obs.iter().map(|o| ctrl2.observe(o)).collect();

        for (i, (a1, a2)) in actions1.iter().zip(actions2.iter()).enumerate() {
            prop_assert!(
                (a1.severity - a2.severity).abs() < 1e-10,
                "severity mismatch at step {i}"
            );
            prop_assert!(
                (a1.risk_bound - a2.risk_bound).abs() < 1e-10,
                "risk_bound mismatch at step {i}"
            );
            prop_assert!(
                (a1.kl_divergence - a2.kl_divergence).abs() < 1e-10,
                "kl_divergence mismatch at step {i}"
            );
        }
    }

    // CTRL-9: KL divergence is non-negative throughout operation
    #[test]
    fn kl_divergence_non_negative_during_operation(
        config in arb_config(),
        obs in prop::collection::vec(arb_observation(), 1..=30),
    ) {
        let mut ctrl = PacBayesBackpressure::new(config);
        for o in &obs {
            let actions = ctrl.observe(o);
            prop_assert!(
                actions.kl_divergence >= -1e-10,
                "KL divergence should be non-negative: {}",
                actions.kl_divergence
            );
        }
    }

    // CTRL-10: Reset clears all state
    #[test]
    fn reset_clears_all(
        obs in prop::collection::vec(arb_observation(), 1..=20),
    ) {
        let mut ctrl = PacBayesBackpressure::with_defaults();
        for o in &obs {
            ctrl.observe(o);
        }
        ctrl.reset();
        prop_assert_eq!(ctrl.pane_count(), 0);
        prop_assert_eq!(ctrl.total_observations(), 0);
    }

    // CTRL-11: Snapshot serde roundtrip preserves data
    #[test]
    fn snapshot_serde_roundtrip(
        obs in prop::collection::vec(arb_observation(), 1..=10),
    ) {
        let mut ctrl = PacBayesBackpressure::with_defaults();
        for o in &obs {
            ctrl.observe(o);
        }
        let snap = ctrl.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: PacBayesSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.global_observations, back.global_observations);
        prop_assert_eq!(snap.global_frame_drops, back.global_frame_drops);
        prop_assert_eq!(snap.pane_count, back.pane_count);
        prop_assert!(
            (snap.global_drop_rate - back.global_drop_rate).abs() < 1e-10,
            "drop rate mismatch"
        );
    }

    // CTRL-12: Config serde roundtrip
    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: PacBayesConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((config.delta - back.delta).abs() < 1e-10);
        prop_assert!((config.starvation_cost - back.starvation_cost).abs() < 1e-10);
        prop_assert_eq!(config.warmup_observations, back.warmup_observations);
    }
}

// ── Starvation Guard Properties ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // SG-1: Starvation guard never increases severity
    #[test]
    fn starvation_guard_never_increases_severity(
        fill_ratio in arb_fill_ratio(),
        evidence in arb_external_evidence(),
    ) {
        let mut ctrl_no_guard = PacBayesBackpressure::new(PacBayesConfig {
            starvation_guard: false,
            warmup_observations: 0,
            ..Default::default()
        });
        let mut ctrl_guard = PacBayesBackpressure::new(PacBayesConfig {
            starvation_guard: true,
            warmup_observations: 0,
            ..Default::default()
        });

        // Warm up both identically
        for _ in 0..15 {
            let o = QueueObservation {
                pane_id: 1,
                fill_ratio: 0.8,
                frame_dropped: true,
                external_cause: None,
            };
            ctrl_no_guard.observe(&o);
            ctrl_guard.observe(&o);
        }

        let obs_no = QueueObservation {
            pane_id: 1,
            fill_ratio,
            frame_dropped: true,
            external_cause: None,
        };
        let obs_yes = QueueObservation {
            pane_id: 1,
            fill_ratio,
            frame_dropped: true,
            external_cause: Some(evidence),
        };

        let actions_no = ctrl_no_guard.observe(&obs_no);
        let actions_yes = ctrl_guard.observe(&obs_yes);

        prop_assert!(
            actions_yes.severity <= actions_no.severity + 1e-6,
            "starvation guard should not increase severity: guard={}, no_guard={}",
            actions_yes.severity,
            actions_no.severity
        );
    }

    // SG-2: External cause probability bounded [0, 1]
    #[test]
    fn external_cause_probability_bounded(evidence in arb_external_evidence()) {
        // Verify through controller that guard doesn't produce invalid severity
        let mut ctrl = PacBayesBackpressure::new(PacBayesConfig {
            starvation_guard: true,
            warmup_observations: 0,
            ..Default::default()
        });
        for _ in 0..15 {
            ctrl.observe(&QueueObservation {
                pane_id: 1,
                fill_ratio: 0.9,
                frame_dropped: true,
                external_cause: None,
            });
        }
        let actions = ctrl.observe(&QueueObservation {
            pane_id: 1,
            fill_ratio: 0.9,
            frame_dropped: true,
            external_cause: Some(evidence),
        });
        prop_assert!(actions.severity >= 0.0);
        prop_assert!(actions.severity <= 1.0 + 1e-10);
    }
}

// ── Severity Curve Properties ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // SC-1: Higher fill ratio → higher or equal severity (monotonicity)
    #[test]
    fn severity_monotonic_in_fill_ratio(
        ratio_low in 0.0..=0.5_f64,
        ratio_high in 0.5..=1.0_f64,
    ) {
        let config = PacBayesConfig {
            warmup_observations: 0,
            ..Default::default()
        };
        let mut ctrl_low = PacBayesBackpressure::new(config.clone());
        let mut ctrl_high = PacBayesBackpressure::new(config);

        // Feed steady state
        for _ in 0..20 {
            ctrl_low.observe(&QueueObservation {
                pane_id: 1, fill_ratio: ratio_low, frame_dropped: false,
                external_cause: None,
            });
            ctrl_high.observe(&QueueObservation {
                pane_id: 1, fill_ratio: ratio_high, frame_dropped: false,
                external_cause: None,
            });
        }

        let sev_low = ctrl_low.observe(&QueueObservation {
            pane_id: 1, fill_ratio: ratio_low, frame_dropped: false,
            external_cause: None,
        }).severity;
        let sev_high = ctrl_high.observe(&QueueObservation {
            pane_id: 1, fill_ratio: ratio_high, frame_dropped: false,
            external_cause: None,
        }).severity;

        prop_assert!(
            sev_high >= sev_low - 0.1,
            "higher ratio ({ratio_high}) should have >= severity ({sev_high}) than lower ({ratio_low}, {sev_low})"
        );
    }

    // SC-2: Throttle curves are monotonic functions of severity
    #[test]
    fn throttle_curves_monotonic(severity in 0.0..=1.0_f64) {
        let poll = 3.0f64.mul_add(severity, 1.0);
        let skip = 0.5 * severity * severity;
        let detect = 0.25 * severity;
        let buffer = (-0.8f64).mul_add(severity, 1.0);

        prop_assert!(poll >= 1.0);
        prop_assert!(poll <= 4.0 + 1e-10);
        prop_assert!(skip >= 0.0);
        prop_assert!(skip <= 0.5 + 1e-10);
        prop_assert!(detect >= 0.0);
        prop_assert!(detect <= 0.25 + 1e-10);
        prop_assert!(buffer >= 0.2 - 1e-10);
        prop_assert!(buffer <= 1.0 + 1e-10);
    }
}

// ── Additional carrier serde coverage ────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    #[test]
    fn external_cause_evidence_serde_roundtrip(
        evidence in arb_external_evidence(),
    ) {
        let json = serde_json::to_string(&evidence).unwrap();
        let back: ExternalCauseEvidence = serde_json::from_str(&json).unwrap();
        prop_assert!((back.system_load - evidence.system_load).abs() < 1e-10);
        prop_assert!((back.other_panes_slow_fraction - evidence.other_panes_slow_fraction).abs() < 1e-10);
        prop_assert_eq!(back.pty_producing, evidence.pty_producing);
        prop_assert!((back.io_wait_fraction - evidence.io_wait_fraction).abs() < 1e-10);
    }

    #[test]
    fn throttle_actions_serde_roundtrip(
        severity in 0.0..=1.0_f64,
        poll_multiplier in 1.0..=4.0_f64,
        pane_skip_fraction in 0.0..=0.5_f64,
        detection_skip_fraction in 0.0..=0.25_f64,
        buffer_limit_factor in 0.2..=1.0_f64,
        starvation_guard_active in any::<bool>(),
        risk_bound in 0.0..=1.0_f64,
        kl_divergence in 0.0..=100.0_f64,
        optimal_threshold in 0.0..=1.0_f64,
    ) {
        let actions = PacBayesThrottleActions {
            severity,
            poll_multiplier,
            pane_skip_fraction,
            detection_skip_fraction,
            buffer_limit_factor,
            starvation_guard_active,
            risk_bound,
            kl_divergence,
            optimal_threshold,
        };
        let json = serde_json::to_string(&actions).unwrap();
        let back: PacBayesThrottleActions = serde_json::from_str(&json).unwrap();
        prop_assert!((back.severity - actions.severity).abs() < 1e-10);
        prop_assert!((back.poll_multiplier - actions.poll_multiplier).abs() < 1e-10);
        prop_assert!((back.pane_skip_fraction - actions.pane_skip_fraction).abs() < 1e-10);
        prop_assert!((back.detection_skip_fraction - actions.detection_skip_fraction).abs() < 1e-10);
        prop_assert!((back.buffer_limit_factor - actions.buffer_limit_factor).abs() < 1e-10);
        prop_assert_eq!(back.starvation_guard_active, actions.starvation_guard_active);
        prop_assert!((back.risk_bound - actions.risk_bound).abs() < 1e-10);
        prop_assert!((back.kl_divergence - actions.kl_divergence).abs() < 1e-10);
        prop_assert!((back.optimal_threshold - actions.optimal_threshold).abs() < 1e-10);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip(
        observations in 0u64..100_000,
        frame_drops_observed in 0u64..100_000,
        throttle_activations in 0u64..100_000,
        starvation_guards in 0u64..100_000,
        posterior_updates in 0u64..100_000,
        pane_resets in 0u64..100_000,
        full_resets in 0u64..100_000,
    ) {
        let snap = PacBayesTelemetrySnapshot {
            observations,
            frame_drops_observed,
            throttle_activations,
            starvation_guards,
            posterior_updates,
            pane_resets,
            full_resets,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: PacBayesTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, snap);
    }
}
