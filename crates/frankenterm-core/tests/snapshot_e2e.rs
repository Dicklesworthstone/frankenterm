//! Hermetic snapshot persistence and manual-restore contract tests with
//! structured reports.
//!
//! These tests exercise `SnapshotEngine` capture + persistence and the
//! session-restore query path (`session_restore`) against a real SQLite file.
//! They intentionally avoid requiring a live WezTerm instance.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use frankenterm_core::config::SnapshotConfig;
use frankenterm_core::session_restore::{
    CheckpointRole, RestoreError, RestoredPaneState, SessionRestoreConfig, SessionRestorer,
    load_checkpoint_by_id, load_latest_checkpoint, session_doctor, show_session,
};
use frankenterm_core::session_topology::{PaneNode, TopologySnapshot};
use frankenterm_core::snapshot_engine::{
    SnapshotCaptureOptions, SnapshotEngine, SnapshotError, SnapshotTrigger,
};
use frankenterm_core::wezterm::{MockWezterm, PaneInfo, PaneSize, WeztermInterface};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    use frankenterm_core::runtime_async::CompatRuntime;
    let runtime = frankenterm_core::runtime_async::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future);
}

#[derive(Debug)]
struct E2ETestReport {
    test_name: String,
    phases: Vec<PhaseReport>,
    total_duration_ms: u64,
    passed: bool,
    failure_reason: Option<String>,
    pane_reports: Vec<PaneTestReport>,
}

impl E2ETestReport {
    /// A contract result is only positive when at least one pane report exists,
    /// every pane carries at least one exercised check, and no exercised check
    /// failed. This deliberately rejects the vacuous truth of `all()` over an
    /// empty pane-report collection.
    fn all_pane_contracts_exercised_and_pass(&self) -> bool {
        !self.pane_reports.is_empty()
            && self
                .pane_reports
                .iter()
                .all(PaneTestReport::all_exercised_checks_pass)
    }

    fn pane_contract_checks_present(&self) -> bool {
        !self.pane_reports.is_empty()
            && self.pane_reports.iter().all(|pane| !pane.checks.is_empty())
    }

    fn exercised_checks_present(&self) -> bool {
        self.pane_reports
            .iter()
            .flat_map(|pane| pane.checks.iter())
            .any(|check| check.status != PaneCheckStatus::NotTested)
    }
}

const HERMETIC_NOT_TESTED_CONTRACTS: [&str; 13] = [
    "mux_stop_start",
    "authenticated_restart_execution",
    "automatic_startup_restore",
    "process_continuity",
    "historical_scrollback_replay",
    "terminal_render_state_replay",
    "real_mux_geometry",
    "user_tab_order_round_trip",
    "real_mux_active_tab_identity",
    "native_resize_zoom_rendering",
    "native_keypress_latency",
    "target_class_large_session_scale",
    "full_session_continuity",
];

impl Serialize for E2ETestReport {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let exercised_checks_passed = self
            .pane_reports
            .iter()
            .flat_map(|pane| pane.checks.iter())
            .filter(|check| check.status == PaneCheckStatus::Passed)
            .count();
        let not_tested_contracts = HERMETIC_NOT_TESTED_CONTRACTS
            .into_iter()
            .chain(
                self.pane_reports
                    .iter()
                    .flat_map(|pane| pane.checks.iter())
                    .filter(|check| check.status == PaneCheckStatus::NotTested)
                    .map(|check| check.contract),
            )
            .collect::<std::collections::BTreeSet<_>>();
        let pane_contract_checks_present = self.pane_contract_checks_present();
        let exercised_checks_present = self.exercised_checks_present();
        let exercised_contracts_passed =
            self.passed && self.all_pane_contracts_exercised_and_pass();

        let mut state = serializer.serialize_struct("E2ETestReport", 13)?;
        state.serialize_field("test_name", &self.test_name)?;
        state.serialize_field("scope", "hermetic_sqlite_mock_mux")?;
        state.serialize_field("phases", &self.phases)?;
        state.serialize_field("total_duration_ms", &self.total_duration_ms)?;
        state.serialize_field("test_assertions_passed", &self.passed)?;
        state.serialize_field("exercised_contracts_passed", &exercised_contracts_passed)?;
        state.serialize_field(
            "pane_contract_checks_present",
            &pane_contract_checks_present,
        )?;
        state.serialize_field("exercised_checks_present", &exercised_checks_present)?;
        state.serialize_field("exercised_checks_passed", &exercised_checks_passed)?;
        state.serialize_field("not_tested_contracts", &not_tested_contracts)?;
        state.serialize_field("not_tested_contract_count", &not_tested_contracts.len())?;
        state.serialize_field("failure_reason", &self.failure_reason)?;
        state.serialize_field("pane_reports", &self.pane_reports)?;
        state.end()
    }
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
    source_artifact_hash: String,
    observed_artifact_hash: String,
    checks: Vec<PaneContractCheck>,
}

impl PaneTestReport {
    fn all_exercised_checks_pass(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == PaneCheckStatus::Passed)
            && self
                .checks
                .iter()
                .all(|check| check.status != PaneCheckStatus::Failed)
    }
}

#[test]
fn not_tested_only_pane_report_never_passes() {
    let report = PaneTestReport {
        pane_id: 1,
        source_artifact_hash: "not-tested".to_string(),
        observed_artifact_hash: "not-tested".to_string(),
        checks: vec![
            PaneContractCheck::not_tested("process_continuity"),
            PaneContractCheck::not_tested("scrollback_render_replay"),
        ],
    };
    assert!(!report.all_exercised_checks_pass());
}

#[test]
fn one_failed_check_overrides_a_passed_check_in_pane_and_e2e_reports() {
    let pane_report = PaneTestReport {
        pane_id: 1,
        source_artifact_hash: "source".to_string(),
        observed_artifact_hash: "observed".to_string(),
        checks: vec![
            PaneContractCheck::exercised("checkpoint_shape", true),
            PaneContractCheck::exercised("checkpoint_content", false),
        ],
    };
    assert!(!pane_report.all_exercised_checks_pass());

    let report = E2ETestReport {
        test_name: "passed-and-failed-negative-control".to_string(),
        phases: Vec::new(),
        total_duration_ms: 0,
        passed: true,
        failure_reason: None,
        pane_reports: vec![pane_report],
    };
    assert!(!report.all_pane_contracts_exercised_and_pass());

    let serialized = serde_json::to_value(report).expect("serialize negative control");
    assert_eq!(
        serialized["exercised_contracts_passed"].as_bool(),
        Some(false)
    );
    assert_eq!(serialized["exercised_checks_passed"].as_u64(), Some(1));
    assert_eq!(serialized["exercised_checks_present"].as_bool(), Some(true));
}

#[test]
fn one_unexercised_pane_prevents_a_passing_serialized_report() {
    let report = E2ETestReport {
        test_name: "mixed-contract-proof".to_string(),
        phases: Vec::new(),
        total_duration_ms: 0,
        passed: true,
        failure_reason: None,
        pane_reports: vec![
            PaneTestReport {
                pane_id: 1,
                source_artifact_hash: "not-tested".to_string(),
                observed_artifact_hash: "not-tested".to_string(),
                checks: vec![PaneContractCheck::not_tested("process_continuity")],
            },
            PaneTestReport {
                pane_id: 2,
                source_artifact_hash: "source".to_string(),
                observed_artifact_hash: "observed".to_string(),
                checks: vec![PaneContractCheck::exercised("checkpoint_shape", true)],
            },
        ],
    };

    let serialized = serde_json::to_value(report).expect("serialize proof report");
    assert_eq!(
        serialized["exercised_contracts_passed"].as_bool(),
        Some(false)
    );
}

#[test]
fn passing_phase_only_report_does_not_fabricate_missing_pane_checks() {
    let report = E2ETestReport {
        test_name: "phase-only-contract-proof".to_string(),
        phases: vec![PhaseReport {
            phase: "topology_roundtrip".to_string(),
            duration_ms: 0,
            status: "passed".to_string(),
            details: json!({"scope": "serialized_topology"}),
        }],
        total_duration_ms: 0,
        passed: true,
        failure_reason: None,
        pane_reports: Vec::new(),
    };

    let serialized = serde_json::to_value(report).expect("serialize phase-only report");
    assert_eq!(serialized["test_assertions_passed"].as_bool(), Some(true));
    assert_eq!(
        serialized["exercised_contracts_passed"].as_bool(),
        Some(false)
    );
    assert_eq!(
        serialized["pane_contract_checks_present"].as_bool(),
        Some(false)
    );
    assert_eq!(serialized["exercised_checks_passed"].as_u64(), Some(0));
    assert_eq!(
        serialized["exercised_checks_present"].as_bool(),
        Some(false)
    );
}

#[test]
fn empty_check_vector_is_reported_as_absent_and_cannot_pass() {
    let report = E2ETestReport {
        test_name: "empty-check-contract-proof".to_string(),
        phases: Vec::new(),
        total_duration_ms: 0,
        passed: true,
        failure_reason: None,
        pane_reports: vec![PaneTestReport {
            pane_id: 1,
            source_artifact_hash: "source".to_string(),
            observed_artifact_hash: "observed".to_string(),
            checks: Vec::new(),
        }],
    };

    let serialized = serde_json::to_value(report).expect("serialize empty-check report");
    assert_eq!(
        serialized["pane_contract_checks_present"].as_bool(),
        Some(false)
    );
    assert_eq!(
        serialized["exercised_contracts_passed"].as_bool(),
        Some(false)
    );
}

#[derive(Debug, Serialize)]
struct PaneContractCheck {
    contract: &'static str,
    status: PaneCheckStatus,
}

impl PaneContractCheck {
    fn exercised(contract: &'static str, passed: bool) -> Self {
        Self {
            contract,
            status: if passed {
                PaneCheckStatus::Passed
            } else {
                PaneCheckStatus::Failed
            },
        }
    }

    const fn not_tested(contract: &'static str) -> Self {
        Self {
            contract,
            status: PaneCheckStatus::NotTested,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PaneCheckStatus {
    Passed,
    Failed,
    NotTested,
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
    conn.execute_batch(frankenterm_core::storage::migrations::mux_sessions_schema_sql().unwrap())
        .expect("snapshot E2E fixture must install the canonical mux_sessions schema");
    conn.execute_batch(
        "
        PRAGMA foreign_keys = ON;
        PRAGMA journal_mode = WAL;

        CREATE TABLE IF NOT EXISTS session_checkpoints (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
            checkpoint_at INTEGER NOT NULL,
            checkpoint_type TEXT NOT NULL CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
            state_hash TEXT NOT NULL,
            pane_count INTEGER NOT NULL,
            total_bytes INTEGER NOT NULL,
            metadata_json TEXT,
            checkpoint_role TEXT NOT NULL DEFAULT 'snapshot'
                CHECK(checkpoint_role IN ('snapshot','restore_intent','restore_receipt')),
            topology_json TEXT,
            restore_intent_checkpoint_id INTEGER
                REFERENCES session_checkpoints(id) ON DELETE CASCADE,
            CHECK(checkpoint_role = 'restore_receipt' OR restore_intent_checkpoint_id IS NULL)
        );

        CREATE TABLE IF NOT EXISTS restore_attempt_lifecycle (
            intent_checkpoint_id INTEGER PRIMARY KEY
                REFERENCES session_checkpoints(id)
                ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED,
            session_id TEXT NOT NULL
                REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
            source_checkpoint_id INTEGER NOT NULL,
            outcome_checkpoint_id INTEGER
                REFERENCES session_checkpoints(id) ON DELETE SET NULL,
            status TEXT NOT NULL
                CHECK(status IN ('intent','outcome_complete','resolved','reconciliation_required')),
            created_at INTEGER NOT NULL,
            resolved_at INTEGER,
            CHECK(intent_checkpoint_id <> source_checkpoint_id),
            CHECK(outcome_checkpoint_id IS NULL OR outcome_checkpoint_id <> intent_checkpoint_id),
            CHECK(outcome_checkpoint_id IS NULL OR outcome_checkpoint_id <> source_checkpoint_id),
            CHECK(created_at >= 0),
            CHECK(resolved_at IS NULL OR resolved_at >= created_at),
            CHECK(
                (status = 'intent'
                    AND outcome_checkpoint_id IS NULL
                    AND resolved_at IS NULL)
                OR (status = 'outcome_complete'
                    AND outcome_checkpoint_id IS NOT NULL
                    AND resolved_at IS NULL)
                OR (status = 'reconciliation_required'
                    AND resolved_at IS NULL)
                OR (status = 'resolved'
                    AND resolved_at IS NOT NULL)
            )
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

        CREATE TABLE IF NOT EXISTS pane_scrollback_summary (
            pane_id INTEGER PRIMARY KEY,
            retained_segment_count INTEGER NOT NULL,
            first_seq INTEGER NOT NULL,
            last_seq INTEGER NOT NULL,
            first_captured_at INTEGER NOT NULL,
            last_captured_at INTEGER NOT NULL
        );

        CREATE TRIGGER output_segments_scrollback_summary_ai
        AFTER INSERT ON output_segments BEGIN
            INSERT INTO pane_scrollback_summary (
                pane_id, retained_segment_count, first_seq, last_seq,
                first_captured_at, last_captured_at
            ) VALUES (
                new.pane_id, 1, new.seq, new.seq, new.captured_at, new.captured_at
            )
            ON CONFLICT(pane_id) DO UPDATE SET
                retained_segment_count = retained_segment_count + 1,
                first_seq = min(first_seq, new.seq),
                last_seq = max(last_seq, new.seq),
                first_captured_at = min(first_captured_at, new.captured_at),
                last_captured_at = max(last_captured_at, new.captured_at);
        END;

        CREATE TRIGGER output_segments_scrollback_summary_ad
        AFTER DELETE ON output_segments BEGIN
            DELETE FROM pane_scrollback_summary
            WHERE pane_id = old.pane_id
              AND NOT EXISTS (
                  SELECT 1 FROM output_segments WHERE pane_id = old.pane_id
              );
            UPDATE pane_scrollback_summary
            SET retained_segment_count = (
                    SELECT count(*) FROM output_segments WHERE pane_id = old.pane_id
                ),
                first_seq = (
                    SELECT min(seq) FROM output_segments WHERE pane_id = old.pane_id
                ),
                last_seq = (
                    SELECT max(seq) FROM output_segments WHERE pane_id = old.pane_id
                ),
                first_captured_at = (
                    SELECT min(captured_at)
                    FROM output_segments WHERE pane_id = old.pane_id
                ),
                last_captured_at = (
                    SELECT max(captured_at)
                    FROM output_segments WHERE pane_id = old.pane_id
                )
            WHERE pane_id = old.pane_id;
        END;

        CREATE INDEX IF NOT EXISTS idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
        CREATE INDEX IF NOT EXISTS idx_checkpoints_session_role_latest
            ON session_checkpoints(session_id, checkpoint_role, checkpoint_at DESC, id DESC);
        CREATE INDEX IF NOT EXISTS idx_checkpoints_session_role_causal
            ON session_checkpoints(session_id, checkpoint_role, id DESC);
        CREATE INDEX IF NOT EXISTS idx_checkpoints_global_latest
            ON session_checkpoints(checkpoint_at DESC, id DESC);
        CREATE INDEX IF NOT EXISTS idx_checkpoints_global_snapshot_latest
            ON session_checkpoints(checkpoint_at DESC, id DESC)
            WHERE checkpoint_role = 'snapshot';
        CREATE UNIQUE INDEX IF NOT EXISTS idx_checkpoints_restore_intent_outcome
            ON session_checkpoints(restore_intent_checkpoint_id)
            WHERE restore_intent_checkpoint_id IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_mux_sessions_clean_checkpoint
            ON mux_sessions(clean_checkpoint_id);
        CREATE INDEX IF NOT EXISTS idx_restore_attempt_lifecycle_session_status
            ON restore_attempt_lifecycle(session_id, status, intent_checkpoint_id);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_restore_attempt_lifecycle_outcome
            ON restore_attempt_lifecycle(outcome_checkpoint_id)
            WHERE outcome_checkpoint_id IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
        CREATE INDEX IF NOT EXISTS idx_pane_state_pane ON mux_pane_state(pane_id);
        CREATE INDEX IF NOT EXISTS idx_output_segments_pane_seq ON output_segments(pane_id, seq);
        ",
    )
    .expect("create schema");
    conn.execute_batch(
        frankenterm_core::storage::migrations::session_retained_size_schema_sql()
            .expect("locate canonical v40 retained-size schema"),
    )
    .expect("install canonical v40 retained-size authority");
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
    hex::encode(hasher.finalize())
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
            source_artifact_hash: pane_info_hash(&pane),
            observed_artifact_hash: restored_state_hash(restored),
            checks: vec![
                PaneContractCheck::exercised(
                    "persisted_pane_state",
                    normalize_cwd(pane.cwd.as_deref()) == restored.cwd
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
                ),
                PaneContractCheck::exercised(
                    "checkpoint_shape",
                    checkpoint.pane_count == 1 && checkpoints.len() == 1,
                ),
                PaneContractCheck::exercised("captured_command_absent", restored.command.is_none()),
            ],
        };
        report.pane_reports.push(pane_report);

        let success = report.all_pane_contracts_exercised_and_pass();
        let passed_panes = report
            .pane_reports
            .iter()
            .filter(|pane_result| pane_result.all_exercised_checks_pass())
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
fn e2e_targeted_checkpoint_load_distinguishes_versions() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name: "e2e_targeted_checkpoint_load_distinguishes_versions".to_string(),
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
            source_artifact_hash: old_hash.clone(),
            observed_artifact_hash: new_hash.clone(),
            checks: vec![
                PaneContractCheck::exercised(
                    "checkpoint_versions_distinct",
                    checkpoint_versions_distinct,
                ),
                PaneContractCheck::exercised("new_checkpoint_has_extra_pane", new_has_extra_pane),
                PaneContractCheck::exercised("latest_checkpoint_selection", latest_matches_new),
            ],
        });

        let success = report.all_pane_contracts_exercised_and_pass();
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
        engine
            .close_after_checkpoint(&manual_changed)
            .await
            .expect("close exact latest checkpoint");
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
                "layout_settled_pane_count": summary.layout_settled_pane_count(),
                "layout_failed_pane_count": summary.layout_failed_pane_count(),
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
        let (startup_checkpoint_id, startup_metadata_json, shutdown_clean, clean_checkpoint_id): (
            i64,
            String,
            i64,
            Option<i64>,
        ) = verify_conn
            .query_row(
                "SELECT c.id, c.metadata_json, s.shutdown_clean, s.clean_checkpoint_id
                     FROM session_checkpoints c
                     JOIN mux_sessions s ON s.session_id = c.session_id
                     WHERE c.session_id = ?1
                       AND c.checkpoint_role = 'restore_receipt'
                     ORDER BY c.checkpoint_at DESC, c.id DESC
                     LIMIT 1",
                [snapshot.session_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("restore receipt should be recorded");
        let startup_metadata: Value =
            serde_json::from_str(&startup_metadata_json).expect("parse startup metadata");
        let restored_new_pane_id = *summary
            .layout_result
            .pane_id_map
            .get(&pane.pane_id)
            .expect("restored pane mapping must exist");
        let pane_text = wezterm
            .get_text(restored_new_pane_id, false)
            .await
            .expect("read restored pane text");
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
            .any(|info| info.checkpoint_role == CheckpointRole::RestoreReceipt);
        let startup_checkpoint_distinct = startup_checkpoint_id != snapshot.checkpoint_id;
        let startup_mapping_matches = startup_metadata["old_to_new"][old_id_key.as_str()].as_u64()
            == Some(restored_new_pane_id);
        let session_clean = shutdown_clean == 1;
        let clean_receipt_matches = clean_checkpoint_id == Some(startup_checkpoint_id);
        let detect_cleared = redetected.is_none();
        let pty_input_untouched = pane_text.is_empty();

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
                "clean_receipt_matches": clean_receipt_matches,
                "detect_cleared": detect_cleared,
                "pty_input_untouched": pty_input_untouched,
            }),
        );

        report.pane_reports.push(PaneTestReport {
            pane_id: pane.pane_id,
            source_artifact_hash: pane_info_hash(&pane),
            observed_artifact_hash: restored_state_hash(latest_state),
            checks: vec![
                PaneContractCheck::exercised(
                    "capture_state_preserved",
                    latest_retains_capture && capture_state_preserved,
                ),
                PaneContractCheck::exercised(
                    "restore_receipt_mapping",
                    startup_checkpoint_exists
                        && startup_checkpoint_distinct
                        && startup_mapping_matches,
                ),
                PaneContractCheck::exercised(
                    "authority_settled_without_pty_input",
                    session_clean && clean_receipt_matches && detect_cleared && pty_input_untouched,
                ),
            ],
        });

        let success = report.all_pane_contracts_exercised_and_pass()
            && summary.layout_settled_pane_count() == 1
            && summary.layout_failed_pane_count() == 0;

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
fn e2e_fixture_complex_layout_executes_session_restorer_flow() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name: "e2e_fixture_complex_layout_executes_session_restorer_flow".to_string(),
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
                 (session_id, checkpoint_at, checkpoint_type, state_hash,
                  pane_count, total_bytes, metadata_json, checkpoint_role,
                  topology_json)
                 VALUES (?1, ?2, 'event', ?3, ?4, ?5, ?6, 'snapshot', ?7)",
                params![
                    session_id,
                    checkpoint_at,
                    "0123456789abcdef",
                    fixture_panes.len() as i64,
                    0i64,
                    json!({"fixture":"snapshot_complex_layout.json"}).to_string(),
                    fixture_json,
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
                "layout_settled_pane_count": summary.layout_settled_pane_count(),
                "layout_failed_pane_count": summary.layout_failed_pane_count(),
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
                source_artifact_hash: hash_text(
                    &json!({
                        "window_id": pane.window_id,
                        "tab_id": pane.tab_id,
                        "cwd": pane.cwd.as_deref().map(normalize_cwd_str),
                    })
                    .to_string(),
                ),
                observed_artifact_hash: hash_text(
                    &json!({
                        "window_id": new_state.window_id,
                        "tab_id": new_state.tab_id,
                        "cwd": new_state.cwd,
                        "is_active": new_state.is_active,
                    })
                    .to_string(),
                ),
                checks: vec![
                    PaneContractCheck::exercised("local_cwd", cwd_matches),
                    PaneContractCheck::exercised("tab_membership", tab_consistent),
                    PaneContractCheck::exercised("active_selection", active_matches),
                ],
            });
        }

        let mapped_new_ids: HashSet<_> = summary
            .layout_result
            .pane_id_map
            .values()
            .copied()
            .collect();
        let success = summary.layout_settled_pane_count() == fixture_panes.len()
            && summary.layout_failed_pane_count() == 0
            && summary.layout_result.windows_created == 1
            && summary.layout_result.tabs_created == 2
            && restored_panes.len() == fixture_panes.len()
            && unique_windows.len() == 1
            && unique_tabs.len() == 2
            && mapped_new_ids.len() == fixture_panes.len()
            && active_new_pane.is_active
            && report.all_pane_contracts_exercised_and_pass();

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
fn e2e_restore_rejects_unsafe_scrollback_before_mux_or_authority_effects() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name: "e2e_restore_rejects_unsafe_scrollback_before_mux_or_authority_effects"
                .to_string(),
            phases: Vec::new(),
            total_duration_ms: 0,
            passed: false,
            failure_reason: None,
            pane_reports: Vec::new(),
        };

        let (_tmp, db_path) = setup_test_db();
        let engine = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let pane = make_pane(7, 0, 0, 24, 80, "codex-agent", "file:///tmp/agents");

        let seed_start = Instant::now();
        {
            let conn = Connection::open(db_path.as_str()).expect("open db for scrollback seed");
            insert_output_segment(&conn, pane.pane_id, 0, "first line\n", 5_100);
            insert_output_segment(&conn, pane.pane_id, 1, "second line\n", 5_200);
        }
        let capture_start = Instant::now();
        let snapshot = engine
            .capture_with_options(
                std::slice::from_ref(&pane),
                SnapshotTrigger::Startup,
                SnapshotCaptureOptions {
                    include_scrollback: true,
                    metadata: None,
                },
            )
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
        let error = restorer
            .restore(&session, &checkpoint, wezterm.clone())
            .await
            .expect_err("scrollback replay through PTY input must fail closed");
        let safe_rejection = matches!(error, RestoreError::SafeScrollbackReplayUnavailable);
        add_phase(
            &mut report,
            "restore_safety_preflight",
            restore_start,
            if safe_rejection { "ok" } else { "error" },
            json!({
                "safe_scrollback_rejection": safe_rejection,
                "error_kind": if safe_rejection {
                    "safe_scrollback_replay_unavailable"
                } else {
                    "unexpected"
                },
            }),
        );

        let verify_start = Instant::now();
        let panes = wezterm
            .list_panes()
            .await
            .expect("list panes after safety rejection");
        let authority_rows: i64 = Connection::open(db_path.as_str())
            .expect("open restore safety verification database")
            .query_row(
                "SELECT COUNT(*)
                 FROM session_checkpoints
                 WHERE session_id = ?1
                   AND checkpoint_role IN ('restore_intent', 'restore_receipt')",
                [snapshot.session_id.as_str()],
                |row| row.get(0),
            )
            .expect("count restore authority rows");
        let redetected = restorer
            .detect()
            .expect("detect after rejected restore should work");

        let no_mux_effects = panes.is_empty();
        let no_authority_effects = authority_rows == 0;
        let remains_restore_candidate = redetected
            .as_ref()
            .is_some_and(|candidate| candidate.session_id == snapshot.session_id);
        report.pane_reports.push(PaneTestReport {
            pane_id: pane.pane_id,
            source_artifact_hash: hash_text("first line\nsecond line\n"),
            observed_artifact_hash: hash_text(""),
            checks: vec![
                PaneContractCheck::exercised("mux_side_effects_absent", no_mux_effects),
                PaneContractCheck::exercised("authority_side_effects_absent", no_authority_effects),
                PaneContractCheck::not_tested("process_continuity"),
            ],
        });
        let success = safe_rejection
            && no_mux_effects
            && no_authority_effects
            && remains_restore_candidate
            && report.all_pane_contracts_exercised_and_pass();

        add_phase(
            &mut report,
            "verify_no_external_or_authority_effects",
            verify_start,
            if success { "ok" } else { "error" },
            json!({
                "mux_pane_count": panes.len(),
                "restore_authority_rows": authority_rows,
                "remains_restore_candidate": remains_restore_candidate,
            }),
        );

        report.total_duration_ms = run_start.elapsed().as_millis() as u64;
        report.passed = success;
        report.failure_reason = if success {
            None
        } else {
            Some("scrollback safety preflight mutated mux or restore authority".to_string())
        };
        emit_report(&report);
        assert!(
            report.passed,
            "{}",
            serde_json::to_string_pretty(&report).expect("pretty report")
        );
    });
}

// ─────────────────────────────────────────────────────────────────────────
// [wa-bo6f] Persistence-boundary and manual-layout-restore tests.
//
// Each test captures with engine A, drops engine A, then builds fresh handles
// against the same database. This proves persistence/query behavior and the
// explicitly exercised manual layout subset only. It does not stop or start a
// mux process, resume a process, replay scrollback, preserve render state, or
// establish full-session continuity.
//
// Every test emits a structured E2ETestReport (JSON on [E2E_REPORT] lines)
// with per-phase timing so a CI log alone is enough to diagnose a failure
// without re-running locally.
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn e2e_persisted_session_identity_survives_engine_rebuild_and_manual_layout_restore() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name:
                "e2e_persisted_session_identity_survives_engine_rebuild_and_manual_layout_restore"
                    .to_string(),
            phases: Vec::new(),
            total_duration_ms: 0,
            passed: false,
            failure_reason: None,
            pane_reports: Vec::new(),
        };

        let (_tmp, db_path) = setup_test_db();

        // Three panes across two windows — enough topology to make pane-id,
        // tab-id, and cwd preservation meaningfully distinguishable.
        let panes = vec![
            make_pane(11, 0, 0, 24, 80, "claude-code", "file:///tmp/roundtrip-a"),
            make_pane(22, 1, 0, 24, 80, "codex-agent", "file:///tmp/roundtrip-b"),
            make_pane(33, 2, 1, 40, 120, "shell", "file:///tmp/roundtrip-c"),
        ];

        // ── Phase 1: save (engine A captures, then is dropped) ──────────
        let captured_session_id;
        let captured_checkpoint_id;
        let captured_pane_count;
        {
            let save_start = Instant::now();
            let engine_a = SnapshotEngine::new(
                db_path.clone(),
                SnapshotConfig {
                    retention_count: 5,
                    retention_days: 365,
                    ..SnapshotConfig::default()
                },
            );
            let snapshot = engine_a
                .capture(&panes, SnapshotTrigger::Startup)
                .await
                .expect("capture before handle rebuild");
            captured_session_id = snapshot.session_id.clone();
            captured_checkpoint_id = snapshot.checkpoint_id;
            captured_pane_count = snapshot.pane_count;
            add_phase(
                &mut report,
                "save_via_engine_a",
                save_start,
                "ok",
                json!({
                    "session_id": snapshot.session_id,
                    "checkpoint_id": snapshot.checkpoint_id,
                    "pane_count": snapshot.pane_count,
                    "trigger": "startup",
                }),
            );
            // Only the engine handle drops here; no mux process is exercised.
        }

        // ── Phase 2: rebuild engine/restorer handles on the same DB.
        let rebuild_start = Instant::now();
        let _engine_b = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let restorer = SessionRestorer::new(
            db_path.clone(),
            SessionRestoreConfig {
                restore_scrollback: false,
                ..SessionRestoreConfig::default()
            },
        );
        add_phase(
            &mut report,
            "rebuild_process_local_handles",
            rebuild_start,
            "ok",
            json!({
                "db_path": db_path.as_str(),
                "engine_a_dropped": true,
            }),
        );

        // ── Phase 3: detect — must rediscover the captured session.
        let detect_start = Instant::now();
        let candidate = restorer
            .detect()
            .expect("detect after handle rebuild must not error")
            .expect("detect after handle rebuild must find the captured session");
        let session_id_matches = candidate.session_id == captured_session_id;
        add_phase(
            &mut report,
            "detect_unclean_session_after_handle_rebuild",
            detect_start,
            if session_id_matches { "ok" } else { "error" },
            json!({
                "detected_session_id": candidate.session_id,
                "captured_session_id": captured_session_id,
                "session_id_matches": session_id_matches,
            }),
        );

        // ── Phase 4: load checkpoint and compare to original inputs.
        let load_start = Instant::now();
        let checkpoint = restorer
            .load_checkpoint(&candidate)
            .expect("load checkpoint after handle rebuild");
        let checkpoint_id_matches = checkpoint.checkpoint_id == captured_checkpoint_id;
        let pane_count_matches = checkpoint.pane_count == captured_pane_count;

        let original_pane_ids: HashSet<u64> = panes.iter().map(|p| p.pane_id).collect();
        let restored_pane_ids: HashSet<u64> =
            checkpoint.pane_states.iter().map(|s| s.pane_id).collect();
        let pane_ids_match = original_pane_ids == restored_pane_ids;

        let original_cwds: HashMap<u64, Option<String>> = panes
            .iter()
            .map(|p| (p.pane_id, normalize_cwd(p.cwd.as_deref())))
            .collect();
        let restored_cwds: HashMap<u64, Option<String>> = checkpoint
            .pane_states
            .iter()
            .map(|s| (s.pane_id, s.cwd.clone()))
            .collect();
        let cwds_match = original_cwds == restored_cwds;

        add_phase(
            &mut report,
            "load_and_compare_identity",
            load_start,
            if checkpoint_id_matches && pane_count_matches && pane_ids_match && cwds_match {
                "ok"
            } else {
                "error"
            },
            json!({
                "checkpoint_id_matches": checkpoint_id_matches,
                "pane_count_matches": pane_count_matches,
                "pane_ids_match": pane_ids_match,
                "cwds_match": cwds_match,
                "loaded_checkpoint_id": checkpoint.checkpoint_id,
                "loaded_pane_count": checkpoint.pane_count,
                "loaded_pane_ids": restored_pane_ids.iter().copied().collect::<Vec<_>>(),
            }),
        );

        // Per-pane hash report (used by the roll-up assertion below).
        for pane in &panes {
            let restored_pane = checkpoint
                .pane_states
                .iter()
                .find(|s| s.pane_id == pane.pane_id)
                .expect("each captured pane must reappear after handle rebuild");
            let content_match = normalize_cwd(pane.cwd.as_deref()) == restored_pane.cwd
                && pane.effective_rows()
                    == restored_pane
                        .terminal_state
                        .as_ref()
                        .map(|t| u32::from(t.rows))
                        .unwrap_or_default()
                && pane.effective_cols()
                    == restored_pane
                        .terminal_state
                        .as_ref()
                        .map(|t| u32::from(t.cols))
                        .unwrap_or_default();
            report.pane_reports.push(PaneTestReport {
                pane_id: pane.pane_id,
                source_artifact_hash: pane_info_hash(pane),
                observed_artifact_hash: restored_state_hash(restored_pane),
                checks: vec![
                    PaneContractCheck::exercised("persisted_pane_state", content_match),
                    PaneContractCheck::exercised(
                        "persisted_pane_identity_set",
                        pane_count_matches && pane_ids_match,
                    ),
                    PaneContractCheck::not_tested("process_continuity"),
                ],
            });
        }

        // ── Phase 5: manual layout restore, then confirm authority settles.
        let restore_start = Instant::now();
        let wezterm = Arc::new(MockWezterm::new());
        let summary = restorer
            .restore(&candidate, &checkpoint, wezterm.clone())
            .await
            .expect("restore must succeed against reconstituted DB");
        let restore_ok = summary.layout_settled_pane_count() == captured_pane_count
            && summary.layout_failed_pane_count() == 0
            && summary.session_id == captured_session_id;
        add_phase(
            &mut report,
            "restore_via_fresh_restorer",
            restore_start,
            if restore_ok { "ok" } else { "error" },
            json!({
                "layout_settled_pane_count": summary.layout_settled_pane_count(),
                "layout_failed_pane_count": summary.layout_failed_pane_count(),
                "summary_session_id": summary.session_id,
                "session_id_match": summary.session_id == captured_session_id,
            }),
        );

        let post_detect_start = Instant::now();
        let post_detect = restorer
            .detect()
            .expect("post-restore detect must not error");
        let detect_cleared = post_detect.is_none();
        add_phase(
            &mut report,
            "post_restore_detect_is_cleared",
            post_detect_start,
            if detect_cleared { "ok" } else { "error" },
            json!({ "detect_returned_none": detect_cleared }),
        );

        let success = session_id_matches
            && checkpoint_id_matches
            && pane_count_matches
            && pane_ids_match
            && cwds_match
            && restore_ok
            && detect_cleared
            && report.all_pane_contracts_exercised_and_pass();
        report.total_duration_ms = run_start.elapsed().as_millis() as u64;
        report.passed = success;
        report.failure_reason = if success {
            None
        } else {
            Some("persistence, manual layout restore, or authority settlement broke".to_string())
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
fn e2e_persisted_scrollback_bytes_survive_engine_rebuild_but_replay_is_rejected() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name:
                "e2e_persisted_scrollback_bytes_survive_engine_rebuild_but_replay_is_rejected"
                    .to_string(),
            phases: Vec::new(),
            total_duration_ms: 0,
            passed: false,
            failure_reason: None,
            pane_reports: Vec::new(),
        };

        let (_tmp, db_path) = setup_test_db();
        let pane = make_pane(
            7,
            0,
            0,
            24,
            80,
            "claude-code",
            "file:///tmp/scrollback-roundtrip",
        );

        // Known scrollback content — includes an ANSI color escape, a tab,
        // and a UTF-8 multi-byte character so we can detect byte-level loss.
        let segments: Vec<(i64, String, i64)> = vec![
            (0, "line 1 — plain ASCII\n".to_string(), 6_000),
            (
                1,
                "line 2 \t with \x1b[31mred\x1b[0m color\n".to_string(),
                6_100,
            ),
            (2, "line 3 🦀 emoji and résumé accents\n".to_string(), 6_200),
        ];
        let expected_concatenated: String = segments.iter().map(|(_, c, _)| c.clone()).collect();
        let expected_bytes_hash = hash_text(&expected_concatenated);

        let captured_session_id;
        let captured_checkpoint_id;
        {
            let save_start = Instant::now();
            let engine_a = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let conn = Connection::open(db_path.as_str()).expect("seed scrollback db");
            for (seq, content, ts) in &segments {
                insert_output_segment(&conn, pane.pane_id, *seq, content, *ts);
            }
            let snap = engine_a
                .capture_with_options(
                    std::slice::from_ref(&pane),
                    SnapshotTrigger::Startup,
                    SnapshotCaptureOptions {
                        include_scrollback: true,
                        metadata: None,
                    },
                )
                .await
                .expect("capture before handle rebuild");
            captured_session_id = snap.session_id.clone();
            captured_checkpoint_id = snap.checkpoint_id;
            add_phase(
                &mut report,
                "save_plus_scrollback_seed",
                save_start,
                "ok",
                json!({
                    "session_id": snap.session_id,
                    "checkpoint_id": snap.checkpoint_id,
                    "segments_seeded": segments.len(),
                    "total_bytes": expected_concatenated.len(),
                }),
            );
            // engine_a dropped here.
        }

        // ── rebuild handles: new engine + fail-closed restorer ──────
        let rebuild_start = Instant::now();
        let _engine_b = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let restorer = SessionRestorer::new(
            db_path.clone(),
            SessionRestoreConfig {
                restore_scrollback: true,
                ..SessionRestoreConfig::default()
            },
        );
        add_phase(
            &mut report,
            "rebuild_handles_with_fail_closed_scrollback_restorer",
            rebuild_start,
            "ok",
            json!({ "restore_scrollback": true }),
        );

        let candidate = restorer
            .detect()
            .expect("detect after handle rebuild")
            .expect("candidate must exist");
        let checkpoint = restorer
            .load_checkpoint(&candidate)
            .expect("load checkpoint");
        let wezterm = Arc::new(MockWezterm::new());

        // Verify the persisted source bytes and checkpoint boundary directly.
        // They remain durable across the engine rebuild, but are not treated as
        // authoritative logical lines or injected through PTY input.
        let persistence_start = Instant::now();
        let persisted_segments = {
            let conn = Connection::open(db_path.as_str()).expect("open rebuilt scrollback db");
            let mut stmt = conn
                .prepare(
                    "SELECT seq, content, content_len
                     FROM output_segments
                     WHERE pane_id = ?1 AND seq <= ?2
                     ORDER BY seq ASC",
                )
                .expect("prepare persisted scrollback query");
            stmt.query_map(
                params![
                    i64::try_from(pane.pane_id).expect("fixture pane id fits i64"),
                    2_i64
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .expect("query persisted scrollback")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode persisted scrollback")
        };
        let persisted_concatenated: String = persisted_segments
            .iter()
            .map(|(_, content, _)| content.as_str())
            .collect();
        let persisted_metadata_matches = persisted_segments.len() == segments.len()
            && persisted_segments.iter().zip(&segments).all(
                |((actual_seq, actual_content, actual_len), (seq, content, _))| {
                    *actual_seq == *seq
                        && actual_content == content
                        && usize::try_from(*actual_len).ok() == Some(content.len())
                },
            );
        let checkpoint_boundary_matches = checkpoint
            .pane_states
            .iter()
            .find(|state| state.pane_id == pane.pane_id)
            .is_some_and(|state| state.scrollback_checkpoint_seq == Some(2));
        let persisted_bytes_match = persisted_concatenated == expected_concatenated;
        add_phase(
            &mut report,
            "verify_persisted_scrollback_bytes",
            persistence_start,
            if persisted_metadata_matches && checkpoint_boundary_matches && persisted_bytes_match {
                "ok"
            } else {
                "error"
            },
            json!({
                "segments": persisted_segments.len(),
                "metadata_matches": persisted_metadata_matches,
                "checkpoint_boundary_matches": checkpoint_boundary_matches,
                "persisted_bytes_hash": hash_text(&persisted_concatenated),
            }),
        );

        // ── restore via the real SessionRestorer flow ────────────────────
        let restore_start = Instant::now();
        let error = restorer
            .restore(&candidate, &checkpoint, wezterm.clone())
            .await
            .expect_err("raw output fragments must not be replayed through PTY input");
        let safe_rejection = matches!(error, RestoreError::SafeScrollbackReplayUnavailable);
        add_phase(
            &mut report,
            "reject_unsafe_scrollback_replay",
            restore_start,
            if safe_rejection { "ok" } else { "error" },
            json!({
                "safe_scrollback_rejection": safe_rejection,
                "error_kind": if safe_rejection {
                    "safe_scrollback_replay_unavailable"
                } else {
                    "unexpected"
                },
            }),
        );

        // The capability preflight must reject before either mux mutation or
        // durable restore-authority mutation.
        let verify_start = Instant::now();
        let panes = wezterm
            .list_panes()
            .await
            .expect("list panes after scrollback safety rejection");
        let authority_rows: i64 = Connection::open(db_path.as_str())
            .expect("open restore authority verification database")
            .query_row(
                "SELECT COUNT(*)
                 FROM session_checkpoints
                 WHERE session_id = ?1
                   AND checkpoint_role IN ('restore_intent', 'restore_receipt')",
                [captured_session_id.as_str()],
                |row| row.get(0),
            )
            .expect("count restore authority rows");
        let redetected = restorer
            .detect()
            .expect("detect after rejected scrollback restore");
        let no_mux_effects = panes.is_empty();
        let no_authority_effects = authority_rows == 0;
        let remains_restore_candidate = redetected
            .as_ref()
            .is_some_and(|session| session.session_id == captured_session_id);
        report.pane_reports.push(PaneTestReport {
            pane_id: pane.pane_id,
            source_artifact_hash: expected_bytes_hash.clone(),
            observed_artifact_hash: hash_text(&persisted_concatenated),
            checks: vec![
                PaneContractCheck::exercised(
                    "persisted_scrollback_bytes",
                    persisted_metadata_matches && persisted_bytes_match,
                ),
                PaneContractCheck::exercised(
                    "restore_side_effects_absent",
                    no_mux_effects && no_authority_effects,
                ),
                PaneContractCheck::not_tested("process_continuity"),
            ],
        });
        let success = safe_rejection
            && persisted_metadata_matches
            && checkpoint_boundary_matches
            && persisted_bytes_match
            && candidate.session_id == captured_session_id
            && checkpoint.checkpoint_id == captured_checkpoint_id
            && no_mux_effects
            && no_authority_effects
            && remains_restore_candidate
            && report.all_pane_contracts_exercised_and_pass();
        add_phase(
            &mut report,
            "verify_persistence_and_no_restore_side_effects",
            verify_start,
            if success { "ok" } else { "error" },
            json!({
                "expected_bytes_hash": expected_bytes_hash,
                "persisted_len": persisted_concatenated.len(),
                "mux_pane_count": panes.len(),
                "restore_authority_rows": authority_rows,
                "remains_restore_candidate": remains_restore_candidate,
            }),
        );

        report.total_duration_ms = run_start.elapsed().as_millis() as u64;
        report.passed = success;
        report.failure_reason = if success {
            None
        } else {
            Some("scrollback persistence or fail-closed restore boundary was violated".to_string())
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
fn e2e_persisted_capture_topology_survives_engine_rebuild_and_maps_all_panes() {
    run_async_test(async {
        let run_start = Instant::now();
        let mut report = E2ETestReport {
            test_name: "e2e_persisted_capture_topology_survives_engine_rebuild_and_maps_all_panes"
                .to_string(),
            phases: Vec::new(),
            total_duration_ms: 0,
            passed: false,
            failure_reason: None,
            pane_reports: Vec::new(),
        };

        // Use the checked-in complex-layout fixture as a source of pane,
        // tab/window, and cwd identities. PaneInfo capture cannot observe the
        // fixture's nested split tree, so this test does not claim that tree.
        let parse_start = Instant::now();
        let complex_json = std::fs::read_to_string(fixture_path("snapshot_complex_layout.json"))
            .expect("read complex layout fixture");
        let fixture_topology =
            TopologySnapshot::from_json(&complex_json).expect("parse complex layout fixture");
        let fixture_panes = collect_fixture_panes(&fixture_topology);
        add_phase(
            &mut report,
            "load_fixture_topology",
            parse_start,
            "ok",
            json!({
                "pane_count": fixture_topology.pane_count(),
                "window_count": fixture_topology.windows.len(),
                "flat_pane_ids": fixture_panes.iter().map(|p| p.pane_id).collect::<Vec<_>>(),
            }),
        );

        let (_tmp, db_path) = setup_test_db();

        // Build PaneInfo inputs that mirror the fixture leaves. The engine
        // synthesizes a capture topology from these flat observations; the
        // persisted synthesized artifact is the authority tested below.
        let panes: Vec<PaneInfo> = fixture_panes
            .iter()
            .map(|fp| {
                make_pane(
                    fp.pane_id,
                    fp.tab_id,
                    fp.window_id,
                    24,
                    80,
                    &format!("pane-{}", fp.pane_id),
                    fp.cwd.as_deref().unwrap_or("file:///tmp/fixture-default"),
                )
            })
            .collect();

        let captured_session_id;
        let captured_checkpoint_id;
        let captured_topology_hash;
        {
            let save_start = Instant::now();
            let engine_a = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
            let snap = engine_a
                .capture(&panes, SnapshotTrigger::Startup)
                .await
                .expect("capture synthesized topology");
            captured_session_id = snap.session_id.clone();
            captured_checkpoint_id = snap.checkpoint_id;
            let conn = Connection::open(db_path.as_str()).expect("open db");
            let topology_json = conn
                .query_row(
                    "SELECT topology_json FROM mux_sessions WHERE session_id = ?1",
                    params![captured_session_id],
                    |row| row.get::<_, String>(0),
                )
                .expect("read captured topology_json");
            captured_topology_hash = hash_text(&topology_json);
            add_phase(
                &mut report,
                "save_via_engine_a",
                save_start,
                "ok",
                json!({
                    "session_id": snap.session_id,
                    "checkpoint_id": snap.checkpoint_id,
                    "pane_count": snap.pane_count,
                }),
            );
        }

        // ── rebuild process-local handles ───────────────────────────────
        let rebuild_start = Instant::now();
        let _engine_b = SnapshotEngine::new(db_path.clone(), SnapshotConfig::default());
        let restorer = SessionRestorer::new(db_path.clone(), SessionRestoreConfig::default());
        add_phase(
            &mut report,
            "rebuild_process_local_handles",
            rebuild_start,
            "ok",
            json!({}),
        );

        // ── load + verify the persisted capture artifact is parseable,
        // stable under its own codec, and retains the captured pane set.
        let verify_start = Instant::now();
        let reloaded_topology_json: String = Connection::open(db_path.as_str())
            .expect("reopen database after handle rebuild")
            .query_row(
                "SELECT topology_json FROM mux_sessions WHERE session_id = ?1",
                params![captured_session_id],
                |row| row.get(0),
            )
            .expect("reload persisted topology_json");
        let persisted_hash_match = hash_text(&reloaded_topology_json) == captured_topology_hash;
        let reparsed_topology = TopologySnapshot::from_json(&reloaded_topology_json)
            .expect("persisted topology_json must re-parse");
        let reparsed_panes = collect_fixture_panes(&reparsed_topology);
        let reparsed_pane_ids: HashSet<u64> = reparsed_panes.iter().map(|p| p.pane_id).collect();
        let original_pane_ids: HashSet<u64> = panes.iter().map(|p| p.pane_id).collect();
        let pane_set_match = reparsed_pane_ids == original_pane_ids;
        let pane_count_match = reparsed_topology.pane_count() == panes.len();
        let window_count_match = !reparsed_topology.windows.is_empty();
        let topology_self_roundtrip = TopologySnapshot::from_json(
            &reparsed_topology
                .to_json()
                .expect("serialize reparsed topology"),
        )
        .expect("reparse roundtripped topology")
            == reparsed_topology;

        add_phase(
            &mut report,
            "verify_persisted_capture_topology",
            verify_start,
            if persisted_hash_match
                && pane_set_match
                && pane_count_match
                && window_count_match
                && topology_self_roundtrip
            {
                "ok"
            } else {
                "error"
            },
            json!({
                "persisted_hash_match": persisted_hash_match,
                "pane_set_match": pane_set_match,
                "pane_count_match": pane_count_match,
                "window_count_match": window_count_match,
                "topology_self_roundtrip": topology_self_roundtrip,
                "reparsed_pane_ids": reparsed_pane_ids.iter().copied().collect::<Vec<_>>(),
            }),
        );

        // ── manual layout restore + confirm every source pane is mapped.
        let candidate = restorer
            .detect()
            .expect("detect after handle rebuild")
            .expect("candidate must exist");
        let checkpoint = restorer
            .load_checkpoint(&candidate)
            .expect("load checkpoint");
        let wezterm = Arc::new(MockWezterm::new());
        let restore_start = Instant::now();
        let summary = restorer
            .restore(&candidate, &checkpoint, wezterm.clone())
            .await
            .expect("restore persisted capture topology");
        let pane_id_map_complete = summary.layout_result.pane_id_map.len() == panes.len()
            && summary.layout_result.failed_panes.is_empty();
        add_phase(
            &mut report,
            "manual_layout_restore_maps_all_panes",
            restore_start,
            if pane_id_map_complete { "ok" } else { "error" },
            json!({
                "layout_settled_pane_count": summary.layout_settled_pane_count(),
                "layout_failed_pane_count": summary.layout_failed_pane_count(),
                "checkpoint_id_match": summary.checkpoint_id == captured_checkpoint_id,
            }),
        );

        let success = persisted_hash_match
            && pane_set_match
            && pane_count_match
            && window_count_match
            && topology_self_roundtrip
            && pane_id_map_complete
            && summary.session_id == captured_session_id
            && summary.checkpoint_id == captured_checkpoint_id;

        report.total_duration_ms = run_start.elapsed().as_millis() as u64;
        report.passed = success;
        report.failure_reason = if success {
            None
        } else {
            Some("persisted capture topology or manual pane mapping changed".to_string())
        };
        emit_report(&report);
        assert!(
            report.passed,
            "{}",
            serde_json::to_string_pretty(&report).expect("pretty report")
        );
    });
}
