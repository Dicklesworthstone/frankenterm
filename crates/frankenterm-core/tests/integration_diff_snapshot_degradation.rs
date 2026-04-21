//! Integration test: differential snapshots → degradation manager.
//!
//! Exercises the cross-module flow where the diff snapshot engine tracks
//! pane changes and the degradation manager responds to subsystem health:
//!
//!   DirtyTracker.mark_*(pane_id)
//!     → DiffSnapshotEngine.capture_diff(panes, topology, now)
//!       → DiffChain.restore_latest() / restore_at(seq)
//!         → DegradationManager.enter_degraded(Capture, reason)
//!           → DegradationManager.queue_write(kind, size)
//!
//! This mirrors the real session-resume flow: the diff engine captures
//! incremental state, and when capture or DB subsystems fail, the
//! degradation manager queues writes and pauses workflows until recovery.

use std::collections::HashMap;

use frankenterm_core::degradation::{DegradationManager, OverallStatus, Subsystem};
use frankenterm_core::differential_snapshot::{
    BaseSnapshot, DiffChain, DiffSnapshotEngine, DirtyField, DirtyTracker,
};
use frankenterm_core::session_pane_state::{PaneStateSnapshot, TerminalState};
use frankenterm_core::session_topology::{
    PaneNode, TOPOLOGY_SCHEMA_VERSION, TabSnapshot, TopologySnapshot, WindowSnapshot,
};

// ── Helpers ─────────────────────────────────────────────────────────────

fn make_terminal(rows: u16, cols: u16) -> TerminalState {
    TerminalState {
        rows,
        cols,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: String::new(),
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
                is_active: id == pane_ids[0],
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

fn pane_map(pane_ids: &[u64]) -> HashMap<u64, PaneStateSnapshot> {
    pane_ids
        .iter()
        .map(|&id| (id, make_pane_state(id, 24, 80)))
        .collect()
}

// ── Tests ───────────────────────────────────────────────────────────────

/// DirtyTracker feeds into DiffSnapshotEngine: only marked panes produce
/// diff entries, and the chain grows monotonically.
#[test]
fn dirty_tracker_drives_diff_capture() {
    let mut engine = DiffSnapshotEngine::new(10);
    let pane_ids = [1, 2, 3];

    // Initialize with base snapshot.
    let base = BaseSnapshot::new(
        1000,
        make_topology(&pane_ids),
        vec![
            make_pane_state(1, 24, 80),
            make_pane_state(2, 24, 80),
            make_pane_state(3, 24, 80),
        ],
    );
    engine.initialize(base);
    assert!(engine.is_initialized());
    assert_eq!(engine.chain_len(), 0);

    // No dirty panes → capture_diff returns None.
    let current = pane_map(&pane_ids);
    assert!(engine.capture_diff(&current, None, 2000).is_none());

    // Mark pane 1 as having new output.
    engine.tracker_mut().mark_output(1);
    assert!(!engine.tracker().is_clean());
    assert_eq!(engine.tracker().dirty_count(), 1);

    // Capture diff — should produce a diff with entries.
    let diff = engine.capture_diff(&current, None, 3000);
    assert!(diff.is_some());
    let diff = diff.unwrap();
    assert_eq!(diff.captured_at, 3000);
    assert!(!diff.diffs.is_empty());

    // After capture, tracker is cleared.
    assert!(engine.tracker().is_clean());
    assert_eq!(engine.chain_len(), 1);

    // Mark panes 2 and 3.
    engine.tracker_mut().mark_metadata(2);
    engine.tracker_mut().mark_output(3);
    assert_eq!(engine.tracker().dirty_count(), 2);

    let diff2 = engine.capture_diff(&current, None, 4000);
    assert!(diff2.is_some());
    assert_eq!(engine.chain_len(), 2);
}

/// DiffChain restore produces consistent state after multiple diffs,
/// and compact merges diffs into the base.
#[test]
fn diff_chain_restore_and_compact() {
    let pane_ids = [1, 2];

    // Use a single DiffSnapshotEngine to build diffs.
    let mut engine = DiffSnapshotEngine::new(10);
    engine.initialize(BaseSnapshot::new(
        1000,
        make_topology(&pane_ids),
        vec![make_pane_state(1, 24, 80), make_pane_state(2, 24, 80)],
    ));

    let current = pane_map(&pane_ids);

    // Diff 0: pane 1 scrollback changed.
    engine.tracker_mut().mark_output(1);
    let diff0 = engine.capture_diff(&current, None, 2000).unwrap();

    // Diff 1: pane 2 metadata changed.
    engine.tracker_mut().mark_metadata(2);
    let diff1 = engine.capture_diff(&current, None, 3000).unwrap();

    // Diff 2: new pane 3 created.
    engine.tracker_mut().mark_created(3);
    let mut current_with_3 = current.clone();
    current_with_3.insert(3, make_pane_state(3, 24, 80));
    let diff2 = engine.capture_diff(&current_with_3, None, 4000).unwrap();

    // Engine's internal chain has all 3 diffs.
    assert_eq!(engine.chain_len(), 3);

    // Restore latest from the engine's chain — should reflect pane 3 creation.
    let latest = engine.restore_latest().unwrap();
    assert!(
        latest.pane_states.contains_key(&3),
        "restored state should include created pane 3"
    );
    assert_eq!(latest.pane_states.len(), 3);

    // Also test standalone DiffChain with the captured diffs.
    let base = BaseSnapshot::new(
        1000,
        make_topology(&pane_ids),
        vec![make_pane_state(1, 24, 80), make_pane_state(2, 24, 80)],
    );
    let mut chain = DiffChain::new(base);
    chain.push_diff(diff0);
    chain.push_diff(diff1);
    chain.push_diff(diff2);
    assert_eq!(chain.chain_len(), 3);

    // Restore from standalone chain — latest should also have 3 panes.
    let chain_latest = chain.restore_latest();
    assert_eq!(chain_latest.pane_states.len(), 3);
    assert!(chain_latest.pane_states.contains_key(&3));

    // Compact merges all diffs into base.
    let merged = chain.compact();
    assert!(merged > 0);
    assert_eq!(chain.chain_len(), 0);

    // Restore after compact should match pre-compact latest.
    let post_compact = chain.restore_latest();
    assert_eq!(
        post_compact.pane_states.len(),
        chain_latest.pane_states.len()
    );
}

/// Degradation manager responds to capture failures: when capture subsystem
/// degrades, writes are queued and workflows paused.
#[test]
fn degradation_responds_to_capture_failure() {
    let mut dm = DegradationManager::new();

    // Initially healthy.
    assert_eq!(dm.overall_status(), OverallStatus::Healthy);
    assert!(!dm.is_degraded(Subsystem::Capture));
    assert!(!dm.has_degradations());

    // Simulate capture pipeline failure.
    dm.enter_degraded(
        Subsystem::Capture,
        "snapshot capture timed out after 5s".to_string(),
    );
    assert!(dm.is_degraded(Subsystem::Capture));
    assert_eq!(dm.overall_status(), OverallStatus::Degraded);

    // Queue writes that couldn't be committed.
    dm.queue_write("pane_state".to_string(), 2048);
    dm.queue_write("topology".to_string(), 512);
    assert_eq!(dm.queued_write_count(), 2);
    assert_eq!(dm.queued_write_bytes(), 2560);

    // Pause a workflow that depends on snapshots.
    dm.pause_workflow("auto-backup".to_string());
    assert!(dm.is_workflow_paused("auto-backup"));

    // Report reflects all degradations.
    let report = dm.report();
    assert_eq!(report.overall, OverallStatus::Degraded);
    assert_eq!(report.active_degradations.len(), 1);
    assert_eq!(report.queued_write_count, 2);
    assert_eq!(report.paused_workflow_count, 1);

    // Recovery: capture subsystem comes back.
    dm.recover(Subsystem::Capture);
    assert!(!dm.is_degraded(Subsystem::Capture));

    // Drain queued writes for replay.
    let writes = dm.drain_queued_writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(dm.queued_write_count(), 0);

    // Resume workflow.
    dm.resume_workflow("auto-backup");
    assert!(!dm.is_workflow_paused("auto-backup"));
    assert_eq!(dm.overall_status(), OverallStatus::Healthy);
}

/// Full pipeline: diff engine captures state, failure triggers degradation,
/// recovery resumes normal operation with queued write replay.
#[test]
fn full_pipeline_diff_capture_to_degradation_recovery() {
    let mut engine = DiffSnapshotEngine::new(5);
    let mut dm = DegradationManager::new();
    let pane_ids = [1, 2, 3, 4];

    // Phase 1: healthy — initialize and capture diffs.
    let base = BaseSnapshot::new(
        1000,
        make_topology(&pane_ids),
        vec![
            make_pane_state(1, 24, 80),
            make_pane_state(2, 24, 80),
            make_pane_state(3, 24, 80),
            make_pane_state(4, 24, 80),
        ],
    );
    engine.initialize(base);
    assert_eq!(dm.overall_status(), OverallStatus::Healthy);

    // Capture 3 successful diffs.
    let current = pane_map(&pane_ids);
    for i in 0..3u64 {
        engine.tracker_mut().mark_output((i % 4) + 1);
        let diff = engine.capture_diff(&current, None, 2000 + i * 1000);
        assert!(diff.is_some());
    }
    assert_eq!(engine.chain_len(), 3);

    let telem = engine.telemetry().snapshot();
    assert_eq!(telem.diffs_captured, 3);
    assert_eq!(telem.clean_skips, 0);

    // Phase 2: failure — DB write subsystem goes down.
    dm.enter_degraded(Subsystem::DbWrite, "disk I/O error on WAL sync".to_string());
    assert_eq!(dm.overall_status(), OverallStatus::Degraded);

    // We can still capture diffs in memory, but can't persist them.
    engine.tracker_mut().mark_output(1);
    engine.tracker_mut().mark_metadata(2);
    let diff = engine.capture_diff(&current, None, 5000);
    assert!(diff.is_some());

    // Queue the write that couldn't be persisted.
    if dm.is_degraded(Subsystem::DbWrite) {
        let diff_data = diff.unwrap();
        let estimated_size = diff_data.diffs.len() * 256; // rough estimate
        dm.queue_write("diff_snapshot".to_string(), estimated_size);
    }
    assert_eq!(dm.queued_write_count(), 1);

    // Phase 3: capture subsystem also degrades (cascade).
    dm.enter_degraded(Subsystem::Capture, "pane listing timeout".to_string());
    dm.pause_workflow("periodic-snapshot".to_string());

    // Multiple degradations → still Degraded (not Critical unless unavailable).
    assert_eq!(dm.overall_status(), OverallStatus::Degraded);
    assert_eq!(dm.report().active_degradations.len(), 2);

    // Phase 4: recovery — DB comes back first.
    dm.recover(Subsystem::DbWrite);
    let writes = dm.drain_queued_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].kind, "diff_snapshot");

    // Capture recovers next.
    dm.recover(Subsystem::Capture);
    dm.resume_workflow("periodic-snapshot");
    assert_eq!(dm.overall_status(), OverallStatus::Healthy);

    // Phase 5: verify chain integrity after the episode.
    // The engine should still have its in-memory chain intact.
    assert_eq!(engine.chain_len(), 4); // 3 pre-failure + 1 during failure
    let restored = engine.restore_latest().unwrap();
    assert_eq!(restored.pane_states.len(), 4);

    // Compact the chain to merge all diffs.
    let merged = engine.compact();
    assert!(merged.is_some());
    assert_eq!(engine.chain_len(), 0);
}

/// Pane lifecycle events (create/close) flow through dirty tracker into
/// diff entries with the correct DirtyField tags.
#[test]
fn pane_lifecycle_through_dirty_tracker() {
    let mut tracker = DirtyTracker::new();

    // Create + output + metadata for pane 10.
    tracker.mark_created(10);
    tracker.mark_output(10);
    tracker.mark_metadata(10);

    let fields = tracker.dirty_fields(10).unwrap();
    assert!(fields.contains(&DirtyField::Created));
    assert!(fields.contains(&DirtyField::Scrollback));
    assert!(fields.contains(&DirtyField::Metadata));

    // Close pane 10.
    tracker.mark_closed(10);
    let fields = tracker.dirty_fields(10).unwrap();
    assert!(fields.contains(&DirtyField::Closed));

    // Layout change.
    tracker.mark_layout_dirty();
    assert!(tracker.is_layout_dirty());

    // Multiple panes tracked simultaneously.
    tracker.mark_output(20);
    tracker.mark_output(30);
    let dirty_ids = tracker.dirty_pane_ids();
    assert!(dirty_ids.contains(&10));
    assert!(dirty_ids.contains(&20));
    assert!(dirty_ids.contains(&30));
    assert_eq!(tracker.dirty_count(), 3);

    // Clear resets everything.
    tracker.clear();
    assert!(tracker.is_clean());
    assert_eq!(tracker.dirty_count(), 0);
    assert!(!tracker.is_layout_dirty());
}

/// Degradation escalation: degraded → unavailable → critical status,
/// with pattern engine and workflow engine affected.
#[test]
fn degradation_escalation_to_critical() {
    let mut dm = DegradationManager::new();

    // Start with pattern engine degradation.
    dm.enter_degraded(
        Subsystem::PatternEngine,
        "regex compilation failed".to_string(),
    );
    dm.disable_pattern("expensive-regex-rule".to_string());
    assert!(dm.is_pattern_disabled("expensive-regex-rule"));
    assert_eq!(dm.disabled_patterns().len(), 1);

    // Escalate to unavailable.
    dm.enter_unavailable(Subsystem::PatternEngine, "pattern engine OOM".to_string());
    assert!(dm.is_unavailable(Subsystem::PatternEngine));
    assert_eq!(dm.overall_status(), OverallStatus::Critical);

    // Recovery attempt recorded.
    dm.record_recovery_attempt(Subsystem::PatternEngine);

    // Snapshot captures the state.
    let snapshots = dm.snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].subsystem, Subsystem::PatternEngine);
    assert_eq!(snapshots[0].recovery_attempts, 1);

    // Recover → healthy.
    dm.recover(Subsystem::PatternEngine);
    assert_eq!(dm.overall_status(), OverallStatus::Healthy);
    assert!(!dm.has_degradations());
}
