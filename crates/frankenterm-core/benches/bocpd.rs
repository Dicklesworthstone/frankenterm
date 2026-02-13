//! Benchmarks for Bayesian Online Change-Point Detection (BOCPD).
//!
//! Performance budgets:
//! - Single observation update: **< 50μs**
//! - Feature vector compute (1KB): **< 100μs**
//! - Batch 100 panes update: **< 5ms**
//! - Snapshot serialization: **< 500μs**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::bocpd::{BocpdConfig, BocpdManager, BocpdModel, OutputFeatures};
use frankenterm_core::simd_scan::scan_newlines_and_ansi;
use std::hint::black_box;
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
    bench_common::BenchBudget {
        name: "bocpd_scan_primitives",
        budget: "simd_scan throughput should exceed scalar baseline for larger buffers",
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
                model.update((i as f64).mul_add(0.01, 10.0));
            }
            // Spike regime (high values)
            for i in 0..20 {
                model.update((i as f64).mul_add(0.1, 500.0));
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
                    output_rate: (i as f64).mul_add(0.1, 10.0),
                    byte_rate: (i as f64).mul_add(5.0, 500.0),
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
                    output_rate: (pane_id as f64).mul_add(0.01, 10.0 + counter),
                    byte_rate: counter.mul_add(5.0, 500.0),
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
                    output_rate: (i as f64).mul_add(0.5, 10.0),
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
// Scan primitive benchmarks
// =============================================================================

fn bench_scan_primitives(c: &mut Criterion) {
    let mut group = c.benchmark_group("bocpd_scan_primitives");

    for size in [1024usize, 64 * 1024, 1024 * 1024, 16 * 1024 * 1024] {
        let datasets = vec![
            ("plain", generate_terminal_output(size).into_bytes()),
            ("ansi_heavy", generate_ansi_heavy_output(size).into_bytes()),
        ];

        for (dataset_name, bytes) in datasets {
            group.throughput(Throughput::Bytes(bytes.len() as u64));

            group.bench_with_input(
                BenchmarkId::new(format!("{dataset_name}/simd_scan"), size),
                &bytes,
                |b, data| {
                    b.iter(|| {
                        let metrics = scan_newlines_and_ansi(black_box(data));
                        black_box(metrics);
                    });
                },
            );

            group.bench_with_input(
                BenchmarkId::new(format!("{dataset_name}/scalar_baseline"), size),
                &bytes,
                |b, data| {
                    b.iter(|| {
                        let metrics = scalar_scan_baseline(black_box(data));
                        black_box(metrics);
                    });
                },
            );
        }
    }

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

fn generate_ansi_heavy_output(approx_bytes: usize) -> String {
    let lines = [
        "\x1b[2K\x1b[1G\x1b[32mOK\x1b[0m \x1b[90mstatus\x1b[0m\n",
        "\x1b[2K\x1b[1G\x1b[31mERR\x1b[0m \x1b[1mcompile failed\x1b[0m\n",
        "\x1b[2K\x1b[1G\x1b[33mWARN\x1b[0m \x1b[4mretrying\x1b[0m\n",
        "\x1b[2K\x1b[1G\x1b[34mINFO\x1b[0m \x1b[7mprogress\x1b[0m\n",
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

fn scalar_scan_baseline(bytes: &[u8]) -> (usize, usize) {
    let mut newline_count = 0usize;
    let mut ansi_byte_count = 0usize;
    let mut in_escape = false;

    for &b in bytes {
        if b == b'\n' {
            newline_count += 1;
        }
        if b == 0x1b {
            in_escape = true;
            ansi_byte_count += 1;
        } else if in_escape {
            ansi_byte_count += 1;
            if (0x40..=0x7E).contains(&b) && b != b'[' {
                in_escape = false;
            }
        }
    }

    (newline_count, ansi_byte_count)
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
        bench_batch_100_panes,
        bench_scan_primitives
);
criterion_main!(benches);
