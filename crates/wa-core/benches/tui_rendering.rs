//! Benchmarks for ftui TUI rendering performance.
//!
//! Measures frame rendering latency across all views and terminal sizes.
//! These baselines are used to detect performance regressions during the
//! ratatui→ftui migration (FTUI-08.1).
//!
//! Performance budget: < 16ms per frame (60 fps target).

#![cfg(feature = "ftui")]

use std::sync::Arc;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use wa_core::circuit_breaker::{CircuitBreakerStatus, CircuitStateKind};
use wa_core::tui::{
    AppConfig, EventFilters, EventView, HealthStatus, PaneView, QueryClient, QueryError,
    SearchResultView, TriageAction, TriageItemView, View, WaModel, WorkflowProgressView,
};

mod bench_common;

#[allow(dead_code)] // Referenced by CI budget enforcement tooling
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "tui_render_80x24",
        budget: "p50 < 1ms, p99 < 5ms (standard terminal)",
    },
    bench_common::BenchBudget {
        name: "tui_render_120x50",
        budget: "p50 < 2ms, p99 < 8ms (large terminal)",
    },
    bench_common::BenchBudget {
        name: "tui_render_40x10",
        budget: "p50 < 500µs, p99 < 2ms (small terminal)",
    },
    bench_common::BenchBudget {
        name: "tui_update_key",
        budget: "p50 < 100µs, p99 < 500µs (key event processing)",
    },
    bench_common::BenchBudget {
        name: "tui_refresh_data",
        budget: "p50 < 500µs, p99 < 2ms (data refresh from mock query)",
    },
];

// -- Mock QueryClient for benchmarks --

struct BenchQuery {
    pane_count: usize,
    event_count: usize,
}

impl BenchQuery {
    fn small() -> Self {
        Self {
            pane_count: 3,
            event_count: 5,
        }
    }

    fn large() -> Self {
        Self {
            pane_count: 20,
            event_count: 100,
        }
    }
}

impl QueryClient for BenchQuery {
    fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
        Ok((0..self.pane_count)
            .map(|i| PaneView {
                pane_id: i as u64,
                title: format!("pane-{i}"),
                domain: if i % 3 == 0 {
                    "ssh".to_string()
                } else {
                    "local".to_string()
                },
                cwd: Some(format!("/home/user/project-{i}")),
                is_excluded: false,
                agent_type: match i % 4 {
                    0 => Some("codex".to_string()),
                    1 => Some("claude".to_string()),
                    2 => Some("gemini".to_string()),
                    _ => None,
                },
                pane_state: "active".to_string(),
                last_activity_ts: Some(1700000000 + i as i64 * 60),
                unhandled_event_count: (i % 5) as u32,
            })
            .collect())
    }

    fn list_events(&self, _filters: &EventFilters) -> Result<Vec<EventView>, QueryError> {
        Ok((0..self.event_count)
            .map(|i| EventView {
                id: i as i64,
                rule_id: format!("codex.usage.{}", ["reached", "warning", "error"][i % 3]),
                pane_id: (i % self.pane_count.max(1)) as u64,
                severity: ["info", "warning", "error"][i % 3].to_string(),
                message: format!("Event {i}: benchmark test event with some detail text"),
                timestamp: 1700000000 + i as i64 * 30,
                handled: i % 4 == 0,
                triage_state: None,
                labels: vec![],
                note: None,
            })
            .collect())
    }

    fn list_triage_items(&self) -> Result<Vec<TriageItemView>, QueryError> {
        Ok((0..5)
            .map(|i| TriageItemView {
                section: "events".to_string(),
                severity: ["warning", "error"][i % 2].to_string(),
                title: format!("Triage item {i}"),
                detail: format!("Detail for triage item {i} that needs attention"),
                actions: vec![TriageAction {
                    label: "Mute".to_string(),
                    command: format!("wa mute {i}"),
                }],
                event_id: Some(i as i64),
                pane_id: Some(i as u64),
                workflow_id: None,
            })
            .collect())
    }

    fn search(&self, _query: &str, limit: usize) -> Result<Vec<SearchResultView>, QueryError> {
        Ok((0..limit.min(10))
            .map(|i| SearchResultView {
                pane_id: i as u64,
                timestamp: 1700000000 + i as i64 * 60,
                snippet: format!("Search result {i}: matched content here"),
                rank: (i as f64).mul_add(-0.1, 1.0),
            })
            .collect())
    }

    fn health(&self) -> Result<HealthStatus, QueryError> {
        Ok(HealthStatus {
            watcher_running: true,
            db_accessible: true,
            wezterm_accessible: true,
            wezterm_circuit: CircuitBreakerStatus {
                state: CircuitStateKind::Closed,
                consecutive_failures: 0,
                failure_threshold: 5,
                success_threshold: 3,
                open_cooldown_ms: 30000,
                open_for_ms: None,
                cooldown_remaining_ms: None,
                half_open_successes: None,
            },
            pane_count: self.pane_count,
            event_count: self.event_count,
            last_capture_ts: Some(1700000000),
        })
    }

    fn is_watcher_running(&self) -> bool {
        true
    }

    fn mark_event_muted(&self, _event_id: i64) -> Result<(), QueryError> {
        Ok(())
    }

    fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError> {
        Ok(vec![WorkflowProgressView {
            id: "wf-1".to_string(),
            workflow_name: "account_rotation".to_string(),
            pane_id: 0,
            current_step: 2,
            total_steps: 5,
            started_at: 1700000000,
            updated_at: 1700000100,
            status: "running".to_string(),
            error: None,
        }])
    }
}

fn make_model(query: BenchQuery) -> WaModel {
    let config = AppConfig {
        refresh_interval: Duration::from_secs(2),
        debug: false,
    };
    let mut model = WaModel::new(Arc::new(query), config);
    model.refresh_data();
    model
}

// -- Benchmarks --

fn bench_render_per_view(c: &mut Criterion) {
    use ftui::Model as _;

    let mut group = c.benchmark_group("tui_render_per_view");

    for &view in View::all() {
        group.bench_with_input(
            BenchmarkId::new("80x24", format!("{view:?}")),
            &view,
            |b, &v| {
                let mut model = make_model(BenchQuery::small());
                model.view_state.current_view = v;
                b.iter(|| {
                    let mut pool = ftui::GraphemePool::new();
                    let mut frame = ftui::Frame::new(80, 24, &mut pool);
                    model.view(&mut frame);
                });
            },
        );
    }

    group.finish();
}

fn bench_render_sizes(c: &mut Criterion) {
    use ftui::Model as _;

    let mut group = c.benchmark_group("tui_render_sizes");

    let sizes: &[(u16, u16, &str)] = &[
        (40, 10, "40x10"),
        (80, 24, "80x24"),
        (120, 50, "120x50"),
        (200, 60, "200x60"),
    ];

    for &(w, h, label) in sizes {
        group.bench_with_input(BenchmarkId::new("Home", label), &(w, h), |b, &(w, h)| {
            let mut model = make_model(BenchQuery::small());
            model.view_state.current_view = View::Home;
            b.iter(|| {
                let mut pool = ftui::GraphemePool::new();
                let mut frame = ftui::Frame::new(w, h, &mut pool);
                model.view(&mut frame);
            });
        });

        group.bench_with_input(BenchmarkId::new("Events", label), &(w, h), |b, &(w, h)| {
            let mut model = make_model(BenchQuery::small());
            model.view_state.current_view = View::Events;
            b.iter(|| {
                let mut pool = ftui::GraphemePool::new();
                let mut frame = ftui::Frame::new(w, h, &mut pool);
                model.view(&mut frame);
            });
        });
    }

    group.finish();
}

fn bench_render_data_scale(c: &mut Criterion) {
    use ftui::Model as _;

    let mut group = c.benchmark_group("tui_render_data_scale");

    // Small dataset (3 panes, 5 events)
    group.bench_function("Events/small", |b| {
        let mut model = make_model(BenchQuery::small());
        model.view_state.current_view = View::Events;
        b.iter(|| {
            let mut pool = ftui::GraphemePool::new();
            let mut frame = ftui::Frame::new(80, 24, &mut pool);
            model.view(&mut frame);
        });
    });

    // Large dataset (20 panes, 100 events)
    group.bench_function("Events/large", |b| {
        let mut model = make_model(BenchQuery::large());
        model.view_state.current_view = View::Events;
        b.iter(|| {
            let mut pool = ftui::GraphemePool::new();
            let mut frame = ftui::Frame::new(80, 24, &mut pool);
            model.view(&mut frame);
        });
    });

    // Panes with large dataset
    group.bench_function("Panes/large", |b| {
        let mut model = make_model(BenchQuery::large());
        model.view_state.current_view = View::Panes;
        b.iter(|| {
            let mut pool = ftui::GraphemePool::new();
            let mut frame = ftui::Frame::new(80, 24, &mut pool);
            model.view(&mut frame);
        });
    });

    group.finish();
}

fn bench_update_key(c: &mut Criterion) {
    use ftui::Model as _;

    let mut group = c.benchmark_group("tui_update_key");

    group.bench_function("Tab", |b| {
        let mut model = make_model(BenchQuery::small());
        b.iter(|| {
            let key = ftui::KeyEvent {
                code: ftui::KeyCode::Tab,
                kind: ftui::KeyEventKind::Press,
                modifiers: ftui::Modifiers::empty(),
            };
            let msg = wa_core::tui::WaMsg::TermEvent(ftui::Event::Key(key));
            model.update(msg);
        });
    });

    group.bench_function("Down", |b| {
        let mut model = make_model(BenchQuery::small());
        model.view_state.current_view = View::Events;
        b.iter(|| {
            let key = ftui::KeyEvent {
                code: ftui::KeyCode::Down,
                kind: ftui::KeyEventKind::Press,
                modifiers: ftui::Modifiers::empty(),
            };
            let msg = wa_core::tui::WaMsg::TermEvent(ftui::Event::Key(key));
            model.update(msg);
        });
    });

    group.finish();
}

fn bench_refresh_data(c: &mut Criterion) {
    let mut group = c.benchmark_group("tui_refresh_data");

    group.bench_function("small", |b| {
        let mut model = make_model(BenchQuery::small());
        b.iter(|| {
            model.refresh_data();
        });
    });

    group.bench_function("large", |b| {
        let mut model = make_model(BenchQuery::large());
        b.iter(|| {
            model.refresh_data();
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_render_per_view,
    bench_render_sizes,
    bench_render_data_scale,
    bench_update_key,
    bench_refresh_data,
);

criterion_main!(benches);
