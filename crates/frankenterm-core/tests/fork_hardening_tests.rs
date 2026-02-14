//! Fork Hardening validation suite (`wa-3kxe.6`).
//!
//! Coverage focus:
//! - Differential snapshot isomorphism under randomized mutation sequences
//! - Capture schedule invariance (same mutations, different capture cadence)
//! - Lock-free SPSC no-loss/no-corruption two-thread roundtrip
//! - Deterministic eviction decisions under memory pressure
//! - Telemetry histogram/aggregate accuracy checks
//! - Platform-specific memory budget integration (Linux cgroup tempdir + macOS fallback)

use proptest::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Barrier};
use std::thread;

use frankenterm_core::differential_snapshot::{BaseSnapshot, DiffSnapshotEngine};
use frankenterm_core::memory_budget::{
    MemoryBudgetConfig, MemoryBudgetManager, cgroups_v2_available,
};
use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::pane_tiers::PaneTier;
use frankenterm_core::scrollback_eviction::{
    EvictionConfig, PaneTierSource, ScrollbackEvictor, SegmentStore,
};
use frankenterm_core::session_pane_state::{PaneStateSnapshot, ScrollbackRef, TerminalState};
use frankenterm_core::session_topology::{
    PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot, WindowSnapshot,
};
use frankenterm_core::spsc_ring_buffer::channel;
use frankenterm_core::telemetry::{Histogram, ResourceSnapshot, TelemetryStore};

fn make_terminal(rows: u16, cols: u16) -> TerminalState {
    TerminalState {
        rows,
        cols,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: "fork-hardening-test".to_string(),
    }
}

fn make_pane_state(pane_id: u64, tick: u64) -> PaneStateSnapshot {
    PaneStateSnapshot::new(pane_id, tick, make_terminal(24, 80))
        .with_cwd(format!("/tmp/pane-{pane_id}"))
        .with_scrollback(ScrollbackRef {
            output_segments_seq: i64::try_from(tick).unwrap_or_default(),
            total_lines_captured: 100 + tick,
            last_capture_at: tick,
        })
}

fn make_topology(pane_ids: &[u64]) -> TopologySnapshot {
    let tabs: Vec<TabSnapshot> = pane_ids
        .iter()
        .copied()
        .map(|pane_id| TabSnapshot {
            tab_id: pane_id,
            title: Some(format!("tab-{pane_id}")),
            pane_tree: PaneNode::Leaf {
                pane_id,
                rows: 24,
                cols: 80,
                cwd: None,
                title: None,
                is_active: false,
            },
            active_pane_id: Some(pane_id),
        })
        .collect();

    TopologySnapshot {
        schema_version: TOPOLOGY_SCHEMA_VERSION,
        captured_at: 1_000,
        workspace_id: None,
        windows: vec![WindowSnapshot {
            window_id: 1,
            title: Some("fork-hardening".to_string()),
            position: None,
            size: None,
            tabs,
            active_tab_index: Some(0),
        }],
    }
}

fn make_base_snapshot(pane_ids: &[u64]) -> BaseSnapshot {
    let panes: Vec<PaneStateSnapshot> = pane_ids
        .iter()
        .copied()
        .map(|pane_id| make_pane_state(pane_id, 1_000))
        .collect();
    BaseSnapshot::new(1_000, make_topology(pane_ids), panes)
}

fn make_live_map(pane_ids: &[u64]) -> HashMap<u64, PaneStateSnapshot> {
    pane_ids
        .iter()
        .copied()
        .map(|pane_id| (pane_id, make_pane_state(pane_id, 1_000)))
        .collect()
}

fn canonical_pane_state(panes: &HashMap<u64, PaneStateSnapshot>) -> String {
    let ordered: BTreeMap<u64, PaneStateSnapshot> = panes
        .iter()
        .map(|(&pane_id, state)| (pane_id, state.clone()))
        .collect();
    serde_json::to_string(&ordered).expect("serialize canonical pane map")
}

#[derive(Debug, Clone)]
enum DiffAction {
    Metadata(u8),
    Scrollback(u8),
    Create(u8),
    Close(u8),
}

fn arb_diff_actions(max_len: usize) -> impl Strategy<Value = Vec<DiffAction>> {
    prop::collection::vec(
        prop_oneof![
            (0u8..=15).prop_map(DiffAction::Metadata),
            (0u8..=15).prop_map(DiffAction::Scrollback),
            (0u8..=15).prop_map(DiffAction::Create),
            (0u8..=15).prop_map(DiffAction::Close),
        ],
        1..=max_len,
    )
}

fn apply_action(
    action: &DiffAction,
    live: &mut HashMap<u64, PaneStateSnapshot>,
    engine: &mut DiffSnapshotEngine,
    tick: u64,
) {
    match action {
        DiffAction::Metadata(id) => {
            let pane_id = u64::from(*id);
            if let Some(state) = live.get_mut(&pane_id) {
                state.cwd = Some(format!("/meta/{tick}/pane/{pane_id}"));
                state.captured_at = tick;
                engine.tracker_mut().mark_metadata(pane_id);
            }
        }
        DiffAction::Scrollback(id) => {
            let pane_id = u64::from(*id);
            if let Some(state) = live.get_mut(&pane_id) {
                state.scrollback_ref = Some(ScrollbackRef {
                    output_segments_seq: i64::try_from(tick).unwrap_or_default(),
                    total_lines_captured: 1_000 + tick,
                    last_capture_at: tick,
                });
                engine.tracker_mut().mark_output(pane_id);
            }
        }
        DiffAction::Create(id) => {
            let pane_id = u64::from(*id);
            if let std::collections::hash_map::Entry::Vacant(slot) = live.entry(pane_id) {
                slot.insert(make_pane_state(pane_id, tick));
                engine.tracker_mut().mark_created(pane_id);
            }
        }
        DiffAction::Close(id) => {
            let pane_id = u64::from(*id);
            if live.remove(&pane_id).is_some() {
                engine.tracker_mut().mark_closed(pane_id);
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn diff_engine_isomorphic_to_reference_state(actions in arb_diff_actions(120)) {
        let initial_panes = vec![0_u64, 1, 2, 3, 4];
        let mut engine = DiffSnapshotEngine::new(0);
        engine.initialize(make_base_snapshot(&initial_panes));

        let mut live = make_live_map(&initial_panes);

        for (step, action) in actions.iter().enumerate() {
            let tick = 10_000 + u64::try_from(step).unwrap_or_default();
            apply_action(action, &mut live, &mut engine, tick);
            let _ = engine.capture_diff(&live, None, tick);

            let restored = engine
                .restore_latest()
                .expect("engine should always have a recoverable latest snapshot");
            let restored_state = canonical_pane_state(&restored.pane_states);
            let live_state = canonical_pane_state(&live);
            prop_assert_eq!(
                restored_state, live_state,
                "state diverged at step {:?} action={:?}",
                step,
                action
            );
        }
    }
}

fn run_schedule(
    capture_every: usize,
) -> (
    HashMap<u64, PaneStateSnapshot>,
    HashMap<u64, PaneStateSnapshot>,
) {
    let initial_panes = vec![0_u64, 1, 2, 3, 4];
    let mut engine = DiffSnapshotEngine::new(0);
    engine.initialize(make_base_snapshot(&initial_panes));
    let mut live = make_live_map(&initial_panes);

    let mut actions = Vec::new();
    for i in 0_u8..64 {
        let action = match i % 4 {
            0 => DiffAction::Metadata(i % 8),
            1 => DiffAction::Scrollback((i + 3) % 8),
            2 => DiffAction::Create((i + 5) % 12),
            _ => DiffAction::Close((i + 7) % 12),
        };
        actions.push(action);
    }

    for (step, action) in actions.iter().enumerate() {
        let tick = 20_000 + u64::try_from(step).unwrap_or_default();
        apply_action(action, &mut live, &mut engine, tick);
        if (step + 1) % capture_every == 0 {
            let _ = engine.capture_diff(&live, None, tick);
        }
    }

    let final_tick = 99_999;
    let _ = engine.capture_diff(&live, None, final_tick);
    let restored = engine
        .restore_latest()
        .expect("schedule run should produce a latest snapshot")
        .pane_states;
    (restored, live)
}

#[test]
fn capture_schedule_invariance_across_batching() {
    let (baseline_restored, baseline_live) = run_schedule(1);
    let baseline = canonical_pane_state(&baseline_restored);
    assert_eq!(baseline, canonical_pane_state(&baseline_live));

    for &capture_every in &[2_usize, 4, 8] {
        let (restored, live) = run_schedule(capture_every);
        assert_eq!(
            canonical_pane_state(&restored),
            baseline,
            "final state diverged for capture_every={capture_every}"
        );
        assert_eq!(
            canonical_pane_state(&restored),
            canonical_pane_state(&live),
            "restored vs live mismatch for capture_every={capture_every}"
        );
    }
}

#[test]
fn spsc_channel_two_thread_roundtrip_has_no_loss() {
    const OPS: u64 = 200_000;
    let (tx, rx) = channel::<u64>(1_024);
    let barrier = Arc::new(Barrier::new(2));
    let producer_barrier = Arc::clone(&barrier);

    let producer = thread::spawn(move || {
        producer_barrier.wait();
        for value in 0..OPS {
            let mut current = value;
            loop {
                match tx.try_send(current) {
                    Ok(()) => break,
                    Err(v) => {
                        current = v;
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
    producer.join().expect("producer thread failed");

    let expected_checksum = OPS.saturating_mul(OPS.saturating_sub(1)) / 2;
    assert_eq!(seen, OPS, "consumer should observe every produced value");
    assert_eq!(
        checksum, expected_checksum,
        "checksum mismatch indicates data loss/corruption"
    );
}

#[derive(Debug, Clone)]
struct FixedStore {
    segments: HashMap<u64, usize>,
}

impl SegmentStore for FixedStore {
    fn count_segments(&self, pane_id: u64) -> Result<usize, String> {
        Ok(*self.segments.get(&pane_id).unwrap_or(&0))
    }

    fn delete_oldest_segments(&self, _pane_id: u64, count: usize) -> Result<usize, String> {
        Ok(count)
    }

    fn list_pane_ids(&self) -> Result<Vec<u64>, String> {
        let mut pane_ids: Vec<u64> = self.segments.keys().copied().collect();
        pane_ids.sort_unstable();
        Ok(pane_ids)
    }
}

#[derive(Debug, Clone)]
struct FixedTierSource {
    tiers: HashMap<u64, PaneTier>,
}

impl PaneTierSource for FixedTierSource {
    fn tier_for(&self, pane_id: u64) -> Option<PaneTier> {
        self.tiers.get(&pane_id).copied()
    }
}

#[test]
fn eviction_planning_is_deterministic_under_identical_inputs() {
    let store = FixedStore {
        segments: HashMap::from([(1, 12_000), (2, 7_000), (3, 2_500), (4, 400), (5, 125)]),
    };
    let tiers = FixedTierSource {
        tiers: HashMap::from([
            (1, PaneTier::Active),
            (2, PaneTier::Thinking),
            (3, PaneTier::Idle),
            (4, PaneTier::Background),
            (5, PaneTier::Dormant),
        ]),
    };
    let evictor = ScrollbackEvictor::new(EvictionConfig::default(), store, tiers);

    let first = evictor
        .plan(MemoryPressureTier::Orange)
        .expect("first plan should succeed");
    let second = evictor
        .plan(MemoryPressureTier::Orange)
        .expect("second plan should succeed");

    let first_json = serde_json::to_value(&first).expect("serialize first plan");
    let second_json = serde_json::to_value(&second).expect("serialize second plan");
    assert_eq!(first_json, second_json, "planning must be deterministic");
    assert!(
        first.total_segments_to_remove > 0,
        "fixture should require eviction"
    );
}

#[test]
fn telemetry_histogram_quantiles_match_known_distribution() {
    let mut histogram = Histogram::new("capture_latency_us", 1_024);
    for value in 1_u32..=100 {
        histogram.record(f64::from(value));
    }

    assert_eq!(histogram.p50(), Some(50.0));
    assert_eq!(histogram.p95(), Some(95.0));
    assert_eq!(histogram.p99(), Some(99.0));

    let (min, max) = histogram.min_max().expect("histogram should have min/max");
    assert!((min - 1.0).abs() < f64::EPSILON);
    assert!((max - 100.0).abs() < f64::EPSILON);
}

#[test]
fn telemetry_hourly_aggregate_matches_expected_values() {
    let snapshots = vec![
        ResourceSnapshot {
            pid: 11,
            rss_bytes: 100,
            virt_bytes: 1_000,
            fd_count: 10,
            io_read_bytes: None,
            io_write_bytes: None,
            cpu_percent: Some(10.0),
            timestamp_secs: 1_700_000_000,
        },
        ResourceSnapshot {
            pid: 11,
            rss_bytes: 200,
            virt_bytes: 2_000,
            fd_count: 20,
            io_read_bytes: None,
            io_write_bytes: None,
            cpu_percent: None,
            timestamp_secs: 1_700_000_100,
        },
        ResourceSnapshot {
            pid: 11,
            rss_bytes: 300,
            virt_bytes: 3_000,
            fd_count: 30,
            io_read_bytes: None,
            io_write_bytes: None,
            cpu_percent: Some(20.0),
            timestamp_secs: 1_700_000_200,
        },
    ];

    let aggregate = TelemetryStore::aggregate_snapshots(1_700_000_000, &snapshots)
        .expect("aggregate should exist for non-empty snapshots");

    assert_eq!(aggregate.sample_count, 3);
    assert_eq!(aggregate.mean_rss_bytes, 200);
    assert_eq!(aggregate.peak_rss_bytes, 300);
    assert_eq!(aggregate.mean_fd_count, 20);
    assert_eq!(aggregate.peak_fd_count, 30);
    assert_eq!(aggregate.mean_cpu_percent, Some(15.0));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_memory_budget_cgroup_tempdir_integration() {
    let _ = cgroups_v2_available();

    let dir = tempfile::tempdir().expect("create temp cgroup dir");
    let base = dir.path().to_string_lossy().to_string();
    let config = MemoryBudgetConfig {
        enabled: true,
        default_budget_bytes: 1_000_000,
        high_ratio: 0.8,
        sample_interval_ms: 1_000,
        cgroup_base_path: base.clone(),
        use_cgroups: true,
        oom_score_adj: 0,
    };

    let manager = MemoryBudgetManager::new(config);
    let budget = manager.register_pane(42, None);
    assert!(
        budget.cgroup_active,
        "tempdir cgroup should be created on Linux"
    );

    std::fs::write(format!("{base}/pane-42/memory.current"), "850000")
        .expect("write synthetic memory.current");
    let summary = manager.sample_all();
    assert_eq!(summary.pane_count, 1);
    assert_eq!(summary.throttled_count, 1);
    assert_eq!(summary.over_budget_count, 0);

    let _ = manager.unregister_pane(42);
    assert!(!std::path::Path::new(&format!("{base}/pane-42")).exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_memory_budget_falls_back_to_advisory_mode() {
    assert!(
        !cgroups_v2_available(),
        "cgroups v2 should report unavailable on macOS"
    );

    let config = MemoryBudgetConfig {
        enabled: true,
        default_budget_bytes: 512 * 1024 * 1024,
        high_ratio: 0.8,
        sample_interval_ms: 1_000,
        cgroup_base_path: "/tmp/frankenterm-cgroup-unused".to_string(),
        use_cgroups: true,
        oom_score_adj: -500,
    };

    let manager = MemoryBudgetManager::new(config);
    let budget = manager.register_pane(7, Some(std::process::id()));
    assert!(
        !budget.cgroup_active,
        "macOS should not activate Linux cgroups"
    );

    let summary = manager.sample_all();
    assert_eq!(summary.pane_count, 1);
    assert!(
        summary.total_current_bytes > 0,
        "macOS fallback should still read process RSS"
    );
}
