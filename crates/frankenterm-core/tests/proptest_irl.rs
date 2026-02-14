//! Property-based tests for MaxEntIRL user preference learning.
//!
//! Bead: wa-283h4.14

use proptest::prelude::*;

use frankenterm_core::user_preferences::{
    IrlConfig, MaxEntIrl, NUM_FEATURES, Observation, PaneState, RewardFunction, UserAction,
    cosine_similarity, dot, extract_features,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_pane_state(id: u64) -> impl Strategy<Value = PaneState> {
    (
        any::<bool>(),  // has_new_output
        0.0..3600.0f64, // time_since_focus_s
        0.0..100.0f64,  // output_rate
        0u32..10,       // error_count
        any::<bool>(),  // process_active
        0.0..1.0f64,    // scroll_depth
        0u32..100,      // interaction_count
    )
        .prop_map(
            move |(has_new_output, tsf, rate, err, proc, scroll, interact)| PaneState {
                has_new_output,
                time_since_focus_s: tsf,
                output_rate: rate,
                error_count: err,
                process_active: proc,
                scroll_depth: scroll,
                interaction_count: interact,
                pane_id: id,
            },
        )
}

fn arb_observation(n_panes: usize) -> impl Strategy<Value = Observation> {
    let panes: Vec<_> = (0..n_panes as u64).map(arb_pane_state).collect();
    (panes, 0..n_panes).prop_map(move |(pane_states, current_idx)| {
        let current_pane_id = pane_states[current_idx].pane_id;
        let target = if n_panes > 1 {
            let other = (current_idx + 1) % n_panes;
            pane_states[other].pane_id
        } else {
            current_pane_id
        };
        Observation {
            pane_states,
            current_pane_id,
            action: UserAction::FocusPane(target),
        }
    })
}

fn arb_theta() -> impl Strategy<Value = [f64; NUM_FEATURES]> {
    prop::array::uniform8(-5.0..5.0f64)
}

fn arb_user_action(n_panes: usize) -> impl Strategy<Value = UserAction> {
    prop_oneof![
        (0..n_panes as u64).prop_map(UserAction::FocusPane),
        Just(UserAction::Scroll),
        Just(UserAction::Resize),
        Just(UserAction::Ignore),
    ]
}

fn arb_irl_config() -> impl Strategy<Value = IrlConfig> {
    (
        0.001_f64..0.5, // learning_rate
        1_usize..200,   // max_iterations
        1e-8_f64..1e-2, // convergence_threshold
        0.0_f64..0.1,   // l2_regularization
        0.9_f64..1.0,   // discount
        1_usize..50,    // min_observations
        10_usize..500,  // max_trajectory_len
    )
        .prop_map(|(lr, mi, ct, l2, disc, mo, mt)| IrlConfig {
            learning_rate: lr,
            max_iterations: mi,
            convergence_threshold: ct,
            l2_regularization: l2,
            discount: disc,
            min_observations: mo,
            max_trajectory_len: mt,
        })
}

// =============================================================================
// Existing property tests (original 9)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn features_always_finite(obs in arb_observation(5)) {
        let f = extract_features(&obs, &obs.action);
        for (i, val) in f.iter().enumerate() {
            prop_assert!(val.is_finite(), "Feature {} is not finite: {}", i, val);
        }
    }

    #[test]
    fn reward_linearity(
        theta in arb_theta(),
        features in prop::array::uniform8(0.0..10.0f64),
        alpha in 0.1..10.0f64,
    ) {
        let mut rf = RewardFunction::new();
        rf.theta = theta;

        let r1 = rf.reward(&features);
        let scaled: [f64; NUM_FEATURES] = std::array::from_fn(|i| features[i] * alpha);
        let r2 = rf.reward(&scaled);

        let diff = alpha.mul_add(-r1, r2).abs();
        prop_assert!(diff < 1e-8, "Linearity violated: {} vs {} * {}", r2, alpha, r1);
    }

    #[test]
    fn reward_monotonic_positive_features(
        theta in arb_theta(),
        base in prop::array::uniform8(0.0..5.0f64),
        boost_idx in 0..NUM_FEATURES,
        boost_amount in 0.01..5.0f64,
    ) {
        let mut rf = RewardFunction::new();
        rf.theta = theta;

        let r_base = rf.reward(&base);
        let mut boosted = base;
        boosted[boost_idx] += boost_amount;
        let r_boosted = rf.reward(&boosted);

        if theta[boost_idx] > 0.0 {
            prop_assert!(r_boosted > r_base, "Positive weight but reward didn't increase");
        } else if theta[boost_idx] < 0.0 {
            prop_assert!(r_boosted < r_base, "Negative weight but reward didn't decrease");
        }
    }

    #[test]
    fn policy_sums_to_one(
        theta in arb_theta(),
        obs in arb_observation(4),
    ) {
        let mut rf = RewardFunction::new();
        rf.theta = theta;
        let policy = rf.policy(&obs);

        prop_assert!(!policy.is_empty(), "Policy should not be empty");

        let sum: f64 = policy.iter().map(|(_, p)| p).sum();
        prop_assert!(
            (sum - 1.0).abs() < 1e-8,
            "Policy sum {} != 1.0",
            sum
        );

        for (_, p) in &policy {
            prop_assert!(*p >= 0.0, "Negative probability: {}", p);
        }
    }

    #[test]
    fn cosine_sim_bounded(
        a in prop::array::uniform8(-10.0..10.0f64),
        b in prop::array::uniform8(-10.0..10.0f64),
    ) {
        let sim = cosine_similarity(&a, &b);
        prop_assert!((-1.0 - 1e-10..=1.0 + 1e-10).contains(&sim),
            "Cosine similarity {} out of bounds", sim);
    }

    #[test]
    fn dot_product_commutative(
        a in prop::array::uniform8(-10.0..10.0f64),
        b in prop::array::uniform8(-10.0..10.0f64),
    ) {
        let ab = dot(&a, &b);
        let ba = dot(&b, &a);
        prop_assert!((ab - ba).abs() < 1e-10, "Dot not commutative: {} vs {}", ab, ba);
    }

    #[test]
    fn online_update_changes_theta(obs in arb_observation(3)) {
        let config = IrlConfig {
            learning_rate: 0.1,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);

        let before = irl.reward.theta;
        irl.online_update(&obs);
        let after = irl.reward.theta;

        let any_changed = before.iter().zip(after.iter()).any(|(b, a)| (b - a).abs() > 1e-15);
        prop_assert!(any_changed, "Online update had no effect on theta");
    }

    #[test]
    fn irl_serde_roundtrip(theta in arb_theta()) {
        let mut rf = RewardFunction::new();
        rf.theta = theta;

        let json = serde_json::to_string(&rf).expect("serialize");
        let parsed: RewardFunction = serde_json::from_str(&json).expect("deserialize");

        for i in 0..NUM_FEATURES {
            prop_assert!(
                (parsed.theta[i] - rf.theta[i]).abs() < 1e-15,
                "Theta[{}] mismatch after roundtrip: {} vs {}",
                i, parsed.theta[i], rf.theta[i]
            );
        }
    }

    #[test]
    fn ring_buffer_bounded(
        n_obs in 10usize..200,
        max_len in 5usize..50,
    ) {
        let config = IrlConfig {
            min_observations: 1,
            max_trajectory_len: max_len,
            learning_rate: 0.001,
            ..IrlConfig::default()
        };
        let mut irl = MaxEntIrl::new(config);

        for i in 0..n_obs {
            let panes = vec![PaneState {
                has_new_output: true,
                time_since_focus_s: 1.0,
                output_rate: 1.0,
                error_count: 0,
                process_active: true,
                scroll_depth: 0.0,
                interaction_count: 0,
                pane_id: (i % 3) as u64,
            }];
            irl.observe(Observation {
                pane_states: panes,
                current_pane_id: 0,
                action: UserAction::FocusPane((i % 3) as u64),
            });
        }

        prop_assert!(
            irl.trajectory().len() <= max_len,
            "Trajectory length {} exceeds max {}",
            irl.trajectory().len(), max_len
        );
    }
}

// =============================================================================
// Serde roundtrip tests for types
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// IrlConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_irl_config_serde(config in arb_irl_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: IrlConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((back.learning_rate - config.learning_rate).abs() < 1e-12,
            "learning_rate: {} vs {}", back.learning_rate, config.learning_rate);
        prop_assert_eq!(back.max_iterations, config.max_iterations);
        prop_assert!((back.discount - config.discount).abs() < 1e-12,
            "discount: {} vs {}", back.discount, config.discount);
        prop_assert_eq!(back.min_observations, config.min_observations);
        prop_assert_eq!(back.max_trajectory_len, config.max_trajectory_len);
    }

    /// IrlConfig default serde roundtrip.
    #[test]
    fn prop_irl_config_default_roundtrip(_dummy in 0..1_u8) {
        let config = IrlConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: IrlConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((back.learning_rate - config.learning_rate).abs() < 1e-15);
        prop_assert_eq!(back.max_iterations, config.max_iterations);
        prop_assert_eq!(back.min_observations, config.min_observations);
    }

    /// IrlConfig deserializes from empty JSON with defaults (serde(default)).
    #[test]
    fn prop_irl_config_from_empty_json(_dummy in 0..1_u8) {
        let back: IrlConfig = serde_json::from_str("{}").unwrap();
        let expected = IrlConfig::default();
        prop_assert!((back.learning_rate - expected.learning_rate).abs() < 1e-15);
        prop_assert_eq!(back.max_iterations, expected.max_iterations);
        prop_assert_eq!(back.min_observations, expected.min_observations);
        prop_assert_eq!(back.max_trajectory_len, expected.max_trajectory_len);
    }

    /// PaneState serde roundtrip preserves all fields.
    #[test]
    fn prop_pane_state_serde(state in arb_pane_state(42)) {
        let json = serde_json::to_string(&state).unwrap();
        let back: PaneState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, state.pane_id);
        prop_assert_eq!(back.has_new_output, state.has_new_output);
        prop_assert_eq!(back.error_count, state.error_count);
        prop_assert_eq!(back.process_active, state.process_active);
        prop_assert_eq!(back.interaction_count, state.interaction_count);
        prop_assert!((back.time_since_focus_s - state.time_since_focus_s).abs() < 1e-10,
            "time_since_focus_s: {} vs {}", back.time_since_focus_s, state.time_since_focus_s);
        prop_assert!((back.output_rate - state.output_rate).abs() < 1e-10,
            "output_rate: {} vs {}", back.output_rate, state.output_rate);
        prop_assert!((back.scroll_depth - state.scroll_depth).abs() < 1e-10,
            "scroll_depth: {} vs {}", back.scroll_depth, state.scroll_depth);
    }

    /// UserAction serde roundtrip for all variants.
    #[test]
    fn prop_user_action_serde(action in arb_user_action(10)) {
        let json = serde_json::to_string(&action).unwrap();
        let back: UserAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, action);
    }

    /// Observation serde roundtrip.
    #[test]
    fn prop_observation_serde(obs in arb_observation(3)) {
        let json = serde_json::to_string(&obs).unwrap();
        let back: Observation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_states.len(), obs.pane_states.len());
        prop_assert_eq!(back.current_pane_id, obs.current_pane_id);
        prop_assert_eq!(back.action, obs.action);
    }

    /// MaxEntIrl serde roundtrip (fresh instance with config).
    #[test]
    fn prop_maxent_irl_serde(config in arb_irl_config()) {
        let irl = MaxEntIrl::new(config);
        let json = serde_json::to_string(&irl).unwrap();
        let back: MaxEntIrl = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.observation_count(), 0);
        prop_assert!((back.config.learning_rate - irl.config.learning_rate).abs() < 1e-12);
    }
}

// =============================================================================
// Mathematical property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Zero theta always gives zero reward regardless of features.
    #[test]
    fn prop_zero_theta_zero_reward(
        features in prop::array::uniform8(0.0..100.0f64),
    ) {
        let rf = RewardFunction::new();
        let r = rf.reward(&features);
        prop_assert!(r.abs() < 1e-15, "Zero theta should give zero reward, got {}", r);
    }

    /// rank_panes returns one entry per pane.
    #[test]
    fn prop_rank_panes_returns_all(
        theta in arb_theta(),
        obs in arb_observation(5),
    ) {
        let mut rf = RewardFunction::new();
        rf.theta = theta;
        let rankings = rf.rank_panes(&obs);
        prop_assert_eq!(rankings.len(), obs.pane_states.len(),
            "rank_panes should return one entry per pane");
    }

    /// rank_panes results are sorted by reward in descending order.
    #[test]
    fn prop_rank_panes_sorted_descending(
        theta in arb_theta(),
        obs in arb_observation(4),
    ) {
        let mut rf = RewardFunction::new();
        rf.theta = theta;
        let rankings = rf.rank_panes(&obs);
        for window in rankings.windows(2) {
            prop_assert!(window[0].1 >= window[1].1,
                "rankings should be descending: {} < {}", window[0].1, window[1].1);
        }
    }

    /// demo_feature_expectation with zero observations returns all zeros.
    #[test]
    fn prop_demo_feature_expectation_zero(_dummy in 0..1_u8) {
        let rf = RewardFunction::new();
        let mean = rf.demo_feature_expectation();
        for (i, val) in mean.iter().enumerate() {
            prop_assert!(val.abs() < 1e-15,
                "demo_feature_expectation[{}] should be 0 with no obs, got {}", i, val);
        }
    }

    /// cosine_similarity of a non-zero vector with itself is ~1.0.
    #[test]
    fn prop_cosine_self_is_one(
        a in prop::array::uniform8(0.1..10.0f64),
    ) {
        let sim = cosine_similarity(&a, &a);
        prop_assert!((sim - 1.0).abs() < 1e-10,
            "cosine_similarity(a, a) should be ~1.0, got {}", sim);
    }

    /// dot product with zero vector is zero.
    #[test]
    fn prop_dot_with_zero_is_zero(
        a in prop::array::uniform8(-10.0..10.0f64),
    ) {
        let zero = [0.0; NUM_FEATURES];
        let result = dot(&a, &zero);
        prop_assert!(result.abs() < 1e-15, "dot(a, 0) should be 0, got {}", result);
    }

    /// Features for Ignore action have is_switch = 0.
    #[test]
    fn prop_features_ignore_no_switch(obs in arb_observation(3)) {
        let f = extract_features(&obs, &UserAction::Ignore);
        // Feature 7 is is_switch (0 when action doesn't change focus)
        prop_assert!(f[7].abs() < 1e-15,
            "Ignore action should have is_switch=0, got {}", f[7]);
    }

    /// Features for FocusPane(current) have is_switch = 0 (no change).
    #[test]
    fn prop_features_focus_self_no_switch(obs in arb_observation(3)) {
        let action = UserAction::FocusPane(obs.current_pane_id);
        let f = extract_features(&obs, &action);
        prop_assert!(f[7].abs() < 1e-15,
            "Focusing current pane should have is_switch=0, got {}", f[7]);
    }
}
