//! Aggregate benchmark suite for Fork Hardening validation (`wa-3kxe.6`).
//!
//! Focus areas:
//! - Memory-growth proxy under sustained diff capture cycles
//! - Lock-free SPSC throughput vs mutex queue baseline
//! - Differential snapshot cost vs full snapshot serialization
//! - Snapshot save-cycle latency (dirty detection + diff + serialize)
//! - Telemetry instrumentation overhead

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::differential_snapshot::{BaseSnapshot, DiffSnapshotEngine};
use frankenterm_core::session_pane_state::{PaneStateSnapshot, ScrollbackRef, TerminalState};
use frankenterm_core::session_topology::{
    PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot, WindowSnapshot,
};
use frankenterm_core::spsc_ring_buffer::channel;
use frankenterm_core::telemetry::{MetricRegistry, ScopeTimer};
use std::collections::{HashMap, VecDeque};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "fork_hardening/memory_growth_proxy",
        budget: "serialized diff payload growth remains < 1KB/cycle at 50 panes / 5 dirty panes",
    },
    bench_common::BenchBudget {
        name: "fork_hardening/spsc_throughput",
        budget: "lock-free SPSC sustains > 10M ops/sec under 2-thread contention",
    },
    bench_common::BenchBudget {
        name: "fork_hardening/diff_capture",
        budget: "differential capture stays sub-linear vs full snapshot at high pane counts",
    },
    bench_common::BenchBudget {
        name: "fork_hardening/snapshot_save_cycle",
        budget: "typical diff save cycle remains under 100ms (50 panes, 10% dirty)",
    },
    bench_common::BenchBudget {
        name: "fork_hardening/telemetry_overhead",
        budget: "telemetry instrumentation overhead stays in same order as uninstrumented loop",
    },
];

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

fn make_pane_state(pane_id: u64, seq: i64) -> PaneStateSnapshot {
    let line_count = u64::try_from(seq.max(0)).unwrap_or_default();
    PaneStateSnapshot::new(pane_id, 1_000, make_terminal(24, 80))
        .with_cwd(format!("/tmp/pane-{pane_id}"))
        .with_scrollback(ScrollbackRef {
            output_segments_seq: seq,
            total_lines_captured: 1_000 + line_count,
            last_capture_at: 1_000,
        })
}

fn make_topology(pane_count: usize) -> TopologySnapshot {
    let tabs: Vec<TabSnapshot> = (0..pane_count)
        .map(|i| TabSnapshot {
            tab_id: u64::try_from(i).unwrap_or_default(),
            title: Some(format!("tab-{i}")),
            pane_tree: PaneNode::Leaf {
                pane_id: u64::try_from(i).unwrap_or_default(),
                rows: 24,
                cols: 80,
                cwd: None,
                title: None,
                is_active: i == 0,
            },
            active_pane_id: Some(u64::try_from(i).unwrap_or_default()),
        })
        .collect();

    TopologySnapshot {
        schema_version: TOPOLOGY_SCHEMA_VERSION,
        captured_at: 1_000,
        workspace_id: None,
        windows: vec![WindowSnapshot {
            window_id: 1,
            title: Some("bench-window".to_string()),
            position: None,
            size: None,
            tabs,
            active_tab_index: Some(0),
        }],
    }
}

fn make_base_snapshot(pane_count: usize) -> BaseSnapshot {
    let panes: Vec<PaneStateSnapshot> = (0..pane_count)
        .map(|i| {
            make_pane_state(
                u64::try_from(i).unwrap_or_default(),
                i64::try_from(i).unwrap_or_default(),
            )
        })
        .collect();
    BaseSnapshot::new(1_000, make_topology(pane_count), panes)
}

fn make_current_panes(pane_count: usize) -> HashMap<u64, PaneStateSnapshot> {
    (0..pane_count)
        .map(|i| {
            let pane_id = u64::try_from(i).unwrap_or_default();
            let seq = i64::try_from(i).unwrap_or_default();
            (pane_id, make_pane_state(pane_id, seq))
        })
        .collect()
}

fn dirty_count(total_panes: usize, dirty_ratio_pct: usize) -> usize {
    let computed = total_panes.saturating_mul(dirty_ratio_pct).div_ceil(100);
    computed.max(1).min(total_panes.max(1))
}

fn run_full_snapshot_capture(total_panes: usize, dirty_ratio_pct: usize, cycle: usize) -> usize {
    let dirty = dirty_count(total_panes, dirty_ratio_pct);
    let mut panes: Vec<PaneStateSnapshot> = (0..total_panes)
        .map(|i| {
            make_pane_state(
                u64::try_from(i).unwrap_or_default(),
                i64::try_from(i).unwrap_or_default(),
            )
        })
        .collect();

    for (idx, pane) in panes.iter_mut().enumerate().take(dirty) {
        pane.cwd = Some(format!("/cycle/{cycle}/pane/{idx}"));
        let cycle_u64 = u64::try_from(cycle).unwrap_or_default();
        pane.scrollback_ref = Some(ScrollbackRef {
            output_segments_seq: i64::try_from(cycle).unwrap_or_default(),
            total_lines_captured: 2_000 + cycle_u64,
            last_capture_at: 1_500 + cycle_u64,
        });
    }

    let full_snapshot = BaseSnapshot::new(2_000, make_topology(total_panes), panes);
    serde_json::to_vec(&full_snapshot)
        .expect("serialize full snapshot")
        .len()
}

fn run_differential_capture(total_panes: usize, dirty_ratio_pct: usize, cycle: usize) -> usize {
    let mut engine = DiffSnapshotEngine::new(0);
    engine.initialize(make_base_snapshot(total_panes));
    let mut current = make_current_panes(total_panes);
    let dirty = dirty_count(total_panes, dirty_ratio_pct);

    for idx in 0..dirty {
        let pane_idx = (idx + cycle) % total_panes.max(1);
        let pane_id = u64::try_from(pane_idx).unwrap_or_default();
        if let Some(state) = current.get_mut(&pane_id) {
            state.cwd = Some(format!("/diff/{cycle}/pane/{pane_idx}"));
            let cycle_u64 = u64::try_from(cycle).unwrap_or_default();
            state.scrollback_ref = Some(ScrollbackRef {
                output_segments_seq: i64::try_from(cycle).unwrap_or_default(),
                total_lines_captured: 2_000 + cycle_u64,
                last_capture_at: 1_500 + cycle_u64,
            });
        }
        engine.tracker_mut().mark_metadata(pane_id);
    }

    let cycle_u64 = u64::try_from(cycle).unwrap_or_default();
    let diff = engine
        .capture_diff(&current, None, 2_000 + cycle_u64)
        .expect("dirty panes should produce a diff");
    serde_json::to_vec(&diff).expect("serialize diff").len()
}

fn run_snapshot_save_cycle(total_panes: usize, dirty_ratio_pct: usize, cycle: usize) -> usize {
    let mut engine = DiffSnapshotEngine::new(16);
    engine.initialize(make_base_snapshot(total_panes));
    let mut current = make_current_panes(total_panes);
    let dirty = dirty_count(total_panes, dirty_ratio_pct);

    for idx in 0..dirty {
        let pane_idx = (idx + cycle) % total_panes.max(1);
        let pane_id = u64::try_from(pane_idx).unwrap_or_default();
        if let Some(state) = current.get_mut(&pane_id) {
            state.cwd = Some(format!("/snapshot/{cycle}/pane/{pane_idx}"));
            let cycle_u64 = u64::try_from(cycle).unwrap_or_default();
            state.scrollback_ref = Some(ScrollbackRef {
                output_segments_seq: i64::try_from(cycle).unwrap_or_default(),
                total_lines_captured: 3_000 + cycle_u64,
                last_capture_at: 2_000 + cycle_u64,
            });
        }
        if idx % 2 == 0 {
            engine.tracker_mut().mark_metadata(pane_id);
        } else {
            engine.tracker_mut().mark_output(pane_id);
        }
    }

    let cycle_u64 = u64::try_from(cycle).unwrap_or_default();
    let diff = engine
        .capture_diff(&current, None, 3_000 + cycle_u64)
        .expect("save cycle should produce diff");
    serde_json::to_vec(&diff)
        .expect("serialize diff for save")
        .len()
}

fn run_memory_growth_proxy(total_panes: usize, dirty_per_cycle: usize, cycles: usize) -> u64 {
    let mut engine = DiffSnapshotEngine::new(64);
    engine.initialize(make_base_snapshot(total_panes));
    let mut current = make_current_panes(total_panes);
    let mut total_bytes = 0_u64;

    for cycle in 0..cycles {
        for offset in 0..dirty_per_cycle {
            let pane_idx = (cycle + offset) % total_panes.max(1);
            let pane_id = u64::try_from(pane_idx).unwrap_or_default();
            if let Some(state) = current.get_mut(&pane_id) {
                state.cwd = Some(format!("/memory/{cycle}/pane/{pane_idx}"));
                let cycle_u64 = u64::try_from(cycle).unwrap_or_default();
                state.scrollback_ref = Some(ScrollbackRef {
                    output_segments_seq: i64::try_from(cycle).unwrap_or_default(),
                    total_lines_captured: 4_000 + cycle_u64,
                    last_capture_at: 2_500 + cycle_u64,
                });
            }
            engine.tracker_mut().mark_metadata(pane_id);
        }

        let cycle_u64 = u64::try_from(cycle).unwrap_or_default();
        if let Some(diff) = engine.capture_diff(&current, None, 4_000 + cycle_u64) {
            let diff_bytes = serde_json::to_vec(&diff).expect("serialize memory-growth diff");
            total_bytes =
                total_bytes.saturating_add(u64::try_from(diff_bytes.len()).unwrap_or_default());
        }
    }

    if cycles == 0 {
        return 0;
    }
    total_bytes / u64::try_from(cycles).unwrap_or(1)
}

fn bench_memory_growth_proxy(c: &mut Criterion) {
    let mut group = c.benchmark_group("fork_hardening/memory_growth_proxy");

    for &cycles in &[100_usize, 1_000_usize, 5_000_usize] {
        group.throughput(Throughput::Elements(
            u64::try_from(cycles).unwrap_or_default(),
        ));
        group.bench_with_input(
            BenchmarkId::from_parameter(cycles),
            &cycles,
            |b, &cycles| {
                b.iter(|| {
                    let bytes_per_cycle = run_memory_growth_proxy(50, 5, cycles);
                    black_box(bytes_per_cycle);
                });
            },
        );
    }

    group.finish();
}

fn bench_spsc_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("fork_hardening/spsc_throughput");
    const OPS: u64 = 500_000;
    group.throughput(Throughput::Elements(OPS));

    for &capacity in &[256_usize, 1_024, 4_096] {
        group.bench_with_input(
            BenchmarkId::new("lockfree_spsc", capacity),
            &capacity,
            |b, &cap| {
                b.iter(|| {
                    let (tx, rx) = channel::<u64>(cap);
                    let barrier = Arc::new(Barrier::new(2));
                    let producer_barrier = Arc::clone(&barrier);

                    let producer = thread::spawn(move || {
                        producer_barrier.wait();
                        for i in 0..OPS {
                            let mut value = i;
                            loop {
                                match tx.try_send(value) {
                                    Ok(()) => break,
                                    Err(v) => {
                                        value = v;
                                        std::hint::spin_loop();
                                    }
                                }
                            }
                        }
                        tx.close();
                    });

                    barrier.wait();
                    let mut seen = 0_u64;
                    let mut checksum = 0_u64;
                    loop {
                        if let Some(value) = rx.try_recv() {
                            seen = seen.saturating_add(1);
                            checksum = checksum.wrapping_add(value);
                            continue;
                        }
                        if rx.is_closed() {
                            break;
                        }
                        std::hint::spin_loop();
                    }

                    producer.join().expect("lockfree producer thread failed");
                    black_box((seen, checksum));
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("mutex_vecdeque_baseline", capacity),
            &capacity,
            |b, &cap| {
                b.iter(|| {
                    let queue = Arc::new(Mutex::new(VecDeque::<u64>::with_capacity(cap)));
                    let closed = Arc::new(AtomicBool::new(false));
                    let barrier = Arc::new(Barrier::new(2));

                    let producer_queue = Arc::clone(&queue);
                    let producer_closed = Arc::clone(&closed);
                    let producer_barrier = Arc::clone(&barrier);
                    let producer = thread::spawn(move || {
                        producer_barrier.wait();
                        for i in 0..OPS {
                            loop {
                                let mut guard =
                                    producer_queue.lock().expect("mutex baseline lock poisoned");
                                if guard.len() < cap {
                                    guard.push_back(i);
                                    break;
                                }
                                drop(guard);
                                std::hint::spin_loop();
                            }
                        }
                        producer_closed.store(true, Ordering::Release);
                    });

                    barrier.wait();
                    let mut seen = 0_u64;
                    let mut checksum = 0_u64;
                    loop {
                        let mut guard = queue.lock().expect("mutex baseline lock poisoned");
                        if let Some(value) = guard.pop_front() {
                            seen = seen.saturating_add(1);
                            checksum = checksum.wrapping_add(value);
                            continue;
                        }
                        if closed.load(Ordering::Acquire) {
                            break;
                        }
                        drop(guard);
                        std::hint::spin_loop();
                    }

                    producer.join().expect("mutex baseline producer failed");
                    black_box((seen, checksum));
                });
            },
        );
    }

    group.finish();
}

fn bench_diff_capture(c: &mut Criterion) {
    let mut group = c.benchmark_group("fork_hardening/diff_capture");

    for &(panes, dirty_ratio) in &[(10_usize, 1_usize), (50, 10), (200, 50)] {
        let label = format!("{panes}p_{dirty_ratio}pct");
        group.throughput(Throughput::Elements(
            u64::try_from(panes).unwrap_or_default(),
        ));

        group.bench_function(BenchmarkId::new("full_snapshot", &label), |b| {
            b.iter(|| {
                let serialized_bytes = run_full_snapshot_capture(panes, dirty_ratio, 1);
                black_box(serialized_bytes);
            });
        });

        group.bench_function(BenchmarkId::new("differential_snapshot", &label), |b| {
            b.iter(|| {
                let serialized_bytes = run_differential_capture(panes, dirty_ratio, 1);
                black_box(serialized_bytes);
            });
        });
    }

    group.finish();
}

fn bench_snapshot_save_cycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("fork_hardening/snapshot_save_cycle");

    for &(panes, dirty_ratio) in &[(50_usize, 10_usize), (200, 10), (200, 50)] {
        let label = format!("{panes}p_{dirty_ratio}pct");
        group.throughput(Throughput::Elements(
            u64::try_from(dirty_count(panes, dirty_ratio)).unwrap_or_default(),
        ));
        group.bench_function(BenchmarkId::new("save_cycle", &label), |b| {
            b.iter(|| {
                let bytes = run_snapshot_save_cycle(panes, dirty_ratio, 1);
                black_box(bytes);
            });
        });
    }

    group.finish();
}

fn bench_telemetry_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("fork_hardening/telemetry_overhead");
    const ITERS: u64 = 200_000;
    group.throughput(Throughput::Elements(ITERS));

    group.bench_function("baseline_loop", |b| {
        b.iter(|| {
            let mut acc = 0_u64;
            for i in 0..ITERS {
                acc = acc.wrapping_mul(1_664_525).wrapping_add(i);
            }
            black_box(acc);
        });
    });

    group.bench_function("record_histogram_loop", |b| {
        let registry = MetricRegistry::new();
        registry.register_histogram("fork_hardening_loop_us", 8_192);

        b.iter(|| {
            let mut acc = 0_u64;
            for i in 0..ITERS {
                let _timer = ScopeTimer::new(&registry, "fork_hardening_loop_us");
                acc = acc.wrapping_mul(1_664_525).wrapping_add(i);
                black_box(acc);
            }
            black_box(registry.counter_count());
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("fork_hardening_benchmarks", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets =
        bench_memory_growth_proxy,
        bench_spsc_throughput,
        bench_diff_capture,
        bench_snapshot_save_cycle,
        bench_telemetry_overhead
);
criterion_main!(benches);
