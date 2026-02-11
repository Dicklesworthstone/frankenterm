//! Benchmarks for causal DAG transfer entropy computation.
//!
//! Performance budgets:
//! - Transfer entropy (pair, 300 samples): **< 500μs**
//! - Permutation test (100 shuffles): **< 50ms**
//! - Full DAG update (50 panes × 300 samples): **< 100ms**

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::causal_dag::{CausalDag, CausalDagConfig, transfer_entropy};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "te_pair_300",
        budget: "p50 < 500us (TE for one pair, 300 samples)",
    },
    bench_common::BenchBudget {
        name: "te_permutation_100",
        budget: "p50 < 50ms (permutation test, 100 shuffles)",
    },
    bench_common::BenchBudget {
        name: "dag_full_update_50_panes",
        budget: "p50 < 100ms (50 panes, 300 samples each)",
    },
];

/// Generate a pseudo-random time series using an LCG.
fn pseudo_random_series(n: usize, seed: u64) -> Vec<f64> {
    let mut state = seed;
    (0..n)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as f64 / (u32::MAX as f64) * 10.0
        })
        .collect()
}

// =============================================================================
// Transfer entropy pair benchmarks
// =============================================================================

fn bench_te_pair(c: &mut Criterion) {
    let mut group = c.benchmark_group("te_pair_300");

    for window in [100, 300, 500] {
        let x = pseudo_random_series(window, 42);
        let y = pseudo_random_series(window, 123);

        group.bench_with_input(
            BenchmarkId::new("independent", window),
            &(x.clone(), y.clone()),
            |b, (x, y)| {
                b.iter(|| transfer_entropy(x, y, 1, 1, 8));
            },
        );

        // Causal: y = lagged x
        let mut y_causal = vec![0.0];
        y_causal.extend_from_slice(&x[..window - 1]);

        group.bench_with_input(
            BenchmarkId::new("causal_lagged", window),
            &(x.clone(), y_causal),
            |b, (x, y)| {
                b.iter(|| transfer_entropy(x, y, 1, 1, 8));
            },
        );
    }

    group.finish();
}

// =============================================================================
// Permutation test benchmarks
// =============================================================================

fn bench_permutation(c: &mut Criterion) {
    let mut group = c.benchmark_group("te_permutation_100");

    let x = pseudo_random_series(300, 42);
    let y = pseudo_random_series(300, 123);
    let te = transfer_entropy(&x, &y, 1, 1, 8);

    for n_perms in [50, 100] {
        group.bench_with_input(BenchmarkId::new("shuffles", n_perms), &n_perms, |b, &n| {
            b.iter(|| frankenterm_core::causal_dag::permutation_test(&x, &y, 1, 1, 8, n, te));
        });
    }

    group.finish();
}

// =============================================================================
// Full DAG update benchmarks
// =============================================================================

fn bench_dag_full_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("dag_full_update_50_panes");

    for n_panes in [10, 25, 50] {
        group.bench_with_input(BenchmarkId::new("panes", n_panes), &n_panes, |b, &n| {
            let config = CausalDagConfig {
                window_size: 300,
                n_permutations: 20, // Reduced for benchmark speed
                n_bins: 8,
                significance_level: 0.05,
                min_te_bits: 0.01,
                ..Default::default()
            };
            let mut dag = CausalDag::new(config);

            for pane_id in 0..n as u64 {
                dag.register_pane(pane_id);
                let series = pseudo_random_series(300, pane_id * 1000 + 42);
                for val in series {
                    dag.observe(pane_id, val);
                }
            }

            b.iter(|| dag.update_dag());
        });
    }

    group.finish();
}

// =============================================================================
// Criterion groups and main
// =============================================================================

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("causal_dag", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_te_pair,
        bench_permutation,
        bench_dag_full_update
);
criterion_main!(benches);
