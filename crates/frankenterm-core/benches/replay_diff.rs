//! Criterion benchmark for replay diff latency and report generation.
//!
//! Bead: ft-og6q6.7.3

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::differential_snapshot::{BaseSnapshot, DiffSnapshotEngine};
use frankenterm_core::replay_performance::{
    ReplayPerformanceBaseline, ReplayPerformanceBudgets, ReplayPerformanceSample,
    compare_against_baseline,
};
use frankenterm_core::session_pane_state::{PaneStateSnapshot, ScrollbackRef, TerminalState};
use frankenterm_core::session_topology::{
    PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot, WindowSnapshot,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "replay_diff/diff_1000_divergences",
        budget: "< 1s for 1000 divergences",
    },
    bench_common::BenchBudget {
        name: "replay_diff/standard_report_generation",
        budget: "< 100ms for standard replay performance report",
    },
];

const DIFF_PANE_COUNT: usize = 1_000;
const DIFF_DIVERGENCES: usize = 1_000;

fn make_terminal() -> TerminalState {
    TerminalState {
        rows: 24,
        cols: 80,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: "replay-diff-bench".to_string(),
    }
}

fn make_pane_state(pane_id: u64, seq: i64) -> PaneStateSnapshot {
    let total_lines = u64::try_from(seq.max(0))
        .unwrap_or_default()
        .saturating_add(100);
    PaneStateSnapshot::new(pane_id, 1_700_000_000_000, make_terminal())
        .with_cwd(format!("/tmp/replay-diff/{pane_id}"))
        .with_scrollback(ScrollbackRef {
            output_segments_seq: seq,
            total_lines_captured: total_lines,
            last_capture_at: 1_700_000_000_000,
        })
}

fn make_topology(pane_count: usize) -> TopologySnapshot {
    let tabs: Vec<TabSnapshot> = (0..pane_count)
        .map(|i| {
            let pane_id = u64::try_from(i).unwrap_or_default();
            TabSnapshot {
                tab_id: pane_id,
                title: Some(format!("tab-{pane_id}")),
                pane_tree: PaneNode::Leaf {
                    pane_id,
                    rows: 24,
                    cols: 80,
                    cwd: None,
                    title: None,
                    is_active: i == 0,
                },
                active_pane_id: Some(pane_id),
            }
        })
        .collect();

    TopologySnapshot {
        schema_version: TOPOLOGY_SCHEMA_VERSION,
        captured_at: 1_700_000_000_000,
        workspace_id: None,
        windows: vec![WindowSnapshot {
            window_id: 1,
            title: Some("replay-diff-window".to_string()),
            position: None,
            size: None,
            tabs,
            active_tab_index: Some(0),
        }],
    }
}

fn build_base_snapshot(pane_count: usize) -> BaseSnapshot {
    let panes: Vec<PaneStateSnapshot> = (0..pane_count)
        .map(|i| {
            let pane_id = u64::try_from(i).unwrap_or_default();
            let seq = i64::try_from(i).unwrap_or_default();
            make_pane_state(pane_id, seq)
        })
        .collect();
    BaseSnapshot::new(1_700_000_000_000, make_topology(pane_count), panes)
}

fn build_current_snapshot(
    pane_count: usize,
    divergences: usize,
) -> HashMap<u64, PaneStateSnapshot> {
    let mut map: HashMap<u64, PaneStateSnapshot> = (0..pane_count)
        .map(|i| {
            let pane_id = u64::try_from(i).unwrap_or_default();
            let seq = i64::try_from(i).unwrap_or_default();
            (pane_id, make_pane_state(pane_id, seq))
        })
        .collect();

    for i in 0..divergences.min(pane_count) {
        let pane_id = u64::try_from(i).unwrap_or_default();
        if let Some(state) = map.get_mut(&pane_id) {
            state.cwd = Some(format!("/tmp/replay-diff/mutated/{pane_id}"));
            state.scrollback_ref = Some(ScrollbackRef {
                output_segments_seq: i64::try_from(10_000 + i).unwrap_or_default(),
                total_lines_captured: 50_000,
                last_capture_at: 1_700_000_010_000,
            });
        }
    }

    map
}

fn run_diff_benchmark() -> usize {
    let base = build_base_snapshot(DIFF_PANE_COUNT);
    let current = build_current_snapshot(DIFF_PANE_COUNT, DIFF_DIVERGENCES);
    let mut engine = DiffSnapshotEngine::new(0);
    engine.initialize(base);

    for i in 0..DIFF_DIVERGENCES {
        let pane_id = u64::try_from(i).unwrap_or_default();
        engine.tracker_mut().mark_metadata(pane_id);
        engine.tracker_mut().mark_output(pane_id);
    }

    let diff = engine
        .capture_diff(&current, None, 1_700_000_020_000)
        .expect("expected divergences for benchmark");
    diff.diffs.len()
}

fn bench_diff_1000_divergences(c: &mut Criterion) {
    let mut group = c.benchmark_group("replay_diff");
    group.throughput(Throughput::Elements(1000));

    group.bench_function("diff_1000_divergences", |b| {
        b.iter_batched(
            || (),
            |_| {
                let divergences = run_diff_benchmark();
                black_box(divergences);
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_standard_report_generation(c: &mut Criterion) {
    let mut group = c.benchmark_group("replay_diff");
    group.throughput(Throughput::Elements(1));

    let budgets = ReplayPerformanceBudgets::default();
    let baseline = ReplayPerformanceBaseline::from_sample(
        "bench-seed",
        "2026-02-24T00:00:00Z",
        ReplayPerformanceSample {
            capture_overhead_ms_per_event: 0.40,
            replay_throughput_events_per_sec: 220_000.0,
            diff_latency_ms_per_1000_divergences: 420.0,
            report_generation_ms: 15.0,
            artifact_read_events_per_sec: 1_100_000.0,
        },
    );
    let sample = ReplayPerformanceSample {
        capture_overhead_ms_per_event: 0.48,
        replay_throughput_events_per_sec: 210_000.0,
        diff_latency_ms_per_1000_divergences: 500.0,
        report_generation_ms: 18.0,
        artifact_read_events_per_sec: 1_050_000.0,
    };

    group.bench_function("standard_report_generation", |b| {
        b.iter(|| {
            let report = compare_against_baseline(budgets.clone(), Some(baseline.clone()), sample);
            let payload = serde_json::to_vec(&report).expect("serialize report");
            black_box(payload.len());
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("replay_diff", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group! {
    name = benches;
    config = bench_config();
    targets = bench_diff_1000_divergences, bench_standard_report_generation
}
criterion_main!(benches);
