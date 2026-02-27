//! Criterion benchmarks for the semantic anomaly pipeline.
//!
//! Bead: ft-344j8.13
//!
//! Latency budgets:
//! - `simd_conformal_update`: < 1µs per observe()
//! - `entropy_gate_check`: < 500ns per evaluate()
//! - `dot_product_384d`: < 100ns per call

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use frankenterm_core::semantic_anomaly::{
    ConformalAnomalyConfig, ConformalAnomalyDetector, EntropyGate, EntropyGateConfig,
    GatedAnomalyDetector, SemanticAnomalyConfig, SemanticAnomalyDetector, SortedCalibrationBuffer,
    dot_product_simd, normalize_simd,
};

// =============================================================================
// Vector math benchmarks
// =============================================================================

fn bench_dot_product(c: &mut Criterion) {
    let mut group = c.benchmark_group("dot_product_simd");

    for &dim in &[8, 64, 128, 384, 768] {
        let a: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.01).collect();
        let b: Vec<f32> = (0..dim).map(|i| (i as f32).mul_add(-0.005, 1.0)).collect();

        group.bench_with_input(BenchmarkId::new("dim", dim), &dim, |bench, _| {
            bench.iter(|| dot_product_simd(&a, &b));
        });
    }

    group.finish();
}

fn bench_normalize(c: &mut Criterion) {
    let mut group = c.benchmark_group("normalize_simd");

    for &dim in &[8, 64, 384, 768] {
        let v: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.01).collect();

        group.bench_with_input(BenchmarkId::new("dim", dim), &dim, |bench, _| {
            bench.iter(|| normalize_simd(&v));
        });
    }

    group.finish();
}

// =============================================================================
// Calibration buffer benchmarks
// =============================================================================

fn bench_calibration_buffer(c: &mut Criterion) {
    let mut group = c.benchmark_group("calibration_buffer");

    for &capacity in &[50, 200, 1000] {
        // Benchmark insert (full buffer, with eviction).
        group.bench_with_input(
            BenchmarkId::new("insert_evicting", capacity),
            &capacity,
            |bench, &cap| {
                let mut buf = SortedCalibrationBuffer::new(cap);
                // Fill the buffer.
                for i in 0..cap {
                    buf.insert(i as f32 * 0.1);
                }
                let mut score = cap as f32 * 0.1;
                bench.iter(|| {
                    score += 0.001;
                    buf.insert(score);
                });
            },
        );

        // Benchmark count_geq (rank query).
        group.bench_with_input(
            BenchmarkId::new("count_geq", capacity),
            &capacity,
            |bench, &cap| {
                let mut buf = SortedCalibrationBuffer::new(cap);
                for i in 0..cap {
                    buf.insert(i as f32 * 0.1);
                }
                bench.iter(|| buf.count_geq(5.0));
            },
        );

        // Benchmark conformal_p_value.
        group.bench_with_input(
            BenchmarkId::new("conformal_p_value", capacity),
            &capacity,
            |bench, &cap| {
                let mut buf = SortedCalibrationBuffer::new(cap);
                for i in 0..cap {
                    buf.insert(i as f32 * 0.1);
                }
                bench.iter(|| buf.conformal_p_value(5.0));
            },
        );
    }

    group.finish();
}

// =============================================================================
// Conformal detector benchmarks (Target: < 1µs per observe)
// =============================================================================

fn bench_conformal_observe(c: &mut Criterion) {
    let mut group = c.benchmark_group("conformal_observe");
    group.sample_size(100);

    for &dim in &[8, 384] {
        let config = ConformalAnomalyConfig {
            min_calibration: 10,
            calibration_window: 200,
            alpha: 0.05,
            centroid_alpha: 0.1,
        };
        let mut det = ConformalAnomalyDetector::new(config);

        // Warmup with base vector.
        let base: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect();
        for _ in 0..50 {
            det.observe(&base);
        }

        // Create a slightly different vector for each iteration.
        let test_vec: Vec<f32> = (0..dim)
            .map(|i| {
                if i == 0 {
                    0.98
                } else if i == 1 {
                    0.02
                } else {
                    0.0
                }
            })
            .collect();

        group.bench_with_input(BenchmarkId::new("dim", dim), &dim, |bench, _| {
            bench.iter(|| det.observe(&test_vec));
        });
    }

    group.finish();
}

// =============================================================================
// Z-score detector benchmarks
// =============================================================================

fn bench_zscore_observe(c: &mut Criterion) {
    let mut group = c.benchmark_group("zscore_observe");

    for &dim in &[8, 384] {
        let config = SemanticAnomalyConfig::default();
        let mut det = SemanticAnomalyDetector::new(config);

        let base: Vec<f32> = (0..dim).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect();
        for _ in 0..20 {
            det.observe(&base);
        }

        let test_vec: Vec<f32> = (0..dim)
            .map(|i| {
                if i == 0 {
                    0.98
                } else if i == 1 {
                    0.02
                } else {
                    0.0
                }
            })
            .collect();

        group.bench_with_input(BenchmarkId::new("dim", dim), &dim, |bench, _| {
            bench.iter(|| det.observe(&test_vec));
        });
    }

    group.finish();
}

// =============================================================================
// Entropy gate benchmarks (Target: < 500ns per evaluate)
// =============================================================================

fn bench_entropy_gate(c: &mut Criterion) {
    let mut group = c.benchmark_group("entropy_gate");

    // Low-entropy segment (progress bar / constant data).
    let low_entropy = vec![b'='; 200];
    // High-entropy segment (diverse bytes).
    let mut high_entropy = Vec::with_capacity(256);
    for b in 0..=255u8 {
        high_entropy.push(b);
    }
    // Medium-entropy segment (natural language).
    let medium_entropy = b"The quick brown fox jumps over the lazy dog. Build complete.".to_vec();

    for (label, segment) in [
        ("low_entropy_200B", low_entropy.as_slice()),
        ("high_entropy_256B", high_entropy.as_slice()),
        ("medium_entropy_60B", medium_entropy.as_slice()),
    ] {
        group.bench_with_input(
            BenchmarkId::new("evaluate", label),
            &segment,
            |bench, &seg| {
                let mut gate = EntropyGate::new(EntropyGateConfig::default());
                bench.iter(|| gate.evaluate(seg));
            },
        );
    }

    // Benchmark with varying segment sizes.
    for &size in &[64, 256, 1024, 4096] {
        let segment: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        group.bench_with_input(
            BenchmarkId::new("evaluate_size", size),
            &size,
            |bench, _| {
                let mut gate = EntropyGate::new(EntropyGateConfig::default());
                bench.iter(|| gate.evaluate(&segment));
            },
        );
    }

    group.finish();
}

// =============================================================================
// Full pipeline benchmark (GatedAnomalyDetector)
// =============================================================================

fn bench_gated_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("gated_pipeline");
    group.sample_size(50);

    let gate_config = EntropyGateConfig {
        min_entropy_bits_per_byte: 2.0,
        min_segment_bytes: 4,
        enabled: true,
    };
    let detector_config = ConformalAnomalyConfig {
        min_calibration: 10,
        calibration_window: 200,
        alpha: 0.05,
        centroid_alpha: 0.1,
    };

    // Simulate a mock embedding function (returns fixed 8d vector).
    let embed_fn = |_: &[u8]| -> Vec<f32> { vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0] };

    // High-entropy segment → passes gate, processed by detector.
    let mut diverse_segment = Vec::with_capacity(256);
    for b in 0..=255u8 {
        diverse_segment.push(b);
    }

    // Warmup the detector.
    let mut gated = GatedAnomalyDetector::new(gate_config.clone(), detector_config.clone());
    for _ in 0..50 {
        gated.observe(&diverse_segment, embed_fn);
    }

    group.bench_function("high_entropy_pass", |bench| {
        bench.iter(|| gated.observe(&diverse_segment, embed_fn));
    });

    // Low-entropy segment → skipped by gate (embed_fn never called).
    let low_segment = vec![b'A'; 200];
    group.bench_function("low_entropy_skip", |bench| {
        bench.iter(|| gated.observe(&low_segment, |_| panic!("should not be called")));
    });

    group.finish();
}

// =============================================================================
// Criterion harness
// =============================================================================

criterion_group!(
    benches,
    bench_dot_product,
    bench_normalize,
    bench_calibration_buffer,
    bench_conformal_observe,
    bench_zscore_observe,
    bench_entropy_gate,
    bench_gated_pipeline,
);
criterion_main!(benches);
