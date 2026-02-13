//! End-to-end snapshot/restore roundtrip tests with structured reports.
//!
//! These tests exercise `SnapshotEngine` capture + persistence and the
//! session-restore query path (`session_restore`) against a real SQLite file.
//! They intentionally avoid requiring a live WezTerm instance.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use frankenterm_core::config::SnapshotConfig;
use frankenterm_core::session_restore::{
    RestoredPaneState, SessionRestoreConfig, SessionRestorer, load_checkpoint_by_id,
    load_latest_checkpoint, session_doctor, show_session,
};
use frankenterm_core::session_topology::TopologySnapshot;
use frankenterm_core::snapshot_engine::{SnapshotEngine, SnapshotError, SnapshotTrigger};
use frankenterm_core::wezterm::{PaneInfo, PaneSize};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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

        CREATE INDEX IF NOT EXISTS idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
        CREATE INDEX IF NOT EXISTS idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
        CREATE INDEX IF NOT EXISTS idx_pane_state_pane ON mux_pane_state(pane_id);
        ",
    )
    .expect("create schema");
    (tmp, db_path)
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

#[tokio::test]
async fn e2e_snapshot_roundtrip_single_pane_report() {
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
}

#[tokio::test]
async fn e2e_snapshot_roundtrip_targeted_checkpoint_restore() {
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
}

#[tokio::test]
async fn e2e_snapshot_dedup_retention_and_detect_cycle() {
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
