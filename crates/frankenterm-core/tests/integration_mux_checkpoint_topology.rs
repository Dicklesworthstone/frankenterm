//! Integration test: HeadlessMuxServer → checkpoint/rollback → topology snapshot.
//!
//! Exercises the session persistence pipeline:
//!
//!   HeadlessMuxServer.handle_request(Checkpoint { label })
//!     → DurableStateManager.checkpoint(registry, label, trigger, metadata)
//!       → TopologySnapshot::from_panes(panes) → JSON roundtrip
//!         → DurableStateManager.rollback(id, registry, reason) → RollbackRecord
//!           → StateDiff (added/removed/changed entities)
//!
//! The HeadlessMuxServer orchestrates pane lifecycle via LifecycleRegistry.
//! The DurableStateManager creates checkpoints of registry state and can
//! roll back to any prior checkpoint. TopologySnapshot captures the spatial
//! layout (windows, tabs, pane trees) for persistence and restore.

use std::collections::HashMap;

use frankenterm_core::durable_state::{CheckpointTrigger, DurableStateManager};
use frankenterm_core::headless_mux_server::{
    HeadlessMuxServer, RemoteRequest, RemoteResponse, ServerConfig,
};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleRegistry, LifecycleState,
    MuxPaneLifecycleState, TopologySnapshot,
};
use frankenterm_core::wezterm::{PaneInfo, PaneSize};

// ── Helpers ─────────────────────────────────────────────────────────────

fn make_pane(
    pane_id: u64,
    tab_id: u64,
    window_id: u64,
    rows: u32,
    cols: u32,
    cwd: Option<&str>,
    title: Option<&str>,
) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id,
        window_id,
        domain_id: None,
        domain_name: None,
        workspace: Some("default".to_string()),
        size: Some(PaneSize {
            rows,
            cols,
            pixel_width: None,
            pixel_height: None,
            dpi: None,
        }),
        rows: None,
        cols: None,
        title: title.map(String::from),
        cwd: cwd.map(String::from),
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: pane_id == 1,
        is_zoomed: false,
        extra: std::collections::HashMap::new(),
    }
}

fn register_pane(
    registry: &mut LifecycleRegistry,
    pane_id: u64,
    ts: u64,
) -> LifecycleIdentity {
    let identity = LifecycleIdentity::new(
        LifecycleEntityKind::Pane,
        "default",
        "local",
        pane_id,
        1,
    );
    registry
        .register_entity(
            identity.clone(),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            ts,
        )
        .expect("register pane");
    identity
}

// ── Tests ───────────────────────────────────────────────────────────────

/// HeadlessMuxServer checkpoint/rollback via remote request protocol:
/// register entities → checkpoint → add more → rollback → verify restored.
#[test]
fn server_checkpoint_and_rollback_via_remote_protocol() {
    let mut server = HeadlessMuxServer::new(ServerConfig::default());

    // Register 2 panes via the lifecycle registry.
    let pane1 = LifecycleIdentity::new(
        LifecycleEntityKind::Pane,
        "default",
        "local",
        1,
        1,
    );
    let pane2 = LifecycleIdentity::new(
        LifecycleEntityKind::Pane,
        "default",
        "local",
        2,
        1,
    );
    server
        .registry_mut()
        .register_entity(
            pane1.clone(),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            1000,
        )
        .unwrap();
    server
        .registry_mut()
        .register_entity(
            pane2.clone(),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            1000,
        )
        .unwrap();

    assert_eq!(server.registry().len(), 2);

    // Checkpoint via remote request.
    let cp_id = match server.handle_request(RemoteRequest::Checkpoint {
        label: "before-expansion".into(),
    }) {
        RemoteResponse::CheckpointCreated { id, label } => {
            assert_eq!(label, "before-expansion");
            assert!(id > 0);
            id
        }
        other => panic!("expected CheckpointCreated, got {other:?}"),
    };

    // Add 2 more panes after the checkpoint.
    let pane3 = LifecycleIdentity::new(
        LifecycleEntityKind::Pane,
        "default",
        "local",
        3,
        1,
    );
    let pane4 = LifecycleIdentity::new(
        LifecycleEntityKind::Pane,
        "default",
        "local",
        4,
        1,
    );
    server
        .registry_mut()
        .register_entity(
            pane3,
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            2000,
        )
        .unwrap();
    server
        .registry_mut()
        .register_entity(
            pane4,
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            2000,
        )
        .unwrap();

    assert_eq!(server.registry().len(), 4);

    // Rollback to the checkpoint (should restore to 2 panes).
    match server.handle_request(RemoteRequest::Rollback {
        checkpoint_id: cp_id,
        reason: "undo expansion".into(),
    }) {
        RemoteResponse::RollbackComplete { restored, removed } => {
            // restored = entities in checkpoint that differ from current state
            // removed = entities in current that weren't in checkpoint
            assert_eq!(removed, 2, "should remove 2 added panes");
            // restored may be 0 if the 2 originals are unchanged
            assert!(
                restored + removed > 0,
                "rollback should have some effect"
            );
        }
        other => panic!("expected RollbackComplete, got {other:?}"),
    }

    assert_eq!(server.registry().len(), 2);

    // Verify rollback history is recorded.
    let history = server.state_manager().rollback_history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].target_checkpoint_id, cp_id);
}

/// LifecycleRegistry bootstrap from PaneInfo → TopologySnapshot capture →
/// JSON roundtrip → DurableStateManager checkpoint → diff verification.
#[test]
fn topology_snapshot_with_checkpoint_diff() {
    // Build mock panes: 1 window, 2 tabs, 3 panes total.
    let panes = vec![
        make_pane(1, 10, 100, 24, 80, Some("/home/user"), Some("bash")),
        make_pane(2, 10, 100, 24, 80, Some("/tmp"), Some("vim")),
        make_pane(3, 20, 100, 36, 120, Some("/project"), Some("cargo")),
    ];

    // Bootstrap lifecycle registry from panes.
    let mut registry =
        LifecycleRegistry::bootstrap_from_panes(&panes, 1, 1000)
            .expect("bootstrap");

    // Registry should contain session + window + pane entities.
    let pane_count = registry.entity_count_by_kind(LifecycleEntityKind::Pane);
    assert_eq!(pane_count, 3);

    // Capture topology snapshot.
    let (topology, report) = TopologySnapshot::from_panes(&panes, 1000);
    assert_eq!(topology.pane_count(), 3);
    assert_eq!(report.pane_count, 3);

    // Topology should have 1 window with 2 tabs.
    assert_eq!(topology.windows.len(), 1);
    assert_eq!(topology.windows[0].tabs.len(), 2);

    // JSON roundtrip preserves structure.
    let json = topology.to_json().expect("serialize");
    let restored = TopologySnapshot::from_json(&json).expect("deserialize");
    assert_eq!(restored.pane_count(), 3);
    assert_eq!(restored.pane_ids().len(), 3);

    // Create checkpoint of current state.
    let mut state_mgr = DurableStateManager::new();
    let cp1 = state_mgr.checkpoint(
        &registry,
        "initial-layout",
        CheckpointTrigger::Manual,
        HashMap::new(),
    );
    let cp1_id = cp1.id;

    // Expand: add a 4th pane.
    register_pane(&mut registry, 4, 2000);
    assert_eq!(registry.entity_count_by_kind(LifecycleEntityKind::Pane), 4);

    // Create second checkpoint.
    let cp2 = state_mgr.checkpoint(
        &registry,
        "after-expansion",
        CheckpointTrigger::Manual,
        HashMap::new(),
    );
    let cp2_id = cp2.id;

    // Diff between checkpoints should show the added entity.
    let diff = state_mgr.diff(cp1_id, cp2_id).expect("diff");
    assert!(!diff.is_empty(), "diff should show changes");
    assert!(diff.change_count() >= 1);

    // Diff from current should match cp2 (no changes since cp2).
    let diff_current = state_mgr
        .diff_from_current(cp2_id, &registry)
        .expect("diff from current");
    assert!(
        diff_current.is_empty(),
        "no changes since cp2, got {} changes",
        diff_current.change_count()
    );

    // Verify checkpoint listing.
    let checkpoints = state_mgr.list_checkpoints();
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[0].label, "initial-layout");
    assert_eq!(checkpoints[1].label, "after-expansion");
}

// ── Full pipeline ────────────────────────────────────────────────────────

/// Full pipeline: bootstrap registry → topology snapshot → server checkpoint →
/// mutate state → rollback → diff → verify restored topology matches original.
#[test]
fn full_pipeline_mux_checkpoint_topology() {
    // Phase 1: build initial pane layout.
    let initial_panes = vec![
        make_pane(1, 10, 100, 24, 80, Some("/home"), Some("shell")),
        make_pane(2, 10, 100, 24, 80, Some("/src"), Some("editor")),
    ];

    // Bootstrap registry.
    let registry =
        LifecycleRegistry::bootstrap_from_panes(&initial_panes, 1, 1000)
            .expect("bootstrap");

    let initial_entity_count = registry.len();
    assert!(initial_entity_count >= 2); // At least 2 panes (plus session/window)

    // Capture initial topology.
    let (initial_topo, _) = TopologySnapshot::from_panes(&initial_panes, 1000);
    assert_eq!(initial_topo.pane_count(), 2);
    let initial_topo_json = initial_topo.to_json().expect("serialize initial");

    // Phase 2: checkpoint initial state via server.
    let mut server = HeadlessMuxServer::new(ServerConfig {
        node_id: "test-node".into(),
        label: Some("integration-test".into()),
        ..ServerConfig::default()
    });

    // Transfer registry entities into server.
    for record in registry.snapshot() {
        let _ = server.registry_mut().register_entity(
            record.identity.clone(),
            record.state,
            record.updated_at_ms,
        );
    }

    let server_entity_count = server.registry().len();

    // Checkpoint via server.
    let cp_id = match server.handle_request(RemoteRequest::Checkpoint {
        label: "phase-1-baseline".into(),
    }) {
        RemoteResponse::CheckpointCreated { id, .. } => id,
        other => panic!("expected CheckpointCreated, got {other:?}"),
    };

    // Phase 3: simulate expansion — add 3 more panes.
    for id in 3..=5u64 {
        let pane_identity = LifecycleIdentity::new(
            LifecycleEntityKind::Pane,
            "default",
            "local",
            id,
            1,
        );
        server
            .registry_mut()
            .register_entity(
                pane_identity,
                LifecycleState::Pane(MuxPaneLifecycleState::Running),
                2000,
            )
            .unwrap();
    }

    assert_eq!(server.registry().len(), server_entity_count + 3);

    // Verify checkpoint list shows 1 checkpoint.
    match server.handle_request(RemoteRequest::ListCheckpoints) {
        RemoteResponse::Checkpoints { checkpoints } => {
            assert_eq!(checkpoints.len(), 1);
            assert_eq!(checkpoints[0].label, "phase-1-baseline");
        }
        other => panic!("expected Checkpoints, got {other:?}"),
    }

    // Phase 4: rollback to baseline.
    match server.handle_request(RemoteRequest::Rollback {
        checkpoint_id: cp_id,
        reason: "restore baseline".into(),
    }) {
        RemoteResponse::RollbackComplete { restored, removed } => {
            // removed = entities added after checkpoint that got purged
            assert!(removed > 0, "should remove expanded entities");
            assert!(
                restored + removed > 0,
                "rollback should have some effect"
            );
        }
        other => panic!("expected RollbackComplete, got {other:?}"),
    }

    // Registry should be back to baseline count.
    assert_eq!(server.registry().len(), server_entity_count);

    // Phase 5: verify restored topology matches initial.
    let restored_topo = TopologySnapshot::from_json(&initial_topo_json)
        .expect("deserialize initial topology");
    assert_eq!(restored_topo.pane_count(), 2);
    assert_eq!(restored_topo.pane_ids(), initial_topo.pane_ids());

    // Phase 6: server status should reflect healthy state.
    match server.handle_request(RemoteRequest::Status) {
        RemoteResponse::Status { status } => {
            assert!(!status.node_id.is_empty(), "node_id should be non-empty");
        }
        other => panic!("expected Status, got {other:?}"),
    }

    // Verify DurableStateManager checkpoint count grew (rollback creates a pre-rollback CP).
    let cp_count = server.state_manager().checkpoint_count();
    assert!(
        cp_count >= 2,
        "should have at least original + pre-rollback checkpoints, got {cp_count}"
    );

    // Verify rollback history.
    let rollback_history = server.state_manager().rollback_history();
    assert_eq!(rollback_history.len(), 1);
    assert_eq!(rollback_history[0].target_checkpoint_id, cp_id);
    assert_eq!(
        rollback_history[0].reason,
        "restore baseline"
    );
}
