//! End-to-end snapshot/restore roundtrip tests with structured reports.
//!
//! These tests exercise `SnapshotEngine` capture + persistence and the
//! session-restore query path (`session_restore`) against a real SQLite file.
//! They intentionally avoid requiring a live WezTerm instance.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use frankenterm_core::config::SnapshotConfig;
use frankenterm_core::restore_process::LaunchConfig;
use frankenterm_core::session_restore::{
    RestoredPaneState, SessionRestoreConfig, SessionRestorer, load_checkpoint_by_id,
    load_latest_checkpoint, session_doctor, show_session,
};
use frankenterm_core::session_topology::{PaneNode, TopologySnapshot};
use frankenterm_core::snapshot_engine::{SnapshotEngine, SnapshotError, SnapshotTrigger};
use frankenterm_core::wezterm::{MockWezterm, PaneInfo, PaneSize, WeztermInterface};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    use frankenterm_core::runtime_compat::CompatRuntime;
    let runtime = frankenterm_core::runtime_compat::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future);
}

#[derive(Debug, Serialize)]
struct E2ETestReport {
    test_name: String,
    phases: Vec<PhaseReport>,
    total_duration_ms: u64,
    passed: bool,
    failure_reason: Option<String>,
    pane_reports: Vec<PaneTestReport>,
}

#[derive(Debug, Serialize)]
struct PhaseReport {
    phase: String,
    duration_ms: u64,
    status: String,
    details: Value,
}

#[derive(Debug, Serialize)]
struct PaneTestReport {
    pane_id: u64,
    original_content_hash: String,
    restored_content_hash: String,
    content_match: bool,
    layout_match: bool,
    process_match: bool,
}

#[derive(Debug, Clone)]
struct FixturePaneState {
    pane_id: u64,
    window_id: u64,
    tab_id: u64,
    cwd: Option<String>,
}

fn setup_test_db() -> (tempfile::NamedTempFile, Arc<String>) {
    let tmp = tempfile::NamedTempFile::new().expect("create temp db");
    let db_path = Arc::new(tmp.path().to_string_lossy().to_string());
    let conn = Connection::open(db_path.as_str()).expect("open temp db");
    conn.execute_batch(
        "
        PRAGMA foreign_keys = ON;
        PRAGMA journal_mode = WAL;

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

        CREATE TABLE IF NOT EXISTS output_segments (
            id INTEGER PRIMARY KEY,
            pane_id INTEGER NOT NULL,
            seq INTEGER NOT NULL,
            content TEXT NOT NULL,
            content_len INTEGER NOT NULL,
            content_hash TEXT,
            captured_at INTEGER NOT NULL,
            UNIQUE(pane_id, seq)
        );

        CREATE INDEX IF NOT EXISTS idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
        CREATE INDEX IF NOT EXISTS idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
        CREATE INDEX IF NOT EXISTS idx_pane_state_pane ON mux_pane_state(pane_id);
        CREATE INDEX IF NOT EXISTS idx_output_segments_pane_seq ON output_segments(pane_id, seq);
        ",
    )
    .expect("create schema");
    (tmp, db_path)
}

fn insert_output_segment(
    conn: &Connection,
    pane_id: u64,
    seq: i64,
    content: &str,
    captured_at: i64,
) {
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            pane_id as i64,
            seq,
            content,
            content.len() as i64,
            captured_at
        ],
    )
    .expect("insert output segment");
}

fn make_pane(
    pane_id: u64,
    tab_id: u64,
    window_id: u64,
    rows: u32,
    cols: u32,
    title: &str,
    cwd: &str,
) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id,
        window_id,
        domain_id: None,
        domain_name: Some("local".to_string()),
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
        title: Some(title.to_string()),
        cwd: Some(cwd.to_string()),
        tty_name: None,
        cursor_x: Some(0),
        cursor_y: Some(0),
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: pane_id == 0,
        is_zoomed: false,
        extra: HashMap::new(),
    }
}

fn add_phase(
    report: &mut E2ETestReport,
    phase: &str,
    start: Instant,
    status: &str,
    details: Value,
) {
    report.phases.push(PhaseReport {
        phase: phase.to_string(),
        duration_ms: start.elapsed().as_millis() as u64,
        status: status.to_string(),
        details,
    });
}

fn hash_text(payload: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn normalize_cwd_str(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("file://") {
        if rest.starts_with('/') {
            rest.to_string()
        } else if let Some(slash) = rest.find('/') {
            rest[slash..].to_string()
        } else {
            rest.to_string()
        }
    } else {
        raw.to_string()
    }
}

fn normalize_cwd(cwd: Option<&str>) -> Option<String> {
    cwd.map(normalize_cwd_str)
}

fn pane_info_hash(pane: &PaneInfo) -> String {
    hash_text(
        &json!({
            "pane_id": pane.pane_id,
            "cwd": normalize_cwd(pane.cwd.as_deref()),
            "title": pane.title,
            "rows": pane.effective_rows(),
            "cols": pane.effective_cols(),
            "domain_name": pane.domain_name,
        })
        .to_string(),
    )
}

fn restored_state_hash(state: &RestoredPaneState) -> String {
    hash_text(
        &json!({
            "pane_id": state.pane_id,
            "cwd": state.cwd,
            "command": state.command,
            "rows": state.terminal_state.as_ref().map(|t| t.rows),
            "cols": state.terminal_state.as_ref().map(|t| t.cols),
            "title": state.terminal_state.as_ref().map(|t| t.title.clone()),
        })
        .to_string(),
    )
}

fn emit_report(report: &E2ETestReport) {
    eprintln!(
        "[E2E_REPORT] {}",
        serde_json::to_string(report).expect("serialize report")
    );
}

fn checkpoint_count(db_path: &str) -> i64 {
    let conn = Connection::open(db_path).expect("open db");
    conn.query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
        row.get::<_, i64>(0)
    })
    .expect("count checkpoints")
}

fn fixture_path(file_name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("session_persistence")
        .join(file_name)
}

fn collect_fixture_panes_from_node(
    window_id: u64,
    tab_id: u64,
    node: &PaneNode,
    panes: &mut Vec<FixturePaneState>,
) {
    match node {
        PaneNode::Leaf { pane_id, cwd, .. } => panes.push(FixturePaneState {
            pane_id: *pane_id,
            window_id,
            tab_id,
            cwd: cwd.clone(),
        }),
        PaneNode::HSplit { children } | PaneNode::VSplit { children } => {
            for (_, child) in children {
                collect_fixture_panes_from_node(window_id, tab_id, child, panes);
            }
        }
    }
}

fn collect_fixture_panes(topology: &TopologySnapshot) -> Vec<FixturePaneState> {
    let mut panes = Vec::new();
    for window in &topology.windows {
        for tab in &window.tabs {
            collect_fixture_panes_from_node(
                window.window_id,
                tab.tab_id,
                &tab.pane_tree,
                &mut panes,
            );
        }
    }
    panes.sort_by_key(|pane| pane.pane_id);
    panes
}

#[test]
fn e2e_snapshot_roundtrip_single_pane_report() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name: "e2e_snapshot_roundtrip_single_pane_report".to_string(),
            phases: Vec::new(),
            total_duration_ms: 0,
            passed: false,
            failure_reason: None,
            pane_reports: Vec::new(),
        };

        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(
            db_path.clone(),
            SnapshotConfig {
                retention_count: 5,
                retention_days: 365,
                ..SnapshotConfig::default()
            },
        );

        let pane = make_pane(0, 0, 0, 24, 80, "claude-code", "file:///tmp/alpha");

        let capture_start = Instant::now();
        let snapshot = engine
            .capture(std::slice::from_ref(&pane), SnapshotTrigger::Startup)
            .await
            .expect("capture startup snapshot");
        add_phase(
            &mut report,
            "capture",
            capture_start,
            "ok",
            json!({
                "session_id": snapshot.session_id,
                "checkpoint_id": snapshot.checkpoint_id,
                "pane_count": snapshot.pane_count,
                "trigger": "startup",
            }),
        );

        let load_start = Instant::now();
        let checkpoint = load_checkpoint_by_id(db_path.as_str(), snapshot.checkpoint_id)
            .expect("load checkpoint")
            .expect("checkpoint should exist");
        let (_session, checkpoints) =
            show_session(db_path.as_str(), &snapshot.session_id).expect("show session");
        add_phase(
            &mut report,
            "load_and_query",
            load_start,
            "ok",
            json!({
                "loaded_checkpoint_id": checkpoint.checkpoint_id,
                "loaded_panes": checkpoint.pane_states.len(),
                "session_checkpoint_count": checkpoints.len(),
            }),
        );

        let compare_start = Instant::now();
        let restored = checkpoint
            .pane_states
            .iter()
            .find(|state| state.pane_id == pane.pane_id)
            .expect("restored pane state exists");

        let pane_report = PaneTestReport {
            pane_id: pane.pane_id,
            original_content_hash: pane_info_hash(&pane),
            restored_content_hash: restored_state_hash(restored),
            content_match: normalize_cwd(pane.cwd.as_deref()) == restored.cwd
                && pane.effective_rows()
                    == restored
                        .terminal_state
                        .as_ref()
                        .map(|t| u32::from(t.rows))
                        .unwrap_or_default()
                && pane.effective_cols()
                    == restored
                        .terminal_state
                        .as_ref()
                        .map(|t| u32::from(t.cols))
                        .unwrap_or_default(),
            layout_match: checkpoint.pane_count == 1 && checkpoints.len() == 1,
            process_match: restored.command.is_none(),
        };
        report.pane_reports.push(pane_report);

        let success = report.pane_reports.iter().all(|pane_result| {
            pane_result.content_match && pane_result.layout_match && pane_result.process_match
        });
        let passed_panes = report
            .pane_reports
            .iter()
            .filter(|pane_result| {
                pane_result.content_match && pane_result.layout_match && pane_result.process_match
            })
            .count();
        let total_panes = report.pane_reports.len();
        add_phase(
            &mut report,
            "compare",
            compare_start,
            if success { "ok" } else { "error" },
            json!({
                "passed_panes": passed_panes,
                "total_panes": total_panes,
            }),
        );

        report.total_duration_ms = run_start.elapsed().as_millis() as u64;
        report.passed = success;
        report.failure_reason = if success {
            None
        } else {
            Some("pane fidelity mismatch".to_string())
        };
        emit_report(&report);
        assert!(
            report.passed,
            "{}",
            serde_json::to_string_pretty(&report).expect("pretty report")
        );
    });
}

#[test]
fn e2e_snapshot_roundtrip_targeted_checkpoint_restore() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name: "e2e_snapshot_roundtrip_targeted_checkpoint_restore".to_string(),
            phases: Vec::new(),
            total_duration_ms: 0,
            passed: false,
            failure_reason: None,
            pane_reports: Vec::new(),
        };

        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());

        let panes_v1 = vec![
            make_pane(0, 0, 0, 24, 80, "agent-a", "file:///tmp/a"),
            make_pane(1, 0, 0, 24, 80, "agent-b", "file:///tmp/b"),
        ];
        let panes_v2 = vec![
            make_pane(0, 0, 0, 24, 100, "agent-a-editing", "file:///tmp/a"),
            make_pane(1, 0, 0, 24, 80, "agent-b", "file:///tmp/b"),
            make_pane(2, 1, 0, 30, 120, "agent-c", "file:///tmp/c"),
        ];

        let capture_start = Instant::now();
        let first = engine
            .capture(&panes_v1, SnapshotTrigger::Startup)
            .await
            .expect("capture v1");
        let second = engine
            .capture(&panes_v2, SnapshotTrigger::Manual)
            .await
            .expect("capture v2");
        add_phase(
            &mut report,
            "capture_versions",
            capture_start,
            "ok",
            json!({
                "first_checkpoint": first.checkpoint_id,
                "second_checkpoint": second.checkpoint_id,
                "first_panes": first.pane_count,
                "second_panes": second.pane_count,
            }),
        );

        let restore_start = Instant::now();
        let old_cp = load_checkpoint_by_id(db_path.as_str(), first.checkpoint_id)
            .expect("load old checkpoint")
            .expect("old checkpoint exists");
        let new_cp = load_checkpoint_by_id(db_path.as_str(), second.checkpoint_id)
            .expect("load new checkpoint")
            .expect("new checkpoint exists");
        let latest = load_latest_checkpoint(db_path.as_str(), &first.session_id)
            .expect("load latest checkpoint")
            .expect("latest checkpoint exists");
        add_phase(
            &mut report,
            "targeted_restore_load",
            restore_start,
            "ok",
            json!({
                "old_loaded_id": old_cp.checkpoint_id,
                "new_loaded_id": new_cp.checkpoint_id,
                "latest_loaded_id": latest.checkpoint_id,
                "old_pane_count": old_cp.pane_states.len(),
                "new_pane_count": new_cp.pane_states.len(),
            }),
        );

        let compare_start = Instant::now();
        let old_pane0 = old_cp
            .pane_states
            .iter()
            .find(|pane| pane.pane_id == 0)
            .expect("pane 0 in old checkpoint");
        let new_pane0 = new_cp
            .pane_states
            .iter()
            .find(|pane| pane.pane_id == 0)
            .expect("pane 0 in new checkpoint");

        let old_hash = restored_state_hash(old_pane0);
        let new_hash = restored_state_hash(new_pane0);
        let latest_matches_new = latest.checkpoint_id == second.checkpoint_id;
        let checkpoint_versions_distinct = old_hash != new_hash;
        let new_has_extra_pane = new_cp.pane_states.iter().any(|pane| pane.pane_id == 2);

        report.pane_reports.push(PaneTestReport {
            pane_id: 0,
            original_content_hash: old_hash.clone(),
            restored_content_hash: new_hash.clone(),
            content_match: checkpoint_versions_distinct,
            layout_match: new_has_extra_pane,
            process_match: latest_matches_new,
        });

        let success = checkpoint_versions_distinct && new_has_extra_pane && latest_matches_new;
        add_phase(
            &mut report,
            "compare_versions",
            compare_start,
            if success { "ok" } else { "error" },
            json!({
                "checkpoint_versions_distinct": checkpoint_versions_distinct,
                "new_has_extra_pane": new_has_extra_pane,
                "latest_matches_new": latest_matches_new,
            }),
        );

        report.total_duration_ms = run_start.elapsed().as_millis() as u64;
        report.passed = success;
        report.failure_reason = if success {
            None
        } else {
            Some("targeted checkpoint restore assertions failed".to_string())
        };
        emit_report(&report);
        assert!(
            report.passed,
            "{}",
            serde_json::to_string_pretty(&report).expect("pretty report")
        );
    });
}

#[test]
fn e2e_snapshot_dedup_retention_and_detect_cycle() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name: "e2e_snapshot_dedup_retention_and_detect_cycle".to_string(),
            phases: Vec::new(),
            total_duration_ms: 0,
            passed: false,
            failure_reason: None,
            pane_reports: Vec::new(),
        };

        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(
            db_path.clone(),
            SnapshotConfig {
                retention_count: 2,
                retention_days: 365,
                ..SnapshotConfig::default()
            },
        );
        let pane = make_pane(0, 0, 0, 24, 80, "agent-a", "file:///tmp/a");
        let pane_changed = make_pane(0, 0, 0, 30, 100, "agent-a-resized", "file:///tmp/a");

        let dedup_start = Instant::now();
        let startup = engine
            .capture(std::slice::from_ref(&pane), SnapshotTrigger::Startup)
            .await
            .expect("startup capture");
        let periodic_same = engine
            .capture(std::slice::from_ref(&pane), SnapshotTrigger::Periodic)
            .await;
        assert!(matches!(periodic_same, Err(SnapshotError::NoChanges)));
        let manual_same = engine
            .capture(std::slice::from_ref(&pane), SnapshotTrigger::Manual)
            .await
            .expect("manual same-state capture");
        let manual_changed = engine
            .capture(std::slice::from_ref(&pane_changed), SnapshotTrigger::Manual)
            .await
            .expect("manual changed capture");

        add_phase(
            &mut report,
            "capture_dedup",
            dedup_start,
            "ok",
            json!({
                "startup_checkpoint": startup.checkpoint_id,
                "manual_same_checkpoint": manual_same.checkpoint_id,
                "manual_changed_checkpoint": manual_changed.checkpoint_id,
                "periodic_same_result": "no_changes",
                "checkpoint_count_pre_cleanup": checkpoint_count(db_path.as_str()),
            }),
        );

        let cleanup_start = Instant::now();
        let deleted = engine.cleanup().await.expect("cleanup snapshots");
        let remaining = checkpoint_count(db_path.as_str());
        let doctor = session_doctor(db_path.as_str()).expect("session doctor");
        add_phase(
            &mut report,
            "cleanup_and_doctor",
            cleanup_start,
            "ok",
            json!({
                "deleted_checkpoints": deleted,
                "remaining_checkpoints": remaining,
                "doctor": {
                    "total_sessions": doctor.total_sessions,
                    "unclean_sessions": doctor.unclean_sessions,
                    "total_checkpoints": doctor.total_checkpoints,
                    "orphaned_pane_states": doctor.orphaned_pane_states,
                    "total_data_bytes": doctor.total_data_bytes,
                },
            }),
        );

        let detect_start = Instant::now();
        let restorer = SessionRestorer::new(db_path.clone(), SessionRestoreConfig::default());
        let detected_before_shutdown = restorer.detect().expect("detect before shutdown");
        engine.mark_shutdown().await.expect("mark shutdown");
        let detected_after_shutdown = restorer.detect().expect("detect after shutdown");
        add_phase(
            &mut report,
            "detect_cycle",
            detect_start,
            "ok",
            json!({
                "detected_before_shutdown": detected_before_shutdown.as_ref().map(|c| c.session_id.clone()),
                "detected_after_shutdown": detected_after_shutdown.as_ref().map(|c| c.session_id.clone()),
            }),
        );

        let success = deleted >= 1
            && remaining <= 2
            && detected_before_shutdown.is_some()
            && detected_after_shutdown.is_none();
        report.total_duration_ms = run_start.elapsed().as_millis() as u64;
        report.passed = success;
        report.failure_reason = if success {
            None
        } else {
            Some("dedup/retention/detect assertions failed".to_string())
        };
        emit_report(&report);
        assert!(
            report.passed,
            "{}",
            serde_json::to_string_pretty(&report).expect("pretty report")
        );
    });
}

#[test]
fn e2e_restore_bookkeeping_preserves_manual_restore_checkpoint() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name: "e2e_restore_bookkeeping_preserves_manual_restore_checkpoint".to_string(),
            phases: Vec::new(),
            total_duration_ms: 0,
            passed: false,
            failure_reason: None,
            pane_reports: Vec::new(),
        };

        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let pane = make_pane(7, 0, 0, 24, 80, "restore-agent", "file:///tmp/restore");

        let capture_start = Instant::now();
        let snapshot = engine
            .capture(std::slice::from_ref(&pane), SnapshotTrigger::Startup)
            .await
            .expect("capture startup snapshot for restore e2e");
        add_phase(
            &mut report,
            "capture",
            capture_start,
            "ok",
            json!({
                "session_id": snapshot.session_id,
                "checkpoint_id": snapshot.checkpoint_id,
                "pane_count": snapshot.pane_count,
            }),
        );

        let restorer = SessionRestorer::new(db_path.clone(), SessionRestoreConfig::default());
        let detect_start = Instant::now();
        let session = restorer
            .detect()
            .expect("detect restore candidate")
            .expect("candidate should exist before restore");
        let checkpoint = restorer
            .load_checkpoint(&session)
            .expect("load checkpoint for restore");
        add_phase(
            &mut report,
            "detect_and_load",
            detect_start,
            "ok",
            json!({
                "detected_session": session.session_id,
                "loaded_checkpoint": checkpoint.checkpoint_id,
                "loaded_panes": checkpoint.pane_states.len(),
            }),
        );

        let wezterm = Arc::new(MockWezterm::new());
        let restore_start = Instant::now();
        let summary = restorer
            .restore(&session, &checkpoint, wezterm.clone())
            .await
            .expect("restore should succeed");
        add_phase(
            &mut report,
            "restore",
            restore_start,
            "ok",
            json!({
                "restored_count": summary.restored_count(),
                "failed_count": summary.failed_count(),
                "summary_checkpoint_id": summary.checkpoint_id,
            }),
        );

        let verify_start = Instant::now();
        let latest = load_latest_checkpoint(db_path.as_str(), &snapshot.session_id)
            .expect("load latest preferred checkpoint")
            .expect("latest preferred checkpoint should exist");
        let latest_state = latest
            .pane_states
            .iter()
            .find(|state| state.pane_id == pane.pane_id)
            .expect("capture checkpoint should retain original pane state");
        let (_session_info, checkpoints) =
            show_session(db_path.as_str(), &snapshot.session_id).expect("show restored session");

        let verify_conn = Connection::open(db_path.as_str()).expect("open verification db");
        let (startup_checkpoint_id, startup_metadata_json, shutdown_clean): (i64, String, i64) =
            verify_conn
                .query_row(
                    "SELECT c.id, c.metadata_json, s.shutdown_clean
                     FROM session_checkpoints c
                     JOIN mux_sessions s ON s.session_id = c.session_id
                     WHERE c.session_id = ?1 AND c.checkpoint_type = 'startup'
                     ORDER BY c.checkpoint_at DESC
                     LIMIT 1",
                    [snapshot.session_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("startup checkpoint should be recorded");
        let startup_metadata: Value =
            serde_json::from_str(&startup_metadata_json).expect("parse startup metadata");
        let restored_new_pane_id = *summary
            .layout_result
            .pane_id_map
            .get(&pane.pane_id)
            .expect("restored pane mapping must exist");
        let banner_text = wezterm
            .get_text(restored_new_pane_id, false)
            .await
            .expect("read restore banner");
        let redetected = restorer
            .detect()
            .expect("detect after restore should succeed");

        let old_id_key = pane.pane_id.to_string();
        let latest_retains_capture = latest.checkpoint_id == snapshot.checkpoint_id;
        let capture_state_preserved = latest_state.cwd == normalize_cwd(pane.cwd.as_deref())
            && latest_state
                .terminal_state
                .as_ref()
                .is_some_and(|state| u32::from(state.rows) == pane.effective_rows())
            && latest_state
                .terminal_state
                .as_ref()
                .is_some_and(|state| u32::from(state.cols) == pane.effective_cols());
        let startup_checkpoint_exists = checkpoints
            .iter()
            .any(|info| info.checkpoint_type.as_deref() == Some("startup"));
        let startup_checkpoint_distinct = startup_checkpoint_id != snapshot.checkpoint_id;
        let startup_mapping_matches = startup_metadata["old_to_new"][old_id_key.as_str()].as_u64()
            == Some(restored_new_pane_id);
        let session_clean = shutdown_clean == 1;
        let detect_cleared = redetected.is_none();
        let banner_written = banner_text.contains("Session restored");

        add_phase(
            &mut report,
            "verify_restore_bookkeeping",
            verify_start,
            "ok",
            json!({
                "preferred_checkpoint_id": latest.checkpoint_id,
                "startup_checkpoint_id": startup_checkpoint_id,
                "startup_checkpoint_exists": startup_checkpoint_exists,
                "startup_checkpoint_distinct": startup_checkpoint_distinct,
                "startup_mapping_matches": startup_mapping_matches,
                "session_clean": session_clean,
                "detect_cleared": detect_cleared,
                "banner_written": banner_written,
            }),
        );

        report.pane_reports.push(PaneTestReport {
            pane_id: pane.pane_id,
            original_content_hash: pane_info_hash(&pane),
            restored_content_hash: restored_state_hash(latest_state),
            content_match: latest_retains_capture && capture_state_preserved,
            layout_match: startup_checkpoint_exists
                && startup_checkpoint_distinct
                && startup_mapping_matches,
            process_match: session_clean && detect_cleared && banner_written,
        });

        let success = report.pane_reports.iter().all(|pane_result| {
            pane_result.content_match && pane_result.layout_match && pane_result.process_match
        }) && summary.restored_count() == 1
            && summary.failed_count() == 0;

        report.total_duration_ms = run_start.elapsed().as_millis() as u64;
        report.passed = success;
        report.failure_reason = if success {
            None
        } else {
            Some("restore bookkeeping/manual-restore invariants failed".to_string())
        };
        emit_report(&report);
        assert!(
            report.passed,
            "{}",
            serde_json::to_string_pretty(&report).expect("pretty report")
        );
    });
}

#[test]
fn e2e_snapshot_fixture_topology_roundtrip() {
    let run_start = Instant::now();
    let mut report = E2ETestReport {
        test_name: "e2e_snapshot_fixture_topology_roundtrip".to_string(),
        phases: Vec::new(),
        total_duration_ms: 0,
        passed: false,
        failure_reason: None,
        pane_reports: Vec::new(),
    };

    let parse_start = Instant::now();
    let single_json =
        std::fs::read_to_string(fixture_path("snapshot_single_pane.json")).expect("read fixture");
    let complex_json = std::fs::read_to_string(fixture_path("snapshot_complex_layout.json"))
        .expect("read fixture");
    let single = TopologySnapshot::from_json(&single_json).expect("parse single fixture");
    let complex = TopologySnapshot::from_json(&complex_json).expect("parse complex fixture");
    add_phase(
        &mut report,
        "load_fixtures",
        parse_start,
        "ok",
        json!({
            "single_panes": single.pane_count(),
            "single_windows": single.windows.len(),
            "complex_panes": complex.pane_count(),
            "complex_windows": complex.windows.len(),
            "complex_tabs_window0": complex.windows.first().map(|w| w.tabs.len()).unwrap_or(0),
        }),
    );

    let roundtrip_start = Instant::now();
    let single_roundtrip = TopologySnapshot::from_json(
        &single
            .to_json()
            .expect("serialize single fixture to json for roundtrip"),
    )
    .expect("roundtrip parse single");
    let complex_roundtrip = TopologySnapshot::from_json(
        &complex
            .to_json()
            .expect("serialize complex fixture to json for roundtrip"),
    )
    .expect("roundtrip parse complex");
    let success = single == single_roundtrip
        && complex == complex_roundtrip
        && single.pane_count() == 1
        && complex.pane_count() == 4;
    add_phase(
        &mut report,
        "roundtrip",
        roundtrip_start,
        if success { "ok" } else { "error" },
        json!({
            "single_roundtrip_equal": single == single_roundtrip,
            "complex_roundtrip_equal": complex == complex_roundtrip,
            "single_pane_count": single.pane_count(),
            "complex_pane_count": complex.pane_count(),
        }),
    );

    report.total_duration_ms = run_start.elapsed().as_millis() as u64;
    report.passed = success;
    report.failure_reason = if success {
        None
    } else {
        Some("fixture topology roundtrip assertions failed".to_string())
    };
    emit_report(&report);
    assert!(
        report.passed,
        "{}",
        serde_json::to_string_pretty(&report).expect("pretty report")
    );
}

#[test]
fn e2e_fixture_complex_layout_restore_executes_real_restore_flow() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name: "e2e_fixture_complex_layout_restore_executes_real_restore_flow".to_string(),
            phases: Vec::new(),
            total_duration_ms: 0,
            passed: false,
            failure_reason: None,
            pane_reports: Vec::new(),
        };

        let (_tmp, db_path) = setup_test_db();
        let fixture_json = std::fs::read_to_string(fixture_path("snapshot_complex_layout.json"))
            .expect("read complex layout fixture");
        let topology =
            TopologySnapshot::from_json(&fixture_json).expect("parse complex layout fixture");
        let fixture_panes = collect_fixture_panes(&topology);
        let checkpoint_at = i64::try_from(topology.captured_at).expect("fixture timestamp fits");
        let session_id = "sess-fixture-complex";

        let seed_start = Instant::now();
        {
            let conn = Connection::open(db_path.as_str()).expect("open seeded fixture db");
            conn.execute(
                "INSERT INTO mux_sessions
                 (session_id, created_at, last_checkpoint_at, shutdown_clean, topology_json, ft_version)
                 VALUES (?1, ?2, ?3, 0, ?4, ?5)",
                params![
                    session_id,
                    checkpoint_at,
                    checkpoint_at,
                    fixture_json,
                    "0.1.0",
                ],
            )
            .expect("insert fixture session");
            conn.execute(
                "INSERT INTO session_checkpoints
                 (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes, metadata_json)
                 VALUES (?1, ?2, 'event', ?3, ?4, ?5, ?6)",
                params![
                    session_id,
                    checkpoint_at,
                    "fixture-complex-hash",
                    fixture_panes.len() as i64,
                    0i64,
                    json!({"fixture":"snapshot_complex_layout.json"}).to_string(),
                ],
            )
            .expect("insert fixture checkpoint");
            let checkpoint_id = conn.last_insert_rowid();

            for pane in &fixture_panes {
                let terminal_json = json!({
                    "rows": 24,
                    "cols": 80,
                    "cursor_row": 0,
                    "cursor_col": 0,
                    "is_alt_screen": false,
                    "title": format!("fixture-pane-{}", pane.pane_id),
                })
                .to_string();
                conn.execute(
                    "INSERT INTO mux_pane_state
                     (checkpoint_id, pane_id, cwd, command, terminal_state_json)
                     VALUES (?1, ?2, ?3, NULL, ?4)",
                    params![
                        checkpoint_id,
                        pane.pane_id as i64,
                        normalize_cwd(pane.cwd.as_deref()),
                        terminal_json,
                    ],
                )
                .expect("insert fixture pane state");
            }

            add_phase(
                &mut report,
                "seed_fixture_checkpoint",
                seed_start,
                "ok",
                json!({
                    "session_id": session_id,
                    "checkpoint_at": checkpoint_at,
                    "pane_count": fixture_panes.len(),
                    "window_count": topology.windows.len(),
                    "tab_count": topology.windows.iter().map(|window| window.tabs.len()).sum::<usize>(),
                }),
            );
        }

        let restorer = SessionRestorer::new(db_path.clone(), SessionRestoreConfig::default());
        let load_start = Instant::now();
        let session = restorer
            .detect()
            .expect("detect fixture restore candidate")
            .expect("fixture session should be restorable");
        let checkpoint = restorer
            .load_checkpoint(&session)
            .expect("load fixture checkpoint");
        add_phase(
            &mut report,
            "detect_and_load",
            load_start,
            "ok",
            json!({
                "detected_session": session.session_id,
                "checkpoint_id": checkpoint.checkpoint_id,
                "pane_states_loaded": checkpoint.pane_states.len(),
            }),
        );

        let wezterm = Arc::new(MockWezterm::new());
        let restore_start = Instant::now();
        let summary = restorer
            .restore(&session, &checkpoint, wezterm.clone())
            .await
            .expect("restore from fixture should succeed");
        add_phase(
            &mut report,
            "restore",
            restore_start,
            "ok",
            json!({
                "restored_count": summary.restored_count(),
                "failed_count": summary.failed_count(),
                "windows_created": summary.layout_result.windows_created,
                "tabs_created": summary.layout_result.tabs_created,
            }),
        );

        let verify_start = Instant::now();
        let restored_panes = wezterm.list_panes().await.expect("list restored panes");
        let unique_windows: HashSet<_> = restored_panes.iter().map(|pane| pane.window_id).collect();
        let unique_tabs: HashSet<_> = restored_panes.iter().map(|pane| pane.tab_id).collect();
        let active_old_pane_id = topology.windows[0].tabs[topology.windows[0]
            .active_tab_index
            .expect("fixture active tab index")]
        .active_pane_id
        .expect("fixture active pane id");
        let active_new_pane_id = *summary
            .layout_result
            .pane_id_map
            .get(&active_old_pane_id)
            .expect("active pane must be mapped");
        let active_new_pane = wezterm
            .pane_state(active_new_pane_id)
            .await
            .expect("active pane state should exist");

        let mut restored_tabs_by_old_tab = HashMap::new();
        for pane in &fixture_panes {
            let new_pane_id = *summary
                .layout_result
                .pane_id_map
                .get(&pane.pane_id)
                .expect("fixture pane must be restored");
            let new_state = wezterm
                .pane_state(new_pane_id)
                .await
                .expect("restored pane state should exist");
            let tab_consistent = match restored_tabs_by_old_tab.get(&pane.tab_id) {
                Some(expected_tab_id) => *expected_tab_id == new_state.tab_id,
                None => {
                    restored_tabs_by_old_tab.insert(pane.tab_id, new_state.tab_id);
                    true
                }
            };
            let active_matches = new_state.is_active == (pane.pane_id == active_old_pane_id);
            let cwd_matches = pane.cwd.as_deref().map(normalize_cwd_str).as_deref()
                == Some(new_state.cwd.as_str());

            report.pane_reports.push(PaneTestReport {
                pane_id: pane.pane_id,
                original_content_hash: hash_text(
                    &json!({
                        "window_id": pane.window_id,
                        "tab_id": pane.tab_id,
                        "cwd": pane.cwd.as_deref().map(normalize_cwd_str),
                    })
                    .to_string(),
                ),
                restored_content_hash: hash_text(
                    &json!({
                        "window_id": new_state.window_id,
                        "tab_id": new_state.tab_id,
                        "cwd": new_state.cwd,
                        "is_active": new_state.is_active,
                    })
                    .to_string(),
                ),
                content_match: cwd_matches,
                layout_match: tab_consistent,
                process_match: active_matches,
            });
        }

        let mapped_new_ids: HashSet<_> = summary
            .layout_result
            .pane_id_map
            .values()
            .copied()
            .collect();
        let success = summary.restored_count() == fixture_panes.len()
            && summary.failed_count() == 0
            && summary.layout_result.windows_created == 1
            && summary.layout_result.tabs_created == 2
            && restored_panes.len() == fixture_panes.len()
            && unique_windows.len() == 1
            && unique_tabs.len() == 2
            && mapped_new_ids.len() == fixture_panes.len()
            && active_new_pane.is_active
            && report.pane_reports.iter().all(|pane_result| {
                pane_result.content_match && pane_result.layout_match && pane_result.process_match
            });

        add_phase(
            &mut report,
            "verify_restored_layout",
            verify_start,
            if success { "ok" } else { "error" },
            json!({
                "restored_panes": restored_panes.len(),
                "unique_windows": unique_windows.len(),
                "unique_tabs": unique_tabs.len(),
                "active_old_pane_id": active_old_pane_id,
                "active_new_pane_id": active_new_pane_id,
            }),
        );

        report.total_duration_ms = run_start.elapsed().as_millis() as u64;
        report.passed = success;
        report.failure_reason = if success {
            None
        } else {
            Some("fixture-backed restore flow assertions failed".to_string())
        };
        emit_report(&report);
        assert!(
            report.passed,
            "{}",
            serde_json::to_string_pretty(&report).expect("pretty report")
        );
    });
}

#[test]
fn e2e_restore_replays_scrollback_then_relaunches_agent() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name: "e2e_restore_replays_scrollback_then_relaunches_agent".to_string(),
            phases: Vec::new(),
            total_duration_ms: 0,
            passed: false,
            failure_reason: None,
            pane_reports: Vec::new(),
        };

        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let pane = make_pane(7, 0, 0, 24, 80, "codex-agent", "file:///tmp/agents");

        let capture_start = Instant::now();
        let snapshot = engine
            .capture(std::slice::from_ref(&pane), SnapshotTrigger::Startup)
            .await
            .expect("capture source snapshot");
        add_phase(
            &mut report,
            "capture",
            capture_start,
            "ok",
            json!({
                "session_id": snapshot.session_id,
                "checkpoint_id": snapshot.checkpoint_id,
                "pane_count": snapshot.pane_count,
            }),
        );

        let seed_start = Instant::now();
        {
            let conn = Connection::open(db_path.as_str()).expect("open db for scrollback seed");
            let agent_json = r#"{"agent_type":"codex","session_id":"sess-42","state":"running"}"#;
            conn.execute(
                "UPDATE mux_pane_state
                 SET command = ?3,
                     agent_metadata_json = ?4,
                     scrollback_checkpoint_seq = ?5,
                     last_output_at = ?6
                 WHERE checkpoint_id = ?1 AND pane_id = ?2",
                params![
                    snapshot.checkpoint_id,
                    pane.pane_id as i64,
                    "codex",
                    agent_json,
                    1i64,
                    5_200i64,
                ],
            )
            .expect("seed pane restore metadata");
            insert_output_segment(&conn, pane.pane_id, 0, "first line\n", 5_100);
            insert_output_segment(&conn, pane.pane_id, 1, "second line\n", 5_200);
        }
        add_phase(
            &mut report,
            "seed_scrollback_and_agent_metadata",
            seed_start,
            "ok",
            json!({
                "pane_id": pane.pane_id,
                "scrollback_checkpoint_seq": 1,
                "segments_seeded": 2,
                "agent_type": "codex",
            }),
        );

        let restorer = SessionRestorer::new(
            db_path.clone(),
            SessionRestoreConfig {
                restore_scrollback: true,
                process_relaunch: LaunchConfig {
                    launch_agents: true,
                    launch_delay_ms: 0,
                    agent_commands: HashMap::from([(
                        "codex".to_string(),
                        "codex --resume".to_string(),
                    )]),
                    ..LaunchConfig::default()
                },
                ..SessionRestoreConfig::default()
            },
        );
        let session = restorer
            .detect()
            .expect("detect scrollback restore candidate")
            .expect("scrollback restore candidate should exist");
        let checkpoint = restorer
            .load_checkpoint(&session)
            .expect("load checkpoint for scrollback restore");

        let wezterm = Arc::new(MockWezterm::new());
        let restore_start = Instant::now();
        let summary = restorer
            .restore(&session, &checkpoint, wezterm.clone())
            .await
            .expect("restore with scrollback + relaunch should succeed");
        add_phase(
            &mut report,
            "restore",
            restore_start,
            "ok",
            json!({
                "restored_count": summary.restored_count(),
                "scrollback_restored": summary.scrollback_restored_count(),
                "scrollback_failed": summary.scrollback_failed_count(),
                "scrollback_skipped": summary.scrollback_skipped_count(),
            }),
        );

        let verify_start = Instant::now();
        let new_pane_id = *summary
            .layout_result
            .pane_id_map
            .get(&pane.pane_id)
            .expect("restored pane mapping should exist");
        let content = wezterm
            .get_text(new_pane_id, false)
            .await
            .expect("read restored pane content");
        let expected_cd = format!(
            "cd {}\r",
            normalize_cwd_str(pane.cwd.as_deref().unwrap_or(""))
        );
        let first_line_offset = content.find("first line").expect("replayed first line");
        let second_line_offset = content.find("second line").expect("replayed second line");
        let banner_offset = content
            .find("Session restored")
            .expect("restore banner should exist");
        let cd_offset = content.find(&expected_cd).expect("expected cd command");
        let agent_offset = content
            .find("codex --resume\r")
            .expect("expected agent relaunch command");
        let redetected = restorer
            .detect()
            .expect("detect after successful restore should work");

        let content_order_ok = first_line_offset < second_line_offset
            && second_line_offset < banner_offset
            && banner_offset < cd_offset
            && cd_offset < agent_offset;
        let success = summary.restored_count() == 1
            && summary.failed_count() == 0
            && summary.scrollback_restored_count() == 1
            && summary.scrollback_failed_count() == 0
            && summary.scrollback_skipped_count() == 0
            && summary.scrollback_error.is_none()
            && content_order_ok
            && redetected.is_none();

        report.pane_reports.push(PaneTestReport {
            pane_id: pane.pane_id,
            original_content_hash: hash_text("first line\nsecond line\n"),
            restored_content_hash: hash_text(&content),
            content_match: content.contains("first line")
                && content.contains("second line")
                && content_order_ok,
            layout_match: summary.restored_count() == 1 && summary.failed_count() == 0,
            process_match: content.contains(&expected_cd) && content.contains("codex --resume\r"),
        });

        add_phase(
            &mut report,
            "verify_content_and_relaunch_order",
            verify_start,
            if success { "ok" } else { "error" },
            json!({
                "new_pane_id": new_pane_id,
                "content_order_ok": content_order_ok,
                "banner_before_relaunch": banner_offset < cd_offset,
                "detect_cleared": redetected.is_none(),
            }),
        );

        report.total_duration_ms = run_start.elapsed().as_millis() as u64;
        report.passed = success;
        report.failure_reason = if success {
            None
        } else {
            Some("scrollback replay + agent relaunch assertions failed".to_string())
        };
        emit_report(&report);
        assert!(
            report.passed,
            "{}",
            serde_json::to_string_pretty(&report).expect("pretty report")
        );
    });
}
