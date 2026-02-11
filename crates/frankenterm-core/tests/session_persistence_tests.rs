//! Integration tests for the session persistence pipeline.
//!
//! Tests the full capture → persist → query → cleanup → restore flow across
//! multiple modules: snapshot_engine, session_pane_state, session_topology,
//! agent_correlator, and storage.
//!
//! All tests use in-memory or tempfile SQLite — no running WezTerm required.
//!
//! Bead: wa-rsaf.1

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::Connection;

use frankenterm_core::config::SnapshotConfig;
use frankenterm_core::session_pane_state::{
    AgentMetadata, PaneStateSnapshot, ProcessInfo, TerminalState,
};
use frankenterm_core::session_topology::{PaneNode, TopologySnapshot};
use frankenterm_core::snapshot_engine::{SnapshotEngine, SnapshotError, SnapshotTrigger};
use frankenterm_core::wezterm::{PaneInfo, PaneSize};

// =============================================================================
// Test helpers
// =============================================================================

fn make_pane(id: u64, tab_id: u64, window_id: u64, rows: u32, cols: u32) -> PaneInfo {
    PaneInfo {
        pane_id: id,
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
        title: Some(format!("pane-{id}")),
        cwd: Some(format!("file:///home/user/project-{id}")),
        tty_name: None,
        cursor_x: Some(5),
        cursor_y: Some(10),
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: id == 0,
        is_zoomed: false,
        extra: HashMap::new(),
    }
}

fn make_pane_simple(id: u64) -> PaneInfo {
    make_pane(id, 0, 0, 24, 80)
}

fn setup_test_db() -> (tempfile::NamedTempFile, Arc<String>) {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let db_path = Arc::new(tmp.path().to_str().unwrap().to_string());

    let conn = Connection::open(db_path.as_str()).unwrap();
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS mux_sessions (
            session_id TEXT PRIMARY KEY,
            created_at INTEGER NOT NULL,
            last_checkpoint_at INTEGER,
            shutdown_clean INTEGER NOT NULL DEFAULT 0,
            topology_json TEXT NOT NULL,
            window_metadata_json TEXT,
            ft_version TEXT NOT NULL,
            host_id TEXT
        );
        CREATE TABLE IF NOT EXISTS session_checkpoints (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
            checkpoint_at INTEGER NOT NULL,
            checkpoint_type TEXT NOT NULL CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
            state_hash TEXT NOT NULL,
            pane_count INTEGER NOT NULL,
            total_bytes INTEGER NOT NULL,
            metadata_json TEXT
        );
        CREATE TABLE IF NOT EXISTS mux_pane_state (
            id INTEGER PRIMARY KEY,
            checkpoint_id INTEGER NOT NULL REFERENCES session_checkpoints(id) ON DELETE CASCADE,
            pane_id INTEGER NOT NULL,
            cwd TEXT,
            command TEXT,
            env_json TEXT,
            terminal_state_json TEXT NOT NULL,
            agent_metadata_json TEXT,
            scrollback_checkpoint_seq INTEGER,
            last_output_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
        CREATE INDEX IF NOT EXISTS idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
        CREATE INDEX IF NOT EXISTS idx_pane_state_pane ON mux_pane_state(pane_id);
        PRAGMA foreign_keys = ON;
        ",
    )
    .unwrap();

    (tmp, db_path)
}

// =============================================================================
// Full pipeline: capture → persist → query → cleanup
// =============================================================================

#[tokio::test]
async fn full_pipeline_capture_persist_query_cleanup() {
    let (_tmp, db_path) = setup_test_db();
    let config = SnapshotConfig {
        retention_count: 3,
        retention_days: 365,
        ..SnapshotConfig::default()
    };
    let engine = SnapshotEngine::new(db_path.clone(), config);

    // Capture initial state (3 panes across 2 tabs)
    let panes = vec![
        make_pane(0, 0, 0, 24, 80),
        make_pane(1, 0, 0, 24, 80),
        make_pane(2, 1, 0, 30, 120),
    ];
    let r1 = engine
        .capture(&panes, SnapshotTrigger::Startup)
        .await
        .unwrap();
    assert_eq!(r1.pane_count, 3);
    assert!(r1.total_bytes > 0);

    // Verify DB state
    let conn = Connection::open(db_path.as_str()).unwrap();
    let session_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(session_count, 1);

    let checkpoint_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(checkpoint_count, 1);

    let pane_state_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(pane_state_count, 3);

    // Verify checkpoint type is startup
    let cp_type: String = conn
        .query_row(
            "SELECT checkpoint_type FROM session_checkpoints WHERE id = ?1",
            [r1.checkpoint_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(cp_type, "startup");

    // Capture changed state (new pane added)
    let panes2 = vec![
        make_pane(0, 0, 0, 24, 80),
        make_pane(1, 0, 0, 24, 80),
        make_pane(2, 1, 0, 30, 120),
        make_pane(3, 1, 0, 30, 120), // new pane
    ];
    let r2 = engine
        .capture(&panes2, SnapshotTrigger::Periodic)
        .await
        .unwrap();
    assert_eq!(r2.pane_count, 4);
    assert_eq!(r1.session_id, r2.session_id); // same session

    // Now we have 2 checkpoints, 7 pane states total
    drop(conn);

    let conn = Connection::open(db_path.as_str()).unwrap();
    let total_pane_states: i64 = conn
        .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(total_pane_states, 7); // 3 + 4
}

#[tokio::test]
async fn cleanup_removes_old_preserves_recent() {
    let (_tmp, db_path) = setup_test_db();
    let config = SnapshotConfig {
        retention_count: 2,
        retention_days: 365,
        ..SnapshotConfig::default()
    };
    let engine = SnapshotEngine::new(db_path.clone(), config);

    // Create 5 snapshots with different pane data
    for i in 0..5u64 {
        let panes = vec![make_pane(i, 0, 0, 24 + i as u32, 80)];
        engine
            .capture(&panes, SnapshotTrigger::Manual)
            .await
            .unwrap();
    }

    let conn = Connection::open(db_path.as_str()).unwrap();
    let before: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(before, 5);

    drop(conn);

    // Cleanup: keep 2
    let deleted = engine.cleanup().await.unwrap();
    assert_eq!(deleted, 3);

    let conn = Connection::open(db_path.as_str()).unwrap();
    let after: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(after, 2);

    // Verify pane states were cascade-deleted too
    let remaining_pane_states: i64 = conn
        .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining_pane_states, 2); // 1 pane per remaining checkpoint
}

#[tokio::test]
async fn shutdown_marks_session_clean() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    let panes = vec![make_pane_simple(0)];
    let r = engine
        .capture(&panes, SnapshotTrigger::Startup)
        .await
        .unwrap();

    // Verify not yet clean
    let conn = Connection::open(db_path.as_str()).unwrap();
    let clean: i64 = conn
        .query_row(
            "SELECT shutdown_clean FROM mux_sessions WHERE session_id = ?1",
            [&r.session_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(clean, 0);
    drop(conn);

    // Mark shutdown
    engine.mark_shutdown().await.unwrap();

    let conn = Connection::open(db_path.as_str()).unwrap();
    let clean: i64 = conn
        .query_row(
            "SELECT shutdown_clean FROM mux_sessions WHERE session_id = ?1",
            [&r.session_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(clean, 1);
}

// =============================================================================
// Deduplication
// =============================================================================

#[tokio::test]
async fn dedup_periodic_skips_identical_state() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
    let panes = vec![make_pane_simple(0), make_pane_simple(1)];

    // First capture OK
    let r1 = engine.capture(&panes, SnapshotTrigger::Periodic).await;
    assert!(r1.is_ok());

    // Same panes, periodic → skip
    let r2 = engine.capture(&panes, SnapshotTrigger::Periodic).await;
    assert!(matches!(r2, Err(SnapshotError::NoChanges)));

    // Verify only 1 checkpoint exists
    let conn = Connection::open(db_path.as_str()).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn dedup_manual_does_not_skip() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
    let panes = vec![make_pane_simple(0)];

    engine
        .capture(&panes, SnapshotTrigger::Manual)
        .await
        .unwrap();
    engine
        .capture(&panes, SnapshotTrigger::Manual)
        .await
        .unwrap();

    let conn = Connection::open(db_path.as_str()).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn dedup_periodic_captures_after_change() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    let panes1 = vec![make_pane_simple(0)];
    engine
        .capture(&panes1, SnapshotTrigger::Periodic)
        .await
        .unwrap();

    // Add a pane → different hash → should capture
    let panes2 = vec![make_pane_simple(0), make_pane_simple(1)];
    let r2 = engine.capture(&panes2, SnapshotTrigger::Periodic).await;
    assert!(r2.is_ok());
    assert_eq!(r2.unwrap().pane_count, 2);
}

// =============================================================================
// Error handling
// =============================================================================

#[tokio::test]
async fn capture_empty_panes_returns_error() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    let result = engine.capture(&[], SnapshotTrigger::Manual).await;
    assert!(matches!(result, Err(SnapshotError::NoPanes)));
}

// =============================================================================
// Topology integration
// =============================================================================

#[tokio::test]
async fn multi_tab_topology_persisted() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    // 2 tabs, 3 panes total
    let panes = vec![
        make_pane(0, 0, 0, 24, 80),  // tab 0, left pane
        make_pane(1, 0, 0, 24, 80),  // tab 0, right pane (vsplit)
        make_pane(2, 1, 0, 30, 120), // tab 1, single pane
    ];

    let result = engine
        .capture(&panes, SnapshotTrigger::Startup)
        .await
        .unwrap();

    // Verify topology is stored in mux_sessions
    let conn = Connection::open(db_path.as_str()).unwrap();
    let topology_json: String = conn
        .query_row(
            "SELECT topology_json FROM mux_sessions WHERE session_id = ?1",
            [&result.session_id],
            |row| row.get(0),
        )
        .unwrap();

    let topology: TopologySnapshot = serde_json::from_str(&topology_json).unwrap();
    assert_eq!(topology.pane_count(), 3);
    assert_eq!(topology.windows.len(), 1);
    assert_eq!(topology.windows[0].tabs.len(), 2);
}

#[tokio::test]
async fn multi_window_topology_persisted() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    // 2 windows, 1 pane each
    let panes = vec![
        make_pane(0, 0, 0, 24, 80),  // window 0
        make_pane(1, 1, 1, 30, 120), // window 1
    ];

    let result = engine
        .capture(&panes, SnapshotTrigger::Startup)
        .await
        .unwrap();

    let conn = Connection::open(db_path.as_str()).unwrap();
    let topology_json: String = conn
        .query_row(
            "SELECT topology_json FROM mux_sessions WHERE session_id = ?1",
            [&result.session_id],
            |row| row.get(0),
        )
        .unwrap();

    let topology: TopologySnapshot = serde_json::from_str(&topology_json).unwrap();
    assert_eq!(topology.windows.len(), 2);
}

// =============================================================================
// Per-pane state integration
// =============================================================================

#[tokio::test]
async fn pane_state_cwd_and_title_persisted() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    let mut pane = make_pane_simple(42);
    pane.cwd = Some("file:///home/alice/code".to_string());
    pane.title = Some("vim main.rs".to_string());

    let result = engine
        .capture(&[pane], SnapshotTrigger::Manual)
        .await
        .unwrap();

    let conn = Connection::open(db_path.as_str()).unwrap();
    let (cwd, command): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT cwd, command FROM mux_pane_state WHERE checkpoint_id = ?1",
            [result.checkpoint_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert!(cwd.is_some());
}

#[tokio::test]
async fn pane_state_terminal_json_valid() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
    let panes = vec![make_pane(0, 0, 0, 30, 120)];

    let result = engine
        .capture(&panes, SnapshotTrigger::Manual)
        .await
        .unwrap();

    let conn = Connection::open(db_path.as_str()).unwrap();
    let terminal_json: String = conn
        .query_row(
            "SELECT terminal_state_json FROM mux_pane_state WHERE checkpoint_id = ?1",
            [result.checkpoint_id],
            |row| row.get(0),
        )
        .unwrap();

    // terminal_state_json stores just the TerminalState portion
    let terminal: TerminalState = serde_json::from_str(&terminal_json).unwrap();
    assert_eq!(terminal.rows, 30);
    assert_eq!(terminal.cols, 120);
}

#[tokio::test]
async fn agent_metadata_from_title_persisted() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    let mut pane = make_pane_simple(1);
    pane.title = Some("claude-code".to_string());

    let result = engine
        .capture(&[pane], SnapshotTrigger::Manual)
        .await
        .unwrap();

    let conn = Connection::open(db_path.as_str()).unwrap();
    let agent_json: Option<String> = conn
        .query_row(
            "SELECT agent_metadata_json FROM mux_pane_state WHERE checkpoint_id = ?1 AND pane_id = 1",
            [result.checkpoint_id],
            |row| row.get(0),
        )
        .unwrap();

    let agent_json = agent_json.expect("agent_metadata_json should be present");
    let meta: AgentMetadata = serde_json::from_str(&agent_json).unwrap();
    assert_eq!(meta.agent_type, "claude_code");
}

// =============================================================================
// Session continuity
// =============================================================================

#[tokio::test]
async fn session_id_stable_across_captures() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    let panes1 = vec![make_pane_simple(0)];
    let panes2 = vec![make_pane_simple(0), make_pane_simple(1)];
    let panes3 = vec![
        make_pane_simple(0),
        make_pane_simple(1),
        make_pane_simple(2),
    ];

    let r1 = engine
        .capture(&panes1, SnapshotTrigger::Startup)
        .await
        .unwrap();
    let r2 = engine
        .capture(&panes2, SnapshotTrigger::Periodic)
        .await
        .unwrap();
    let r3 = engine
        .capture(&panes3, SnapshotTrigger::Manual)
        .await
        .unwrap();

    assert_eq!(r1.session_id, r2.session_id);
    assert_eq!(r2.session_id, r3.session_id);

    // But all have different checkpoint IDs
    assert_ne!(r1.checkpoint_id, r2.checkpoint_id);
    assert_ne!(r2.checkpoint_id, r3.checkpoint_id);
}

#[tokio::test]
async fn session_id_format_valid() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    let result = engine
        .capture(&[make_pane_simple(0)], SnapshotTrigger::Startup)
        .await
        .unwrap();

    assert!(result.session_id.starts_with("sess-"));
    assert!(result.session_id.len() > 20);
}

// =============================================================================
// Topology reconstruction unit tests
// =============================================================================

#[test]
fn topology_from_panes_single_pane() {
    let panes = vec![make_pane_simple(0)];
    let (topo, report) = TopologySnapshot::from_panes(&panes, 1000);

    assert_eq!(topo.pane_count(), 1);
    assert_eq!(report.pane_count, 1);
    assert_eq!(report.window_count, 1);
    assert_eq!(report.tab_count, 1);
}

#[test]
fn topology_from_panes_hsplit() {
    // Two panes stacked vertically: same cols, different rows (top=24, bottom=24)
    let panes = vec![make_pane(0, 0, 0, 24, 80), make_pane(1, 0, 0, 24, 80)];
    let (topo, _) = TopologySnapshot::from_panes(&panes, 1000);

    assert_eq!(topo.pane_count(), 2);
    let tree = &topo.windows[0].tabs[0].pane_tree;

    // Should be either HSplit or VSplit depending on inference
    match tree {
        PaneNode::HSplit { .. } | PaneNode::VSplit { .. } => {}
        PaneNode::Leaf { .. } => panic!("Expected a split node, got leaf"),
    }
}

#[test]
fn topology_from_panes_multiple_tabs() {
    let panes = vec![
        make_pane(0, 0, 0, 24, 80), // tab 0
        make_pane(1, 0, 0, 24, 80), // tab 0
        make_pane(2, 1, 0, 24, 80), // tab 1
    ];
    let (topo, report) = TopologySnapshot::from_panes(&panes, 1000);

    assert_eq!(report.tab_count, 2);
    assert_eq!(topo.windows[0].tabs.len(), 2);
}

#[test]
fn topology_roundtrip_complex() {
    // 2 windows, 3 tabs total, 6 panes
    let panes = vec![
        make_pane(0, 0, 0, 24, 80),
        make_pane(1, 0, 0, 24, 80),
        make_pane(2, 1, 0, 30, 120),
        make_pane(3, 2, 1, 24, 80),
        make_pane(4, 2, 1, 24, 80),
        make_pane(5, 2, 1, 24, 80),
    ];
    let (topo, _) = TopologySnapshot::from_panes(&panes, 1000);

    let json = topo.to_json().unwrap();
    let restored = TopologySnapshot::from_json(&json).unwrap();

    assert_eq!(topo.pane_count(), restored.pane_count());
    assert_eq!(topo.windows.len(), restored.windows.len());
}

#[test]
fn topology_empty_snapshot() {
    let topo = TopologySnapshot::empty(1000);
    assert_eq!(topo.pane_count(), 0);
    assert!(topo.windows.is_empty());

    let json = topo.to_json().unwrap();
    let restored = TopologySnapshot::from_json(&json).unwrap();
    assert_eq!(restored.pane_count(), 0);
}

// =============================================================================
// Pane matching
// =============================================================================

#[test]
fn pane_matching_by_cwd_and_title() {
    let old_panes = vec![
        make_pane(10, 0, 0, 24, 80),  // cwd: project-10, title: pane-10
        make_pane(20, 0, 0, 30, 120), // cwd: project-20, title: pane-20
    ];
    let (old_topo, _) = TopologySnapshot::from_panes(&old_panes, 1000);

    // After restart: new pane IDs, but same cwds
    let new_panes = vec![
        make_pane(100, 0, 0, 24, 80),  // same cwd as old pane 10
        make_pane(200, 0, 0, 30, 120), // same cwd as old pane 20
    ];
    // Override cwds to match
    let mut new_panes = new_panes;
    new_panes[0].cwd = Some("file:///home/user/project-10".to_string());
    new_panes[1].cwd = Some("file:///home/user/project-20".to_string());

    let mapping = frankenterm_core::session_topology::match_panes(&old_topo, &new_panes);

    // Should match old→new by cwd
    assert!(mapping.mappings.contains_key(&10) || mapping.mappings.contains_key(&20));
}

// =============================================================================
// PaneStateSnapshot unit tests
// =============================================================================

#[test]
fn pane_state_builder_chain() {
    let terminal = TerminalState {
        rows: 24,
        cols: 80,
        cursor_row: 5,
        cursor_col: 10,
        is_alt_screen: false,
        title: "bash".to_string(),
    };

    let snapshot = PaneStateSnapshot::new(42, 1000, terminal)
        .with_cwd("/home/user".to_string())
        .with_shell("bash".to_string())
        .with_process(ProcessInfo {
            name: "vim".to_string(),
            pid: Some(1234),
            argv: Some(vec!["vim".to_string(), "main.rs".to_string()]),
        })
        .with_agent(AgentMetadata {
            agent_type: "claude_code".to_string(),
            session_id: Some("sess-123".to_string()),
            state: Some("working".to_string()),
        });

    assert_eq!(snapshot.pane_id, 42);
    assert_eq!(snapshot.cwd.as_deref(), Some("/home/user"));
    assert_eq!(snapshot.shell.as_deref(), Some("bash"));
    assert!(snapshot.foreground_process.is_some());
    assert!(snapshot.agent.is_some());
    assert_eq!(snapshot.agent.as_ref().unwrap().agent_type, "claude_code");
}

#[test]
fn pane_state_json_roundtrip_with_all_fields() {
    let terminal = TerminalState {
        rows: 30,
        cols: 120,
        cursor_row: 15,
        cursor_col: 42,
        is_alt_screen: true,
        title: "nvim".to_string(),
    };

    let snapshot = PaneStateSnapshot::new(7, 999999, terminal)
        .with_cwd("/tmp/test".to_string())
        .with_shell("zsh".to_string())
        .with_process(ProcessInfo {
            name: "nvim".to_string(),
            pid: Some(5678),
            argv: Some(vec![
                "nvim".to_string(),
                "+42".to_string(),
                "file.rs".to_string(),
            ]),
        })
        .with_agent(AgentMetadata {
            agent_type: "codex".to_string(),
            session_id: None,
            state: Some("idle".to_string()),
        });

    let json = snapshot.to_json().unwrap();
    let restored = PaneStateSnapshot::from_json(&json).unwrap();

    assert_eq!(restored.pane_id, 7);
    assert_eq!(restored.terminal.is_alt_screen, true);
    assert_eq!(restored.terminal.rows, 30);
    assert_eq!(restored.terminal.cols, 120);
    assert_eq!(restored.cwd.as_deref(), Some("/tmp/test"));
    assert_eq!(restored.shell.as_deref(), Some("zsh"));
    assert_eq!(restored.foreground_process.as_ref().unwrap().name, "nvim");
    assert_eq!(restored.agent.as_ref().unwrap().agent_type, "codex");
}

#[test]
fn pane_state_from_pane_info_extracts_all_fields() {
    let pane = PaneInfo {
        pane_id: 5,
        tab_id: 1,
        window_id: 0,
        domain_id: None,
        domain_name: None,
        workspace: Some("dev".to_string()),
        size: Some(PaneSize {
            rows: 40,
            cols: 160,
            pixel_width: Some(1920),
            pixel_height: Some(1080),
            dpi: None,
        }),
        rows: None,
        cols: None,
        title: Some("python3 -c 'print(1)'".to_string()),
        cwd: Some("file:///opt/project".to_string()),
        tty_name: Some("/dev/pts/5".to_string()),
        cursor_x: Some(20),
        cursor_y: Some(15),
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: true,
        is_zoomed: false,
        extra: HashMap::new(),
    };

    let snapshot = PaneStateSnapshot::from_pane_info(&pane, 2000, false);

    assert_eq!(snapshot.pane_id, 5);
    assert_eq!(snapshot.terminal.rows, 40);
    assert_eq!(snapshot.terminal.cols, 160);
    assert!(!snapshot.terminal.is_alt_screen);
    // CWD should be normalized (file:// prefix stripped)
    assert!(snapshot.cwd.is_some());
}

#[test]
fn pane_state_size_budget_small_not_truncated() {
    let terminal = TerminalState {
        rows: 24,
        cols: 80,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: "bash".to_string(),
    };

    let snapshot = PaneStateSnapshot::new(0, 1000, terminal).with_cwd("/home/user".to_string());

    let (json, was_truncated) = snapshot.to_json_budgeted().unwrap();
    assert!(!was_truncated, "Small snapshot should not be truncated");
    let _restored: PaneStateSnapshot = serde_json::from_str(&json).unwrap();
    assert!(json.len() < 65_536);
}

#[test]
fn pane_state_safe_env_list_filters_correctly() {
    let terminal = TerminalState {
        rows: 24,
        cols: 80,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: "bash".to_string(),
    };

    let env = vec![
        ("PATH".to_string(), "/usr/bin".to_string()),
        ("HOME".to_string(), "/home/user".to_string()),
        ("RANDOM_THING".to_string(), "should_be_dropped".to_string()),
        ("MY_CUSTOM".to_string(), "also_dropped".to_string()),
    ];

    let snapshot = PaneStateSnapshot::new(0, 1000, terminal).with_env_from_iter(env.into_iter());

    let captured = snapshot.env.as_ref().unwrap();
    assert!(captured.vars.contains_key("PATH"));
    assert!(captured.vars.contains_key("HOME"));
    assert!(!captured.vars.contains_key("RANDOM_THING"));
    assert!(!captured.vars.contains_key("MY_CUSTOM"));
}

#[test]
fn pane_state_env_redacts_secrets() {
    let env = vec![
        ("HOME".to_string(), "/home/user".to_string()),
        ("PATH".to_string(), "/usr/bin:/usr/local/bin".to_string()),
        (
            "AWS_SECRET_ACCESS_KEY".to_string(),
            "AKIA...secret".to_string(),
        ),
        (
            "DATABASE_URL".to_string(),
            "postgres://user:pass@host/db".to_string(),
        ),
        ("API_KEY".to_string(), "sk-live-abcdef".to_string()),
    ];

    let terminal = TerminalState {
        rows: 24,
        cols: 80,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: "bash".to_string(),
    };

    let snapshot = PaneStateSnapshot::new(0, 1000, terminal).with_env_from_iter(env.into_iter());

    let captured = snapshot.env.as_ref().unwrap();

    // Safe vars should be present
    assert!(captured.vars.contains_key("HOME"));
    assert!(captured.vars.contains_key("PATH"));

    // Secret vars should NOT be present
    assert!(!captured.vars.contains_key("AWS_SECRET_ACCESS_KEY"));
    assert!(!captured.vars.contains_key("DATABASE_URL"));
    assert!(!captured.vars.contains_key("API_KEY"));

    // Redacted count should reflect the removed vars
    assert!(captured.redacted_count > 0);
}

// =============================================================================
// Schema / foreign key integrity
// =============================================================================

#[tokio::test]
async fn foreign_key_cascade_on_session_delete() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    let panes = vec![make_pane_simple(0), make_pane_simple(1)];
    let result = engine
        .capture(&panes, SnapshotTrigger::Manual)
        .await
        .unwrap();

    let conn = Connection::open(db_path.as_str()).unwrap();

    // Verify data exists
    let cp_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(cp_count > 0);

    let ps_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
        .unwrap();
    assert!(ps_count > 0);

    // Delete the session → should cascade to checkpoints and pane states
    conn.execute(
        "DELETE FROM mux_sessions WHERE session_id = ?1",
        [&result.session_id],
    )
    .unwrap();

    let cp_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(cp_after, 0);

    let ps_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(ps_after, 0);
}

#[tokio::test]
async fn foreign_key_cascade_on_checkpoint_delete() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    let panes = vec![
        make_pane_simple(0),
        make_pane_simple(1),
        make_pane_simple(2),
    ];
    let result = engine
        .capture(&panes, SnapshotTrigger::Manual)
        .await
        .unwrap();

    let conn = Connection::open(db_path.as_str()).unwrap();
    let ps_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(ps_before, 3);

    // Delete the checkpoint → pane states should cascade
    conn.execute(
        "DELETE FROM session_checkpoints WHERE id = ?1",
        [result.checkpoint_id],
    )
    .unwrap();

    let ps_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(ps_after, 0);
}

// =============================================================================
// Agent correlator integration
// =============================================================================

#[test]
fn agent_correlator_detects_from_title() {
    let mut correlator = frankenterm_core::agent_correlator::AgentCorrelator::new();

    let pane = PaneInfo {
        pane_id: 1,
        tab_id: 0,
        window_id: 0,
        domain_id: None,
        domain_name: None,
        workspace: None,
        size: Some(PaneSize {
            rows: 24,
            cols: 80,
            pixel_width: None,
            pixel_height: None,
            dpi: None,
        }),
        rows: None,
        cols: None,
        title: Some("codex".to_string()),
        cwd: None,
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: false,
        is_zoomed: false,
        extra: HashMap::new(),
    };

    correlator.update_from_pane_info(&pane);
    let meta = correlator.get_metadata(1);
    assert!(meta.is_some());
    let meta = meta.unwrap();
    assert_eq!(meta.agent_type, "codex");
}

#[test]
fn agent_correlator_multiple_panes() {
    let mut correlator = frankenterm_core::agent_correlator::AgentCorrelator::new();

    let pane1 = PaneInfo {
        pane_id: 1,
        title: Some("claude-code".to_string()),
        ..make_pane_simple(1)
    };
    let pane2 = PaneInfo {
        pane_id: 2,
        title: Some("codex".to_string()),
        ..make_pane_simple(2)
    };
    let pane3 = PaneInfo {
        pane_id: 3,
        title: Some("bash".to_string()),
        ..make_pane_simple(3)
    };

    correlator.update_from_pane_info(&pane1);
    correlator.update_from_pane_info(&pane2);
    correlator.update_from_pane_info(&pane3);

    assert_eq!(correlator.tracked_pane_count(), 2); // bash not tracked

    let m1 = correlator.get_metadata(1).unwrap();
    assert_eq!(m1.agent_type, "claude_code");

    let m2 = correlator.get_metadata(2).unwrap();
    assert_eq!(m2.agent_type, "codex");

    assert!(correlator.get_metadata(3).is_none());
}

// =============================================================================
// Snapshot trigger types
// =============================================================================

#[tokio::test]
async fn all_trigger_types_stored_correctly() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    let triggers = [
        (SnapshotTrigger::Startup, "startup"),
        (SnapshotTrigger::Manual, "event"),
        (SnapshotTrigger::Event, "event"),
    ];

    for (i, (trigger, expected_type)) in triggers.iter().enumerate() {
        let panes = vec![make_pane(i as u64, 0, 0, 24 + i as u32, 80)];
        let result = engine.capture(&panes, *trigger).await.unwrap();

        let conn = Connection::open(db_path.as_str()).unwrap();
        let cp_type: String = conn
            .query_row(
                "SELECT checkpoint_type FROM session_checkpoints WHERE id = ?1",
                [result.checkpoint_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            cp_type, *expected_type,
            "trigger {:?} should store as '{}'",
            trigger, expected_type
        );
    }
}

// =============================================================================
// Large pane count
// =============================================================================

#[tokio::test]
async fn capture_50_panes() {
    let (_tmp, db_path) = setup_test_db();
    let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

    let panes: Vec<PaneInfo> = (0..50)
        .map(|i| make_pane(i, i / 5, i / 25, 24, 80))
        .collect();

    let result = engine
        .capture(&panes, SnapshotTrigger::Manual)
        .await
        .unwrap();
    assert_eq!(result.pane_count, 50);
    assert!(result.total_bytes > 0);

    let conn = Connection::open(db_path.as_str()).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mux_pane_state WHERE checkpoint_id = ?1",
            [result.checkpoint_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 50);
}

// =============================================================================
// Topology pane_ids and pane_count helpers
// =============================================================================

#[test]
fn topology_pane_ids_complete() {
    let panes: Vec<PaneInfo> = (0..10).map(|i| make_pane(i, 0, 0, 24, 80)).collect();
    let (topo, _) = TopologySnapshot::from_panes(&panes, 1000);

    let mut ids = topo.pane_ids();
    ids.sort();
    let expected: Vec<u64> = (0..10).collect();
    assert_eq!(ids, expected);
    assert_eq!(topo.pane_count(), 10);
}

// =============================================================================
// PaneNode tree structure
// =============================================================================

#[test]
fn pane_node_leaf_count() {
    let leaf = PaneNode::Leaf {
        pane_id: 0,
        rows: 24,
        cols: 80,
        cwd: None,
        title: None,
        is_active: true,
    };
    assert_eq!(leaf.pane_count(), 1);
}

#[test]
fn pane_node_split_count() {
    let split = PaneNode::HSplit {
        children: vec![
            (
                0.5,
                PaneNode::Leaf {
                    pane_id: 0,
                    rows: 12,
                    cols: 80,
                    cwd: None,
                    title: None,
                    is_active: true,
                },
            ),
            (
                0.5,
                PaneNode::Leaf {
                    pane_id: 1,
                    rows: 12,
                    cols: 80,
                    cwd: None,
                    title: None,
                    is_active: false,
                },
            ),
        ],
    };
    assert_eq!(split.pane_count(), 2);
}
