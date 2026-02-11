//! Benchmarks for spectral fingerprinting (FFT, PSD, classification).
//!
//! Performance budgets:
//! - 1024-point FFT + PSD: **< 50μs**
//! - Full classify pipeline (window + FFT + PSD + peaks): **< 100μs**
//! - Peak detection on 513-bin PSD: **< 10μs**

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::spectral::{
    SpectralConfig, classify_signal, detect_peaks, hann_window, power_spectral_density,
    spectral_flatness,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "fft_psd_1024",
        budget: "p50 < 50us (1024-point FFT + PSD)",
    },
    bench_common::BenchBudget {
        name: "classify_pipeline_1024",
        budget: "p50 < 100us (full classify pipeline, 1024 samples)",
    },
    bench_common::BenchBudget {
        name: "peak_detection_513",
        budget: "p50 < 10us (peak detection, 513 bins)",
    },
];

/// Generate a pseudo-random time series using an iterated LCG.
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

/// Generate a sine wave signal.
fn sine_signal(n: usize, freq_cycles: usize) -> Vec<f64> {
    use std::f64::consts::PI;
    (0..n)
        .map(|i| (2.0 * PI * freq_cycles as f64 * i as f64 / n as f64).sin())
        .collect()
}

// =============================================================================
// FFT + PSD benchmarks
// =============================================================================

fn bench_fft_psd(c: &mut Criterion) {
    let mut group = c.benchmark_group("fft_psd_1024");

    for size in [256, 512, 1024, 2048] {
        let signal = pseudo_random_series(size, 42);

        group.bench_with_input(BenchmarkId::new("random", size), &signal, |b, signal| {
            b.iter(|| power_spectral_density(signal));
        });
    }

    // Sine wave — tests worst case for twiddle factor computation
    let sine = sine_signal(1024, 32);
    group.bench_with_input(BenchmarkId::new("sine_1024", 1024), &sine, |b, signal| {
        b.iter(|| power_spectral_density(signal));
    });

    group.finish();
}

// =============================================================================
// Full classify pipeline benchmarks
// =============================================================================

fn bench_classify_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("classify_pipeline_1024");
    let config = SpectralConfig::default();

    // Random signal (Burst/Steady)
    let random = pseudo_random_series(1024, 42);
    group.bench_with_input(BenchmarkId::new("random", 1024), &random, |b, signal| {
        b.iter(|| classify_signal(signal, &config));
    });

    // Periodic signal (Polling)
    let sine = sine_signal(1024, 32);
    group.bench_with_input(BenchmarkId::new("sine", 1024), &sine, |b, signal| {
        b.iter(|| classify_signal(signal, &config));
    });

    // Idle signal (zeros)
    let zeros = vec![0.0; 1024];
    group.bench_with_input(BenchmarkId::new("idle", 1024), &zeros, |b, signal| {
        b.iter(|| classify_signal(signal, &config));
    });

    group.finish();
}

// =============================================================================
// Peak detection benchmarks
// =============================================================================

fn bench_peak_detection(c: &mut Criterion) {
    let mut group = c.benchmark_group("peak_detection_513");

    // Flat PSD (no peaks — fast path)
    let flat_psd = vec![1.0; 513];
    group.bench_with_input(BenchmarkId::new("flat", 513), &flat_psd, |b, psd| {
        b.iter(|| detect_peaks(psd, 6.0, 10.0));
    });

    // PSD with a few peaks
    let mut peaked_psd = vec![1.0; 513];
    peaked_psd[32] = 100.0;
    peaked_psd[64] = 80.0;
    peaked_psd[128] = 60.0;
    group.bench_with_input(BenchmarkId::new("peaked", 513), &peaked_psd, |b, psd| {
        b.iter(|| detect_peaks(psd, 6.0, 10.0));
    });

    group.finish();
}

// =============================================================================
// Hann window + spectral flatness benchmarks
// =============================================================================

fn bench_hann_flatness(c: &mut Criterion) {
    let mut group = c.benchmark_group("hann_flatness");

    let signal = pseudo_random_series(1024, 42);
    group.bench_with_input(BenchmarkId::new("hann_1024", 1024), &signal, |b, signal| {
        b.iter(|| hann_window(signal));
    });

    let psd = vec![1.0; 513];
    group.bench_with_input(BenchmarkId::new("flatness_513", 513), &psd, |b, psd| {
        b.iter(|| spectral_flatness(psd));
    });

    group.finish();
}

// =============================================================================
// Criterion groups and main
// =============================================================================

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("spectral", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_fft_psd,
        bench_classify_pipeline,
        bench_peak_detection,
        bench_hann_flatness
);
criterion_main!(benches);
