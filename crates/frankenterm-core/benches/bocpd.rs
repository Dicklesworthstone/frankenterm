//! Benchmarks for Bayesian Online Change-Point Detection (BOCPD).
//!
//! Performance budgets:
//! - Single observation update: **< 50μs**
//! - Feature vector compute (1KB): **< 100μs**
//! - Batch 100 panes update: **< 5ms**
//! - Snapshot serialization: **< 500μs**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::bocpd::{BocpdConfig, BocpdManager, BocpdModel, OutputFeatures};
use std::time::Duration;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "bocpd_single_update",
        budget: "p50 < 50us (single observation update)",
    },
    bench_common::BenchBudget {
        name: "bocpd_feature_vector",
        budget: "p50 < 100us (feature vector compute per 1KB)",
    },
    bench_common::BenchBudget {
        name: "bocpd_batch_100_panes",
        budget: "p50 < 5ms (batch 100 pane updates)",
    },
    bench_common::BenchBudget {
        name: "bocpd_snapshot",
        budget: "p50 < 500us (snapshot serialization)",
    },
];

// =============================================================================
// Single-observation update benchmarks
// =============================================================================

fn bench_single_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("bocpd_single_update");

    // Cold model (no history)
    group.bench_function("cold_model", |b| {
        b.iter(|| {
            let mut model = BocpdModel::new(BocpdConfig::default());
            model.update(42.0);
        });
    });

    // Warm model — pre-fed with N observations
    for warmup in [50, 100, 200] {
        group.bench_with_input(BenchmarkId::new("warm_model", warmup), &warmup, |b, &n| {
            let mut model = BocpdModel::new(BocpdConfig::default());
            for i in 0..n {
                model.update(i as f64 * 0.1);
            }
            let mut counter = n as f64;
            b.iter(|| {
                counter += 0.1;
                model.update(counter);
            });
        });
    }

    // Regime change scenario: stable → spike
    group.bench_function("regime_change_spike", |b| {
        b.iter(|| {
            let mut model = BocpdModel::new(BocpdConfig {
                hazard_rate: 0.01,
                detection_threshold: 0.5,
                min_observations: 10,
                max_run_length: 100,
            });
            // Stable regime (low values)
            for i in 0..30 {
                model.update(10.0 + (i as f64) * 0.01);
            }
            // Spike regime (high values)
            for i in 0..20 {
                model.update(500.0 + (i as f64) * 0.1);
            }
        });
    });

    group.finish();
}

// =============================================================================
// Feature vector computation benchmarks
// =============================================================================

fn bench_feature_vector(c: &mut Criterion) {
    let mut group = c.benchmark_group("bocpd_feature_vector");

    // Generate realistic terminal output of varying sizes
    let sizes = [256, 1024, 4096];

    for size in sizes {
        let text = generate_terminal_output(size);
        group.throughput(Throughput::Bytes(text.len() as u64));
        group.bench_with_input(BenchmarkId::new("compute", size), &text, |b, text| {
            let elapsed = Duration::from_millis(500);
            b.iter(|| OutputFeatures::compute(text, elapsed));
        });
    }

    // Measure entropy computation specifically via large output
    let big_text = generate_terminal_output(8192);
    group.bench_function("compute_8kb", |b| {
        let elapsed = Duration::from_millis(1000);
        b.iter(|| OutputFeatures::compute(&big_text, elapsed));
    });

    group.finish();
}

// =============================================================================
// Batch multi-pane benchmarks
// =============================================================================

fn bench_batch_100_panes(c: &mut Criterion) {
    let mut group = c.benchmark_group("bocpd_batch_100_panes");

    // Sequential update of 100 pane models
    group.bench_function("sequential_update", |b| {
        let config = BocpdConfig::default();
        let mut manager = BocpdManager::new(config);
        for pane_id in 0..100 {
            manager.register_pane(pane_id);
        }
        // Warm up each pane with 10 observations
        for pane_id in 0..100 {
            for i in 0..10 {
                let features = OutputFeatures {
                    output_rate: 10.0 + (i as f64) * 0.1,
                    byte_rate: 500.0 + (i as f64) * 5.0,
                    entropy: 4.5,
                    unique_line_ratio: 0.8,
                    ansi_density: 0.05,
                };
                manager.observe(pane_id, features);
            }
        }

        let mut counter = 0.0f64;
        b.iter(|| {
            counter += 0.1;
            for pane_id in 0..100 {
                let features = OutputFeatures {
                    output_rate: 10.0 + counter + (pane_id as f64) * 0.01,
                    byte_rate: 500.0 + counter * 5.0,
                    entropy: 4.5,
                    unique_line_ratio: 0.8,
                    ansi_density: 0.05,
                };
                manager.observe(pane_id, features);
            }
        });
    });

    // Snapshot serialization
    group.bench_function("snapshot_serialize", |b| {
        let config = BocpdConfig::default();
        let mut manager = BocpdManager::new(config);
        for pane_id in 0..100 {
            manager.register_pane(pane_id);
            for i in 0..25 {
                let features = OutputFeatures {
                    output_rate: 10.0 + (i as f64) * 0.5,
                    byte_rate: 500.0,
                    entropy: 4.0,
                    unique_line_ratio: 0.7,
                    ansi_density: 0.03,
                };
                manager.observe(pane_id, features);
            }
        }

        b.iter(|| {
            let snapshot = manager.snapshot();
            serde_json::to_string(&snapshot).unwrap()
        });
    });

    // Register + unregister churn
    group.bench_function("register_unregister_churn", |b| {
        b.iter(|| {
            let config = BocpdConfig::default();
            let mut manager = BocpdManager::new(config);
            for pane_id in 0..100 {
                manager.register_pane(pane_id);
            }
            for pane_id in 0..50 {
                manager.unregister_pane(pane_id);
            }
            for pane_id in 100..150 {
                manager.register_pane(pane_id);
            }
        });
    });

    group.finish();
}

// =============================================================================
// Helpers
// =============================================================================

fn generate_terminal_output(approx_bytes: usize) -> String {
    let lines = [
        "$ cargo test --lib -- bocpd\n",
        "   Compiling frankenterm-core v0.1.0\n",
        "    Finished test [unoptimized + debuginfo] target(s) in 3.42s\n",
        "     Running unittests src/lib.rs\n",
        "test bocpd::tests::basic_model_creation ... ok\n",
        "test bocpd::tests::change_point_detection ... ok\n",
        "\x1b[32mtest result: ok.\x1b[0m 31 passed; 0 failed\n",
        "warning: unused variable `x`\n",
        "  --> src/bocpd.rs:42:9\n",
        "error[E0308]: mismatched types\n",
    ];

    let mut output = String::with_capacity(approx_bytes + 128);
    let mut idx = 0;
    while output.len() < approx_bytes {
        output.push_str(lines[idx % lines.len()]);
        idx += 1;
    }
    output.truncate(approx_bytes);
    output
}

// =============================================================================
// Criterion groups and main
// =============================================================================

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("bocpd", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_single_update,
        bench_feature_vector,
        bench_batch_100_panes
);
criterion_main!(benches);
