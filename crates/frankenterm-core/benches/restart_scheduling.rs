//! Criterion benchmarks for automatic restart scheduling (wa-lr93).
//!
//! Performance budgets:
//! - `scheduling_decision_latency`: evaluate 1,440 one-minute candidates **< 1ms**
//! - `activity_profile_update`: EWMA hourly update **< 10us**
//! - `hazard_urgency_computation`: 100 urgency scores **< 50us total**

use std::hint::black_box;
use std::time::{Duration, SystemTime};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::survival::{
    ActivityProfile, HazardForecastPoint, RestartMode, RestartScheduler, RestartSchedulerConfig,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "scheduling_decision_latency",
        budget: "p50 < 1ms (score/select over 1,440 windows)",
    },
    bench_common::BenchBudget {
        name: "activity_profile_update",
        budget: "p50 < 10us (hourly EWMA update)",
    },
    bench_common::BenchBudget {
        name: "hazard_urgency_computation",
        budget: "p50 < 50us total (100 urgency scores)",
    },
];

fn scheduler_config() -> RestartSchedulerConfig {
    RestartSchedulerConfig {
        mode: RestartMode::Automatic { min_score: 0.7 },
        hazard_threshold: 0.8,
        urgency_steepness: 8.0,
        cooldown_hours: 12.0,
        schedule_horizon_minutes: 24 * 60,
        activity_ewma_alpha: 0.2,
        default_activity: 0.4,
        pre_restart_snapshot: true,
        advance_warning_minutes: 30,
    }
}

fn synthetic_forecast(count: u32) -> Vec<HazardForecastPoint> {
    (0..count)
        .map(|offset| {
            let hazard_rate = 0.4 + (f64::from(offset % 200) / 200.0);
            let predicted_activity = f64::from((offset * 17) % 100) / 100.0;
            HazardForecastPoint {
                offset_minutes: offset,
                hazard_rate,
                predicted_activity: Some(predicted_activity),
            }
        })
        .collect()
}

fn bench_scheduling_decision_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("restart_scheduling/decision");
    let scheduler = RestartScheduler::new(scheduler_config());
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(8 * 3600);

    for &candidate_count in &[60u32, 360, 1_440] {
        let forecast = synthetic_forecast(candidate_count);
        group.throughput(Throughput::Elements(u64::from(candidate_count)));
        group.bench_with_input(
            BenchmarkId::from_parameter(candidate_count),
            &forecast,
            |b, points| {
                b.iter(|| black_box(scheduler.recommend(now, points)));
            },
        );
    }

    group.finish();
}

fn bench_activity_profile_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("restart_scheduling/activity_profile_update");
    group.bench_function("hourly_ewma_update", |b| {
        let mut profile = ActivityProfile::new(0.2, 0.4);
        let mut hour = 0u8;
        let mut activity = 0.0f64;

        b.iter(|| {
            profile.update_hour(hour, activity);
            hour = (hour + 1) % 24;
            activity = if activity >= 0.95 {
                0.05
            } else {
                activity + 0.05
            };
            black_box(profile.predict_hour(hour));
        });
    });
    group.finish();
}

fn bench_hazard_urgency_computation(c: &mut Criterion) {
    let mut group = c.benchmark_group("restart_scheduling/hazard_urgency");
    let scheduler = RestartScheduler::new(scheduler_config());
    let hazards: Vec<f64> = (0..100).map(|i| 0.2 + f64::from(i) * 0.015).collect();
    let elapsed = Some(Duration::from_secs(24 * 3600));

    group.throughput(Throughput::Elements(hazards.len() as u64));
    group.bench_function("100_scores", |b| {
        b.iter(|| {
            let total = hazards
                .iter()
                .map(|hazard| {
                    scheduler
                        .score_components(*hazard, 0.35, elapsed)
                        .hazard_urgency
                })
                .sum::<f64>();
            black_box(total);
        });
    });
    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("restart_scheduling", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_scheduling_decision_latency,
        bench_activity_profile_update,
        bench_hazard_urgency_computation
);
criterion_main!(benches);
