//! Benchmarks for Bayesian Online Change-Point Detection (BOCPD).
//!
//! Performance budgets:
//! - Single observation update: **< 50μs**
//! - Feature vector compute (1KB): **< 100μs**
//! - Batch 100 panes update: **< 5ms**
//! - Snapshot serialization: **< 500μs**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::bocpd::{
    BocpdConfig, BocpdDetectorKind, BocpdManager, BocpdModel, OutputFeatures,
};
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
    bench_common::BenchBudget {
        name: "bocpd_detector_ab",
        budget: "Shiryaev-Roberts detector cost vs BOCPD baseline on a synthetic-changepoint \
                 stream (env FT_BOCPD_DETECTOR=shiryaev_roberts/bocpd A/B)",
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
                ..Default::default()
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
        let elapsed = Duration::from_secs(1);
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
// Detector A/B: BOCPD (baseline) vs Shiryaev-Roberts (candidate)
// =============================================================================

/// Runtime detector selector for the env-gated A/B.
///
/// The `scripts/round4-bench-ab.sh --gate env:FT_BOCPD_DETECTOR=shiryaev_roberts/bocpd`
/// driver builds this bench binary ONCE and runs it twice back-to-back, setting
/// `FT_BOCPD_DETECTOR` to the OFF value (baseline) then the ON value (candidate).
/// Because the selector is read at run time and `BocpdConfig::detector` is a
/// public field, both arms are reachable from the same binary with no rebuild,
/// and they genuinely diverge in the benched path: the SR arm runs the extra
/// `update_shiryaev_roberts_statistic` e-statistic recursion per observation,
/// while the BOCPD arm runs the recent-change-mass scan. Anything that is not a
/// recognized SR token (including empty / unset / "bocpd" / "0" / "off") maps to
/// the BOCPD baseline so the OFF arm is unambiguous.
fn detector_from_env() -> BocpdDetectorKind {
    match std::env::var("FT_BOCPD_DETECTOR")
        .ok()
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("shiryaev_roberts" | "sr" | "1" | "on") => BocpdDetectorKind::ShiryaevRoberts,
        _ => BocpdDetectorKind::Bocpd,
    }
}

/// Deterministic synthetic-changepoint trace: a stable regime followed by a
/// runaway linear drift. Mirrors the in-crate `synthetic_runaway_corpus` shape so
/// the A/B exercises the detection-relevant path (the alarm crossing), not just
/// steady-state updates — the point at which the two detectors diverge most.
fn synthetic_changepoint_trace(stable_len: usize, drift_len: usize) -> Vec<f64> {
    let noise = [-0.90, 0.35, 0.75, -0.25, 0.50, -0.65, 0.15, -0.05];
    let mut values = Vec::with_capacity(stable_len + drift_len);
    for i in 0..stable_len {
        values.push(100.0 + noise[i % noise.len()]);
    }
    for i in 0..drift_len {
        let drift = (i as f64).mul_add(0.18, 0.20);
        values.push(100.0 + drift + noise[i % noise.len()]);
    }
    values
}

fn bench_detector_ab(c: &mut Criterion) {
    // Read ONCE per process; constant for the whole criterion run so the
    // baseline/candidate arms are clean (the driver flips the env var between the
    // two back-to-back runs of this same binary).
    let detector = detector_from_env();

    let mut group = c.benchmark_group("bocpd_detector_ab");
    let trace = synthetic_changepoint_trace(96, 96);
    group.throughput(Throughput::Elements(trace.len() as u64));

    // The bench id is CONSTANT across both arms (only the env-selected `detector`
    // differs) so the A/B comparator pairs baseline vs candidate samples by the
    // same `--group bocpd_detector_ab --id changepoint_stream`.
    group.bench_function("changepoint_stream", |b| {
        b.iter(|| {
            // Fresh model per iteration so every sample measures the same
            // stable→runaway detection workload from a clean run-length posterior.
            let mut model = BocpdModel::new(BocpdConfig {
                hazard_rate: 0.02,
                detection_threshold: 0.65,
                min_observations: 32,
                max_run_length: 160,
                detector,
            });
            for &value in &trace {
                black_box(model.update(black_box(value)));
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
        bench_scan_primitives,
        bench_detector_ab
);
criterion_main!(benches);
