//! Property-based tests for Bayesian evidence ledger invariants.
//!
//! Bead: wa-t391
//!
//! Validates:
//! 1. Posterior sums to 1: normalized probabilities always sum to 1
//! 2. Evidence monotonicity: repeated evidence for state X increases P(X)
//! 3. State index roundtrip: from_index(index(s)) == s for all states
//! 4. Pane lifecycle: add/remove/reset preserves correct count
//! 5. Feedback shifts prior: repeated feedback increases prior for that state
//! 6. Ledger size bounded: never exceeds max_ledger_entries
//! 7. Observation count tracks: each update increments count by 1
//! 8. Bayes factor positive: BF is always > 0 after observations
//! 9. Classification deterministic: same evidence → same result
//! 10. Snapshot consistent: snapshot pane_count matches actual count

use proptest::prelude::*;

use frankenterm_core::bayesian_ledger::{
    BayesianClassifier, Evidence, LedgerConfig, PaneState,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_pane_id() -> impl Strategy<Value = u64> {
    1_u64..1000
}

fn arb_output_rate() -> impl Strategy<Value = f64> {
    0.0_f64..100.0
}

fn arb_entropy() -> impl Strategy<Value = f64> {
    0.0_f64..8.0
}

fn arb_time_since() -> impl Strategy<Value = f64> {
    0.0_f64..300.0
}

fn arb_scrollback_growth() -> impl Strategy<Value = f64> {
    0.0_f64..2000.0
}

fn arb_evidence() -> impl Strategy<Value = Evidence> {
    prop_oneof![
        arb_output_rate().prop_map(Evidence::OutputRate),
        arb_entropy().prop_map(Evidence::Entropy),
        arb_time_since().prop_map(Evidence::TimeSinceOutput),
        arb_scrollback_growth().prop_map(Evidence::ScrollbackGrowth),
        Just(Evidence::PatternDetection("tool_use".to_string())),
        Just(Evidence::PatternDetection("rate_limited".to_string())),
        Just(Evidence::PatternDetection("error".to_string())),
        Just(Evidence::PatternDetection("thinking".to_string())),
    ]
}

fn arb_state() -> impl Strategy<Value = PaneState> {
    (0_usize..PaneState::COUNT).prop_map(|i| PaneState::from_index(i).unwrap())
}

fn arb_config() -> impl Strategy<Value = LedgerConfig> {
    (
        1_usize..20,       // min_observations
        1.0_f64..100.0,    // bayes_factor_threshold
        0.1_f64..10.0,     // dirichlet_alpha
        5_usize..200,      // max_ledger_entries
    )
        .prop_map(|(min_obs, bf_thresh, alpha, max_entries)| LedgerConfig {
            min_observations: min_obs,
            bayes_factor_threshold: bf_thresh,
            dirichlet_alpha: alpha,
            max_ledger_entries: max_entries,
        })
}

// =============================================================================
// Property: Posterior sums to 1
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn posterior_sums_to_one(
        evidence in proptest::collection::vec(arb_evidence(), 1..20),
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        for ev in evidence {
            clf.update(1, ev);
        }

        let result = clf.classify(1).unwrap();
        let sum: f64 = result.posterior.values().sum();
        prop_assert!((sum - 1.0).abs() < 1e-6,
            "posterior should sum to 1.0, got {}", sum);
    }
}

// =============================================================================
// Property: Evidence monotonicity — repeated evidence increases favored state
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn active_evidence_increases_active_probability(
        n in 1_usize..10,
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig {
            min_observations: 1,
            ..Default::default()
        });

        // Start with neutral evidence.
        clf.update(1, Evidence::OutputRate(5.0));
        let before = clf.classify(1).unwrap();
        let p_before = *before.posterior.get("active").unwrap_or(&0.0);

        // Add strong Active evidence.
        for _ in 0..n {
            clf.update(1, Evidence::OutputRate(15.0));
            clf.update(1, Evidence::Entropy(5.0));
        }

        let after = clf.classify(1).unwrap();
        let p_after = *after.posterior.get("active").unwrap_or(&0.0);

        prop_assert!(p_after >= p_before - 1e-10,
            "active prob should not decrease with active evidence: {} -> {}", p_before, p_after);
    }
}

// =============================================================================
// Property: State index roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn state_index_roundtrip(
        state in arb_state(),
    ) {
        let idx = state.index();
        let back = PaneState::from_index(idx);
        prop_assert_eq!(back, Some(state));
    }

    #[test]
    fn invalid_index_returns_none(
        idx in PaneState::COUNT..100_usize,
    ) {
        prop_assert_eq!(PaneState::from_index(idx), None);
    }
}

// =============================================================================
// Property: Pane lifecycle
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pane_add_remove_count(
        pane_ids in proptest::collection::hash_set(arb_pane_id(), 1..20),
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());

        // Add all panes.
        for &id in &pane_ids {
            clf.update(id, Evidence::OutputRate(5.0));
        }
        prop_assert_eq!(clf.pane_count(), pane_ids.len());

        // Remove half.
        let to_remove: Vec<u64> = pane_ids.iter().take(pane_ids.len() / 2).copied().collect();
        for &id in &to_remove {
            clf.remove_pane(id);
        }
        prop_assert_eq!(clf.pane_count(), pane_ids.len() - to_remove.len());
    }

    #[test]
    fn reset_pane_zeroes_observation_count(
        evidence in proptest::collection::vec(arb_evidence(), 1..10),
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        for ev in &evidence {
            clf.update(1, ev.clone());
        }

        clf.reset_pane(1);
        let result = clf.classify(1).unwrap();
        prop_assert_eq!(result.observation_count, 0,
            "observation count should be 0 after reset");
    }
}

// =============================================================================
// Property: Feedback shifts prior
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn feedback_increases_prior(
        state in arb_state(),
        n in 5_usize..30,
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());

        let prior_before = clf.log_prior()[state.index()];

        for _ in 0..n {
            clf.record_feedback(1, state);
        }

        let prior_after = clf.log_prior()[state.index()];
        prop_assert!(prior_after >= prior_before - 1e-10,
            "log-prior for {:?} should increase with feedback: {} -> {}",
            state, prior_before, prior_after);
    }

    #[test]
    fn feedback_count_tracks(
        n in 1_usize..50,
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        for _ in 0..n {
            clf.record_feedback(1, PaneState::Active);
        }
        prop_assert_eq!(clf.feedback_count(), n as u64);
    }
}

// =============================================================================
// Property: Ledger size bounded
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn ledger_size_bounded(
        max_entries in 5_usize..50,
        n_evidence in 10_usize..100,
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig {
            max_ledger_entries: max_entries,
            ..Default::default()
        });

        for _ in 0..n_evidence {
            clf.update(1, Evidence::OutputRate(10.0));
        }

        let result = clf.classify(1).unwrap();
        prop_assert!(result.ledger.len() <= max_entries,
            "ledger len {} should be <= max {}", result.ledger.len(), max_entries);
    }
}

// =============================================================================
// Property: Observation count tracks
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn observation_count_tracks(
        n in 1_usize..50,
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        for _ in 0..n {
            clf.update(1, Evidence::OutputRate(5.0));
        }

        let result = clf.classify(1).unwrap();
        prop_assert_eq!(result.observation_count, n as u64);
    }
}

// =============================================================================
// Property: Bayes factor positive
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn bayes_factor_positive(
        evidence in proptest::collection::vec(arb_evidence(), 1..15),
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        for ev in evidence {
            clf.update(1, ev);
        }

        let result = clf.classify(1).unwrap();
        prop_assert!(result.bayes_factor > 0.0,
            "bayes factor should be > 0, got {}", result.bayes_factor);
        prop_assert!(!result.bayes_factor.is_nan(),
            "bayes factor should not be NaN");
    }
}

// =============================================================================
// Property: Classification deterministic
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn classification_deterministic(
        evidence in proptest::collection::vec(arb_evidence(), 2..10),
    ) {
        let config = LedgerConfig {
            min_observations: 1,
            ..Default::default()
        };

        // Run 1
        let mut clf1 = BayesianClassifier::new(config.clone());
        for ev in &evidence {
            clf1.update(1, ev.clone());
        }
        let r1 = clf1.classify(1).unwrap();

        // Run 2 with same evidence
        let mut clf2 = BayesianClassifier::new(LedgerConfig {
            min_observations: 1,
            ..Default::default()
        });
        for ev in &evidence {
            clf2.update(1, ev.clone());
        }
        let r2 = clf2.classify(1).unwrap();

        prop_assert_eq!(r1.classification, r2.classification,
            "same evidence should produce same classification");
        prop_assert!((r1.bayes_factor - r2.bayes_factor).abs() < 1e-10,
            "bayes factors should match: {} vs {}", r1.bayes_factor, r2.bayes_factor);
    }
}

// =============================================================================
// Property: Posterior all non-negative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn posterior_all_nonnegative(
        evidence in proptest::collection::vec(arb_evidence(), 1..20),
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        for ev in evidence {
            clf.update(1, ev);
        }

        let result = clf.classify(1).unwrap();
        for (state_name, &prob) in &result.posterior {
            prop_assert!(prob >= 0.0,
                "P({}) should be >= 0, got {}", state_name, prob);
            prop_assert!(prob <= 1.0,
                "P({}) should be <= 1, got {}", state_name, prob);
        }
    }
}

// =============================================================================
// Property: Snapshot consistent with state
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn snapshot_consistent(
        n_panes in 1_usize..10,
        n_evidence in 1_usize..10,
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        for id in 0..n_panes as u64 {
            for _ in 0..n_evidence {
                clf.update(id, Evidence::OutputRate(5.0));
            }
        }

        let snap = clf.snapshot();
        prop_assert_eq!(snap.pane_count, n_panes as u64);
        prop_assert_eq!(snap.panes.len(), n_panes);

        // Prior sums to ~1.
        let prior_sum: f64 = snap.prior.values().sum();
        prop_assert!((prior_sum - 1.0).abs() < 1e-6,
            "snapshot prior should sum to 1.0, got {}", prior_sum);
    }
}

// =============================================================================
// Property: Multiple panes independent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn multiple_panes_independent(
        rate_a in 0.0_f64..5.0,
        rate_b in 20.0_f64..50.0,
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig {
            min_observations: 1,
            ..Default::default()
        });

        // Pane A: idle-like evidence.
        for _ in 0..10 {
            clf.update(1, Evidence::OutputRate(rate_a));
        }

        // Pane B: active-like evidence.
        for _ in 0..10 {
            clf.update(2, Evidence::OutputRate(rate_b));
        }

        let ra = clf.classify(1).unwrap();
        let rb = clf.classify(2).unwrap();

        // They should generally classify differently.
        // At minimum, their posteriors should differ.
        let pa_active = *ra.posterior.get("active").unwrap_or(&0.0);
        let pb_active = *rb.posterior.get("active").unwrap_or(&0.0);

        // Pane B (high rate) should have higher P(active) than pane A (low rate).
        prop_assert!(pb_active >= pa_active - 0.01,
            "higher rate pane should have higher P(active): {} vs {}", pb_active, pa_active);
    }
}

// =============================================================================
// Property: All states represented in posterior
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn all_states_in_posterior(
        evidence in proptest::collection::vec(arb_evidence(), 1..10),
    ) {
        let mut clf = BayesianClassifier::new(LedgerConfig::default());
        for ev in evidence {
            clf.update(1, ev);
        }

        let result = clf.classify(1).unwrap();
        prop_assert_eq!(result.posterior.len(), PaneState::COUNT,
            "posterior should have all {} states, got {}", PaneState::COUNT, result.posterior.len());
    }
}
