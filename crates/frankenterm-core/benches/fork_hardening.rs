//! Benchmarks for fork-hardening primitives (`wa-3kxe` / `wa-3kxe.7`).
//!
//! Tracks three core paths used by the hardening effort:
//! - lock-free capture queue roundtrip throughput
//! - differential snapshot dirty-capture path
//! - telemetry instrumentation overhead

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::differential_snapshot::{BaseSnapshot, DiffSnapshotEngine};
use frankenterm_core::session_pane_state::{PaneStateSnapshot, TerminalState};
use frankenterm_core::session_topology::{
    PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot, WindowSnapshot,
};
use frankenterm_core::spsc_ring_buffer::channel;
use frankenterm_core::telemetry::{MetricRegistry, ScopeTimer};
use std::collections::HashMap;
use std::hint::black_box;
use std::sync::mpsc::{TryRecvError, TrySendError, sync_channel};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "fork_hardening/spsc_capture_path/spsc_ring",
        budget: "500k enqueue+dequeue ops; lock-free ring path should stay comfortably sub-200ms",
    },
    bench_common::BenchBudget {
        name: "fork_hardening/snapshot_capture_path/diff_capture",
        budget: "dirty diff capture for 50-200 panes should remain sub-250ms",
    },
    bench_common::BenchBudget {
        name: "fork_hardening/telemetry_overhead/with_scope_timer",
        budget: "scope timer instrumentation should stay sub-20ms for tiny hot-path work",
    },
];

const SPSC_OPS: u64 = 500_000;

fn bench_spsc_capture_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("fork_hardening/spsc_capture_path");
    group.sample_size(20);

    for &capacity in &[64usize, 256, 1024] {
        group.throughput(Throughput::Elements(SPSC_OPS));

        group.bench_with_input(
            BenchmarkId::new("spsc_ring", capacity),
            &capacity,
            |b, &cap| {
                b.iter(|| {
                    let (tx, rx) = channel::<u64>(cap);
                    let mut checksum = 0u64;

                    for i in 0..SPSC_OPS {
                        let mut next = i;
                        loop {
                            match tx.try_send(next) {
                                Ok(()) => break,
                                Err(v) => {
                                    next = v;
                                    std::hint::spin_loop();
                                }
                            }
                        }

                        loop {
                            if let Some(v) = rx.try_recv() {
                                checksum = checksum.wrapping_add(v);
                                break;
                            }
                            std::hint::spin_loop();
                        }
                    }

                    black_box(checksum);
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("sync_channel", capacity),
            &capacity,
            |b, &cap| {
                b.iter(|| {
                    let (tx, rx) = sync_channel::<u64>(cap);
                    let mut checksum = 0u64;

                    for i in 0..SPSC_OPS {
                        let mut next = i;
                        loop {
                            match tx.try_send(next) {
                                Ok(()) => break,
                                Err(TrySendError::Full(v)) => {
                                    next = v;
                                    std::hint::spin_loop();
                                }
                                Err(TrySendError::Disconnected(_)) => {
                                    panic!("sync channel unexpectedly disconnected");
                                }
                            }
                        }

                        loop {
                            match rx.try_recv() {
                                Ok(v) => {
                                    checksum = checksum.wrapping_add(v);
                                    break;
                                }
                                Err(TryRecvError::Empty) => {
                                    std::hint::spin_loop();
                                }
                                Err(TryRecvError::Disconnected) => {
                                    panic!("sync channel unexpectedly disconnected");
                                }
                            }
                        }
                    }

                    black_box(checksum);
                });
            },
        );
    }

    group.finish();
}

fn make_terminal(rows: u16, cols: u16) -> TerminalState {
    TerminalState {
        rows,
        cols,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: "fork-hardening-bench".to_string(),
    }
}

fn make_pane_state(pane_id: u64) -> PaneStateSnapshot {
    PaneStateSnapshot::new(pane_id, 1_000, make_terminal(24, 80))
        .with_cwd(format!("/tmp/fork-hardening/pane-{pane_id}"))
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
        captured_at: 1_000,
        workspace_id: None,
        windows: vec![WindowSnapshot {
            window_id: 0,
            title: Some("fork-hardening-window".to_string()),
            position: None,
            size: None,
            tabs,
            active_tab_index: Some(0),
        }],
    }
}

fn make_base(pane_count: usize) -> BaseSnapshot {
    let panes: Vec<PaneStateSnapshot> =
        (0..pane_count).map(|i| make_pane_state(i as u64)).collect();
    BaseSnapshot::new(1_000, make_topology(pane_count), panes)
}

fn make_live_panes(pane_count: usize, dirty_count: usize) -> HashMap<u64, PaneStateSnapshot> {
    let mut panes: HashMap<u64, PaneStateSnapshot> = (0..pane_count)
        .map(|i| (i as u64, make_pane_state(i as u64)))
        .collect();

    for pane_id in 0..dirty_count.min(pane_count) {
        if let Some(state) = panes.get_mut(&(pane_id as u64)) {
            state.cwd = Some(format!("/tmp/fork-hardening/dirty-{pane_id}"));
        }
    }
    panes
}

fn bench_snapshot_capture_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("fork_hardening/snapshot_capture_path");
    group.sample_size(20);

    for &(pane_count, dirty_count) in &[(50usize, 3usize), (200, 5), (200, 20)] {
        let label = format!("{pane_count}p_{dirty_count}d");
        let base = make_base(pane_count);
        let current = make_live_panes(pane_count, dirty_count);
        let dirty_ids: Vec<u64> = (0..dirty_count.min(pane_count) as u64).collect();

        group.throughput(Throughput::Elements(dirty_ids.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("full_snapshot_clone", &label),
            &current,
            |b, current| {
                b.iter(|| {
                    let cloned = current.clone();
                    black_box(cloned.len());
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("diff_capture", &label), &(), |b, ()| {
            let mut engine = DiffSnapshotEngine::new(0);
            engine.initialize(base.clone());

            b.iter(|| {
                if engine.chain_len() > 64 {
                    engine.initialize(base.clone());
                }

                for &pane_id in &dirty_ids {
                    engine.tracker_mut().mark_metadata(pane_id);
                }

                let diff = engine
                    .capture_diff(&current, None, 2_000)
                    .expect("dirty diff should be produced");
                black_box(diff.diffs.len());
            });
        });
    }

    group.finish();
}

fn hot_path_work(mut seed: u64) -> u64 {
    for _ in 0..128 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed ^= seed >> 13;
    }
    seed
}

fn bench_telemetry_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("fork_hardening/telemetry_overhead");
    group.sample_size(25);

    let registry = MetricRegistry::new();
    registry.register_histogram("fork_hardening_scope_timer_us", 4096);

    group.bench_function("baseline_no_timer", |b| {
        b.iter(|| {
            let value = hot_path_work(black_box(0x0BAD_5EED));
            black_box(value);
        });
    });

    group.bench_function("with_scope_timer", |b| {
        b.iter(|| {
            let _timer = ScopeTimer::new(&registry, "fork_hardening_scope_timer_us");
            let value = hot_path_work(black_box(0x0BAD_5EED));
            black_box(value);
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("fork_hardening", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_spsc_capture_path, bench_snapshot_capture_path, bench_telemetry_overhead
);
criterion_main!(benches);
