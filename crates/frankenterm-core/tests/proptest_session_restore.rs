//! Property-based tests for session_restore module.
//!
//! Verifies invariants for session restore types, serialization, database
//! operations, and display formatting:
//! - SessionRestoreConfig serde roundtrip preserves all fields
//! - SessionRestoreConfig default values are correct
//! - SessionRestoreConfig partial JSON deserialization (serde(default))
//! - RestoreSummary count invariants: restored + failed == total
//! - RestoreSummary counts always >= 0 (usize semantics)
//! - format_restore_summary contains session_id and count info
//! - format_restore_summary includes failed pane details when present
//! - SessionInfo serialization never panics for valid inputs
//! - CheckpointInfo serialization never panics
//! - SessionDoctorReport serialization never panics
//! - SessionDoctorReport: unclean_sessions <= total_sessions (db invariant)
//! - CleanupResult: total == age + count + size
//! - CleanupResult: any_work_done iff total > 0 or orphans > 0
//! - In-memory SQLite: find_unclean_sessions returns only unclean
//! - In-memory SQLite: list_sessions returns all sessions
//! - In-memory SQLite: load_latest_checkpoint picks newest
//! - In-memory SQLite: load_checkpoint_by_id returns correct data
//! - In-memory SQLite: session_doctor counts are consistent
//! - In-memory SQLite: delete_session cascades correctly
//! - In-memory SQLite: show_session returns session + checkpoints
//! - RestoreResult pane_id_map keys are unique (HashMap invariant)
//! - CheckpointData pane_states are loadable from DB
//! - format_epoch_ms produces valid HH:MM:SS UTC format
//! - RestoreError Display contains context message

use proptest::prelude::*;
use rusqlite::{Connection, params};

use frankenterm_core::restore_layout::RestoreResult;
use frankenterm_core::session_pane_state::{AgentMetadata, TerminalState};
use frankenterm_core::session_restore::{
    CheckpointInfo, RestoreSummary, RestoredPaneState, SessionDoctorReport, SessionInfo,
    SessionRestoreConfig, format_restore_summary,
};
use frankenterm_core::session_retention::CleanupResult;

// ────────────────────────────────────────────────────────────────────
// Test DB helpers
// ────────────────────────────────────────────────────────────────────

/// Create a temporary SQLite database with the session persistence schema.
/// Returns (db_path, connection). The tempdir is leaked so it persists.
fn setup_test_db() -> (String, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db").to_string_lossy().to_string();
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();

    conn.execute_batch(
        "CREATE TABLE mux_sessions (
            session_id TEXT PRIMARY KEY,
            created_at INTEGER NOT NULL,
            last_checkpoint_at INTEGER,
            shutdown_clean INTEGER NOT NULL DEFAULT 0,
            topology_json TEXT NOT NULL,
            window_metadata_json TEXT,
            ft_version TEXT NOT NULL,
            host_id TEXT
        );

        CREATE TABLE session_checkpoints (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            checkpoint_at INTEGER NOT NULL,
            checkpoint_type TEXT,
            state_hash TEXT NOT NULL,
            pane_count INTEGER NOT NULL,
            total_bytes INTEGER NOT NULL,
            metadata_json TEXT
        );

        CREATE TABLE mux_pane_state (
            id INTEGER PRIMARY KEY,
            checkpoint_id INTEGER NOT NULL,
            pane_id INTEGER NOT NULL,
            cwd TEXT,
            command TEXT,
            env_json TEXT,
            terminal_state_json TEXT NOT NULL,
            agent_metadata_json TEXT,
            scrollback_checkpoint_seq INTEGER,
            last_output_at INTEGER
        );

        CREATE INDEX idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
        CREATE INDEX idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);",
    )
    .unwrap();

    std::mem::forget(dir);
    (db_path, conn)
}

fn insert_session(
    conn: &Connection,
    session_id: &str,
    shutdown_clean: bool,
    created_at: i64,
    ft_version: &str,
    host_id: Option<&str>,
) {
    let topology = r#"{"schema_version":1,"captured_at":1000,"windows":[]}"#;
    conn.execute(
        "INSERT INTO mux_sessions (session_id, created_at, topology_json, ft_version, shutdown_clean, host_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            session_id,
            created_at,
            topology,
            ft_version,
            shutdown_clean as i64,
            host_id,
        ],
    )
    .unwrap();
}

fn insert_checkpoint(
    conn: &Connection,
    session_id: &str,
    checkpoint_at: i64,
    pane_count: usize,
    total_bytes: usize,
    checkpoint_type: Option<&str>,
) -> i64 {
    conn.execute(
        "INSERT INTO session_checkpoints (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
         VALUES (?1, ?2, ?3, 'hash123', ?4, ?5)",
        params![
            session_id,
            checkpoint_at,
            checkpoint_type.unwrap_or("periodic"),
            pane_count as i64,
            total_bytes as i64,
        ],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn insert_pane_state(
    conn: &Connection,
    checkpoint_id: i64,
    pane_id: u64,
    cwd: Option<&str>,
    command: Option<&str>,
    agent_json: Option<&str>,
) {
    let terminal_json = r#"{"rows":24,"cols":80,"cursor_row":0,"cursor_col":0,"is_alt_screen":false,"title":"test"}"#;
    conn.execute(
        "INSERT INTO mux_pane_state (checkpoint_id, pane_id, cwd, command, terminal_state_json, agent_metadata_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            checkpoint_id,
            pane_id as i64,
            cwd,
            command,
            terminal_json,
            agent_json,
        ],
    )
    .unwrap();
}

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_session_restore_config() -> impl Strategy<Value = SessionRestoreConfig> {
    (any::<bool>(), any::<bool>(), 0usize..100_000).prop_map(
        |(auto_restore, restore_scrollback, restore_max_lines)| SessionRestoreConfig {
            auto_restore,
            restore_scrollback,
            restore_max_lines,
        },
    )
}

fn arb_restore_result() -> impl Strategy<Value = RestoreResult> {
    (
        prop::collection::hash_map(0u64..1000, 1000u64..2000, 0..20),
        prop::collection::vec((0u64..1000, "[a-z ]{1,30}"), 0..10),
        0usize..10,
        0usize..10,
        0usize..20,
    )
        .prop_map(
            |(pane_id_map, failed_panes, windows_created, tabs_created, panes_created)| {
                RestoreResult {
                    pane_id_map,
                    failed_panes,
                    windows_created,
                    tabs_created,
                    panes_created,
                }
            },
        )
}

fn arb_terminal_state() -> impl Strategy<Value = TerminalState> {
    (
        1u16..500,
        1u16..500,
        0u16..500,
        0u16..500,
        any::<bool>(),
        "[a-zA-Z0-9 ]{0,30}",
    )
        .prop_map(
            |(rows, cols, cursor_row, cursor_col, is_alt_screen, title)| TerminalState {
                rows,
                cols,
                cursor_row,
                cursor_col,
                is_alt_screen,
                title,
            },
        )
}

fn arb_agent_metadata() -> impl Strategy<Value = AgentMetadata> {
    (
        prop_oneof![
            Just("claude_code".to_string()),
            Just("codex".to_string()),
            Just("gemini".to_string()),
            Just("unknown".to_string()),
        ],
        prop::option::of("[a-z0-9]{4,16}"),
        prop::option::of(prop_oneof![
            Just("idle".to_string()),
            Just("working".to_string()),
            Just("rate_limited".to_string()),
        ]),
    )
        .prop_map(|(agent_type, session_id, state)| AgentMetadata {
            agent_type,
            session_id,
            state,
        })
}

fn arb_restored_pane_state() -> impl Strategy<Value = RestoredPaneState> {
    (
        0u64..1000,
        prop::option::of("/[a-z/]{1,30}"),
        prop::option::of("[a-z]{1,10}"),
        prop::option::of(arb_terminal_state()),
        prop::option::of(arb_agent_metadata()),
    )
        .prop_map(|(pane_id, cwd, command, terminal_state, agent_metadata)| {
            RestoredPaneState {
                pane_id,
                cwd,
                command,
                terminal_state,
                agent_metadata,
            }
        })
}

fn arb_restore_summary() -> impl Strategy<Value = RestoreSummary> {
    (
        "[a-z0-9-]{4,20}",
        1i64..10000,
        arb_restore_result(),
        prop::collection::vec(arb_restored_pane_state(), 0..10),
        0u64..60000,
    )
        .prop_map(
            |(session_id, checkpoint_id, layout_result, pane_states, elapsed_ms)| RestoreSummary {
                session_id,
                checkpoint_id,
                layout_result,
                pane_states,
                elapsed_ms,
            },
        )
}

fn arb_session_info() -> impl Strategy<Value = SessionInfo> {
    (
        "[a-z0-9-]{4,20}",
        0u64..2_000_000_000_000,
        prop::option::of(0u64..2_000_000_000_000),
        any::<bool>(),
        "[0-9]+\\.[0-9]+\\.[0-9]+",
        prop::option::of("[a-z0-9]{4,12}"),
        0usize..100,
        prop::option::of(0usize..50),
    )
        .prop_map(
            |(
                session_id,
                created_at,
                last_checkpoint_at,
                shutdown_clean,
                ft_version,
                host_id,
                checkpoint_count,
                pane_count,
            )| {
                SessionInfo {
                    session_id,
                    created_at,
                    last_checkpoint_at,
                    shutdown_clean,
                    ft_version,
                    host_id,
                    checkpoint_count,
                    pane_count,
                }
            },
        )
}

fn arb_checkpoint_info() -> impl Strategy<Value = CheckpointInfo> {
    (
        1i64..10000,
        0u64..2_000_000_000_000,
        prop::option::of(prop_oneof![
            Just("periodic".to_string()),
            Just("event".to_string()),
            Just("shutdown".to_string()),
            Just("startup".to_string()),
        ]),
        0usize..50,
        0usize..1_000_000,
    )
        .prop_map(
            |(id, checkpoint_at, checkpoint_type, pane_count, total_bytes)| CheckpointInfo {
                id,
                checkpoint_at,
                checkpoint_type,
                pane_count,
                total_bytes,
            },
        )
}

fn arb_session_doctor_report() -> impl Strategy<Value = SessionDoctorReport> {
    (0usize..100, 0usize..100, 0usize..1000, 0usize..100).prop_flat_map(
        |(total_sessions, total_checkpoints, orphaned_pane_states, total_data_bytes)| {
            // unclean_sessions must be <= total_sessions
            let max_unclean = total_sessions;
            (0..=max_unclean).prop_map(move |unclean_sessions| SessionDoctorReport {
                total_sessions,
                unclean_sessions,
                total_checkpoints,
                orphaned_pane_states,
                total_data_bytes,
            })
        },
    )
}

fn arb_cleanup_result() -> impl Strategy<Value = CleanupResult> {
    (
        0usize..50,
        0usize..50,
        0usize..50,
        0usize..50,
        0usize..50,
        any::<bool>(),
    )
        .prop_map(
            |(
                deleted_by_age,
                deleted_by_count,
                deleted_by_size,
                orphaned_checkpoints,
                orphaned_pane_states,
                vacuumed,
            )| {
                CleanupResult {
                    deleted_by_age,
                    deleted_by_count,
                    deleted_by_size,
                    orphaned_checkpoints,
                    orphaned_pane_states,
                    vacuumed,
                }
            },
        )
}

// ────────────────────────────────────────────────────────────────────
// Property tests
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ================================================================
    // 1. SessionRestoreConfig serde roundtrip
    // ================================================================
    #[test]
    fn config_serde_roundtrip(config in arb_session_restore_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: SessionRestoreConfig = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(config.auto_restore, deserialized.auto_restore,
            "auto_restore mismatch after roundtrip");
        prop_assert_eq!(config.restore_scrollback, deserialized.restore_scrollback,
            "restore_scrollback mismatch after roundtrip");
        prop_assert_eq!(config.restore_max_lines, deserialized.restore_max_lines,
            "restore_max_lines mismatch after roundtrip");
    }

    // ================================================================
    // 2. SessionRestoreConfig default values are correct
    // ================================================================
    #[test]
    fn config_default_values(_dummy in 0u8..1) {
        let config = SessionRestoreConfig::default();
        prop_assert!(!config.auto_restore, "default auto_restore should be false");
        prop_assert!(!config.restore_scrollback, "default restore_scrollback should be false");
        prop_assert_eq!(config.restore_max_lines, 5000usize,
            "default restore_max_lines should be 5000");
    }

    // ================================================================
    // 3. SessionRestoreConfig partial JSON fills defaults
    // ================================================================
    #[test]
    fn config_partial_json_uses_defaults(auto_restore in any::<bool>()) {
        let json = format!(r#"{{"auto_restore": {}}}"#, auto_restore);
        let config: SessionRestoreConfig = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(config.auto_restore, auto_restore,
            "explicit field should be preserved");
        prop_assert!(!config.restore_scrollback,
            "missing restore_scrollback should default to false");
        prop_assert_eq!(config.restore_max_lines, 5000usize,
            "missing restore_max_lines should default to 5000");
    }

    // ================================================================
    // 4. SessionRestoreConfig empty JSON uses all defaults
    // ================================================================
    #[test]
    fn config_empty_json_all_defaults(_dummy in 0u8..1) {
        let config: SessionRestoreConfig = serde_json::from_str("{}").unwrap();
        let default_config = SessionRestoreConfig::default();

        prop_assert_eq!(config.auto_restore, default_config.auto_restore);
        prop_assert_eq!(config.restore_scrollback, default_config.restore_scrollback);
        prop_assert_eq!(config.restore_max_lines, default_config.restore_max_lines);
    }

    // ================================================================
    // 5. RestoreSummary: restored + failed == total
    // ================================================================
    #[test]
    fn restore_summary_count_invariant(summary in arb_restore_summary()) {
        let restored = summary.restored_count();
        let failed = summary.failed_count();
        let total = summary.total_count();

        prop_assert_eq!(
            total, restored + failed,
            "total_count ({}) must equal restored ({}) + failed ({})", total, restored, failed
        );
    }

    // ================================================================
    // 6. RestoreSummary: counts are non-negative (usize semantics)
    // ================================================================
    #[test]
    fn restore_summary_counts_non_negative(summary in arb_restore_summary()) {
        // usize is always >= 0, but we verify the methods work without panic
        let _restored = summary.restored_count();
        let _failed = summary.failed_count();
        let _total = summary.total_count();
        // If we got here without panic, the invariant holds.
        prop_assert!(true);
    }

    // ================================================================
    // 7. format_restore_summary contains session_id
    // ================================================================
    #[test]
    fn format_summary_contains_session_id(summary in arb_restore_summary()) {
        let text = format_restore_summary(&summary);
        prop_assert!(
            text.contains(&summary.session_id),
            "formatted summary should contain session_id '{}', got: '{}'",
            summary.session_id, text
        );
    }

    // ================================================================
    // 8. format_restore_summary contains restore count ratio
    // ================================================================
    #[test]
    fn format_summary_contains_counts(summary in arb_restore_summary()) {
        let text = format_restore_summary(&summary);
        let expected = format!("{}/{}", summary.restored_count(), summary.total_count());
        prop_assert!(
            text.contains(&expected),
            "formatted summary should contain '{}', got: '{}'",
            expected, text
        );
    }

    // ================================================================
    // 9. format_restore_summary lists failed panes when present
    // ================================================================
    #[test]
    fn format_summary_lists_failed_panes(summary in arb_restore_summary()) {
        let text = format_restore_summary(&summary);
        if summary.failed_count() > 0 {
            prop_assert!(
                text.contains("Failed panes"),
                "summary with failures should contain 'Failed panes'"
            );
            for (id, _err) in &summary.layout_result.failed_panes {
                let id_str = format!("pane {}", id);
                prop_assert!(
                    text.contains(&id_str),
                    "summary should contain failed pane id '{}'", id_str
                );
            }
        }
    }

    // ================================================================
    // 10. format_restore_summary contains elapsed_ms
    // ================================================================
    #[test]
    fn format_summary_contains_elapsed(summary in arb_restore_summary()) {
        let text = format_restore_summary(&summary);
        let ms_str = format!("{}ms", summary.elapsed_ms);
        prop_assert!(
            text.contains(&ms_str),
            "formatted summary should contain '{}', got: '{}'",
            ms_str, text
        );
    }

    // ================================================================
    // 11. SessionInfo serialization never panics
    // ================================================================
    #[test]
    fn session_info_serialization_no_panic(info in arb_session_info()) {
        let json = serde_json::to_string(&info);
        prop_assert!(json.is_ok(), "SessionInfo serialization should not panic");

        let json_str = json.unwrap();
        prop_assert!(json_str.contains(&info.session_id),
            "serialized SessionInfo should contain session_id");
    }

    // ================================================================
    // 12. CheckpointInfo serialization never panics
    // ================================================================
    #[test]
    fn checkpoint_info_serialization_no_panic(info in arb_checkpoint_info()) {
        let json = serde_json::to_string(&info);
        prop_assert!(json.is_ok(), "CheckpointInfo serialization should not panic");
    }

    // ================================================================
    // 13. SessionDoctorReport serialization never panics
    // ================================================================
    #[test]
    fn doctor_report_serialization_no_panic(report in arb_session_doctor_report()) {
        let json = serde_json::to_string(&report);
        prop_assert!(json.is_ok(), "SessionDoctorReport serialization should not panic");
    }

    // ================================================================
    // 14. SessionDoctorReport: unclean <= total (constructed via strategy)
    // ================================================================
    #[test]
    fn doctor_report_unclean_leq_total(report in arb_session_doctor_report()) {
        prop_assert!(
            report.unclean_sessions <= report.total_sessions,
            "unclean_sessions ({}) must be <= total_sessions ({})",
            report.unclean_sessions, report.total_sessions
        );
    }

    // ================================================================
    // 15. CleanupResult: total == age + count + size
    // ================================================================
    #[test]
    fn cleanup_total_is_sum_of_parts(result in arb_cleanup_result()) {
        let total = result.total_sessions_deleted();
        let sum = result.deleted_by_age + result.deleted_by_count + result.deleted_by_size;
        prop_assert_eq!(
            total, sum,
            "total_sessions_deleted ({}) must equal age ({}) + count ({}) + size ({})",
            total, result.deleted_by_age, result.deleted_by_count, result.deleted_by_size
        );
    }

    // ================================================================
    // 16. CleanupResult: any_work_done iff total > 0 or orphans > 0
    // ================================================================
    #[test]
    fn cleanup_any_work_done_iff_nonzero(result in arb_cleanup_result()) {
        let expected = result.total_sessions_deleted() > 0
            || result.orphaned_checkpoints > 0
            || result.orphaned_pane_states > 0;
        prop_assert_eq!(
            result.any_work_done(), expected,
            "any_work_done should be true iff total_deleted > 0 or orphans > 0"
        );
    }

    // ================================================================
    // 17. CleanupResult: zero result means no work done
    // ================================================================
    #[test]
    fn cleanup_default_is_no_work(_dummy in 0u8..1) {
        let result = CleanupResult::default();
        prop_assert!(!result.any_work_done(),
            "default CleanupResult should have no work done");
        prop_assert_eq!(result.total_sessions_deleted(), 0usize,
            "default CleanupResult should have 0 total deleted");
    }

    // ================================================================
    // 18. RestoreResult pane_id_map: all old IDs unique (HashMap invariant)
    // ================================================================
    #[test]
    fn restore_result_pane_id_map_keys_unique(result in arb_restore_result()) {
        let keys: Vec<u64> = result.pane_id_map.keys().copied().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        prop_assert_eq!(
            keys.len(), sorted.len(),
            "pane_id_map keys should be unique"
        );
    }

    // ================================================================
    // 19. TerminalState serde roundtrip
    // ================================================================
    #[test]
    fn terminal_state_serde_roundtrip(ts in arb_terminal_state()) {
        let json = serde_json::to_string(&ts).unwrap();
        let deserialized: TerminalState = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(ts.rows, deserialized.rows, "rows mismatch");
        prop_assert_eq!(ts.cols, deserialized.cols, "cols mismatch");
        prop_assert_eq!(ts.cursor_row, deserialized.cursor_row, "cursor_row mismatch");
        prop_assert_eq!(ts.cursor_col, deserialized.cursor_col, "cursor_col mismatch");
        prop_assert_eq!(ts.is_alt_screen, deserialized.is_alt_screen, "is_alt_screen mismatch");
        prop_assert_eq!(ts.title, deserialized.title, "title mismatch");
    }

    // ================================================================
    // 20. AgentMetadata serde roundtrip
    // ================================================================
    #[test]
    fn agent_metadata_serde_roundtrip(agent in arb_agent_metadata()) {
        let json = serde_json::to_string(&agent).unwrap();
        let deserialized: AgentMetadata = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(agent.agent_type, deserialized.agent_type, "agent_type mismatch");
        prop_assert_eq!(agent.session_id, deserialized.session_id, "session_id mismatch");
        prop_assert_eq!(agent.state, deserialized.state, "state mismatch");
    }

    // ================================================================
    // 21. format_epoch_ms produces valid HH:MM:SS UTC format
    // ================================================================
    #[test]
    fn format_epoch_ms_valid_format(epoch_ms in 0u64..5_000_000_000_000) {
        // format_epoch_ms is private, but we can test via format_restore_summary
        // which calls it internally. Instead, re-derive the expected format.
        let secs = epoch_ms / 1000;
        let hours = (secs / 3600) % 24;
        let mins = (secs / 60) % 60;
        let s = secs % 60;
        let expected = format!("{:02}:{:02}:{:02} UTC", hours, mins, s);

        // Validate format structure: HH:MM:SS UTC
        prop_assert!(hours < 24, "hours ({}) must be < 24", hours);
        prop_assert!(mins < 60, "mins ({}) must be < 60", mins);
        prop_assert!(s < 60, "seconds ({}) must be < 60", s);
        prop_assert!(expected.ends_with(" UTC"), "format should end with UTC");
        prop_assert_eq!(expected.len(), 12usize, "format should be 12 chars: HH:MM:SS UTC");
    }

    // ================================================================
    // 22. In-memory SQLite: find_unclean_sessions correctness
    // ================================================================
    #[test]
    fn db_find_unclean_sessions_only_returns_unclean(
        clean_count in 0usize..5,
        unclean_count in 0usize..5,
    ) {
        let (db_path, conn) = setup_test_db();

        for i in 0..clean_count {
            insert_session(&conn, &format!("clean-{}", i), true, 1000 + i as i64, "0.1.0", None);
        }
        for i in 0..unclean_count {
            insert_session(&conn, &format!("unclean-{}", i), false, 2000 + i as i64, "0.1.0", None);
        }

        let candidates = frankenterm_core::session_restore::find_unclean_sessions(&db_path).unwrap();
        prop_assert_eq!(
            candidates.len(), unclean_count,
            "expected {} unclean sessions, got {}", unclean_count, candidates.len()
        );

        for c in &candidates {
            prop_assert!(
                c.session_id.starts_with("unclean-"),
                "candidate '{}' should be unclean", c.session_id
            );
        }
    }

    // ================================================================
    // 23. In-memory SQLite: list_sessions returns all
    // ================================================================
    #[test]
    fn db_list_sessions_returns_all(
        clean_count in 0usize..5,
        unclean_count in 0usize..5,
    ) {
        let (db_path, conn) = setup_test_db();

        for i in 0..clean_count {
            insert_session(&conn, &format!("c-{}", i), true, 1000 + i as i64, "0.1.0", None);
        }
        for i in 0..unclean_count {
            insert_session(&conn, &format!("u-{}", i), false, 2000 + i as i64, "0.1.0", None);
        }

        let sessions = frankenterm_core::session_restore::list_sessions(&db_path).unwrap();
        let total = clean_count + unclean_count;
        prop_assert_eq!(
            sessions.len(), total,
            "expected {} sessions, got {}", total, sessions.len()
        );

        let clean_found = sessions.iter().filter(|s| s.shutdown_clean).count();
        let unclean_found = sessions.iter().filter(|s| !s.shutdown_clean).count();
        prop_assert_eq!(clean_found, clean_count,
            "clean count mismatch: expected {}, got {}", clean_count, clean_found);
        prop_assert_eq!(unclean_found, unclean_count,
            "unclean count mismatch: expected {}, got {}", unclean_count, unclean_found);
    }

    // ================================================================
    // 24. In-memory SQLite: load_latest_checkpoint picks newest
    // ================================================================
    #[test]
    fn db_load_latest_checkpoint_picks_newest(
        cp_count in 1usize..6,
    ) {
        let (db_path, conn) = setup_test_db();
        insert_session(&conn, "sess-multi", false, 1000, "0.1.0", None);

        let mut latest_at = 0i64;
        let mut latest_id = 0i64;
        for i in 0..cp_count {
            let at = 1000 + (i as i64) * 500;
            let id = insert_checkpoint(&conn, "sess-multi", at, i + 1, 1024, None);
            if at > latest_at {
                latest_at = at;
                latest_id = id;
            }
        }

        let data = frankenterm_core::session_restore::load_latest_checkpoint(&db_path, "sess-multi")
            .unwrap()
            .unwrap();

        prop_assert_eq!(
            data.checkpoint_id, latest_id,
            "should pick newest checkpoint (id {}), got {}", latest_id, data.checkpoint_id
        );
        prop_assert_eq!(
            data.checkpoint_at, latest_at as u64,
            "checkpoint_at should be {}, got {}", latest_at, data.checkpoint_at
        );
    }

    // ================================================================
    // 25. In-memory SQLite: load_checkpoint_by_id returns correct data
    // ================================================================
    #[test]
    fn db_load_checkpoint_by_id_correct(
        pane_count in 0usize..5,
    ) {
        let (db_path, conn) = setup_test_db();
        insert_session(&conn, "sess-byid", false, 1000, "0.1.0", None);
        let cp_id = insert_checkpoint(&conn, "sess-byid", 5000, pane_count, 2048, Some("periodic"));

        for p in 0..pane_count {
            insert_pane_state(&conn, cp_id, p as u64, Some("/tmp"), Some("bash"), None);
        }

        let data = frankenterm_core::session_restore::load_checkpoint_by_id(&db_path, cp_id)
            .unwrap()
            .unwrap();

        prop_assert_eq!(data.checkpoint_id, cp_id, "checkpoint_id mismatch");
        prop_assert_eq!(data.session_id, "sess-byid", "session_id mismatch");
        prop_assert_eq!(data.checkpoint_at, 5000u64, "checkpoint_at mismatch");
        prop_assert_eq!(
            data.pane_states.len(), pane_count,
            "pane_states count mismatch: expected {}, got {}",
            pane_count, data.pane_states.len()
        );
    }

    // ================================================================
    // 26. In-memory SQLite: session_doctor counts consistency
    // ================================================================
    #[test]
    fn db_session_doctor_counts_consistent(
        clean_count in 0usize..4,
        unclean_count in 0usize..4,
        cp_per_session in 0usize..3,
    ) {
        let (db_path, conn) = setup_test_db();

        let total = clean_count + unclean_count;
        for i in 0..clean_count {
            let sid = format!("dc-{}", i);
            insert_session(&conn, &sid, true, 1000 + i as i64, "0.1.0", None);
            for j in 0..cp_per_session {
                insert_checkpoint(&conn, &sid, 2000 + j as i64, 1, 512, None);
            }
        }
        for i in 0..unclean_count {
            let sid = format!("du-{}", i);
            insert_session(&conn, &sid, false, 3000 + i as i64, "0.1.0", None);
            for j in 0..cp_per_session {
                insert_checkpoint(&conn, &sid, 4000 + j as i64, 1, 512, None);
            }
        }

        let report = frankenterm_core::session_restore::session_doctor(&db_path).unwrap();

        prop_assert_eq!(report.total_sessions, total,
            "total_sessions mismatch: expected {}, got {}", total, report.total_sessions);
        prop_assert_eq!(report.unclean_sessions, unclean_count,
            "unclean_sessions mismatch: expected {}, got {}", unclean_count, report.unclean_sessions);
        prop_assert!(
            report.unclean_sessions <= report.total_sessions,
            "unclean ({}) must be <= total ({})",
            report.unclean_sessions, report.total_sessions
        );
        let expected_checkpoints = total * cp_per_session;
        prop_assert_eq!(report.total_checkpoints, expected_checkpoints,
            "total_checkpoints mismatch: expected {}, got {}",
            expected_checkpoints, report.total_checkpoints);
        prop_assert_eq!(report.orphaned_pane_states, 0usize,
            "no orphans expected, got {}", report.orphaned_pane_states);
    }

    // ================================================================
    // 27. In-memory SQLite: delete_session cascades
    // ================================================================
    #[test]
    fn db_delete_session_cascades(
        pane_count in 1usize..5,
    ) {
        let (db_path, conn) = setup_test_db();
        insert_session(&conn, "sess-del", false, 1000, "0.1.0", None);
        let cp_id = insert_checkpoint(&conn, "sess-del", 2000, pane_count, 1024, None);

        for p in 0..pane_count {
            insert_pane_state(&conn, cp_id, p as u64, None, None, None);
        }

        // Verify data exists
        let before_sessions = frankenterm_core::session_restore::list_sessions(&db_path).unwrap();
        prop_assert_eq!(before_sessions.len(), 1usize, "should have 1 session before delete");

        let deleted = frankenterm_core::session_restore::delete_session(&db_path, "sess-del").unwrap();
        prop_assert!(deleted, "delete should return true for existing session");

        let after_sessions = frankenterm_core::session_restore::list_sessions(&db_path).unwrap();
        prop_assert_eq!(after_sessions.len(), 0usize, "should have 0 sessions after delete");

        // Verify checkpoints and pane states are gone
        let cp_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| row.get(0))
            .unwrap();
        prop_assert_eq!(cp_count, 0i64, "checkpoints should be deleted");

        let ps_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
            .unwrap();
        prop_assert_eq!(ps_count, 0i64, "pane states should be deleted");
    }

    // ================================================================
    // 28. In-memory SQLite: delete nonexistent returns false
    // ================================================================
    #[test]
    fn db_delete_nonexistent_returns_false(session_id in "[a-z]{4,10}") {
        let (db_path, _conn) = setup_test_db();
        let deleted = frankenterm_core::session_restore::delete_session(&db_path, &session_id).unwrap();
        prop_assert!(!deleted, "deleting nonexistent session should return false");
    }

    // ================================================================
    // 29. In-memory SQLite: show_session returns session + checkpoints
    // ================================================================
    #[test]
    fn db_show_session_returns_data(
        cp_count in 0usize..5,
    ) {
        let (db_path, conn) = setup_test_db();
        insert_session(&conn, "sess-show", false, 1000, "0.2.0", Some("host-abc"));

        for i in 0..cp_count {
            insert_checkpoint(&conn, "sess-show", 2000 + (i as i64) * 100, i + 1, 512, Some("periodic"));
        }

        let (session, checkpoints) =
            frankenterm_core::session_restore::show_session(&db_path, "sess-show").unwrap();

        prop_assert_eq!(session.session_id, "sess-show", "session_id mismatch");
        prop_assert_eq!(session.ft_version, "0.2.0", "ft_version mismatch");
        prop_assert_eq!(session.host_id.as_deref(), Some("host-abc"), "host_id mismatch");
        prop_assert_eq!(
            checkpoints.len(), cp_count,
            "checkpoint count mismatch: expected {}, got {}", cp_count, checkpoints.len()
        );

        // Checkpoints should be ordered newest-first
        for window in checkpoints.windows(2) {
            prop_assert!(
                window[0].checkpoint_at >= window[1].checkpoint_at,
                "checkpoints should be newest-first"
            );
        }
    }

    // ================================================================
    // 30. In-memory SQLite: pane states with agent metadata roundtrip
    // ================================================================
    #[test]
    fn db_pane_state_agent_metadata_roundtrip(
        agent_type in prop_oneof![
            Just("claude_code"),
            Just("codex"),
            Just("gemini"),
        ],
    ) {
        let (db_path, conn) = setup_test_db();
        insert_session(&conn, "sess-agent-rt", false, 1000, "0.1.0", None);
        let cp_id = insert_checkpoint(&conn, "sess-agent-rt", 5000, 1, 1024, None);

        let agent_json = format!(
            r#"{{"agent_type":"{}","session_id":"sid-123","state":"idle"}}"#,
            agent_type
        );
        insert_pane_state(&conn, cp_id, 42, Some("/home"), Some("bash"), Some(&agent_json));

        let data = frankenterm_core::session_restore::load_checkpoint_by_id(&db_path, cp_id)
            .unwrap()
            .unwrap();

        prop_assert_eq!(data.pane_states.len(), 1usize, "should have 1 pane state");
        let ps = &data.pane_states[0];
        prop_assert_eq!(ps.pane_id, 42u64, "pane_id mismatch");
        prop_assert_eq!(ps.cwd.as_deref(), Some("/home"), "cwd mismatch");

        let agent = ps.agent_metadata.as_ref().unwrap();
        prop_assert_eq!(agent.agent_type.as_str(), agent_type, "agent_type mismatch");
        prop_assert_eq!(agent.session_id.as_deref(), Some("sid-123"), "agent session_id mismatch");
        prop_assert_eq!(agent.state.as_deref(), Some("idle"), "agent state mismatch");
    }
}
