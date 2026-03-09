//! Criterion benchmarks for the unified dashboard state aggregator (ft-3hbv9).
//!
//! Measures the hot paths consumed by TUI rendering, robot-mode JSON,
//! and web API responses:
//!
//! 1. `DashboardManager::snapshot()` — build complete state from subsystems.
//! 2. `adapt_dashboard()` — convert `DashboardState` → render-ready `DashboardModel`.
//! 3. Individual panel builds via targeted updates.
//!
//! Performance budget: snapshot + adapt should complete in < 1ms combined
//! so the TUI render loop stays well under the 16ms/frame target.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use frankenterm_core::backpressure::{BackpressureSnapshot, BackpressureTier};
use frankenterm_core::cost_tracker::{
    AlertSeverity, BudgetAlert, CostDashboardSnapshot, PaneCostSummary, ProviderCostSummary,
};
use frankenterm_core::dashboard::DashboardManager;
use frankenterm_core::quota_gate::{QuotaGateSnapshot, QuotaGateTelemetrySnapshot};
use frankenterm_core::rate_limit_tracker::{ProviderRateLimitStatus, ProviderRateLimitSummary};
use frankenterm_core::tui::view_adapters::adapt_dashboard;

mod bench_common;

#[allow(dead_code)]
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "dashboard_snapshot",
        budget: "p50 < 50µs, p99 < 200µs (full dashboard snapshot from all subsystems)",
    },
    bench_common::BenchBudget {
        name: "dashboard_adapt",
        budget: "p50 < 100µs, p99 < 500µs (adapt DashboardState → DashboardModel)",
    },
    bench_common::BenchBudget {
        name: "dashboard_snapshot_adapt_combined",
        budget: "p50 < 200µs, p99 < 1ms (snapshot + adapt end-to-end, must stay under 1ms for 60fps budget)",
    },
    bench_common::BenchBudget {
        name: "dashboard_incremental_update",
        budget: "p50 < 20µs, p99 < 100µs (single subsystem update + re-snapshot)",
    },
];

// =============================================================================
// Synthetic data generators
// =============================================================================

fn make_cost_snapshot(provider_count: usize, pane_count: usize) -> CostDashboardSnapshot {
    let providers: Vec<ProviderCostSummary> = (0..provider_count)
        .map(|i| {
            let agent_type = match i % 4 {
                0 => "codex",
                1 => "claude_code",
                2 => "gemini",
                _ => "cursor",
            };
            ProviderCostSummary {
                agent_type: agent_type.to_string(),
                total_tokens: 50_000 + (i as u64 * 10_000),
                total_cost_usd: (i as f64).mul_add(5.0, 25.0),
                pane_count: pane_count / provider_count.max(1),
                record_count: 100 + i as u64 * 20,
            }
        })
        .collect();

    let panes: Vec<PaneCostSummary> = (0..pane_count)
        .map(|i| PaneCostSummary {
            pane_id: i as u64,
            agent_type: providers[i % provider_count.max(1)].agent_type.clone(),
            total_tokens: 10_000 + (i as u64 * 500),
            total_cost_usd: (i as f64).mul_add(0.5, 5.0),
            record_count: 20 + i as u64 * 2,
            last_updated_ms: 1_700_000_000_000_i64 + (i as i64 * 1000),
        })
        .collect();

    let alerts: Vec<BudgetAlert> = providers
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 3 == 0)
        .map(|(_, p)| BudgetAlert {
            agent_type: p.agent_type.clone(),
            current_cost_usd: p.total_cost_usd,
            budget_limit_usd: p.total_cost_usd * 1.1,
            usage_fraction: 0.9,
            severity: AlertSeverity::Warning,
        })
        .collect();

    let grand_total_cost: f64 = providers.iter().map(|p| p.total_cost_usd).sum();
    let grand_total_tokens: u64 = providers.iter().map(|p| p.total_tokens).sum();

    CostDashboardSnapshot {
        providers,
        panes,
        alerts,
        grand_total_cost_usd: grand_total_cost,
        grand_total_tokens,
    }
}

fn make_rate_limits(provider_count: usize) -> Vec<ProviderRateLimitSummary> {
    (0..provider_count)
        .map(|i| {
            let agent_type = match i % 4 {
                0 => "codex",
                1 => "claude_code",
                2 => "gemini",
                _ => "cursor",
            };
            let status = match i % 3 {
                0 => ProviderRateLimitStatus::Clear,
                1 => ProviderRateLimitStatus::PartiallyLimited,
                _ => ProviderRateLimitStatus::FullyLimited,
            };
            ProviderRateLimitSummary {
                agent_type: agent_type.to_string(),
                status,
                limited_pane_count: if status == ProviderRateLimitStatus::Clear {
                    0
                } else {
                    2 + i
                },
                total_pane_count: 5 + i,
                earliest_clear_secs: if status == ProviderRateLimitStatus::Clear {
                    0
                } else {
                    30 + i as u64 * 10
                },
                total_events: i * 3,
            }
        })
        .collect()
}

fn make_backpressure(tier: BackpressureTier, paused_count: usize) -> BackpressureSnapshot {
    BackpressureSnapshot {
        tier,
        timestamp_epoch_ms: 1_700_000_000_000,
        capture_depth: 500,
        capture_capacity: 1000,
        write_depth: 200,
        write_capacity: 1000,
        duration_in_tier_ms: 5000,
        transitions: 3,
        paused_panes: (0..paused_count as u64).collect(),
    }
}

fn make_quota(evaluations: u64) -> QuotaGateSnapshot {
    QuotaGateSnapshot {
        telemetry: QuotaGateTelemetrySnapshot {
            evaluations,
            allowed: evaluations * 80 / 100,
            warned: evaluations * 15 / 100,
            blocked: evaluations * 5 / 100,
        },
    }
}

fn populate_manager(provider_count: usize, pane_count: usize) -> DashboardManager {
    let mut mgr = DashboardManager::new();
    mgr.update_costs(make_cost_snapshot(provider_count, pane_count));
    mgr.update_rate_limits(make_rate_limits(provider_count));
    mgr.update_backpressure(make_backpressure(BackpressureTier::Yellow, 3));
    mgr.update_quota(make_quota(1000));
    mgr
}

// =============================================================================
// Benchmarks: DashboardManager::snapshot()
// =============================================================================

fn bench_dashboard_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("dashboard_snapshot");

    // Scale: (providers, panes)
    let configs: &[(usize, usize, &str)] = &[
        (3, 5, "small_3p_5panes"),
        (8, 50, "medium_8p_50panes"),
        (16, 200, "large_16p_200panes"),
    ];

    for &(providers, panes, label) in configs {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &(providers, panes),
            |b, &(p, n)| {
                let mut mgr = populate_manager(p, n);
                b.iter(|| {
                    let state = mgr.snapshot();
                    black_box(&state);
                });
            },
        );
    }

    group.finish();
}

// =============================================================================
// Benchmarks: adapt_dashboard() view adapter
// =============================================================================

fn bench_dashboard_adapt(c: &mut Criterion) {
    let mut group = c.benchmark_group("dashboard_adapt");

    let configs: &[(usize, usize, &str)] = &[
        (3, 5, "small_3p_5panes"),
        (8, 50, "medium_8p_50panes"),
        (16, 200, "large_16p_200panes"),
    ];

    for &(providers, panes, label) in configs {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &(providers, panes),
            |b, &(p, n)| {
                let mut mgr = populate_manager(p, n);
                let state = mgr.snapshot();
                b.iter(|| {
                    let model = adapt_dashboard(black_box(&state));
                    black_box(&model);
                });
            },
        );
    }

    group.finish();
}

// =============================================================================
// Benchmarks: snapshot + adapt combined (end-to-end hot path)
// =============================================================================

fn bench_dashboard_snapshot_adapt_combined(c: &mut Criterion) {
    let mut group = c.benchmark_group("dashboard_snapshot_adapt_combined");

    let configs: &[(usize, usize, &str)] = &[
        (3, 5, "small_3p_5panes"),
        (8, 50, "medium_8p_50panes"),
        (16, 200, "large_16p_200panes"),
    ];

    for &(providers, panes, label) in configs {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &(providers, panes),
            |b, &(p, n)| {
                let mut mgr = populate_manager(p, n);
                b.iter(|| {
                    let state = mgr.snapshot();
                    let model = adapt_dashboard(black_box(&state));
                    black_box(&model);
                });
            },
        );
    }

    group.finish();
}

// =============================================================================
// Benchmarks: incremental single-subsystem update + re-snapshot
// =============================================================================

fn bench_dashboard_incremental_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("dashboard_incremental_update");

    // Pre-populate with realistic data.
    let mut mgr = populate_manager(8, 50);
    // Take initial snapshot so telemetry is warm.
    let _ = mgr.snapshot();

    group.bench_function("cost_update_only", |b| {
        let cost = make_cost_snapshot(8, 50);
        b.iter(|| {
            mgr.update_costs(black_box(cost.clone()));
            let state = mgr.snapshot();
            black_box(&state);
        });
    });

    group.bench_function("rate_limit_update_only", |b| {
        let rl = make_rate_limits(8);
        b.iter(|| {
            mgr.update_rate_limits(black_box(rl.clone()));
            let state = mgr.snapshot();
            black_box(&state);
        });
    });

    group.bench_function("backpressure_update_only", |b| {
        let bp = make_backpressure(BackpressureTier::Red, 5);
        b.iter(|| {
            mgr.update_backpressure(black_box(bp.clone()));
            let state = mgr.snapshot();
            black_box(&state);
        });
    });

    group.bench_function("quota_update_only", |b| {
        let q = make_quota(2000);
        b.iter(|| {
            mgr.update_quota(black_box(q.clone()));
            let state = mgr.snapshot();
            black_box(&state);
        });
    });

    group.finish();
}

// =============================================================================
// Benchmarks: summary_line() (status bar hot path)
// =============================================================================

fn bench_dashboard_summary_line(c: &mut Criterion) {
    let mut group = c.benchmark_group("dashboard_summary_line");

    let configs: &[(usize, usize, &str)] = &[(3, 5, "small"), (16, 200, "large")];

    for &(providers, panes, label) in configs {
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &(providers, panes),
            |b, &(p, n)| {
                let mut mgr = populate_manager(p, n);
                let state = mgr.snapshot();
                b.iter(|| {
                    let line = state.summary_line();
                    black_box(&line);
                });
            },
        );
    }

    group.finish();
}

// =============================================================================
// Benchmarks: serde roundtrip (robot-mode JSON output)
// =============================================================================

fn bench_dashboard_serde(c: &mut Criterion) {
    let mut group = c.benchmark_group("dashboard_serde");

    let configs: &[(usize, usize, &str)] =
        &[(3, 5, "small_3p_5panes"), (16, 200, "large_16p_200panes")];

    for &(providers, panes, label) in configs {
        group.bench_with_input(
            BenchmarkId::new("serialize", label),
            &(providers, panes),
            |b, &(p, n)| {
                let mut mgr = populate_manager(p, n);
                let state = mgr.snapshot();
                b.iter(|| {
                    let json = serde_json::to_string(black_box(&state)).unwrap();
                    black_box(json);
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("deserialize", label),
            &(providers, panes),
            |b, &(p, n)| {
                let mut mgr = populate_manager(p, n);
                let state = mgr.snapshot();
                let json = serde_json::to_string(&state).unwrap();
                b.iter(|| {
                    let deser: frankenterm_core::dashboard::DashboardState =
                        serde_json::from_str(black_box(&json)).unwrap();
                    black_box(deser);
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_dashboard_snapshot,
    bench_dashboard_adapt,
    bench_dashboard_snapshot_adapt_combined,
    bench_dashboard_incremental_update,
    bench_dashboard_summary_line,
    bench_dashboard_serde,
);

criterion_main!(benches);
