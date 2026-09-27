//! Criterion benchmarks for CostTracker hot-path operations.
//!
//! Bead: wa-2dss0
//! Required coverage:
//! - CostTracker.record_usage() throughput (varying pane counts: 1, 50, 200)
//! - CostTracker.budget_alerts() latency with 0, 3, 16 budget rules
//! - CostTracker.dashboard_snapshot() for 200 panes x 4 providers
//! - Cost additivity: per-provider sums equal grand total

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::cost_tracker::{BudgetThreshold, CostTracker, CostTrackerConfig};
use frankenterm_core::patterns::AgentType;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "quota_cost_ops/record_usage",
        budget: "record_usage <1µs per call at 200 tracked panes",
    },
    bench_common::BenchBudget {
        name: "quota_cost_ops/budget_alerts",
        budget: "budget_alerts <100µs for 16 budget rules x 200 panes",
    },
    bench_common::BenchBudget {
        name: "quota_cost_ops/dashboard_snapshot",
        budget: "dashboard_snapshot <1ms for 200 panes x 4 providers",
    },
];

// ---------------------------------------------------------------------------
// Agent type cycling
// ---------------------------------------------------------------------------

const AGENT_TYPES: &[AgentType] = &[AgentType::Codex, AgentType::ClaudeCode, AgentType::Gemini];

fn agent_type_at(i: usize) -> AgentType {
    AGENT_TYPES[i % AGENT_TYPES.len()]
}

// ---------------------------------------------------------------------------
// Pre-populated CostTracker
// ---------------------------------------------------------------------------

fn populate_tracker(pane_count: usize, budget_count: usize) -> CostTracker {
    let budgets: Vec<BudgetThreshold> = (0..budget_count)
        .map(|i| {
            let agent_type = agent_type_at(i).to_string();
            BudgetThreshold::new(agent_type, (i as f64).mul_add(10.0, 50.0), 0.8)
        })
        .collect();

    let mut tracker = CostTracker::with_config(CostTrackerConfig { budgets });
    for i in 0..pane_count {
        let at = agent_type_at(i);
        let pane_id = i as u64;
        let tokens = 1000 + (i as u64 % 5000);
        let cost = tokens as f64 * 0.00003;
        let ts = 1_700_000_000_000_i64 + i as i64 * 100;
        tracker.record_usage(pane_id, at, tokens, cost, ts);
    }
    tracker
}

// ---------------------------------------------------------------------------
// Benchmarks: CostTracker.record_usage()
// ---------------------------------------------------------------------------

fn bench_record_usage(c: &mut Criterion) {
    let mut group = c.benchmark_group("quota_cost_ops/record_usage");
    group.measurement_time(Duration::from_secs(5));

    for &pane_count in &[1usize, 50, 200] {
        let batch_size = 1000u64;
        group.throughput(Throughput::Elements(batch_size));
        group.bench_with_input(
            BenchmarkId::new("panes", pane_count),
            &pane_count,
            |b, &pane_count| {
                b.iter(|| {
                    let mut tracker = populate_tracker(pane_count, 0);
                    for j in 0..batch_size {
                        let pane_id = j % pane_count as u64;
                        let at = agent_type_at(pane_id as usize);
                        tracker.record_usage(
                            black_box(pane_id),
                            black_box(at),
                            black_box(500),
                            black_box(0.015),
                            black_box(1_700_000_100_000 + j as i64),
                        );
                    }
                    black_box(tracker.grand_total_tokens());
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks: CostTracker.budget_alerts()
// ---------------------------------------------------------------------------

fn bench_budget_alerts(c: &mut Criterion) {
    let mut group = c.benchmark_group("quota_cost_ops/budget_alerts");
    group.measurement_time(Duration::from_secs(5));

    for &(budget_count, label) in &[(0usize, "0_rules"), (3, "3_rules"), (16, "16_rules")] {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &budget_count,
            |b, &budget_count| {
                let mut tracker = populate_tracker(200, budget_count);
                b.iter(|| {
                    let alerts = tracker.budget_alerts();
                    black_box(alerts.len());
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks: CostTracker.dashboard_snapshot()
// ---------------------------------------------------------------------------

fn bench_dashboard_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("quota_cost_ops/dashboard_snapshot");
    group.measurement_time(Duration::from_secs(5));

    for &(pane_count, label) in &[(5usize, "5_panes"), (50, "50_panes"), (200, "200_panes")] {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &pane_count,
            |b, &pane_count| {
                let mut tracker = populate_tracker(pane_count, 4);
                b.iter(|| {
                    let snapshot = tracker.dashboard_snapshot();
                    black_box(snapshot.grand_total_cost_usd);
                    black_box(snapshot.grand_total_tokens);
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmarks: Cost additivity verification
// ---------------------------------------------------------------------------

fn bench_cost_additivity(c: &mut Criterion) {
    let mut group = c.benchmark_group("quota_cost_ops/cost_additivity");
    group.measurement_time(Duration::from_secs(5));

    for &pane_count in &[50usize, 200] {
        group.throughput(Throughput::Elements(pane_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pane_count),
            &pane_count,
            |b, &pane_count| {
                let tracker = populate_tracker(pane_count, 0);
                b.iter(|| {
                    let per_provider_total: f64 = tracker
                        .all_provider_summaries()
                        .iter()
                        .map(|s| s.total_cost_usd)
                        .sum();
                    let grand = tracker.grand_total_cost();
                    let delta = (per_provider_total - grand).abs();
                    debug_assert!(
                        delta < 1e-6,
                        "additivity violation: per_provider={per_provider_total}, grand={grand}"
                    );
                    black_box(delta);
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Suite
// ---------------------------------------------------------------------------

fn bench_suite(c: &mut Criterion) {
    bench_record_usage(c);
    bench_budget_alerts(c);
    bench_dashboard_snapshot(c);
    bench_cost_additivity(c);
    bench_common::emit_bench_artifacts("quota_cost_ops", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
