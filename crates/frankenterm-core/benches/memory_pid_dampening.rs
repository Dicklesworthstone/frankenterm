//! Round-5 M9 bench: fleet memory eviction magnitude under PID dampening.
//!
//! This is a metric bench as much as a timing bench: the useful signal is total
//! evicted bytes and reclaim-target oscillation across a synthetic pressure
//! replay. The PID arm is explicitly opt-in via `PidDampeningConfig`; the
//! default production strategy remains hysteresis.

use std::fs::{OpenOptions, create_dir_all};
use std::hint::black_box;
use std::io::Write;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::fleet_memory_controller::{
    FleetPressureTier, FleetScrollbackOrchestrator, MemoryDampening, PaneScrollbackInfo,
    PidDampeningConfig, PidReclaimController,
};
use serde::Serialize;

mod bench_common;

const PANE_COUNT: usize = 192;
const REPLAY_CYCLES: &[(FleetPressureTier, Option<f64>)] = &[
    (FleetPressureTier::Elevated, Some(0.08)),
    (FleetPressureTier::Elevated, Some(0.12)),
    (FleetPressureTier::Elevated, Some(0.16)),
    (FleetPressureTier::Elevated, Some(0.20)),
    (FleetPressureTier::Elevated, Some(0.24)),
    (FleetPressureTier::Elevated, Some(0.27)),
    (FleetPressureTier::Elevated, Some(0.30)),
    (FleetPressureTier::Critical, Some(0.06)),
    (FleetPressureTier::Critical, Some(0.09)),
    (FleetPressureTier::Elevated, Some(0.18)),
    (FleetPressureTier::Elevated, Some(0.23)),
    (FleetPressureTier::Elevated, Some(0.28)),
];

const BUDGETS: &[bench_common::BenchBudget] = &[bench_common::BenchBudget {
    name: "memory_pid_dampening/pressure_replay",
    budget: "A/B config memory.dampening=pid should reduce elevated-tier evicted bytes and reclaim-target oscillation at equal pane capacity",
}];

#[derive(Debug, Clone, Copy, Serialize)]
struct ReplayMetric {
    dampening: &'static str,
    total_evicted_bytes: usize,
    eviction_plans: usize,
    eviction_targets: usize,
    reclaim_target_direction_changes: usize,
}

fn build_panes() -> Vec<PaneScrollbackInfo> {
    (0..PANE_COUNT)
        .map(|pane| {
            let warm_pages = 32 + (pane % 48);
            let bytes_per_page = 10_240 + (pane % 7) * 2_048;
            let warm_bytes = warm_pages * bytes_per_page;
            PaneScrollbackInfo {
                pane_id: pane as u64,
                activity_counter: if pane % 5 == 0 { 1 } else { 0 },
                warm_bytes,
                warm_pages,
                estimated_memory_bytes: warm_bytes + 128 * 256,
            }
        })
        .collect()
}

fn apply_eviction_plan(
    panes: &mut [PaneScrollbackInfo],
    plan: &frankenterm_core::fleet_memory_controller::EvictionPlan,
) -> usize {
    let mut evicted_bytes = 0usize;
    for target in &plan.targets {
        if let Some(pane) = panes.iter_mut().find(|pane| pane.pane_id == target.pane_id) {
            if pane.warm_pages == 0 || pane.warm_bytes == 0 {
                continue;
            }
            let pages = target.pages_to_evict.min(pane.warm_pages);
            let bytes = pane
                .warm_bytes
                .saturating_mul(pages)
                .checked_div(pane.warm_pages)
                .unwrap_or(0)
                .min(pane.warm_bytes);
            pane.warm_pages = pane.warm_pages.saturating_sub(pages);
            pane.warm_bytes = pane.warm_bytes.saturating_sub(bytes);
            pane.estimated_memory_bytes = pane.estimated_memory_bytes.saturating_sub(bytes);
            evicted_bytes = evicted_bytes.saturating_add(bytes);
        }
    }
    evicted_bytes
}

fn run_replay(dampening: MemoryDampening) -> ReplayMetric {
    let mut panes = build_panes();
    let mut orchestrator = FleetScrollbackOrchestrator::new();
    let mut pid = PidReclaimController::new();
    let cfg = PidDampeningConfig {
        dampening,
        ..PidDampeningConfig::default()
    };
    let mut total_evicted_bytes = 0usize;
    let mut eviction_plans = 0usize;
    let mut eviction_targets = 0usize;
    let mut previous_target = None;
    let mut previous_direction = 0isize;
    let mut reclaim_target_direction_changes = 0usize;

    for &(tier, headroom) in REPLAY_CYCLES {
        let plan = orchestrator.plan_eviction_damped(tier, &panes, headroom, &mut pid, &cfg);
        if let Some(plan) = plan {
            eviction_plans = eviction_plans.saturating_add(1);
            eviction_targets = eviction_targets.saturating_add(plan.targets.len());
            if let Some(previous) = previous_target {
                let direction = plan.fleet_warm_bytes_target.cmp(&previous);
                let direction = match direction {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                };
                if direction != 0 && previous_direction != 0 && direction != previous_direction {
                    reclaim_target_direction_changes =
                        reclaim_target_direction_changes.saturating_add(1);
                }
                if direction != 0 {
                    previous_direction = direction;
                }
            }
            previous_target = Some(plan.fleet_warm_bytes_target);
            total_evicted_bytes =
                total_evicted_bytes.saturating_add(apply_eviction_plan(&mut panes, &plan));
        }
    }

    ReplayMetric {
        dampening: match dampening {
            MemoryDampening::Hysteresis => "hysteresis",
            MemoryDampening::Pid => "pid",
        },
        total_evicted_bytes,
        eviction_plans,
        eviction_targets,
        reclaim_target_direction_changes,
    }
}

fn emit_metric(metric: ReplayMetric) {
    let _ = create_dir_all("target/criterion");
    let Ok(mut out) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("target/criterion/round5-scroll-mem-cache-metrics.jsonl")
    else {
        return;
    };
    let row = serde_json::json!({
        "bench": "memory_pid_dampening",
        "metric": metric,
    });
    let _ = writeln!(out, "{row}");
}

fn emit_metrics_once() {
    emit_metric(run_replay(MemoryDampening::Hysteresis));
    emit_metric(run_replay(MemoryDampening::Pid));
}

fn bench_pressure_replay(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_pid_dampening");

    for dampening in [MemoryDampening::Hysteresis, MemoryDampening::Pid] {
        group.bench_with_input(
            BenchmarkId::from_parameter(match dampening {
                MemoryDampening::Hysteresis => "hysteresis",
                MemoryDampening::Pid => "pid",
            }),
            &dampening,
            |b, &dampening| {
                b.iter(|| black_box(run_replay(black_box(dampening))));
            },
        );
    }

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    emit_metrics_once();
    bench_pressure_replay(c);
    bench_common::emit_bench_artifacts("memory_pid_dampening", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
