//! Property-based tests for the user_preferences module (MaxEnt IRL).
//!
//! Tests mathematical invariants for dot product, cosine similarity, rank
//! correlation, feature extraction, reward functions, and the IRL learner.

use frankenterm_core::user_preferences::*;
use proptest::prelude::*;

// ============================================================================
// Strategies
// ============================================================================

/// Generate a small f64 vector of given length (avoiding extreme values).
fn arb_f64_vec(len: usize) -> impl Strategy<Value = Vec<f64>> {
    proptest::collection::vec(-100.0f64..100.0, len)
}

/// Generate a fixed-size f64 array of NUM_FEATURES elements.
fn arb_feature_array() -> impl Strategy<Value = [f64; NUM_FEATURES]> {
    proptest::collection::vec(-10.0f64..10.0, NUM_FEATURES).prop_map(|v| {
        let mut arr = [0.0; NUM_FEATURES];
        for (i, &val) in v.iter().enumerate() {
            arr[i] = val;
        }
        arr
    })
}

/// Generate a non-zero f64 vector.
fn arb_nonzero_vec(len: usize) -> impl Strategy<Value = Vec<f64>> {
    arb_f64_vec(len).prop_filter("must be non-zero", |v| v.iter().any(|&x| x.abs() > 1e-10))
}

/// Generate a PaneState with reasonable values.
fn arb_pane_state() -> impl Strategy<Value = PaneState> {
    (
        any::<bool>(),
        0.0f64..3600.0,
        0.0f64..100.0,
        0u32..100,
        any::<bool>(),
        0.0f64..1.0,
        0u32..1000,
        1u64..1000,
    )
        .prop_map(
            |(has_new, tsf, rate, err, active, scroll, interact, pid)| PaneState {
                has_new_output: has_new,
                time_since_focus_s: tsf,
                output_rate: rate,
                error_count: err,
                process_active: active,
                scroll_depth: scroll,
                interaction_count: interact,
                pane_id: pid,
            },
        )
}

/// Generate a vec of PaneStates with unique pane IDs.
fn arb_pane_states(min: usize, max: usize) -> impl Strategy<Value = Vec<PaneState>> {
    proptest::collection::vec(arb_pane_state(), min..=max).prop_map(|mut panes| {
        // Ensure unique IDs
        for (i, pane) in panes.iter_mut().enumerate() {
            pane.pane_id = (i as u64) + 1;
        }
        panes
    })
}

/// Generate a UserAction.
fn arb_user_action() -> impl Strategy<Value = UserAction> {
    prop_oneof![
        (1u64..100).prop_map(UserAction::FocusPane),
        Just(UserAction::Scroll),
        Just(UserAction::Resize),
        Just(UserAction::Ignore),
    ]
}

/// Generate an Observation with consistent action/pane state.
fn arb_observation() -> impl Strategy<Value = Observation> {
    arb_pane_states(1, 5).prop_flat_map(|panes| {
        let pane_ids: Vec<u64> = panes.iter().map(|p| p.pane_id).collect();
        (
            Just(panes),
            proptest::sample::select(pane_ids.clone()),
            prop_oneof![
                proptest::sample::select(pane_ids).prop_map(UserAction::FocusPane),
                Just(UserAction::Scroll),
                Just(UserAction::Resize),
                Just(UserAction::Ignore),
            ],
        )
            .prop_map(|(panes, current, action)| Observation {
                pane_states: panes,
                current_pane_id: current,
                action,
            })
    })
}

/// Generate an IrlConfig with reasonable values.
fn arb_irl_config() -> impl Strategy<Value = IrlConfig> {
    (
        0.001f64..0.5, // learning_rate
        10usize..200,  // max_iterations
        1e-6f64..1e-2, // convergence_threshold
        0.0f64..0.01,  // l2_regularization
        0.9f64..1.0,   // discount
        1usize..50,    // min_observations
        10usize..500,  // max_trajectory_len
    )
        .prop_map(|(lr, mi, ct, l2, disc, mo, mtl)| IrlConfig {
            learning_rate: lr,
            max_iterations: mi,
            convergence_threshold: ct,
            l2_regularization: l2,
            discount: disc,
            min_observations: mo,
            max_trajectory_len: mtl,
        })
}

// ============================================================================
// Property Tests: Math helpers — dot product
// ============================================================================

proptest! {
    /// Property 1: dot product is commutative: dot(a,b) == dot(b,a)
    #[test]
    fn prop_dot_commutative(
        a in arb_f64_vec(8),
        b in arb_f64_vec(8),
    ) {
        let ab = dot(&a, &b);
        let ba = dot(&b, &a);
        prop_assert!((ab - ba).abs() < 1e-10,
                    "dot(a,b)={} != dot(b,a)={}", ab, ba);
    }

    /// Property 2: dot product with zero vector is zero
    #[test]
    fn prop_dot_zero(a in arb_f64_vec(8)) {
        let zero = vec![0.0; 8];
        let result = dot(&a, &zero);
        prop_assert!((result).abs() < 1e-10,
                    "dot(a, 0) = {} should be 0", result);
    }

    /// Property 3: dot product is non-negative for self: dot(a,a) >= 0
    #[test]
    fn prop_dot_self_nonneg(a in arb_f64_vec(8)) {
        let result = dot(&a, &a);
        prop_assert!(result >= -1e-10,
                    "dot(a,a) = {} should be >= 0", result);
    }

    /// Property 4: dot product bilinearity: dot(a, b+c) ~ dot(a,b) + dot(a,c)
    #[test]
    fn prop_dot_bilinear(
        a in arb_f64_vec(4),
        b in arb_f64_vec(4),
        c in arb_f64_vec(4),
    ) {
        let bc: Vec<f64> = b.iter().zip(c.iter()).map(|(x, y)| x + y).collect();
        let lhs = dot(&a, &bc);
        let rhs = dot(&a, &b) + dot(&a, &c);
        prop_assert!((lhs - rhs).abs() < 1e-8,
                    "bilinearity: {} != {}", lhs, rhs);
    }

    // ========================================================================
    // Property Tests: cosine_similarity
    // ========================================================================

    /// Property 5: cosine similarity of a vector with itself is 1.0
    #[test]
    fn prop_cosine_self_is_one(a in arb_nonzero_vec(8)) {
        let cs = cosine_similarity(&a, &a);
        prop_assert!((cs - 1.0).abs() < 1e-10,
                    "cosine(a, a) = {} should be 1.0", cs);
    }

    /// Property 6: cosine similarity is bounded in [-1, 1]
    #[test]
    fn prop_cosine_bounded(
        a in arb_nonzero_vec(8),
        b in arb_nonzero_vec(8),
    ) {
        let cs = cosine_similarity(&a, &b);
        prop_assert!(cs >= -1.0 - 1e-10 && cs <= 1.0 + 1e-10,
                    "cosine = {} should be in [-1, 1]", cs);
    }

    /// Property 7: cosine similarity is symmetric
    #[test]
    fn prop_cosine_symmetric(
        a in arb_nonzero_vec(8),
        b in arb_nonzero_vec(8),
    ) {
        let ab = cosine_similarity(&a, &b);
        let ba = cosine_similarity(&b, &a);
        prop_assert!((ab - ba).abs() < 1e-10,
                    "cosine(a,b)={} != cosine(b,a)={}", ab, ba);
    }

    /// Property 8: cosine similarity is scale-invariant for positive scale
    #[test]
    fn prop_cosine_scale_invariant(
        a in arb_nonzero_vec(4),
        b in arb_nonzero_vec(4),
        k in 0.1f64..100.0,
    ) {
        let scaled: Vec<f64> = a.iter().map(|x| x * k).collect();
        let cs_orig = cosine_similarity(&a, &b);
        let cs_scaled = cosine_similarity(&scaled, &b);
        prop_assert!((cs_orig - cs_scaled).abs() < 1e-8,
                    "cosine should be scale-invariant: {} vs {}", cs_orig, cs_scaled);
    }

    /// Property 9: cosine similarity with zero vector returns 0
    #[test]
    fn prop_cosine_zero_returns_zero(a in arb_f64_vec(8)) {
        let zero = vec![0.0; 8];
        let cs = cosine_similarity(&a, &zero);
        prop_assert!((cs).abs() < 1e-10,
                    "cosine(a, 0) = {} should be 0", cs);
    }

    // ========================================================================
    // Property Tests: rank_correlation
    // ========================================================================

    /// Property 10: rank correlation of identical vectors is 1.0
    #[test]
    fn prop_rank_corr_self_is_one(a in arb_nonzero_vec(5)
        .prop_filter("needs variance", |v| {
            let first = v[0];
            v.iter().any(|&x| (x - first).abs() > 1e-10)
        })
    ) {
        let rc = rank_correlation(&a, &a);
        prop_assert!((rc - 1.0).abs() < 1e-10,
                    "rank_correlation(a, a) = {} should be 1.0", rc);
    }

    /// Property 11: rank correlation is bounded in [-1, 1]
    #[test]
    fn prop_rank_corr_bounded(
        a in arb_f64_vec(5),
        b in arb_f64_vec(5),
    ) {
        let rc = rank_correlation(&a, &b);
        prop_assert!(rc >= -1.0 - 1e-10 && rc <= 1.0 + 1e-10,
                    "rank_correlation = {} should be in [-1, 1]", rc);
    }

    /// Property 12: rank correlation is symmetric
    #[test]
    fn prop_rank_corr_symmetric(
        a in arb_f64_vec(5),
        b in arb_f64_vec(5),
    ) {
        let ab = rank_correlation(&a, &b);
        let ba = rank_correlation(&b, &a);
        prop_assert!((ab - ba).abs() < 1e-10,
                    "rank_correlation(a,b)={} != rank_correlation(b,a)={}", ab, ba);
    }

    /// Property 13: rank correlation of reversed vector is -1.0
    #[test]
    fn prop_rank_corr_reversed(n in 3usize..10) {
        let a: Vec<f64> = (0..n).map(|i| i as f64).collect();
        let b: Vec<f64> = (0..n).rev().map(|i| i as f64).collect();
        let rc = rank_correlation(&a, &b);
        prop_assert!((rc + 1.0).abs() < 1e-10,
                    "rank_correlation of reversed should be -1, got {}", rc);
    }

    /// Property 14: rank correlation returns 0 for single element
    #[test]
    fn prop_rank_corr_single_element(a in -100.0f64..100.0, b in -100.0f64..100.0) {
        let rc = rank_correlation(&[a], &[b]);
        prop_assert!((rc).abs() < 1e-10,
                    "rank_correlation of length 1 should be 0, got {}", rc);
    }

    // ========================================================================
    // Property Tests: Serde roundtrips
    // ========================================================================

    /// Property 15: PaneState serde roundtrip
    #[test]
    fn prop_pane_state_serde_roundtrip(ps in arb_pane_state()) {
        let json = serde_json::to_string(&ps).unwrap();
        let back: PaneState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, ps.pane_id, "pane_id mismatch");
        prop_assert_eq!(back.has_new_output, ps.has_new_output, "has_new_output mismatch");
        prop_assert_eq!(back.error_count, ps.error_count, "error_count mismatch");
        prop_assert_eq!(back.process_active, ps.process_active, "process_active mismatch");
        prop_assert_eq!(back.interaction_count, ps.interaction_count, "interaction_count mismatch");
        prop_assert!((back.time_since_focus_s - ps.time_since_focus_s).abs() < 1e-10,
                    "time_since_focus_s mismatch");
        prop_assert!((back.output_rate - ps.output_rate).abs() < 1e-10,
                    "output_rate mismatch");
        prop_assert!((back.scroll_depth - ps.scroll_depth).abs() < 1e-10,
                    "scroll_depth mismatch");
    }

    /// Property 16: UserAction serde roundtrip
    #[test]
    fn prop_user_action_serde_roundtrip(action in arb_user_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let back: UserAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, action, "UserAction serde roundtrip failed");
    }

    /// Property 17: Observation serde roundtrip preserves structure
    #[test]
    fn prop_observation_serde_roundtrip(obs in arb_observation()) {
        let json = serde_json::to_string(&obs).unwrap();
        let back: Observation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.current_pane_id, obs.current_pane_id,
                       "current_pane_id mismatch");
        prop_assert_eq!(back.pane_states.len(), obs.pane_states.len(),
                       "pane_states length mismatch");
        prop_assert_eq!(back.action, obs.action,
                       "action mismatch");
    }

    /// Property 18: IrlConfig serde roundtrip
    #[test]
    fn prop_irl_config_serde_roundtrip(config in arb_irl_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: IrlConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((back.learning_rate - config.learning_rate).abs() < 1e-10,
                    "learning_rate mismatch");
        prop_assert_eq!(back.max_iterations, config.max_iterations,
                       "max_iterations mismatch");
        prop_assert!((back.convergence_threshold - config.convergence_threshold).abs() < 1e-15,
                    "convergence_threshold mismatch");
        prop_assert!((back.l2_regularization - config.l2_regularization).abs() < 1e-15,
                    "l2_regularization mismatch");
        prop_assert!((back.discount - config.discount).abs() < 1e-10,
                    "discount mismatch");
        prop_assert_eq!(back.min_observations, config.min_observations,
                       "min_observations mismatch");
        prop_assert_eq!(back.max_trajectory_len, config.max_trajectory_len,
                       "max_trajectory_len mismatch");
    }

    /// Property 19: BatchResult serde roundtrip
    #[test]
    fn prop_batch_result_serde_roundtrip(
        iterations in 0usize..1000,
        converged in any::<bool>(),
        norm in 0.0f64..100.0,
    ) {
        let result = BatchResult {
            iterations,
            converged,
            final_gradient_norm: norm,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: BatchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.iterations, iterations, "iterations mismatch");
        prop_assert_eq!(back.converged, converged, "converged mismatch");
        prop_assert!((back.final_gradient_norm - norm).abs() < 1e-10,
                    "final_gradient_norm mismatch");
    }

    // ========================================================================
    // Property Tests: IrlConfig defaults
    // ========================================================================

    /// Property 20: IrlConfig default values match documented defaults
    #[test]
    fn prop_irl_config_default_values(_dummy in Just(())) {
        let config = IrlConfig::default();
        prop_assert!((config.learning_rate - 0.01).abs() < 1e-10, "learning_rate default");
        prop_assert_eq!(config.max_iterations, 100, "max_iterations default");
        prop_assert!((config.convergence_threshold - 1e-4).abs() < 1e-15, "convergence_threshold default");
        prop_assert!((config.l2_regularization - 0.001).abs() < 1e-10, "l2_regularization default");
        prop_assert!((config.discount - 0.99).abs() < 1e-10, "discount default");
        prop_assert_eq!(config.min_observations, 20, "min_observations default");
        prop_assert_eq!(config.max_trajectory_len, 1000, "max_trajectory_len default");
    }

    /// Property 21: IrlConfig deserializes from empty JSON using defaults
    #[test]
    fn prop_irl_config_default_from_empty_json(_dummy in Just(())) {
        let config: IrlConfig = serde_json::from_str("{}").unwrap();
        let default = IrlConfig::default();
        prop_assert!((config.learning_rate - default.learning_rate).abs() < 1e-10,
                    "empty JSON should use default learning_rate");
        prop_assert_eq!(config.max_iterations, default.max_iterations,
                       "empty JSON should use default max_iterations");
    }

    // ========================================================================
    // Property Tests: extract_features
    // ========================================================================

    /// Property 22: extract_features always returns NUM_FEATURES elements
    #[test]
    fn prop_features_correct_length(obs in arb_observation()) {
        let features = extract_features(&obs, &obs.action);
        prop_assert_eq!(features.len(), NUM_FEATURES,
                       "feature vector should have {} elements", NUM_FEATURES);
    }

    /// Property 23: extract_features is deterministic
    #[test]
    fn prop_features_deterministic(obs in arb_observation()) {
        let f1 = extract_features(&obs, &obs.action);
        let f2 = extract_features(&obs, &obs.action);
        for i in 0..NUM_FEATURES {
            prop_assert!((f1[i] - f2[i]).abs() < 1e-15,
                        "feature {} differs: {} vs {}", i, f1[i], f2[i]);
        }
    }

    /// Property 24: all features are finite
    #[test]
    fn prop_features_all_finite(obs in arb_observation()) {
        let features = extract_features(&obs, &obs.action);
        for (i, &f) in features.iter().enumerate() {
            prop_assert!(f.is_finite(), "feature {} is not finite: {}", i, f);
        }
    }

    /// Property 25: is_switch feature is 1.0 only when switching to different pane
    #[test]
    fn prop_features_is_switch_logic(obs in arb_observation()) {
        // Test FocusPane to a different pane
        let other_id = obs.pane_states.iter()
            .find(|p| p.pane_id != obs.current_pane_id)
            .map(|p| p.pane_id);

        if let Some(other) = other_id {
            let f_switch = extract_features(&obs, &UserAction::FocusPane(other));
            prop_assert!((f_switch[7] - 1.0).abs() < 1e-10,
                        "is_switch should be 1.0 when switching panes, got {}", f_switch[7]);
        }

        // Test staying on current pane
        let f_stay = extract_features(&obs, &UserAction::FocusPane(obs.current_pane_id));
        prop_assert!((f_stay[7]).abs() < 1e-10,
                    "is_switch should be 0.0 when focusing current pane, got {}", f_stay[7]);

        // Test non-focus actions
        let f_scroll = extract_features(&obs, &UserAction::Scroll);
        prop_assert!((f_scroll[7]).abs() < 1e-10,
                    "is_switch should be 0.0 for Scroll, got {}", f_scroll[7]);

        let f_ignore = extract_features(&obs, &UserAction::Ignore);
        prop_assert!((f_ignore[7]).abs() < 1e-10,
                    "is_switch should be 0.0 for Ignore, got {}", f_ignore[7]);
    }

    /// Property 26: missing target pane yields zero for target features
    #[test]
    fn prop_features_missing_target_zeros(obs in arb_observation()) {
        let missing_id = 99999u64;
        let f = extract_features(&obs, &UserAction::FocusPane(missing_id));
        // Target pane features (0-4) should be 0.0 when pane is missing
        prop_assert!((f[0]).abs() < 1e-10, "has_output should be 0 for missing pane");
        prop_assert!((f[1]).abs() < 1e-10, "tsf should be 0 for missing pane");
        prop_assert!((f[2]).abs() < 1e-10, "rate should be 0 for missing pane");
        prop_assert!((f[3]).abs() < 1e-10, "error should be 0 for missing pane");
        prop_assert!((f[4]).abs() < 1e-10, "process_active should be 0 for missing pane");
    }

    // ========================================================================
    // Property Tests: RewardFunction
    // ========================================================================

    /// Property 27: new RewardFunction has zero theta and zero observation count
    #[test]
    fn prop_reward_new_is_zero(_dummy in Just(())) {
        let rf = RewardFunction::new();
        prop_assert_eq!(rf.observation_count, 0, "new reward should have 0 observations");
        for (i, &t) in rf.theta.iter().enumerate() {
            prop_assert!((t).abs() < 1e-15,
                        "theta[{}] should be 0 in new reward, got {}", i, t);
        }
    }

    /// Property 28: reward is dot product of theta and features
    #[test]
    fn prop_reward_is_dot_product(
        theta in arb_feature_array(),
        features in arb_feature_array(),
    ) {
        let mut rf = RewardFunction::new();
        rf.theta = theta;
        let r = rf.reward(&features);
        let expected = dot(&theta, &features);
        prop_assert!((r - expected).abs() < 1e-10,
                    "reward {} != dot product {}", r, expected);
    }

    /// Property 29: rank_panes returns all pane IDs from observation
    #[test]
    fn prop_rank_panes_returns_all_ids(obs in arb_observation()) {
        let rf = RewardFunction::new();
        let rankings = rf.rank_panes(&obs);
        let ranked_ids: Vec<u64> = rankings.iter().map(|(id, _)| *id).collect();
        for ps in &obs.pane_states {
            prop_assert!(ranked_ids.contains(&ps.pane_id),
                        "pane_id {} missing from rankings", ps.pane_id);
        }
        prop_assert_eq!(rankings.len(), obs.pane_states.len(),
                       "rankings len {} != pane_states len {}",
                       rankings.len(), obs.pane_states.len());
    }

    /// Property 30: rank_panes returns sorted descending by reward
    #[test]
    fn prop_rank_panes_sorted_descending(
        theta in arb_feature_array(),
        obs in arb_observation(),
    ) {
        let mut rf = RewardFunction::new();
        rf.theta = theta;
        let rankings = rf.rank_panes(&obs);
        for window in rankings.windows(2) {
            prop_assert!(window[0].1 >= window[1].1,
                        "rankings not sorted: {} < {}", window[0].1, window[1].1);
        }
    }

    /// Property 31: policy probabilities sum to 1.0
    #[test]
    fn prop_policy_sums_to_one(
        theta in arb_feature_array(),
        obs in arb_observation(),
    ) {
        let mut rf = RewardFunction::new();
        rf.theta = theta;
        let policy = rf.policy(&obs);
        if !policy.is_empty() {
            let sum: f64 = policy.iter().map(|(_, p)| p).sum();
            prop_assert!((sum - 1.0).abs() < 1e-8,
                        "policy probabilities sum to {} instead of 1.0", sum);
        }
    }

    /// Property 32: all policy probabilities are non-negative
    #[test]
    fn prop_policy_all_nonneg(
        theta in arb_feature_array(),
        obs in arb_observation(),
    ) {
        let mut rf = RewardFunction::new();
        rf.theta = theta;
        let policy = rf.policy(&obs);
        for (action, prob) in &policy {
            prop_assert!(*prob >= -1e-10,
                        "policy prob for {:?} is negative: {}", action, prob);
        }
    }

    /// Property 33: policy with zero theta gives uniform distribution
    #[test]
    fn prop_policy_zero_theta_uniform(obs in arb_observation()) {
        let rf = RewardFunction::new(); // theta = [0; N]
        let policy = rf.policy(&obs);
        if policy.len() > 1 {
            let first_prob = policy[0].1;
            for (_, prob) in &policy {
                prop_assert!((prob - first_prob).abs() < 1e-8,
                            "zero-theta policy should be uniform: {} vs {}", first_prob, prob);
            }
        }
    }

    /// Property 34: demo_feature_expectation starts at zero for new reward
    #[test]
    fn prop_demo_expectation_starts_zero(_dummy in Just(())) {
        let rf = RewardFunction::new();
        let dfe = rf.demo_feature_expectation();
        for (i, &v) in dfe.iter().enumerate() {
            prop_assert!((v).abs() < 1e-15,
                        "demo_feature_expectation[{}] should be 0 for new reward, got {}", i, v);
        }
    }

    // ========================================================================
    // Property Tests: MaxEntIrl
    // ========================================================================

    /// Property 35: MaxEntIrl starts with zero observations
    #[test]
    fn prop_irl_starts_empty(config in arb_irl_config()) {
        let irl = MaxEntIrl::new(config);
        prop_assert_eq!(irl.observation_count(), 0,
                       "new IRL should have 0 observations");
    }

    /// Property 36: observe increments observation count
    #[test]
    fn prop_irl_observe_increments(obs in arb_observation()) {
        let config = IrlConfig {
            min_observations: 1000, // prevent gradient updates
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);
        for i in 1..=5 {
            irl.observe(obs.clone());
            prop_assert_eq!(irl.observation_count(), i,
                           "observation count should be {} after {} observes", i, i);
        }
    }

    /// Property 37: ring buffer respects max_trajectory_len
    #[test]
    fn prop_irl_ring_buffer_bounded(
        obs in arb_observation(),
        max_len in 3usize..20,
    ) {
        let config = IrlConfig {
            min_observations: 10000,
            max_trajectory_len: max_len,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);
        let count = max_len * 3;
        for _ in 0..count {
            irl.observe(obs.clone());
        }
        prop_assert!(irl.trajectory().len() <= max_len,
                    "trajectory len {} exceeds max {}", irl.trajectory().len(), max_len);
    }

    /// Property 38: batch_update before min_observations returns early
    #[test]
    fn prop_irl_batch_before_min_returns_early(
        obs in arb_observation(),
        min_obs in 10usize..50,
    ) {
        let config = IrlConfig {
            min_observations: min_obs,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);
        // Add fewer than min
        for _ in 0..(min_obs - 1) {
            irl.observe(obs.clone());
        }
        let result = irl.batch_update();
        prop_assert_eq!(result.iterations, 0,
                       "batch_update should return 0 iterations before min_observations");
        prop_assert!(!result.converged,
                    "batch_update should not converge before min_observations");
    }

    /// Property 39: observe returns false before min_observations threshold
    #[test]
    fn prop_irl_observe_no_update_before_min(obs in arb_observation()) {
        let config = IrlConfig {
            min_observations: 50,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);
        for _ in 0..49 {
            let updated = irl.observe(obs.clone());
            prop_assert!(!updated,
                        "observe should not trigger gradient update before min_observations");
        }
    }

    /// Property 40: observation_count equals reward.observation_count
    #[test]
    fn prop_irl_obs_count_consistent(obs in arb_observation()) {
        let config = IrlConfig {
            min_observations: 10000,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);
        for _ in 0..10 {
            irl.observe(obs.clone());
        }
        prop_assert_eq!(irl.reward.observation_count, 10,
                       "reward.observation_count should equal total observations");
    }

    // ========================================================================
    // Property Tests: PreferenceMonitor
    // ========================================================================

    /// Property 41: PreferenceMonitor record increases observation count
    #[test]
    fn prop_monitor_record_increases_count(obs in arb_observation()) {
        let config = IrlConfig {
            min_observations: 10000,
            ..IrlConfig::default()
        };
        let mut monitor = PreferenceMonitor::new(config);
        for i in 1..=5 {
            monitor.record(obs.clone());
            prop_assert_eq!(monitor.irl.observation_count(), i,
                           "observation count should be {} after {} records", i, i);
        }
    }

    /// Property 42: priority_scores returns entries for observed panes
    #[test]
    fn prop_monitor_priority_tracks_panes(obs in arb_observation()) {
        let config = IrlConfig {
            min_observations: 10000,
            ..IrlConfig::default()
        };
        let mut monitor = PreferenceMonitor::new(config);
        monitor.record(obs.clone());
        let scores = monitor.priority_scores();
        let score_ids: Vec<u64> = scores.iter().map(|(id, _)| *id).collect();
        for ps in &obs.pane_states {
            prop_assert!(score_ids.contains(&ps.pane_id),
                        "pane {} should be tracked after record", ps.pane_id);
        }
    }

    /// Property 43: priority_scores are sorted descending
    #[test]
    fn prop_monitor_priority_sorted(obs in arb_observation()) {
        let config = IrlConfig {
            min_observations: 10000,
            ..IrlConfig::default()
        };
        let mut monitor = PreferenceMonitor::new(config);
        for _ in 0..3 {
            monitor.record(obs.clone());
        }
        let scores = monitor.priority_scores();
        for window in scores.windows(2) {
            prop_assert!(window[0].1 >= window[1].1,
                        "priority scores not sorted: {} < {}", window[0].1, window[1].1);
        }
    }

    /// Property 44: detect_neglected_panes returns subset of observation pane IDs
    #[test]
    fn prop_detect_neglected_subset(obs in arb_observation()) {
        let config = IrlConfig {
            min_observations: 10000,
            ..IrlConfig::default()
        };
        let monitor = PreferenceMonitor::new(config);
        let neglected = monitor.detect_neglected_panes(&obs, 10, 60.0);
        let obs_ids: Vec<u64> = obs.pane_states.iter().map(|p| p.pane_id).collect();
        for id in &neglected {
            prop_assert!(obs_ids.contains(id),
                        "neglected pane {} not in observation pane IDs", id);
        }
    }

    /// Property 45: predict_next_focus never returns current_pane_id
    #[test]
    fn prop_predict_excludes_current(obs in arb_observation()) {
        let config = IrlConfig {
            min_observations: 10000,
            ..IrlConfig::default()
        };
        let monitor = PreferenceMonitor::new(config);
        if let Some(predicted) = monitor.predict_next_focus(&obs) {
            prop_assert!(predicted != obs.current_pane_id,
                        "predict_next_focus should not return current pane {}", obs.current_pane_id);
        }
    }

    // ========================================================================
    // Property Tests: RewardFunction serde
    // ========================================================================

    /// Property 46: RewardFunction serde roundtrip preserves theta and count
    #[test]
    fn prop_reward_serde_roundtrip(
        theta in arb_feature_array(),
        count in 0usize..1000,
    ) {
        let mut rf = RewardFunction::new();
        rf.theta = theta;
        rf.observation_count = count;
        let json = serde_json::to_string(&rf).unwrap();
        let back: RewardFunction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.observation_count, count, "observation_count mismatch");
        for i in 0..NUM_FEATURES {
            prop_assert!((back.theta[i] - theta[i]).abs() < 1e-10,
                        "theta[{}] mismatch: {} vs {}", i, back.theta[i], theta[i]);
        }
    }

    /// Property 47: RewardFunction default is same as new
    #[test]
    fn prop_reward_default_eq_new(_dummy in Just(())) {
        let d = RewardFunction::default();
        let n = RewardFunction::new();
        prop_assert_eq!(d.observation_count, n.observation_count, "default vs new count");
        for i in 0..NUM_FEATURES {
            prop_assert!((d.theta[i] - n.theta[i]).abs() < 1e-15,
                        "default vs new theta[{}]", i);
        }
    }

    // ========================================================================
    // Property Tests: Cross-module consistency
    // ========================================================================

    /// Property 48: online_update changes theta when actions produce different features
    #[test]
    fn prop_online_update_changes_theta(obs in arb_observation()) {
        // With zero theta, uniform policy. Gradient = phi_demo - E_policy[phi].
        // This is zero when all actions yield identical features (single-pane, no switch).
        // Require at least 2 panes so FocusPane actions differ.
        if obs.pane_states.len() < 2 {
            return Ok(());
        }

        let config = IrlConfig {
            learning_rate: 0.1,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);
        let before = irl.reward.theta;
        irl.online_update(&obs);
        let after = irl.reward.theta;
        let diff: f64 = before.iter().zip(after.iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        prop_assert!(diff > 1e-15,
                    "online_update should change theta with 2+ panes, diff={}", diff);
    }

    /// Property 49: policy includes FocusPane for each pane plus Scroll/Resize/Ignore
    #[test]
    fn prop_policy_covers_all_actions(obs in arb_observation()) {
        let rf = RewardFunction::new();
        let policy = rf.policy(&obs);
        let expected_len = obs.pane_states.len() + 3; // FocusPane per pane + Scroll + Resize + Ignore
        prop_assert_eq!(policy.len(), expected_len,
                       "policy should have {} actions, got {}", expected_len, policy.len());
    }

    /// Property 50: NUM_FEATURES constant is 8
    #[test]
    fn prop_num_features_is_8(_dummy in Just(())) {
        prop_assert_eq!(NUM_FEATURES, 8, "NUM_FEATURES should be 8");
    }
}
