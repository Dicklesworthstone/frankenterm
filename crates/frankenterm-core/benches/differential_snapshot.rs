//! Benchmarks for differential snapshot engine.
//!
//! Performance budgets:
//! - Diff snapshot (200 panes, 5 dirty): **< 50ms**
//! - Compaction (10-element chain, 200 panes): **< 100ms**
//! - Restore from chain (10 diffs, 200 panes): **< 50ms**
//! - Dirty tracker mark/clear (1000 ops): **< 100us**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::differential_snapshot::{
    BaseSnapshot, DiffChain, DiffSnapshot, DiffSnapshotEngine, DirtyTracker, SnapshotDiff,
};
use frankenterm_core::session_pane_state::{PaneStateSnapshot, ScrollbackRef, TerminalState};
use frankenterm_core::session_topology::{
    PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot, WindowSnapshot,
};
use std::collections::HashMap;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "diff_snapshot_capture",
        budget: "p50 < 50ms (200 panes, 5 dirty)",
    },
    bench_common::BenchBudget {
        name: "diff_chain_compact",
        budget: "p50 < 100ms (10-chain, 200 panes)",
    },
    bench_common::BenchBudget {
        name: "diff_chain_restore",
        budget: "p50 < 50ms (10-chain, 200 panes)",
    },
    bench_common::BenchBudget {
        name: "dirty_tracker_ops",
        budget: "p50 < 100us (1000 mark+clear ops)",
    },
];

fn make_terminal(rows: u16, cols: u16) -> TerminalState {
    TerminalState {
        rows,
        cols,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: "bench-pane".to_string(),
    }
}

fn make_pane_state(pane_id: u64) -> PaneStateSnapshot {
    PaneStateSnapshot::new(pane_id, 1000, make_terminal(24, 80))
        .with_cwd(format!("/home/user/project-{pane_id}"))
}

fn make_topology(pane_count: usize) -> TopologySnapshot {
    let tabs: Vec<TabSnapshot> = (0..pane_count)
        .map(|i| TabSnapshot {
            tab_id: i as u64,
            title: Some(format!("tab-{i}")),
            pane_tree: PaneNode::Leaf {
                pane_id: i as u64,
                rows: 24,
                cols: 80,
                cwd: None,
                title: None,
                is_active: i == 0,
            },
            active_pane_id: Some(i as u64),
        })
        .collect();

    TopologySnapshot {
        schema_version: TOPOLOGY_SCHEMA_VERSION,
        captured_at: 1000,
        workspace_id: None,
        windows: vec![WindowSnapshot {
            window_id: 0,
            title: Some("bench-window".to_string()),
            position: None,
            size: None,
            tabs,
            active_tab_index: Some(0),
        }],
    }
}

fn make_base(pane_count: usize) -> BaseSnapshot {
    let pane_states: Vec<PaneStateSnapshot> =
        (0..pane_count).map(|i| make_pane_state(i as u64)).collect();
    BaseSnapshot::new(1000, make_topology(pane_count), pane_states)
}

fn make_current_panes(pane_count: usize) -> HashMap<u64, PaneStateSnapshot> {
    (0..pane_count)
        .map(|i| (i as u64, make_pane_state(i as u64)))
        .collect()
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_diff_capture(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_snapshot/capture");

    for &(total, dirty) in &[(50, 3), (100, 5), (200, 5), (200, 20)] {
        let label = format!("{total}p_{dirty}d");
        let base = make_base(total);

        group.throughput(Throughput::Elements(dirty as u64));
        group.bench_with_input(BenchmarkId::new("diff", &label), &base, |b, base| {
            b.iter(|| {
                let mut engine = DiffSnapshotEngine::new(0);
                engine.initialize(base.clone());

                // Mark `dirty` panes as dirty
                for i in 0..dirty {
                    engine.tracker_mut().mark_metadata(i as u64);
                }

                let mut current = make_current_panes(total);
                // Modify the dirty panes
                for i in 0..dirty {
                    current.get_mut(&(i as u64)).unwrap().cwd = Some(format!("/modified/{i}"));
                }

                engine.capture_diff(&current, None, 2000)
            });
        });
    }

    group.finish();
}

fn bench_chain_restore(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_snapshot/restore");

    for &(total, chain_len) in &[(50, 5), (100, 10), (200, 10), (200, 20)] {
        let label = format!("{total}p_{chain_len}d");
        let base = make_base(total);
        let mut chain = DiffChain::new(base);

        // Build a chain of diffs
        for i in 1..=chain_len {
            let mut diffs = Vec::new();
            // Each diff modifies 5 panes
            for j in 0..5usize {
                let pane_id = ((i * 5 + j) % total) as u64;
                let mut ps = make_pane_state(pane_id);
                ps.cwd = Some(format!("/round/{i}/pane/{j}"));
                diffs.push(SnapshotDiff::PaneMetadataChanged {
                    pane_id,
                    new_state: ps,
                });
            }
            chain.push_diff(DiffSnapshot {
                seq: 0,
                captured_at: 1000 + (i as u64) * 1000,
                diffs,
            });
        }

        group.throughput(Throughput::Elements(chain_len as u64));
        group.bench_with_input(BenchmarkId::new("restore", &label), &chain, |b, chain| {
            b.iter(|| chain.restore_latest());
        });
    }

    group.finish();
}

fn bench_chain_compact(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_snapshot/compact");

    for &(total, chain_len) in &[(50, 5), (100, 10), (200, 10)] {
        let label = format!("{total}p_{chain_len}d");

        group.throughput(Throughput::Elements(chain_len as u64));
        group.bench_with_input(
            BenchmarkId::new("compact", &label),
            &(total, chain_len),
            |b, &(total, chain_len)| {
                b.iter(|| {
                    let base = make_base(total);
                    let mut chain = DiffChain::new(base);

                    for i in 1..=chain_len {
                        let mut diffs = Vec::new();
                        for j in 0..5usize {
                            let pane_id = ((i * 5 + j) % total) as u64;
                            let mut ps = make_pane_state(pane_id);
                            ps.cwd = Some(format!("/round/{i}"));
                            diffs.push(SnapshotDiff::PaneMetadataChanged {
                                pane_id,
                                new_state: ps,
                            });
                        }
                        chain.push_diff(DiffSnapshot {
                            seq: 0,
                            captured_at: 1000 + (i as u64) * 1000,
                            diffs,
                        });
                    }

                    chain.compact()
                });
            },
        );
    }

    group.finish();
}

fn bench_dirty_tracker(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_snapshot/dirty_tracker");

    for &ops in &[100, 500, 1000] {
        group.throughput(Throughput::Elements(ops as u64));
        group.bench_with_input(BenchmarkId::new("mark_clear", ops), &ops, |b, &ops| {
            b.iter(|| {
                let mut tracker = DirtyTracker::new();
                for i in 0..ops as u64 {
                    tracker.mark_output(i % 200);
                    tracker.mark_metadata(i % 200);
                }
                let _ = tracker.dirty_count();
                tracker.clear();
            });
        });
    }

    group.finish();
}

fn bench_diff_with_scrollback(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_snapshot/scrollback_updates");

    for &total in &[50, 100, 200] {
        let base = make_base(total);

        group.throughput(Throughput::Elements(5));
        group.bench_with_input(BenchmarkId::new("5_scrollback", total), &base, |b, base| {
            b.iter(|| {
                let mut engine = DiffSnapshotEngine::new(0);
                engine.initialize(base.clone());

                for i in 0..5u64 {
                    engine.tracker_mut().mark_output(i);
                }

                let mut current = make_current_panes(total);
                for i in 0..5u64 {
                    current.get_mut(&i).unwrap().scrollback_ref = Some(ScrollbackRef {
                        output_segments_seq: 100 + i as i64,
                        total_lines_captured: 5000 + i,
                        last_capture_at: 2000,
                    });
                }

                engine.capture_diff(&current, None, 2000)
            });
        });
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("differential_snapshot", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_diff_capture,
        bench_chain_restore,
        bench_chain_compact,
        bench_dirty_tracker,
        bench_diff_with_scrollback
);
criterion_main!(benches);
