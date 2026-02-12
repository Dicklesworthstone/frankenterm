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

// =============================================================================
// Property tests
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

        let diff = (r2 - alpha * r1).abs();
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
        prop_assert!(sim >= -1.0 - 1e-10 && sim <= 1.0 + 1e-10,
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
