//! Property-based tests for differential snapshot engine telemetry counters (ft-3kxe.14).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. clean_skips increments on clean tracker
//! 3. diffs_captured / total_diff_entries track successful captures
//! 4. layout_diffs tracks layout changes
//! 5. auto_compactions triggers at chain limit
//! 6. manual_compactions tracks explicit compact() calls
//! 7. Serde roundtrip for snapshot
//! 8. Counter monotonicity across repeated captures

use proptest::prelude::*;
use std::collections::HashMap;

use frankenterm_core::differential_snapshot::{
    BaseSnapshot, DiffSnapshotEngine, DiffSnapshotTelemetrySnapshot,
};
use frankenterm_core::session_pane_state::{PaneStateSnapshot, TerminalState};
use frankenterm_core::session_topology::{
    PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot, TOPOLOGY_SCHEMA_VERSION,
};

// =============================================================================
// Helpers
// =============================================================================

fn make_terminal(rows: u16, cols: u16) -> TerminalState {
    TerminalState {
        rows,
        cols,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: "test".to_string(),
    }
}

fn make_pane_state(pane_id: u64, rows: u16, cols: u16) -> PaneStateSnapshot {
    PaneStateSnapshot::new(pane_id, 1000, make_terminal(rows, cols))
        .with_cwd(format!("/home/user/pane-{pane_id}"))
}

fn make_topology(pane_ids: &[u64]) -> TopologySnapshot {
    let tabs: Vec<TabSnapshot> = pane_ids
        .iter()
        .map(|&id| TabSnapshot {
            tab_id: id,
            title: Some(format!("tab-{id}")),
            pane_tree: PaneNode::Leaf {
                pane_id: id,
                rows: 24,
                cols: 80,
                cwd: None,
                title: None,
                is_active: false,
            },
            active_pane_id: Some(id),
        })
        .collect();

    TopologySnapshot {
        schema_version: TOPOLOGY_SCHEMA_VERSION,
        captured_at: 1000,
        workspace_id: None,
        windows: vec![WindowSnapshot {
            window_id: 0,
            title: Some("test-window".to_string()),
            position: None,
            size: None,
            tabs,
            active_tab_index: Some(0),
        }],
    }
}

fn make_base(pane_ids: &[u64]) -> BaseSnapshot {
    let pane_states: Vec<PaneStateSnapshot> = pane_ids
        .iter()
        .map(|&id| make_pane_state(id, 24, 80))
        .collect();
    BaseSnapshot::new(1000, make_topology(pane_ids), pane_states)
}

fn make_current_panes(pane_ids: &[u64]) -> HashMap<u64, PaneStateSnapshot> {
    pane_ids
        .iter()
        .map(|&id| (id, make_pane_state(id, 24, 80)))
        .collect()
}

fn make_engine(pane_ids: &[u64], max_chain_len: usize) -> DiffSnapshotEngine {
    let mut engine = DiffSnapshotEngine::new(max_chain_len);
    engine.initialize(make_base(pane_ids));
    engine
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let engine = make_engine(&[1, 2, 3], 10);
    let snap = engine.telemetry().snapshot();

    assert_eq!(snap.diffs_captured, 0);
    assert_eq!(snap.clean_skips, 0);
    assert_eq!(snap.auto_compactions, 0);
    assert_eq!(snap.manual_compactions, 0);
    assert_eq!(snap.total_diff_entries, 0);
    assert_eq!(snap.layout_diffs, 0);
}

#[test]
fn clean_skip_increments_on_clean_tracker() {
    let mut engine = make_engine(&[1, 2], 10);
    let panes = make_current_panes(&[1, 2]);

    // No dirty marks — should skip
    let result = engine.capture_diff(&panes, None, 2000);
    assert!(result.is_none());

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.clean_skips, 1);
    assert_eq!(snap.diffs_captured, 0);
}

#[test]
fn capture_increments_diffs_captured() {
    let mut engine = make_engine(&[1, 2], 10);
    let panes = make_current_panes(&[1, 2]);

    engine.tracker_mut().mark_output(1);
    let result = engine.capture_diff(&panes, None, 2000);
    assert!(result.is_some());

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.diffs_captured, 1);
    assert_eq!(snap.total_diff_entries, 1); // one PaneScrollbackChanged
}

#[test]
fn multiple_diff_entries_tracked() {
    let mut engine = make_engine(&[1, 2, 3], 10);
    let panes = make_current_panes(&[1, 2, 3]);

    engine.tracker_mut().mark_output(1);
    engine.tracker_mut().mark_metadata(2);
    engine.tracker_mut().mark_output(3);
    let result = engine.capture_diff(&panes, None, 2000);
    assert!(result.is_some());

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.diffs_captured, 1);
    assert_eq!(snap.total_diff_entries, 3);
}

#[test]
fn layout_diffs_tracked() {
    let mut engine = make_engine(&[1, 2], 10);
    let panes = make_current_panes(&[1, 2]);
    let topo = make_topology(&[1, 2]);

    engine.tracker_mut().mark_layout_dirty();
    let result = engine.capture_diff(&panes, Some(&topo), 2000);
    assert!(result.is_some());

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.layout_diffs, 1);
    assert_eq!(snap.diffs_captured, 1);
}

#[test]
fn auto_compaction_tracked() {
    // max_chain_len=2 so after 3 captures, auto-compaction fires
    let mut engine = make_engine(&[1], 2);
    let panes = make_current_panes(&[1]);

    for i in 0..3 {
        engine.tracker_mut().mark_output(1);
        engine.capture_diff(&panes, None, 2000 + i * 1000);
    }

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.diffs_captured, 3);
    assert!(snap.auto_compactions >= 1, "expected auto-compaction after exceeding chain len");
}

#[test]
fn manual_compaction_tracked() {
    let mut engine = make_engine(&[1], 0); // 0 = no auto compact
    let panes = make_current_panes(&[1]);

    engine.tracker_mut().mark_output(1);
    engine.capture_diff(&panes, None, 2000);

    engine.compact();
    engine.compact();

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.manual_compactions, 2);
    assert_eq!(snap.auto_compactions, 0);
}

#[test]
fn uninitialized_compact_does_not_count() {
    let mut engine = DiffSnapshotEngine::new(10);
    // Not initialized — compact returns None
    let result = engine.compact();
    assert!(result.is_none());

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.manual_compactions, 0);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = DiffSnapshotTelemetrySnapshot {
        diffs_captured: 42,
        clean_skips: 10,
        auto_compactions: 3,
        manual_compactions: 7,
        total_diff_entries: 200,
        layout_diffs: 5,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: DiffSnapshotTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn mixed_operations_accumulate() {
    let mut engine = make_engine(&[1, 2], 0);
    let panes = make_current_panes(&[1, 2]);
    let topo = make_topology(&[1, 2]);

    // Clean skip
    engine.capture_diff(&panes, None, 1000);

    // Scrollback change
    engine.tracker_mut().mark_output(1);
    engine.capture_diff(&panes, None, 2000);

    // Layout change + metadata
    engine.tracker_mut().mark_layout_dirty();
    engine.tracker_mut().mark_metadata(2);
    engine.capture_diff(&panes, Some(&topo), 3000);

    // Another clean skip
    engine.capture_diff(&panes, None, 4000);

    // Manual compact
    engine.compact();

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.clean_skips, 2);
    assert_eq!(snap.diffs_captured, 2);
    assert_eq!(snap.layout_diffs, 1);
    assert_eq!(snap.manual_compactions, 1);
    assert_eq!(snap.auto_compactions, 0);
    // First capture: 1 entry (scrollback). Second: 2 entries (layout + metadata)
    assert_eq!(snap.total_diff_entries, 3);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn diffs_captured_equals_successful_calls(
        dirty_panes in prop::collection::vec(1u64..5, 1..20),
    ) {
        let mut engine = make_engine(&[1, 2, 3, 4], 0);
        let panes = make_current_panes(&[1, 2, 3, 4]);
        let mut expected_captures = 0u64;

        for &pid in &dirty_panes {
            engine.tracker_mut().mark_output(pid);
            let result = engine.capture_diff(&panes, None, 2000 + expected_captures * 1000);
            if result.is_some() {
                expected_captures += 1;
            }
        }

        let snap = engine.telemetry().snapshot();
        prop_assert_eq!(snap.diffs_captured, expected_captures);
    }

    #[test]
    fn counters_monotonically_increase(
        rounds in prop::collection::vec(
            (1u64..5, prop::bool::ANY),
            1..15,
        ),
    ) {
        let mut engine = make_engine(&[1, 2, 3, 4], 0);
        let panes = make_current_panes(&[1, 2, 3, 4]);
        let topo = make_topology(&[1, 2, 3, 4]);
        let mut prev = engine.telemetry().snapshot();

        for (i, (pid, with_layout)) in rounds.iter().enumerate() {
            engine.tracker_mut().mark_output(*pid);
            if *with_layout {
                engine.tracker_mut().mark_layout_dirty();
            }
            engine.capture_diff(&panes, Some(&topo), 2000 + (i as u64) * 1000);

            let snap = engine.telemetry().snapshot();
            prop_assert!(
                snap.diffs_captured >= prev.diffs_captured,
                "diffs_captured decreased: {} -> {}",
                prev.diffs_captured, snap.diffs_captured
            );
            prop_assert!(
                snap.total_diff_entries >= prev.total_diff_entries,
                "total_diff_entries decreased: {} -> {}",
                prev.total_diff_entries, snap.total_diff_entries
            );
            prop_assert!(
                snap.layout_diffs >= prev.layout_diffs,
                "layout_diffs decreased: {} -> {}",
                prev.layout_diffs, snap.layout_diffs
            );

            prev = snap;
        }
    }

    #[test]
    fn clean_skips_count_clean_captures(
        pattern in prop::collection::vec(prop::bool::ANY, 1..30),
    ) {
        let mut engine = make_engine(&[1], 0);
        let panes = make_current_panes(&[1]);
        let mut expected_skips = 0u64;

        for (i, &dirty) in pattern.iter().enumerate() {
            if dirty {
                engine.tracker_mut().mark_output(1);
            }
            let result = engine.capture_diff(&panes, None, 2000 + (i as u64) * 1000);
            if result.is_none() {
                expected_skips += 1;
            }
        }

        let snap = engine.telemetry().snapshot();
        prop_assert_eq!(snap.clean_skips, expected_skips);
    }

    #[test]
    fn auto_compaction_fires_at_limit(
        chain_limit in 2usize..8,
        num_captures in 1usize..20,
    ) {
        let mut engine = make_engine(&[1], chain_limit);
        let panes = make_current_panes(&[1]);

        for i in 0..num_captures {
            engine.tracker_mut().mark_output(1);
            engine.capture_diff(&panes, None, 2000 + (i as u64) * 1000);
        }

        let snap = engine.telemetry().snapshot();
        // Auto-compactions should be non-negative
        // If we captured more than chain_limit, at least one compaction should occur
        if num_captures > chain_limit {
            prop_assert!(
                snap.auto_compactions >= 1,
                "expected auto-compaction: captured={}, limit={}, compactions={}",
                num_captures, chain_limit, snap.auto_compactions
            );
        }
        prop_assert_eq!(snap.diffs_captured, num_captures as u64);
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        captured in 0u64..10000,
        skips in 0u64..5000,
        auto_c in 0u64..1000,
        manual_c in 0u64..1000,
        entries in 0u64..50000,
        layouts in 0u64..5000,
    ) {
        let snap = DiffSnapshotTelemetrySnapshot {
            diffs_captured: captured,
            clean_skips: skips,
            auto_compactions: auto_c,
            manual_compactions: manual_c,
            total_diff_entries: entries,
            layout_diffs: layouts,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: DiffSnapshotTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
