//! Criterion benchmarks for adaptive Kalman watchdog thresholds.
//!
//! Performance budgets:
//! - `kalman_scalar_update`: Single predict+update cycle **< 100ns**
//! - `kalman_threshold_computation`: μ + k·σ computation **< 50ns**
//! - `kalman_zscore_health_status`: z-score + classify **< 50ns**
//! - `kalman_batch_4_components`: Update all 4 components **< 500ns**

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_core::kalman_watchdog::{
    AdaptiveWatchdog, AdaptiveWatchdogConfig, ComponentTracker, KalmanFilter,
};
use frankenterm_core::watchdog::Component;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "kalman_scalar_update",
        budget: "p50 < 100ns (single Kalman predict+update)",
    },
    bench_common::BenchBudget {
        name: "kalman_threshold_computation",
        budget: "p50 < 50ns (adaptive threshold mu + k*sigma)",
    },
    bench_common::BenchBudget {
        name: "kalman_zscore_health_status",
        budget: "p50 < 50ns (z-score + health classify)",
    },
    bench_common::BenchBudget {
        name: "kalman_batch_4_components",
        budget: "p50 < 500ns (update all 4 component filters)",
    },
];

fn bench_kalman_scalar_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("kalman/scalar_update");

    group.bench_function("warmed_up", |b| {
        let mut kf = KalmanFilter::new(100.0, 2500.0);
        // Warm up with 20 observations
        for i in 0..20 {
            kf.update((i as f64).mul_add(0.1, 1000.0));
        }

        b.iter(|| {
            kf.update(1001.0);
        });
    });

    group.bench_function("first_observation", |b| {
        b.iter(|| {
            let mut kf = KalmanFilter::new(100.0, 2500.0);
            kf.update(1000.0);
        });
    });

    group.finish();
}

fn bench_kalman_threshold_computation(c: &mut Criterion) {
    let mut group = c.benchmark_group("kalman/threshold_computation");
    let config = AdaptiveWatchdogConfig::default();

    group.bench_function("adaptive_threshold", |b| {
        let mut tracker = ComponentTracker::new(&config, 5_000);
        for i in 0..20 {
            tracker.observe(i * 1000);
        }

        b.iter(|| tracker.adaptive_threshold(3.0));
    });

    group.finish();
}

fn bench_kalman_zscore_health_status(c: &mut Criterion) {
    let mut group = c.benchmark_group("kalman/zscore_health_status");

    group.bench_function("classify_adaptive", |b| {
        let config = AdaptiveWatchdogConfig {
            min_observations: 3,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);
        for i in 0..20 {
            tracker.observe(i * 1000);
        }

        let current_ms = 20_500;
        b.iter(|| tracker.classify(current_ms, &config));
    });

    group.bench_function("classify_warmup", |b| {
        let config = AdaptiveWatchdogConfig {
            min_observations: 100,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);
        tracker.observe(1000);
        tracker.observe(2000);

        b.iter(|| tracker.classify(3000, &config));
    });

    group.finish();
}

fn bench_kalman_batch_4_components(c: &mut Criterion) {
    let mut group = c.benchmark_group("kalman/batch_4_components");

    group.bench_function("observe_all", |b| {
        let config = AdaptiveWatchdogConfig::default();
        let mut wd = AdaptiveWatchdog::new(config);

        // Warm up
        for i in 0..20u64 {
            let t = i * 1000;
            wd.observe(Component::Discovery, t);
            wd.observe(Component::Capture, t);
            wd.observe(Component::Persistence, t);
            wd.observe(Component::Maintenance, t);
        }

        let mut t = 20_000u64;
        b.iter(|| {
            t += 1000;
            wd.observe(Component::Discovery, t);
            wd.observe(Component::Capture, t);
            wd.observe(Component::Persistence, t);
            wd.observe(Component::Maintenance, t);
        });
    });

    group.bench_function("check_health_all", |b| {
        let config = AdaptiveWatchdogConfig::default();
        let mut wd = AdaptiveWatchdog::new(config);

        for i in 0..20u64 {
            let t = i * 1000;
            wd.observe(Component::Discovery, t);
            wd.observe(Component::Capture, t);
            wd.observe(Component::Persistence, t);
            wd.observe(Component::Maintenance, t);
        }

        b.iter(|| wd.check_health(21_000));
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("kalman_watchdog", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_kalman_scalar_update,
        bench_kalman_threshold_computation,
        bench_kalman_zscore_health_status,
        bench_kalman_batch_4_components
);
criterion_main!(benches);
