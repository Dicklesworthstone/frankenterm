//! Benchmarks for MaxEntIRL user preference learning.
//!
//! Bead: wa-283h4.14
//!
//! Performance budgets:
//! - Feature extraction φ(s,a): **< 1us**
//! - Gradient step (10 features): **< 10us**
//! - Full IRL iteration (100 trajectory steps): **< 1ms**
//! - Online update (single observation): **< 5us**
//! - Policy query π(a|s): **< 2us**
//! - Reward ranking (50 panes): **< 50us**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use frankenterm_core::user_preferences::{
    IrlConfig, MaxEntIrl, Observation, PaneState, RewardFunction, UserAction, extract_features,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "feature_extraction",
        budget: "p50 < 1us (extract φ(s,a) for one state-action pair)",
    },
    bench_common::BenchBudget {
        name: "gradient_step",
        budget: "p50 < 10us (one MaxEntIRL gradient update)",
    },
    bench_common::BenchBudget {
        name: "full_iteration",
        budget: "p50 < 1ms (one IRL iteration over 100 trajectory steps)",
    },
    bench_common::BenchBudget {
        name: "online_update",
        budget: "p50 < 5us (incremental θ update from one observation)",
    },
    bench_common::BenchBudget {
        name: "policy_query",
        budget: "p50 < 2us (compute π(a|s) for all actions given θ)",
    },
    bench_common::BenchBudget {
        name: "reward_ranking",
        budget: "p50 < 50us (rank 50 panes by R(s,a) = θᵀφ)",
    },
];

// =============================================================================
// Helpers
// =============================================================================

fn make_pane(id: u64, seed: u64) -> PaneState {
    let r = (seed.wrapping_mul(2654435761) >> 16) as f64 / 65536.0;
    PaneState {
        has_new_output: r > 0.5,
        time_since_focus_s: r * 60.0,
        output_rate: r * 10.0,
        error_count: u32::from(r > 0.8),
        process_active: r > 0.3,
        scroll_depth: r,
        interaction_count: (r * 20.0) as u32,
        pane_id: id,
    }
}

fn make_observation(n_panes: usize, seed: u64) -> Observation {
    let panes: Vec<PaneState> = (0..n_panes as u64)
        .map(|id| make_pane(id, seed.wrapping_add(id)))
        .collect();
    Observation {
        pane_states: panes,
        current_pane_id: 0,
        action: UserAction::FocusPane(1),
    }
}

fn make_trained_reward() -> RewardFunction {
    let mut rf = RewardFunction::new();
    rf.theta = [2.0, -1.0, 0.5, 3.0, 0.1, -0.5, 0.0, 1.0];
    rf
}

fn make_trained_irl(n_obs: usize) -> MaxEntIrl {
    let config = IrlConfig {
        min_observations: 5,
        max_trajectory_len: n_obs + 100,
        learning_rate: 0.01,
        ..IrlConfig::default()
    };
    let mut irl = MaxEntIrl::new(config);
    for i in 0..n_obs {
        let obs = make_observation(5, i as u64 * 7);
        irl.observe(obs);
    }
    irl
}

// =============================================================================
// 1. Feature extraction
// =============================================================================

fn bench_feature_extraction(c: &mut Criterion) {
    let mut group = c.benchmark_group("irl_feature_extraction");

    for n_panes in [3, 10, 20] {
        let obs = make_observation(n_panes, 42);
        let action = UserAction::FocusPane(1);

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::new("panes", n_panes), &n_panes, |b, _| {
            b.iter(|| extract_features(&obs, &action));
        });
    }
    group.finish();
}

// =============================================================================
// 2. Gradient step
// =============================================================================

fn bench_gradient_step(c: &mut Criterion) {
    let mut group = c.benchmark_group("irl_gradient_step");

    for n_obs in [20, 50, 100] {
        let mut irl = make_trained_irl(n_obs);

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::new("obs", n_obs), &n_obs, |b, _| {
            b.iter(|| {
                let obs = make_observation(5, 999);
                irl.observe(obs);
            });
        });
    }
    group.finish();
}

// =============================================================================
// 3. Full iteration (batch update over stored trajectories)
// =============================================================================

fn bench_full_iteration(c: &mut Criterion) {
    let mut group = c.benchmark_group("irl_full_iteration");

    for n_obs in [50, 100, 200] {
        let mut irl = make_trained_irl(n_obs);

        group.throughput(Throughput::Elements(n_obs as u64));
        group.bench_with_input(BenchmarkId::new("traj_len", n_obs), &n_obs, |b, _| {
            b.iter(|| {
                irl.batch_update();
            });
        });
    }
    group.finish();
}

// =============================================================================
// 4. Online update (single observation stochastic gradient)
// =============================================================================

fn bench_online_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("irl_online_update");

    let mut irl = make_trained_irl(50);
    let obs = make_observation(5, 42);

    group.throughput(Throughput::Elements(1));
    group.bench_function("single", |b| {
        b.iter(|| {
            irl.online_update(&obs);
        });
    });
    group.finish();
}

// =============================================================================
// 5. Policy query
// =============================================================================

fn bench_policy_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("irl_policy_query");

    for n_panes in [3, 10, 20] {
        let rf = make_trained_reward();
        let obs = make_observation(n_panes, 42);

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(BenchmarkId::new("panes", n_panes), &n_panes, |b, _| {
            b.iter(|| rf.policy(&obs));
        });
    }
    group.finish();
}

// =============================================================================
// 6. Reward ranking
// =============================================================================

fn bench_reward_ranking(c: &mut Criterion) {
    let mut group = c.benchmark_group("irl_reward_ranking");

    for n_panes in [10, 25, 50] {
        let rf = make_trained_reward();
        let obs = make_observation(n_panes, 42);

        group.throughput(Throughput::Elements(n_panes as u64));
        group.bench_with_input(BenchmarkId::new("panes", n_panes), &n_panes, |b, _| {
            b.iter(|| rf.rank_panes(&obs));
        });
    }
    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("irl", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_feature_extraction,
        bench_gradient_step,
        bench_full_iteration,
        bench_online_update,
        bench_policy_query,
        bench_reward_ranking
);
criterion_main!(benches);
