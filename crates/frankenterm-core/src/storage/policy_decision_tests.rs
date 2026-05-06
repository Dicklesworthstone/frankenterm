//! ft-u6fba Phase 1b: extracted from storage.rs (mod tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to
//! `crate::storage::*`.
//!
//! Renamed from `mod tests` (the original 3,158-line block) to
//! `policy_decision_tests` because `tests` is conventionally reserved for the
//! cargo test-binary entry name.

use super::*;
use rusqlite::Connection;

fn record_audit_action_for_conn(conn: &mut Connection, action: &AuditActionRecord) -> Result<i64> {
    with_writer_backend(conn, |backend| record_audit_action_backend(backend, action))
}

fn typed_decision_context_json(
    action: crate::policy::ActionKind,
    actor: crate::policy::ActorKind,
    surface: crate::policy::PolicySurface,
) -> String {
    let mut context = crate::policy::DecisionContext::empty();
    context.action = action;
    context.actor = actor;
    context.surface = surface;
    context.set_determining_rule("policy.allow");
    context.add_evidence("surface", surface.as_str());
    serde_json::to_string(&context).unwrap()
}

fn typed_sensitive_decision_context_json(
    action: crate::policy::ActionKind,
    actor: crate::policy::ActorKind,
    surface: crate::policy::PolicySurface,
    secret: &str,
) -> String {
    let mut context = crate::policy::DecisionContext::empty();
    context.action = action;
    context.actor = actor;
    context.surface = surface;
    context.text_summary = Some(format!("token {secret}"));
    context.set_determining_rule("policy.allow");
    context.add_evidence("token", secret);
    serde_json::to_string(&context).unwrap()
}

// =========================================================================
// Schema Initialization Tests
// =========================================================================

/// ft-k542h: a fault between v0-init steps must roll back the entire
/// triple atomically. The fault-injection setter forces a synthetic
/// failure after `repair_existing_v0_tables_before_schema_sql`; we then
/// assert that user_version is still 0, no migration rows were
/// recorded, and a subsequent (un-faulted) `initialize_schema` call
/// completes the migration cleanly. This proves the BEGIN IMMEDIATE /
/// ROLLBACK wrapper around the v0 init path actually rolls back.
#[test]
fn v0_init_fault_after_repair_rolls_back_atomically() {
    // Build a minimal v0 database: a stale `audit_actions` table that
    // is missing the `correlation_id` column added in migration v12.
    // This is the exact shape `repair_existing_v0_tables_before_schema_sql`
    // would mutate via ALTER TABLE.
    let conn = Connection::open_in_memory().unwrap();
    // Bootstrap the canonical schema, then regress to the legacy v0
    // shape: drop `audit_actions.correlation_id` (added in migration v12)
    // and reset `user_version` to 0. This simulates an existing v0 DB
    // whose `audit_actions` table predates the column —
    // `repair_existing_v0_tables_before_schema_sql` will want to
    // re-add it via `ALTER TABLE`, which is exactly the change we're
    // asserting rolls back atomically on fault.
    let (preamble, body) = split_schema_sql_pragmas();
    if !preamble.trim().is_empty() {
        conn.execute_batch(&preamble).unwrap();
    }
    conn.execute_batch(&body).unwrap();
    // Drop the index that references correlation_id before dropping the
    // column itself; SQLite refuses DROP COLUMN while a dependent index
    // exists. The repair path will recreate both.
    conn.execute_batch("DROP INDEX IF EXISTS idx_audit_actions_correlation")
        .unwrap();
    conn.execute_batch("ALTER TABLE audit_actions DROP COLUMN correlation_id")
        .unwrap();
    conn.execute_batch("PRAGMA user_version = 0").unwrap();

    // Sanity check: we are starting on an existing v0 DB without the column.
    assert_eq!(get_user_version(&conn).unwrap(), 0);
    assert!(table_exists(&conn, "audit_actions").unwrap());
    assert!(!table_has_column(&conn, "audit_actions", "correlation_id").unwrap());
    assert!(!needs_initialization(&conn).unwrap());

    // Inject a fault that fires immediately after the repair step.
    set_v0_init_fault_for_test(Some(V0InitStep::AfterRepair));

    let err = initialize_schema(&conn).expect_err("fault must propagate");
    assert!(
        err.to_string().contains("ft-k542h fault injection"),
        "expected fault-injection error, got: {err}"
    );

    // Atomicity invariants: the outer transaction rolled back, so:
    // - user_version is still 0 (no migration committed)
    // - the column the repair would have added is NOT present
    // - schema_migrations table either does not exist or holds no rows
    // - the connection is back in autocommit mode (no dangling BEGIN)
    assert_eq!(
        get_user_version(&conn).unwrap(),
        0,
        "user_version must be unchanged after rollback"
    );
    assert!(
        !table_has_column(&conn, "audit_actions", "correlation_id").unwrap(),
        "repair ALTER must have rolled back"
    );
    assert!(
        conn.is_autocommit(),
        "transaction must be closed after rollback (otherwise BEGIN leaked)"
    );

    // Second call (fault cleared by the helper after firing) must
    // succeed and leave the DB in a fully-migrated state. This proves
    // the partial state from the first attempt did not poison the DB.
    initialize_schema(&conn).expect("re-init after rollback must succeed");
    assert_eq!(get_user_version(&conn).unwrap(), SCHEMA_VERSION);
    assert!(
        table_has_column(&conn, "audit_actions", "correlation_id").unwrap(),
        "correlation_id must be present after successful re-init"
    );
}

#[test]
fn schema_initializes_on_fresh_db() {
    let conn = Connection::open_in_memory().unwrap();

    // Should need initialization
    assert!(needs_initialization(&conn).unwrap());

    // Initialize
    initialize_schema(&conn).unwrap();

    // Should not need initialization anymore
    assert!(!needs_initialization(&conn).unwrap());

    // Version should be recorded
    let version = get_schema_version(&conn).unwrap();
    assert_eq!(version, Some(SCHEMA_VERSION));
}

#[test]
fn schema_is_idempotent() {
    let conn = Connection::open_in_memory().unwrap();

    // Initialize twice
    initialize_schema(&conn).unwrap();
    initialize_schema(&conn).unwrap();

    // Should still be valid
    let version = get_schema_version(&conn).unwrap();
    assert_eq!(version, Some(SCHEMA_VERSION));
}

#[test]
fn all_tables_exist_after_init() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let expected_tables = [
        "schema_version",
        "panes",
        "output_segments",
        "segment_embeddings",
        "output_gaps",
        "events",
        "event_labels",
        "event_notes",
        "workflow_executions",
        "workflow_step_logs",
        "audit_actions",
        "action_undo",
        "approval_tokens",
        "config",
        "saved_searches",
        "maintenance_log",
        "pane_bookmarks",
    ];

    for table in &expected_tables {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "Table {table} should exist");
    }
}

#[test]
fn segment_embeddings_schema_is_canonical_after_init() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    assert!(segment_embeddings_table_is_canonical(&conn).unwrap());

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", 1_700_000_000_000i64, 1_700_000_000_000i64, 1],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO output_segments (id, pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![1i64, 1i64, 0i64, "segment", 7i64, 1_700_000_000_000i64],
        )
        .unwrap();

    conn.execute(
            "INSERT INTO segment_embeddings (segment_id, embedder_id, dimension, vector, embedded_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "hash", 8i64, vec![1u8, 2u8], 1_700_000_000i64],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO segment_embeddings (segment_id, embedder_id, dimension, vector, embedded_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "quality", 8i64, vec![3u8, 4u8], 1_700_000_001i64],
        )
        .unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM segment_embeddings WHERE segment_id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
}

// =========================================================================
// Migration Plan Tests
// =========================================================================

#[test]
fn migration_plan_empty_when_at_target() {
    let plan = build_migration_plan(SCHEMA_VERSION, SCHEMA_VERSION).unwrap();
    assert!(plan.steps.is_empty());
    assert_eq!(plan.from_version, SCHEMA_VERSION);
    assert_eq!(plan.to_version, SCHEMA_VERSION);
}

#[test]
fn migration_roundtrip_down_then_up() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let downgrade_target = 3;
    let down_plan = build_migration_plan(SCHEMA_VERSION, downgrade_target).unwrap();
    apply_migration_plan(&conn, &down_plan).unwrap();
    assert_eq!(get_user_version(&conn).unwrap(), downgrade_target);

    let up_plan = build_migration_plan(downgrade_target, SCHEMA_VERSION).unwrap();
    apply_migration_plan(&conn, &up_plan).unwrap();
    assert_eq!(get_user_version(&conn).unwrap(), SCHEMA_VERSION);
}

#[test]
fn migration_v18_preserves_existing_events() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Downgrade just the newest migration (v18 -> v17).
    let down_plan = build_migration_plan(SCHEMA_VERSION, 17).unwrap();
    apply_migration_plan(&conn, &down_plan).unwrap();
    assert_eq!(get_user_version(&conn).unwrap(), 17);

    let now_ms = 1_700_000_000_000i64;

    // Insert pane + event using the pre-v18 schema (no triage columns/tables).
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

    conn.execute(
            "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity, confidence, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![1i64, "codex.usage_limit", "codex", "usage", "warning", 0.95, now_ms],
        )
        .unwrap();

    let count_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count_before, 1);

    // Upgrade back to current schema and verify event row is preserved.
    let up_plan = build_migration_plan(17, SCHEMA_VERSION).unwrap();
    apply_migration_plan(&conn, &up_plan).unwrap();
    assert_eq!(get_user_version(&conn).unwrap(), SCHEMA_VERSION);

    let count_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count_after, 1);

    // New columns/tables should exist after upgrade.
    let triage_state: Option<String> = conn
        .query_row("SELECT triage_state FROM events WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(triage_state.is_none());

    let labels_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='event_labels'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(labels_table, 1);
}

#[test]
fn fts_table_exists_after_init() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='output_segments_fts'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "FTS5 table should exist");
}

#[test]
fn action_history_view_exists_after_init() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name='action_history'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "action_history view should exist");
}

#[test]
fn wal_mode_is_enabled() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    // In-memory databases use "memory" mode, but WAL works on file-based DBs
    assert!(mode == "memory" || mode == "wal");
}

// =========================================================================
// WAL Recovery Tests (wa-o8j)
// =========================================================================

#[test]
fn wal_recovery_passes_on_fresh_in_memory_db() {
    let conn = Connection::open_in_memory().unwrap();
    // Should pass without error on a fresh database
    // Note: in-memory DBs don't have WAL files, but the function should handle this
    check_and_recover_wal(&conn, ":memory:").unwrap();
}

#[test]
fn wal_recovery_passes_integrity_check() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    // After schema init, integrity check should still pass
    check_and_recover_wal(&conn, ":memory:").unwrap();
}

#[test]
fn wal_recovery_with_file_db() {
    use std::fs;
    let temp_dir = std::env::temp_dir();
    let db_path = temp_dir.join(format!("wa_test_wal_recovery_{}.db", std::process::id()));
    let db_path_str = db_path.to_string_lossy().to_string();

    // Create and populate a database
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        initialize_schema(&conn).unwrap();
        // Insert some data to ensure WAL activity
        conn.execute(
                "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at) VALUES (1, 'local', 0, 0)",
                [],
            ).unwrap();
    }

    // Re-open and run recovery
    {
        let conn = Connection::open(&db_path).unwrap();
        check_and_recover_wal(&conn, &db_path_str).unwrap();
        // Verify data is intact
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM panes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    // Cleanup
    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(format!("{db_path_str}-wal"));
    let _ = fs::remove_file(format!("{db_path_str}-shm"));
}

// =========================================================================
// Migration System Tests
// =========================================================================

#[test]
fn user_version_set_on_fresh_db() {
    let conn = Connection::open_in_memory().unwrap();

    // Fresh DB should have user_version = 0
    let initial = get_user_version(&conn).unwrap();
    assert_eq!(initial, 0);

    // After init, should match SCHEMA_VERSION
    initialize_schema(&conn).unwrap();
    let after = get_user_version(&conn).unwrap();
    assert_eq!(after, SCHEMA_VERSION);
}

#[test]
fn user_version_and_schema_version_match() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let user_ver = get_user_version(&conn).unwrap();
    let schema_ver = get_schema_version(&conn).unwrap().unwrap();

    assert_eq!(user_ver, schema_ver);
    assert_eq!(user_ver, SCHEMA_VERSION);
}

#[test]
fn schema_version_audit_trail_recorded() {
    // ft-7tq4z: prior to the fix, fresh DBs took an early-return
    // branch that ran SCHEMA_SQL and recorded a single audit row
    // ("Initial schema", SCHEMA_VERSION). That branch was the
    // root cause of v24's policy_denied_audit table going missing
    // on fresh DBs. The fix routes fresh DBs through
    // run_v0_init_in_transaction → run_migrations(0) so EVERY
    // migration's `up_sql` runs. As a side effect the audit trail
    // now has one row per applied migration. This test pins the
    // new invariant: at least every migration version (>= 1) is
    // recorded with a non-null applied_at, the highest recorded
    // version is SCHEMA_VERSION, and the table is never empty.
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let mut stmt = conn
        .prepare("SELECT version, applied_at, description FROM schema_version")
        .unwrap();
    let rows: Vec<(i32, i64, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i32>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert!(
        !rows.is_empty(),
        "schema_version audit trail must not be empty"
    );
    for (version, applied_at, description) in &rows {
        assert!(
            *version >= 1 && *version <= SCHEMA_VERSION,
            "audit row version {version} out of range 1..={SCHEMA_VERSION}"
        );
        assert!(*applied_at > 0, "applied_at must be set for v{version}");
        assert!(
            !description.trim().is_empty(),
            "description must be set for v{version}"
        );
    }
    let max_recorded = rows.iter().map(|(v, _, _)| *v).max().unwrap();
    assert_eq!(
        max_recorded, SCHEMA_VERSION,
        "highest recorded version must equal SCHEMA_VERSION"
    );
}

#[test]
fn ft_meta_initialized_on_fresh_db() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let (schema_version, min_compatible, created_by, created_at): (i32, String, String, i64) = conn
        .query_row(
            "SELECT schema_version, min_compatible_ft, created_by_ft, created_at \
                 FROM ft_meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();

    assert_eq!(schema_version, SCHEMA_VERSION);
    assert_eq!(min_compatible, crate::VERSION);
    assert_eq!(created_by, crate::VERSION);
    assert!(created_at > 0, "created_at should be set");
}

#[test]
fn ft_version_parse_accepts_semver_core_with_optional_suffix() {
    assert_eq!(
        FtVersion::parse("1.2.3"),
        Some(FtVersion {
            major: 1,
            minor: 2,
            patch: 3
        })
    );
    assert_eq!(
        FtVersion::parse("1.2.3-alpha.1+build.7"),
        Some(FtVersion {
            major: 1,
            minor: 2,
            patch: 3
        })
    );
    assert_eq!(
        FtVersion::parse("4"),
        Some(FtVersion {
            major: 4,
            minor: 0,
            patch: 0
        })
    );
}

#[test]
fn ft_version_parse_rejects_extra_numeric_components() {
    assert!(FtVersion::parse("1.2.3.4").is_none());
    assert!(FtVersion::parse("1.2.3.4-alpha").is_none());
}

#[test]
fn ft_too_old_rejected_by_meta() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    conn.execute(
        "UPDATE ft_meta SET min_compatible_ft = '99.0.0' WHERE id = 1",
        [],
    )
    .unwrap();

    let result = initialize_schema(&conn);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("requires wa"),
        "Error should mention required ft version: {err}"
    );
}

#[test]
fn future_schema_version_rejected() {
    let conn = Connection::open_in_memory().unwrap();

    // Manually set user_version to a future version
    conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
        .unwrap();

    // Initialization should fail
    let result = initialize_schema(&conn);
    assert!(result.is_err());

    let err = result.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("newer than supported"),
        "Error should mention version mismatch: {err_str}"
    );
}

#[test]
fn idempotent_init_preserves_version() {
    let conn = Connection::open_in_memory().unwrap();

    // Initialize
    initialize_schema(&conn).unwrap();
    let version1 = get_user_version(&conn).unwrap();
    let count_after_first: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
        .unwrap();

    // Initialize again (should be no-op).
    initialize_schema(&conn).unwrap();
    let version2 = get_user_version(&conn).unwrap();
    let count_after_second: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
        .unwrap();

    assert_eq!(version1, version2);
    assert_eq!(version1, SCHEMA_VERSION);

    // ft-7tq4z: the audit trail's row count is determined by the
    // number of migrations applied during the FIRST init (one row
    // per migration via run_migrations(0)). The second init must
    // be a true no-op — the count must NOT grow. Pin idempotency
    // by comparing snapshots, not by hard-coding a number that
    // changes every time MIGRATIONS gains a new entry.
    assert!(
        count_after_first > 0,
        "first init must record at least one migration"
    );
    assert_eq!(
        count_after_first, count_after_second,
        "re-running initialize_schema on an up-to-date DB must not add audit rows"
    );
}

#[test]
fn pending_migrations_empty_at_current_version() {
    let pending = pending_migrations(SCHEMA_VERSION);
    assert!(pending.is_empty());
}

#[test]
fn pending_migrations_includes_all_from_zero() {
    let pending = pending_migrations(0);
    assert_eq!(pending.len(), MIGRATIONS.len());
}

#[test]
fn migrations_are_sorted_by_version() {
    let mut prev_version = 0;
    for migration in MIGRATIONS {
        assert!(
            migration.version > prev_version,
            "Migration versions must be strictly increasing"
        );
        prev_version = migration.version;
    }
}

#[test]
fn migration_runner_simulated_upgrade() {
    // This test simulates what happens when we add a new migration
    let conn = Connection::open_in_memory().unwrap();

    // Create a minimal v1 schema without the new audit_actions column.
    conn.execute_batch(
        r"
            CREATE TABLE panes (
                pane_id INTEGER PRIMARY KEY,
                domain TEXT NOT NULL DEFAULT 'local',
                window_id INTEGER,
                tab_id INTEGER,
                title TEXT,
                cwd TEXT,
                tty_name TEXT,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                observed INTEGER NOT NULL DEFAULT 1,
                ignore_reason TEXT,
                last_decision_at INTEGER
            );

            CREATE TABLE audit_actions (
                id INTEGER PRIMARY KEY,
                ts INTEGER NOT NULL,
                actor_kind TEXT NOT NULL,
                actor_id TEXT,
                pane_id INTEGER REFERENCES panes(pane_id) ON DELETE SET NULL,
                domain TEXT,
                action_kind TEXT NOT NULL,
                policy_decision TEXT NOT NULL,
                decision_reason TEXT,
                rule_id TEXT,
                input_summary TEXT,
                verification_summary TEXT,
                result TEXT NOT NULL
            );

            CREATE TABLE workflow_step_logs (
                id INTEGER PRIMARY KEY,
                workflow_id TEXT NOT NULL,
                step_index INTEGER NOT NULL,
                step_name TEXT NOT NULL,
                result_type TEXT NOT NULL,
                result_data TEXT,
                started_at INTEGER NOT NULL,
                completed_at INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL
            );

            CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL,
                rule_id TEXT NOT NULL,
                agent_type TEXT NOT NULL,
                event_type TEXT NOT NULL,
                severity TEXT NOT NULL,
                confidence REAL NOT NULL,
                extracted TEXT,
                matched_text TEXT,
                segment_id INTEGER,
                detected_at INTEGER NOT NULL,
                handled_at INTEGER,
                handled_by_workflow_id TEXT,
                handled_status TEXT,
                dedupe_key TEXT
            );

            CREATE TABLE approval_tokens (
                id INTEGER PRIMARY KEY,
                code_hash TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                used_at INTEGER,
                workspace_id TEXT NOT NULL,
                action_kind TEXT NOT NULL,
                pane_id INTEGER,
                action_fingerprint TEXT NOT NULL
            );
            ",
    )
    .unwrap();
    set_user_version(&conn, 0).unwrap();

    // Tables should exist but version should be 0
    assert!(!needs_initialization(&conn).unwrap());
    assert_eq!(get_user_version(&conn).unwrap(), 0);

    // Run initialization (should apply migrations)
    initialize_schema(&conn).unwrap();

    // Should now be at current version
    assert_eq!(get_user_version(&conn).unwrap(), SCHEMA_VERSION);
    assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

    for (table, column) in [
        ("audit_actions", "decision_context"),
        ("audit_actions", "correlation_id"),
        ("panes", "pane_uuid"),
        ("workflow_step_logs", "audit_action_id"),
        ("workflow_step_logs", "step_id"),
        ("workflow_step_logs", "step_kind"),
        ("workflow_step_logs", "policy_summary"),
        ("workflow_step_logs", "verification_refs"),
        ("workflow_step_logs", "error_code"),
        ("events", "triage_state"),
        ("events", "triage_updated_at"),
        ("events", "triage_updated_by"),
        ("approval_tokens", "plan_hash"),
        ("approval_tokens", "plan_version"),
        ("approval_tokens", "risk_summary"),
    ] {
        assert!(
            table_has_column(&conn, table, column).unwrap(),
            "{table}.{column} should be repaired before v0 DB is marked current"
        );
    }

    let migration_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
        .unwrap();
    assert!(
        usize::try_from(migration_rows).is_ok_and(|rows| rows >= MIGRATIONS.len()),
        "v0 partial repair should replay versioned migrations, not stamp a single current row"
    );
}

#[test]
fn migration_runner_sparse_v0_database_initializes() {
    let conn = Connection::open_in_memory().unwrap();

    conn.execute_batch(
        r"
            CREATE TABLE panes (
                pane_id INTEGER PRIMARY KEY,
                domain TEXT NOT NULL DEFAULT 'local',
                window_id INTEGER,
                tab_id INTEGER,
                title TEXT,
                cwd TEXT,
                tty_name TEXT,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                observed INTEGER NOT NULL DEFAULT 1,
                ignore_reason TEXT,
                last_decision_at INTEGER
            );
            ",
    )
    .unwrap();
    set_user_version(&conn, 0).unwrap();

    assert!(!needs_initialization(&conn).unwrap());
    initialize_schema(&conn).unwrap();

    assert_eq!(get_user_version(&conn).unwrap(), SCHEMA_VERSION);
    assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

    for (table, column) in [
        ("audit_actions", "correlation_id"),
        ("events", "triage_state"),
        ("workflow_step_logs", "audit_action_id"),
    ] {
        assert!(
            table_has_column(&conn, table, column).unwrap(),
            "{table}.{column} should exist after sparse v0 initialization"
        );
    }
}

#[test]
fn fresh_db_init_creates_policy_denied_audit_via_v24_migration() {
    // ft-7tq4z regression fence. Before the fix, a fresh DB (no `panes`
    // table) took the early-return branch in `initialize_schema` that
    // ran `SCHEMA_SQL` and stamped `user_version = SCHEMA_VERSION`
    // directly, skipping `run_migrations(0)`. SCHEMA_SQL was last
    // regenerated before v24, so the `policy_denied_audit` table
    // (added via migration v24) silently never landed on fresh DBs
    // even though `user_version` claimed they were at HEAD.
    //
    // This test pins the fix: open a brand-new in-memory DB,
    // initialize_schema, then assert that BOTH conditions hold —
    // (i) user_version is at SCHEMA_VERSION, (ii) the v24
    // policy_denied_audit table exists with its documented column
    // shape. Either condition failing on its own is the v24-drift
    // bug.
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Invariant 1: the version stamp matches the constant.
    assert_eq!(
        get_user_version(&conn).unwrap(),
        SCHEMA_VERSION,
        "fresh-DB init must end at SCHEMA_VERSION"
    );

    // Invariant 2: the v24 table exists.
    assert!(
        table_exists(&conn, "policy_denied_audit").unwrap(),
        "policy_denied_audit table must exist on a fresh DB at SCHEMA_VERSION"
    );

    // Invariant 3: the v24 table has the columns
    // PolicyDeniedAuditRecord serializes into. If a future migration
    // alters the table without updating SCHEMA_SQL, this catches the
    // column-shape drift in the same place as the table-existence
    // drift.
    for column in [
        "id",
        "ts_ms",
        "agent_id",
        "tool_name",
        "intent_hash",
        "reason",
        "reason_code",
        "rule_id",
        "decision",
    ] {
        assert!(
            table_has_column(&conn, "policy_denied_audit", column).unwrap(),
            "policy_denied_audit must expose column {column} on a fresh DB"
        );
    }

    // Invariant 4: a write through the documented sync path actually
    // lands a row. This is the surface ft-cro2u's audit-persist test
    // exercises in production; if migrations didn't run on the fresh
    // path, the INSERT would fail with "no such table" and the
    // best-effort persist would warn-log + drop the audit silently.
    let record = PolicyDeniedAuditRecord {
        id: 0,
        ts_ms: 1_700_000_000_000,
        agent_id: None,
        tool_name: "wa.test".to_string(),
        intent_hash: None,
        reason: "test reason".to_string(),
        reason_code: PolicyDeniedAuditRecord::REASON_CODE_DENIED.to_string(),
        rule_id: None,
        decision: PolicyDeniedAuditRecord::DECISION_DENIED.to_string(),
    };
    let backend = RusqliteBackend::new(conn);
    let row_id = record_policy_denial_audit_backend(&backend, &record)
        .expect("policy_denied_audit insert must succeed on a fresh DB");
    assert!(row_id > 0);
}

#[test]
fn each_migration_step_can_be_reapplied_without_panicking() {
    for migration in MIGRATIONS.iter().skip(1) {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let previous_version = previous_migration_version(migration.version);
        let down_plan = build_migration_plan(SCHEMA_VERSION, previous_version).unwrap();
        apply_migration_plan(&conn, &down_plan).unwrap();
        assert_eq!(get_user_version(&conn).unwrap(), previous_version);

        let step = MigrationStep {
            migration_version: migration.version,
            resulting_version: migration.version,
            description: migration.description,
            direction: MigrationDirection::Up,
        };

        apply_migration_step(&conn, &step)
            .unwrap_or_else(|err| panic!("first apply failed for v{}: {err}", migration.version));
        assert_eq!(get_user_version(&conn).unwrap(), migration.version);

        apply_migration_step(&conn, &step)
            .unwrap_or_else(|err| panic!("replay apply failed for v{}: {err}", migration.version));
        assert_eq!(get_user_version(&conn).unwrap(), migration.version);
    }
}

#[test]
fn v1_schema_includes_agent_sessions() {
    // Verify the v1 schema includes agent_sessions table
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_sessions'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "agent_sessions table should exist in v1 schema");
}

// =========================================================================
// Basic Insert/Query Tests (validates schema correctness)
// =========================================================================

#[test]
#[allow(clippy::cast_possible_wrap)]
fn can_insert_and_query_pane() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![42i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

    let (pane_id, domain): (i64, String) = conn
        .query_row(
            "SELECT pane_id, domain FROM panes WHERE pane_id = ?1",
            [42i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(pane_id, 42);
    assert_eq!(domain, "local");
}

#[test]
fn query_pane_rejects_invalid_observed_flag() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            42i64,
            "local",
            1_700_000_000_000i64,
            1_700_000_000_000i64,
            2i64
        ],
    )
    .unwrap();

    let err = query_pane(&conn, 42).expect_err("invalid observed flag");
    let message = err.to_string();
    assert!(message.contains("panes.observed"), "{message}");
    assert!(message.contains("must be 0 or 1"), "{message}");
}

#[test]
fn can_insert_segment_with_unique_constraint() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    // Insert pane first (foreign key)
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    // Insert segment
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "hello", 5, now_ms],
        ).unwrap();

    // Duplicate should fail
    let result = conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "world", 5, now_ms],
        );
    assert!(result.is_err(), "Duplicate (pane_id, seq) should fail");
}

#[test]
fn fts_trigger_syncs_on_insert() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    // Insert pane
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    // Insert segment
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "hello world test", 16, now_ms],
        ).unwrap();

    // Search via FTS
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM output_segments_fts WHERE output_segments_fts MATCH 'world'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "FTS should find the inserted content");
}

#[test]
fn fts_search_returns_snippet_and_highlight() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

    let content = "hello world from wezterm";
    let content_len = i64::try_from(content.len()).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, content, content_len, now_ms],
        )
        .unwrap();

    let results = search_fts_with_snippets(&conn, "world", &SearchOptions::default())
        .expect("search should succeed");
    assert_eq!(results.len(), 1);

    let snippet = results[0].snippet.as_deref().expect("snippet");
    assert!(snippet.contains(">>>world<<<"));

    let highlight = results[0].highlight.as_deref().expect("highlight");
    assert!(highlight.contains(">>>world<<<"));
}

#[test]
fn fts_search_scopes_by_pane_and_limit() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    for pane_id in [1i64, 2i64] {
        conn.execute(
                "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![pane_id, "local", now_ms, now_ms, 1],
            )
            .unwrap();
    }

    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "needle alpha", 12i64, now_ms],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![2i64, 0i64, "needle beta", 11i64, now_ms + 1000],
        )
        .unwrap();

    let options = SearchOptions {
        pane_id: Some(2),
        limit: Some(1),
        ..Default::default()
    };

    let results =
        search_fts_with_snippets(&conn, "needle", &options).expect("search should succeed");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].segment.pane_id, 2);
}

#[test]
fn fts_search_invalid_query_is_structured_error() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let err = search_fts_with_snippets(&conn, "\"unterminated", &SearchOptions::default())
        .expect_err("expected invalid query error");

    match err {
        crate::Error::Storage(StorageError::FtsQueryError(msg)) => {
            assert!(msg.contains("Invalid FTS5 query syntax"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn fts_lint_detects_empty_query() {
    let lints = lint_fts_query("   ");
    assert!(
        lints.iter().any(|lint| lint.code == "empty_query"),
        "expected empty_query lint"
    );
    assert!(
        lints
            .iter()
            .any(|lint| lint.severity == SearchLintSeverity::Error),
        "expected error severity for empty query"
    );
}

#[test]
fn fts_lint_detects_unbalanced_quotes() {
    let lints = lint_fts_query("\"unterminated");
    assert!(
        lints.iter().any(|lint| lint.code == "unbalanced_quotes"),
        "expected unbalanced_quotes lint"
    );
}

#[test]
fn fts_lint_detects_operator_misuse() {
    let lints = lint_fts_query("AND error OR");
    assert!(
        lints.iter().any(|lint| lint.code == "leading_operator"),
        "expected leading_operator lint"
    );
    assert!(
        lints.iter().any(|lint| lint.code == "trailing_operator"),
        "expected trailing_operator lint"
    );
}

#[test]
fn fts_lint_warns_on_bad_wildcard_position() {
    let lints = lint_fts_query("err*or");
    assert!(
        lints.iter().any(|lint| lint.code == "wildcard_position"),
        "expected wildcard_position lint"
    );
}

#[test]
fn fts_lint_allows_quoted_phrase() {
    let lints = lint_fts_query("\"error code\"");
    assert!(
        lints
            .iter()
            .all(|lint| lint.severity != SearchLintSeverity::Error),
        "expected no error lints for quoted phrase"
    );
}

#[test]
fn fts_lint_allows_operator_query() {
    let lints = lint_fts_query("error OR warning");
    assert!(
        lints
            .iter()
            .all(|lint| lint.severity != SearchLintSeverity::Error),
        "expected no error lints for operator query"
    );
}

#[test]
fn fts_search_order_is_deterministic_on_ties() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

    let content = "tie breaker needle";
    let content_len = i64::try_from(content.len()).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, content, content_len, now_ms],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 1i64, content, content_len, now_ms + 1000],
        )
        .unwrap();

    let results = search_fts_with_snippets(&conn, "needle", &SearchOptions::default())
        .expect("search should succeed");
    assert_eq!(results.len(), 2);
    assert!(results[0].segment.captured_at <= results[1].segment.captured_at);
}

#[test]
fn can_insert_event_and_mark_handled() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    // Insert pane
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    // Insert unhandled event
    conn.execute(
            "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity, confidence, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![1i64, "codex.usage_limit", "codex", "usage", "warning", 0.95, now_ms],
        ).unwrap();

    // Query unhandled
    let unhandled_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE handled_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unhandled_count, 1);

    // Mark as handled
    conn.execute(
        "UPDATE events SET handled_at = ?1, handled_status = ?2 WHERE id = 1",
        params![now_ms + 1000, "completed"],
    )
    .unwrap();

    // Query unhandled again
    let unhandled_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE handled_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unhandled_count, 0);
}

#[test]
fn can_insert_workflow_execution() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    // Insert pane
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    // Insert workflow execution
    conn.execute(
            "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params!["wf-001", "handle_compaction", 1i64, 0, "running", now_ms, now_ms],
        ).unwrap();

    // Query
    let (name, status): (String, String) = conn
        .query_row(
            "SELECT workflow_name, status FROM workflow_executions WHERE id = ?1",
            ["wf-001"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(name, "handle_compaction");
    assert_eq!(status, "running");
}

// =========================================================================
// Data Structure Serialization Tests
// =========================================================================

#[test]
fn segment_serializes() {
    let segment = Segment {
        id: 1,
        pane_id: 42,
        seq: 100,
        content: "Hello, world!".to_string(),
        content_len: 13,
        content_hash: Some("abc123".to_string()),
        captured_at: 1_234_567_890,
    };

    let json = serde_json::to_string(&segment).unwrap();
    assert!(json.contains("Hello, world!"));
    assert!(json.contains("content_len"));
}

#[test]
fn pane_record_serializes() {
    let pane = PaneRecord {
        pane_id: 1,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: Some(0),
        tab_id: Some(0),
        title: Some("bash".to_string()),
        cwd: Some("/home/user".to_string()),
        tty_name: None,
        first_seen_at: 1_700_000_000_000,
        last_seen_at: 1_700_000_001_000,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    };

    let json = serde_json::to_string(&pane).unwrap();
    assert!(json.contains("local"));
    assert!(json.contains("bash"));
}

#[test]
fn stored_event_serializes() {
    let event = StoredEvent {
        id: 1,
        pane_id: 42,
        rule_id: "codex.usage_limit".to_string(),
        agent_type: "codex".to_string(),
        event_type: "usage".to_string(),
        severity: "warning".to_string(),
        confidence: 0.95,
        extracted: Some(serde_json::json!({"limit": 100})),
        matched_text: Some("Usage limit reached".to_string()),
        segment_id: Some(123),
        detected_at: 1_700_000_000_000,
        dedupe_key: None,
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    };

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("codex.usage_limit"));
    assert!(json.contains("0.95"));
}

#[test]
fn workflow_record_serializes() {
    let workflow = WorkflowRecord {
        id: "wf-001".to_string(),
        workflow_name: "handle_compaction".to_string(),
        pane_id: 42,
        trigger_event_id: Some(1),
        current_step: 2,
        status: "running".to_string(),
        wait_condition: None,
        context: Some(serde_json::json!({"retry_count": 0})),
        result: None,
        error: None,
        started_at: 1_700_000_000_000,
        updated_at: 1_700_000_001_000,
        completed_at: None,
    };

    let json = serde_json::to_string(&workflow).unwrap();
    assert!(json.contains("handle_compaction"));
    assert!(json.contains("wf-001"));
}

// =========================================================================
// wa-4vx.3.8: Audit Actions Tests
// =========================================================================

#[test]
fn audit_action_record_serializes() {
    let action = AuditActionRecord {
        id: 1,
        ts: 1_700_000_000_000,
        actor_kind: "human".to_string(),
        actor_id: Some("user-1".to_string()),
        correlation_id: None,
        pane_id: Some(42),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: Some("ok".to_string()),
        rule_id: Some("policy.allow".to_string()),
        input_summary: Some("echo hi".to_string()),
        verification_summary: Some("prompt_active".to_string()),
        decision_context: Some(typed_decision_context_json(
            crate::policy::ActionKind::SendText,
            crate::policy::ActorKind::Human,
            crate::policy::PolicySurface::Mux,
        )),
        result: "success".to_string(),
    };

    let json = serde_json::to_string(&action).unwrap();
    assert!(json.contains("send_text"));
    assert!(json.contains("policy_decision"));
    assert!(json.contains("decision_context"));
}

#[test]
fn audit_action_redacts_sensitive_fields() {
    let secret = "sk-abc123456789012345678901234567890123456789012345678901";
    let mut action = AuditActionRecord {
        id: 0,
        ts: 1_700_000_000_000,
        actor_kind: "robot".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: Some(
            "token sk-abc123456789012345678901234567890123456789012345678901".to_string(),
        ),
        rule_id: None,
        input_summary: Some(
            "API key sk-abc123456789012345678901234567890123456789012345678901".to_string(),
        ),
        verification_summary: Some("checked prompt".to_string()),
        decision_context: Some(typed_sensitive_decision_context_json(
            crate::policy::ActionKind::SendText,
            crate::policy::ActorKind::Robot,
            crate::policy::PolicySurface::Mux,
            secret,
        )),
        result: "success".to_string(),
    };

    let redactor = Redactor::new();
    action.redact_fields(&redactor);

    let reason = action.decision_reason.unwrap();
    let input = action.input_summary.unwrap();
    let context = action.decision_context.unwrap();

    assert!(reason.contains("[REDACTED]"));
    assert!(input.contains("[REDACTED]"));
    assert!(context.contains("[REDACTED]"));
    assert!(!reason.contains("sk-abc"));
    assert!(!input.contains("sk-abc"));
    assert!(!context.contains("sk-abc"));
}

#[test]
fn audit_stream_record_redacts_sensitive_fields() {
    let secret = "sk-abc123456789012345678901234567890";
    let action = AuditActionRecord {
        id: 42,
        ts: 1_700_000_000_123,
        actor_kind: "robot".to_string(),
        actor_id: Some("cli".to_string()),
        correlation_id: None,
        pane_id: Some(2),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: Some("token sk-abc123456789012345678901234567890".to_string()),
        rule_id: None,
        input_summary: Some("API key sk-abc123456789012345678901234567890".to_string()),
        verification_summary: Some("prompt ok".to_string()),
        decision_context: Some(typed_sensitive_decision_context_json(
            crate::policy::ActionKind::SendText,
            crate::policy::ActorKind::Robot,
            crate::policy::PolicySurface::Mux,
            secret,
        )),
        result: "success".to_string(),
    };

    let redactor = Redactor::new();
    let record = AuditStreamRecord::from_action(action, &redactor);

    let reason = record.decision_reason.unwrap();
    let input = record.input_summary.unwrap();
    let context = record.decision_context.unwrap();

    assert!(reason.contains("[REDACTED]"));
    assert!(input.contains("[REDACTED]"));
    assert!(context.contains("[REDACTED]"));
    assert!(!reason.contains("sk-abc"));
    assert!(!input.contains("sk-abc"));
    assert!(!context.contains("sk-abc"));
}

#[test]
fn can_insert_and_query_audit_actions() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

    let action = AuditActionRecord {
        id: 0,
        ts: now_ms,
        actor_kind: "human".to_string(),
        actor_id: Some("cli".to_string()),
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: Some("ok".to_string()),
        rule_id: None,
        input_summary: Some("echo hi".to_string()),
        verification_summary: Some("prompt".to_string()),
        decision_context: None,
        result: "success".to_string(),
    };

    let id = record_audit_action_for_conn(&mut conn, &action).unwrap();
    assert!(id > 0);

    let query = AuditQuery {
        pane_id: Some(1),
        actor_kind: Some("human".to_string()),
        action_kind: Some("send_text".to_string()),
        ..Default::default()
    };
    let rows = query_audit_actions(&conn, &query).unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].actor_kind, "human");
    assert_eq!(rows[0].action_kind, "send_text");
    assert_eq!(rows[0].policy_decision, "allow");
}

#[test]
fn action_history_includes_undo_metadata() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

    let action = AuditActionRecord {
        id: 0,
        ts: now_ms,
        actor_kind: "human".to_string(),
        actor_id: Some("cli".to_string()),
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: Some("ok".to_string()),
        rule_id: None,
        input_summary: Some("echo hi".to_string()),
        verification_summary: Some("prompt".to_string()),
        decision_context: None,
        result: "success".to_string(),
    };

    let action_id = record_audit_action_for_conn(&mut conn, &action).unwrap();

    let undo = ActionUndoRecord {
        audit_action_id: action_id,
        undoable: true,
        undo_strategy: "manual".to_string(),
        undo_hint: Some("run undo manually".to_string()),
        undo_payload: Some(r#"{"command":"undo"}"#.to_string()),
        undone_at: None,
        undone_by: None,
    };
    with_writer_backend(&mut conn, |backend| {
        upsert_action_undo_backend(backend, &undo)
    })
    .unwrap();

    let rows = query_action_history(&conn, &ActionHistoryQuery::default()).unwrap();
    assert!(!rows.is_empty());

    let row = &rows[0];
    assert_eq!(row.id, action_id);
    assert_eq!(row.action_kind, "send_text");
    assert_eq!(row.undoable, Some(true));
    assert_eq!(row.undo_strategy.as_deref(), Some("manual"));
    assert_eq!(row.undo_hint.as_deref(), Some("run undo manually"));
    assert!(row.workflow_id.is_none());
    assert!(row.step_name.is_none());
}

#[test]
fn action_undo_index_exists_after_init() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_action_undo_undoable'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(count, 1, "idx_action_undo_undoable index should exist");
}

#[test]
fn action_history_orders_by_ts_and_id() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

    let base = AuditActionRecord {
        id: 0,
        ts: 1_000,
        actor_kind: "human".to_string(),
        actor_id: Some("cli".to_string()),
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: None,
        rule_id: None,
        input_summary: Some("first".to_string()),
        verification_summary: None,
        decision_context: None,
        result: "success".to_string(),
    };

    let id1 = record_audit_action_for_conn(&mut conn, &base).unwrap();
    let id2 = record_audit_action_for_conn(
        &mut conn,
        &AuditActionRecord {
            ts: 2_000,
            input_summary: Some("second".to_string()),
            ..base.clone()
        },
    )
    .unwrap();
    let id3 = record_audit_action_for_conn(
        &mut conn,
        &AuditActionRecord {
            ts: 2_000,
            input_summary: Some("third".to_string()),
            ..base
        },
    )
    .unwrap();

    let rows = query_action_history(
        &conn,
        &ActionHistoryQuery {
            limit: Some(10),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].id, id3);
    assert_eq!(rows[1].id, id2);
    assert_eq!(rows[2].id, id1);
}

#[test]
fn action_history_filters_undoable() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

    let base = AuditActionRecord {
        id: 0,
        ts: 1_000,
        actor_kind: "human".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: None,
        rule_id: None,
        input_summary: Some("first".to_string()),
        verification_summary: None,
        decision_context: None,
        result: "success".to_string(),
    };

    let undoable_id = record_audit_action_for_conn(&mut conn, &base).unwrap();
    let non_undoable_id = record_audit_action_for_conn(
        &mut conn,
        &AuditActionRecord {
            ts: 2_000,
            input_summary: Some("second".to_string()),
            ..base
        },
    )
    .unwrap();

    with_writer_backend(&mut conn, |backend| {
        upsert_action_undo_backend(
            backend,
            &ActionUndoRecord {
                audit_action_id: undoable_id,
                undoable: true,
                undo_strategy: "manual".to_string(),
                undo_hint: None,
                undo_payload: None,
                undone_at: None,
                undone_by: None,
            },
        )
    })
    .unwrap();
    with_writer_backend(&mut conn, |backend| {
        upsert_action_undo_backend(
            backend,
            &ActionUndoRecord {
                audit_action_id: non_undoable_id,
                undoable: false,
                undo_strategy: "none".to_string(),
                undo_hint: None,
                undo_payload: None,
                undone_at: None,
                undone_by: None,
            },
        )
    })
    .unwrap();

    let undoable = query_action_history(
        &conn,
        &ActionHistoryQuery {
            undoable: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(undoable.len(), 1);
    assert_eq!(undoable[0].id, undoable_id);

    let non_undoable = query_action_history(
        &conn,
        &ActionHistoryQuery {
            undoable: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(non_undoable.iter().any(|row| row.id == non_undoable_id));
}

#[test]
fn action_history_includes_workflow_step_info() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

    let action = AuditActionRecord {
        id: 0,
        ts: now_ms,
        actor_kind: "workflow".to_string(),
        actor_id: Some("wf-1".to_string()),
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "workflow_step".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: None,
        rule_id: None,
        input_summary: Some("step".to_string()),
        verification_summary: None,
        decision_context: None,
        result: "success".to_string(),
    };
    let action_id = record_audit_action_for_conn(&mut conn, &action).unwrap();

    conn.execute(
            "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params!["wf-1", "test", 1i64, 0i64, "running", now_ms, now_ms],
        )
        .unwrap();

    conn.execute(
            "INSERT INTO workflow_step_logs (workflow_id, audit_action_id, step_index, step_name, result_type, started_at, completed_at, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params!["wf-1", action_id, 0i64, "step-0", "done", now_ms, now_ms, 0i64],
        )
        .unwrap();

    let rows = query_action_history(&conn, &ActionHistoryQuery::default()).unwrap();
    let row = rows.iter().find(|row| row.id == action_id).unwrap();
    assert_eq!(row.workflow_id.as_deref(), Some("wf-1"));
    assert_eq!(row.step_name.as_deref(), Some("step-0"));
}

#[test]
fn action_undo_redaction_applied() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

    let action = AuditActionRecord {
        id: 0,
        ts: now_ms,
        actor_kind: "human".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: None,
        rule_id: None,
        input_summary: Some("hi".to_string()),
        verification_summary: None,
        decision_context: None,
        result: "success".to_string(),
    };
    let action_id = record_audit_action_for_conn(&mut conn, &action).unwrap();

    let secret = "sk-abc123456789012345678901234567890123456789012345678901";
    let mut undo = ActionUndoRecord {
        audit_action_id: action_id,
        undoable: true,
        undo_strategy: "manual".to_string(),
        undo_hint: Some(format!("token {secret}")),
        undo_payload: Some(format!(r#"{{"token":"{secret}"}}"#)),
        undone_at: None,
        undone_by: None,
    };
    let redactor = Redactor::new();
    undo.redact_fields(&redactor);
    with_writer_backend(&mut conn, |backend| {
        upsert_action_undo_backend(backend, &undo)
    })
    .unwrap();

    let (hint, payload): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT undo_hint, undo_payload FROM action_undo WHERE audit_action_id = ?1",
            params![action_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    let hint = hint.expect("undo_hint missing");
    let payload = payload.expect("undo_payload missing");
    assert!(hint.contains("[REDACTED]"));
    assert!(payload.contains("[REDACTED]"));
    assert!(!hint.contains("sk-abc"));
    assert!(!payload.contains("sk-abc"));
}

#[test]
fn purge_audit_actions_removes_old_entries() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", 1i64, 1i64, 1],
        )
        .unwrap();

    let older = AuditActionRecord {
        id: 0,
        ts: 1_000,
        actor_kind: "human".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: None,
        rule_id: None,
        input_summary: Some("old".to_string()),
        verification_summary: None,
        decision_context: None,
        result: "success".to_string(),
    };
    let newer = AuditActionRecord {
        ts: 2_000,
        input_summary: Some("new".to_string()),
        ..older.clone()
    };

    record_audit_action_for_conn(&mut conn, &older).unwrap();
    record_audit_action_for_conn(&mut conn, &newer).unwrap();

    // br-ft-l1jgo: purge_audit_actions_sync was migrated to
    // purge_audit_actions_backend at c64527d9c. Wrap the test
    // conn into a RusqliteBackend for the backend-trait call.
    let backend = crate::storage_backend_trait::RusqliteBackend::new(conn);
    let deleted = purge_audit_actions_backend(&backend, 1_500).unwrap();
    assert_eq!(deleted, 1);

    let conn = backend.into_connection();
    let rows = query_audit_actions(&conn, &AuditQuery::default()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ts, 2_000);
}

#[test]
fn audit_query_filters_and_limits() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", 1i64, 1i64, 1],
        )
        .unwrap();

    let allow = AuditActionRecord {
        id: 0,
        ts: 1_000,
        actor_kind: "human".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: Some("ok".to_string()),
        rule_id: None,
        input_summary: Some("echo hi".to_string()),
        verification_summary: None,
        decision_context: None,
        result: "success".to_string(),
    };
    let deny = AuditActionRecord {
        ts: 2_000,
        actor_kind: "workflow".to_string(),
        actor_id: Some("wf-123".to_string()),
        action_kind: "workflow_run".to_string(),
        policy_decision: "deny".to_string(),
        decision_reason: Some("blocked".to_string()),
        result: "denied".to_string(),
        ..allow.clone()
    };

    record_audit_action_for_conn(&mut conn, &allow).unwrap();
    record_audit_action_for_conn(&mut conn, &deny).unwrap();

    let last_one = query_audit_actions(
        &conn,
        &AuditQuery {
            limit: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(last_one.len(), 1);
    assert_eq!(last_one[0].ts, 2_000);

    let by_pane = query_audit_actions(
        &conn,
        &AuditQuery {
            pane_id: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_pane.len(), 2);

    let by_workflow = query_audit_actions(
        &conn,
        &AuditQuery {
            actor_id: Some("wf-123".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_workflow.len(), 1);
    assert_eq!(by_workflow[0].actor_kind, "workflow");

    let denied = query_audit_actions(
        &conn,
        &AuditQuery {
            policy_decision: Some("deny".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0].policy_decision, "deny");
}

#[test]
fn audit_stream_query_pages_with_cursor() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", 1i64, 1i64, 1],
        )
        .unwrap();

    let base = AuditActionRecord {
        id: 0,
        ts: 1_000,
        actor_kind: "human".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: None,
        rule_id: None,
        input_summary: Some("first".to_string()),
        verification_summary: None,
        decision_context: None,
        result: "success".to_string(),
    };

    let id1 = record_audit_action_for_conn(&mut conn, &base).unwrap();
    let id2 = record_audit_action_for_conn(
        &mut conn,
        &AuditActionRecord {
            ts: 2_000,
            input_summary: Some("second".to_string()),
            ..base.clone()
        },
    )
    .unwrap();
    let id3 = record_audit_action_for_conn(
        &mut conn,
        &AuditActionRecord {
            ts: 3_000,
            input_summary: Some("third".to_string()),
            ..base.clone()
        },
    )
    .unwrap();

    let page1 = query_audit_actions_stream(
        &conn,
        &AuditStreamQuery {
            limit: Some(2),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(page1.records.len(), 2);
    assert!(page1.records[0].id < page1.records[1].id);
    assert_eq!(page1.records[0].id, id1);
    assert_eq!(page1.records[1].id, id2);
    assert_eq!(page1.next_cursor, Some(id2));

    let page2 = query_audit_actions_stream(
        &conn,
        &AuditStreamQuery {
            cursor: page1.next_cursor,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(page2.records.len(), 1);
    assert_eq!(page2.records[0].id, id3);
    assert_eq!(page2.next_cursor, Some(id3));
}

#[test]
fn audit_stream_query_empty_returns_none_cursor() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let page = query_audit_actions_stream(&conn, &AuditStreamQuery::default()).unwrap();
    assert!(page.records.is_empty());
    assert!(page.next_cursor.is_none());
}

#[test]
fn audit_stream_query_respects_limit() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", 1i64, 1i64, 1],
        )
        .unwrap();

    let action = AuditActionRecord {
        id: 0,
        ts: 1_000,
        actor_kind: "human".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: None,
        rule_id: None,
        input_summary: Some("hi".to_string()),
        verification_summary: None,
        decision_context: None,
        result: "success".to_string(),
    };

    record_audit_action_for_conn(&mut conn, &action).unwrap();
    record_audit_action_for_conn(
        &mut conn,
        &AuditActionRecord {
            ts: 2_000,
            input_summary: Some("second".to_string()),
            ..action.clone()
        },
    )
    .unwrap();

    let page = query_audit_actions_stream(
        &conn,
        &AuditStreamQuery {
            limit: Some(1),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(page.records.len(), 1);
    assert_eq!(page.next_cursor, Some(page.records[0].id));
}

#[test]
fn audit_stream_record_serializes_json_schema() {
    let action = AuditActionRecord {
        id: 7,
        ts: 1_700_000_000_999,
        actor_kind: "workflow".to_string(),
        actor_id: Some("wf-123".to_string()),
        correlation_id: Some("corr-1".to_string()),
        pane_id: Some(3),
        domain: Some("local".to_string()),
        action_kind: "workflow_run".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: Some("ok".to_string()),
        rule_id: Some("rule-1".to_string()),
        input_summary: Some("input".to_string()),
        verification_summary: Some("verify".to_string()),
        decision_context: Some(typed_decision_context_json(
            crate::policy::ActionKind::WorkflowRun,
            crate::policy::ActorKind::Workflow,
            crate::policy::PolicySurface::Workflow,
        )),
        result: "success".to_string(),
    };

    let redactor = Redactor::new();
    let record = AuditStreamRecord::from_action(action, &redactor);
    let value = serde_json::to_value(&record).unwrap();

    assert!(value.get("id").is_some());
    assert!(value.get("ts").is_some());
    assert!(value.get("actor_kind").is_some());
    assert!(value.get("action_kind").is_some());
    assert!(value.get("policy_decision").is_some());
    assert!(value.get("result").is_some());
}

#[test]
fn approval_token_record_serializes() {
    let token = ApprovalTokenRecord {
        id: 1,
        code_hash: "sha256:abc123".to_string(),
        created_at: 1_700_000_000_000,
        expires_at: 1_700_000_010_000,
        used_at: None,
        workspace_id: "workspace-a".to_string(),
        action_kind: "send_text".to_string(),
        pane_id: Some(42),
        action_fingerprint: "sha256:fingerprint".to_string(),
        plan_hash: None,
        plan_version: None,
        risk_summary: None,
    };

    let json = serde_json::to_string(&token).unwrap();
    assert!(json.contains("sha256:abc123"));
    assert!(json.contains("workspace-a"));
}

// =========================================================================
// wa-4vx.3.6: Retention & Maintenance Tests
// =========================================================================

#[test]
fn retention_prunes_old_segments_and_fts() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "old content", 11, now_ms - 1000],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 1i64, "new content", 11, now_ms + 1000],
        )
        .unwrap();

    let backend = RusqliteBackend::new(conn);
    let deleted = prune_segments_backend(&backend, now_ms).unwrap();
    assert_eq!(deleted, 1);
    let conn = backend.into_connection();

    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM output_segments", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 1);

    let fts_old: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM output_segments_fts WHERE output_segments_fts MATCH 'old'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fts_old, 0);
}

// [ft-znu6v] Deferred-FTS retention used to strand `last_indexed_seq`
// past a seq reset: `append_segment_sync` restarts a fully-pruned pane
// at seq=0 via `COALESCE(MAX(seq) + 1, 0)`, but `sync_fts_for_pane`
// still read the pre-prune progress row and used the strict
// `seq > last_indexed_seq` filter, silently skipping the reset row
// forever. The prune-time progress rewind in `prune_segments_sync`
// deletes any progress row that points past the surviving MAX(seq)
// (or past the empty set), so the next sync re-enters the
// `include_from_zero` branch and picks seq=0 back up.
#[test]
fn prune_segments_rewinds_stranded_fts_progress_ft_znu6v() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let old_ts = 1_700_000_000_000i64;
    let pane: i64 = 1;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        params![pane, "local", old_ts, old_ts, 1],
    )
    .unwrap();

    for seq in 0..=5i64 {
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            params![pane, seq, format!("old {seq}"), 5, old_ts - 10],
        )
        .unwrap();
    }

    upsert_fts_pane_progress_sync(
        &conn,
        &FtsPaneProgress {
            pane_id: pane as u64,
            last_indexed_seq: 5,
            indexed_count: 6,
            last_indexed_at: old_ts - 5,
        },
    )
    .unwrap();

    let progress_before = get_fts_pane_progress_sync(&conn, pane as u64)
        .unwrap()
        .expect("progress row present before prune");
    assert_eq!(progress_before.last_indexed_seq, 5);

    let backend = RusqliteBackend::new(conn);
    let deleted = prune_segments_backend(&backend, old_ts).unwrap();
    assert_eq!(deleted, 6, "full pre-prune chain must be removed");
    let conn = backend.into_connection();

    let progress_after = get_fts_pane_progress_sync(&conn, pane as u64).unwrap();
    assert!(
        progress_after.is_none(),
        "ft-znu6v: prune must rewind stranded FTS progress, got {:?}",
        progress_after,
    );

    let new_ts = old_ts + 1000;
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        params![pane, 0i64, "postreset zero", 15, new_ts],
    )
    .unwrap();

    let config = FtsSyncConfig::default();
    let (indexed, final_seq) = sync_fts_for_pane(&conn, pane as u64, &config).unwrap();
    assert_eq!(
        indexed, 1,
        "ft-znu6v: post-reset seq=0 must be picked up by incremental sync"
    );
    assert_eq!(
        final_seq, 0,
        "ft-znu6v: incremental sync high-water mark must match the reset chain"
    );

    let fts_hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM output_segments_fts \
                 WHERE output_segments_fts MATCH 'postreset'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        fts_hits, 1,
        "ft-znu6v: the reset-chain row must be searchable after sync"
    );
}

// Partial-prune sibling: retention chops off seqs 3..=5 but leaves
// 0..=2. Without the rewind, a future seq=3 append (MAX(seq)+1 = 3)
// would be skipped by the strict incremental filter. The rewind
// predicate `last_indexed_seq > COALESCE(MAX(seq), -1)` catches
// this truncation case too.
#[test]
fn prune_segments_rewinds_when_partial_prune_truncates_tail_ft_znu6v() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let pane: i64 = 7;
    let boundary_ts = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        params![pane, "local", boundary_ts - 100, boundary_ts, 1],
    )
    .unwrap();

    for (seq, ts) in [
        (0i64, boundary_ts),
        (1, boundary_ts + 1),
        (2, boundary_ts + 2),
        (3, boundary_ts - 10),
        (4, boundary_ts - 9),
        (5, boundary_ts - 8),
    ] {
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            params![pane, seq, format!("tail {seq}"), 6, ts],
        )
        .unwrap();
    }

    upsert_fts_pane_progress_sync(
        &conn,
        &FtsPaneProgress {
            pane_id: pane as u64,
            last_indexed_seq: 5,
            indexed_count: 6,
            last_indexed_at: boundary_ts,
        },
    )
    .unwrap();

    let backend = RusqliteBackend::new(conn);
    let deleted = prune_segments_backend(&backend, boundary_ts).unwrap();
    assert_eq!(deleted, 3, "only pre-boundary rows must be pruned");
    let conn = backend.into_connection();

    let progress_after = get_fts_pane_progress_sync(&conn, pane as u64).unwrap();
    assert!(
        progress_after.is_none(),
        "ft-znu6v: partial prune that leaves MAX(seq)=2 must delete the \
             stranded progress row (last_indexed_seq=5)",
    );
}

#[test]
fn maintenance_log_records_event() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    let backend = RusqliteBackend::new(conn);

    let record = MaintenanceRecord {
        id: 0,
        event_type: "retention_cleanup".to_string(),
        message: Some("cleanup complete".to_string()),
        metadata: Some("{\"deleted\": 1}".to_string()),
        timestamp: 0,
    };

    let id = record_maintenance_backend(&backend, &record).unwrap();
    assert!(id > 0);

    let conn = backend.into_connection();
    let event_type: String = conn
        .query_row(
            "SELECT event_type FROM maintenance_log WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_type, "retention_cleanup");
}

#[test]
fn secret_scan_report_roundtrip() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();
    let backend = RusqliteBackend::new(conn);

    let record = SecretScanReportRecord {
        id: 0,
        scope_hash: "scope-hash".to_string(),
        scope_json: "{\"pane_id\":1}".to_string(),
        report_version: 1,
        last_segment_id: Some(42),
        report_json: "{\"report_version\":1}".to_string(),
        created_at: 1_700_000_000_000,
    };

    let id = record_secret_scan_report_backend(&backend, &record).unwrap();
    assert!(id > 0);

    let fetched = query_latest_secret_scan_report_backend(&backend, "scope-hash")
        .unwrap()
        .expect("report should exist");
    assert_eq!(fetched.scope_hash, "scope-hash");
    assert_eq!(fetched.last_segment_id, Some(42));
    assert_eq!(fetched.report_version, 1);
    assert_eq!(fetched.report_json, "{\"report_version\":1}");

    let conn = backend.into_connection();
    let direct = query_latest_secret_scan_report(&conn, "scope-hash")
        .unwrap()
        .expect("direct fallback should still decode");
    assert_eq!(direct.id, fetched.id);
}

#[test]
fn saved_search_roundtrip() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let record = SavedSearchRecord::new(
        "errors".to_string(),
        "error OR warning".to_string(),
        Some(1),
        25,
        SAVED_SEARCH_SINCE_MODE_LAST_RUN.to_string(),
        None,
    );
    with_writer_backend(&mut conn, |backend| {
        insert_saved_search_backend(backend, &record)
    })
    .unwrap();

    let fetched = with_writer_backend(&mut conn, |backend| {
        query_saved_search_by_name_backend(backend, "errors")
    })
    .unwrap()
    .expect("saved search should exist");
    assert_eq!(fetched.name, "errors");
    assert_eq!(fetched.query, "error OR warning");
    assert_eq!(fetched.pane_id, Some(1));
    assert_eq!(fetched.limit, 25);
    assert_eq!(fetched.since_mode, SAVED_SEARCH_SINCE_MODE_LAST_RUN);

    with_writer_backend(&mut conn, |backend| {
        update_saved_search_schedule_backend(backend, &fetched.id, true, Some(60_000))
    })
    .unwrap();
    let scheduled = with_writer_backend(&mut conn, |backend| {
        query_saved_search_by_name_backend(backend, "errors")
    })
    .unwrap()
    .expect("saved search should exist");
    assert!(scheduled.enabled);
    assert_eq!(scheduled.schedule_interval_ms, Some(60_000));

    let record2 = SavedSearchRecord::new(
        "alpha".to_string(),
        "panic".to_string(),
        None,
        10,
        SAVED_SEARCH_SINCE_MODE_FIXED.to_string(),
        Some(1_700_000_000_000),
    );
    with_writer_backend(&mut conn, |backend| {
        insert_saved_search_backend(backend, &record2)
    })
    .unwrap();

    let list = with_writer_backend(&mut conn, list_saved_searches_backend).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].name, "alpha");
    assert_eq!(list[1].name, "errors");

    let run_ts = now_ms();
    with_writer_backend(&mut conn, |backend| {
        update_saved_search_run_backend(backend, &fetched.id, run_ts, Some(3), None)
    })
    .unwrap();
    let updated = with_writer_backend(&mut conn, |backend| {
        query_saved_search_by_name_backend(backend, "errors")
    })
    .unwrap()
    .expect("saved search should exist");
    assert_eq!(updated.last_run_at, Some(run_ts));
    assert_eq!(updated.last_result_count, Some(3));
    assert!(updated.last_error.is_none());

    let deleted = with_writer_backend(&mut conn, |backend| {
        delete_saved_search_backend(backend, "errors")
    })
    .unwrap();
    assert_eq!(deleted, 1);
    let missing = with_writer_backend(&mut conn, |backend| {
        query_saved_search_by_name_backend(backend, "errors")
    })
    .unwrap();
    assert!(missing.is_none());
}

#[test]
fn saved_search_query_rejects_negative_pane_id() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    conn.execute(
        "INSERT INTO saved_searches (
                id, name, query, pane_id, \"limit\", since_mode, since_ms,
                schedule_interval_ms, enabled, last_run_at, last_result_count, last_error,
                created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            "ss-bad-pane",
            "bad-pane",
            "panic",
            -1i64,
            10i64,
            SAVED_SEARCH_SINCE_MODE_LAST_RUN,
            Option::<i64>::None,
            Option::<i64>::None,
            0i64,
            Option::<i64>::None,
            Option::<i64>::None,
            Option::<String>::None,
            now,
            now,
        ],
    )
    .unwrap();

    let err = with_writer_backend(&mut conn, |backend| {
        query_saved_search_by_name_backend(backend, "bad-pane")
    })
    .expect_err("negative pane id");
    let message = err.to_string();
    assert!(message.contains("saved_searches.pane_id"), "{message}");
    assert!(message.contains("-1"), "{message}");
}

#[test]
fn saved_search_query_rejects_invalid_enabled_flag() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now = now_ms();
    conn.execute(
        "INSERT INTO saved_searches (
                id, name, query, pane_id, \"limit\", since_mode, since_ms,
                schedule_interval_ms, enabled, last_run_at, last_result_count, last_error,
                created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            "ss-bad-enabled",
            "bad-enabled",
            "panic",
            Option::<i64>::None,
            10i64,
            SAVED_SEARCH_SINCE_MODE_LAST_RUN,
            Option::<i64>::None,
            Option::<i64>::None,
            2i64,
            Option::<i64>::None,
            Option::<i64>::None,
            Option::<String>::None,
            now,
            now,
        ],
    )
    .unwrap();

    let err = with_writer_backend(&mut conn, |backend| {
        query_saved_search_by_name_backend(backend, "bad-enabled")
    })
    .expect_err("invalid enabled");
    let message = err.to_string();
    assert!(message.contains("saved_searches.enabled"), "{message}");
    assert!(message.contains("must be 0 or 1"), "{message}");
}

#[test]
fn can_insert_and_consume_approval_token() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", 1i64, 1i64, 1],
        )
        .unwrap();

    let now = now_ms();
    let token = ApprovalTokenRecord {
        id: 0,
        code_hash: "sha256:tokenhash".to_string(),
        created_at: now,
        expires_at: now + 5_000,
        used_at: None,
        workspace_id: "ws".to_string(),
        action_kind: "send_text".to_string(),
        pane_id: Some(1),
        action_fingerprint: "sha256:fp".to_string(),
        plan_hash: None,
        plan_version: None,
        risk_summary: None,
    };

    let _token_id = with_writer_backend(&mut conn, |backend| {
        insert_approval_token_backend(backend, &token)
    })
    .unwrap();

    let consumed = with_writer_backend(&mut conn, |backend| {
        consume_approval_token_backend(
            backend,
            "sha256:tokenhash",
            "ws",
            "send_text",
            Some(1),
            "sha256:fp",
        )
    })
    .unwrap();
    assert!(consumed.is_some());
    assert!(consumed.unwrap().used_at.is_some());

    let second = with_writer_backend(&mut conn, |backend| {
        consume_approval_token_backend(
            backend,
            "sha256:tokenhash",
            "ws",
            "send_text",
            Some(1),
            "sha256:fp",
        )
    })
    .unwrap();
    assert!(second.is_none());
}

#[test]
fn approval_token_query_rejects_negative_pane_id() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Temporarily disable foreign keys to insert a row with an invalid
    // negative pane_id — the test validates that the *query* function
    // detects and rejects this corruption.
    let now = now_ms();
    conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    conn.execute(
        "INSERT INTO approval_tokens (
                code_hash, created_at, expires_at, used_at, workspace_id, action_kind, pane_id,
                action_fingerprint, plan_hash, plan_version, risk_summary
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            "sha256:bad-token",
            now,
            now + 10_000,
            Option::<i64>::None,
            "ws",
            "send_text",
            -3i64,
            "sha256:fp",
            Option::<String>::None,
            Option::<i64>::None,
            Option::<String>::None,
        ],
    )
    .unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

    // br-ft-l1jgo slice: migrate this final #[cfg(test)] caller of the
    // legacy direct-rusqlite helper to the StorageBackend trait surface.
    // Wrap the populated `Connection` into a `RusqliteBackend` (consumes
    // it — `conn` is not used past this point) and call the backend
    // sibling `query_approval_token_by_hash_backend` defined alongside
    // it in storage.rs. Reduces direct rusqlite::Connection refs by 1.
    let backend = crate::storage_backend_trait::RusqliteBackend::new(conn);
    let err = query_approval_token_by_hash_backend(&backend, "sha256:bad-token")
        .expect_err("negative approval pane id");
    let message = err.to_string();
    assert!(message.contains("approval_tokens.pane_id"), "{message}");
    assert!(message.contains("-3"), "{message}");
}

// =========================================================================
// wa-4vx.3.3: Gap Recording Tests
// =========================================================================

#[test]
fn can_record_gap_on_discontinuity() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    // Insert pane
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    // Insert some segments (seq 0, 1, 2)
    for seq in 0..3 {
        conn.execute(
                "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![1i64, seq, format!("content {}", seq), 10, now_ms + seq * 100],
            ).unwrap();
    }

    let backend = RusqliteBackend::new(conn);

    // Record a gap (simulating a discontinuity detected)
    let gap = record_gap_backend(&backend, 1, "sequence_jump")
        .unwrap()
        .expect("should return gap");

    // Verify gap was recorded
    assert_eq!(gap.pane_id, 1);
    assert_eq!(gap.seq_before, 2); // Last seq was 2
    assert_eq!(gap.seq_after, 3); // Next expected would be 3
    assert_eq!(gap.reason, "sequence_jump");

    let conn = backend.into_connection();

    // Query the gap from the database
    let (id, pane_id, seq_before, seq_after, reason): (i64, i64, i64, i64, String) = conn
        .query_row(
            "SELECT id, pane_id, seq_before, seq_after, reason FROM output_gaps WHERE pane_id = ?1",
            [1i64],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();

    assert!(id > 0);
    assert_eq!(pane_id, 1);
    assert_eq!(seq_before, 2);
    assert_eq!(seq_after, 3);
    assert_eq!(reason, "sequence_jump");
}

#[test]
fn gap_reasons_are_stable() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    // Insert pane
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    // Insert initial segment so gaps can be computed (needs seq_before)
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0, "initial content", 15, now_ms],
        )
        .unwrap();

    let backend = RusqliteBackend::new(conn);

    // Record gaps with different reasons
    let reasons = vec![
        "sequence_jump",
        "overlap_detected",
        "cursor_truncation",
        "session_restart",
    ];

    for reason in &reasons {
        record_gap_backend(&backend, 1, reason).unwrap();
    }

    let conn = backend.into_connection();

    // Verify all gaps were recorded with stable reasons
    let mut stmt = conn
        .prepare("SELECT reason FROM output_gaps WHERE pane_id = ?1 ORDER BY id")
        .unwrap();
    let recorded_reasons: Vec<String> = stmt
        .query_map([1i64], |row| row.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(recorded_reasons, reasons);
}

#[test]
fn distributed_explicit_gap_reason_preserves_reported_bounds() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "distributed:agent-a:prod", now_ms, now_ms, 1],
        )
        .unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 1i64, "before gap", 10, now_ms],
        )
        .unwrap();

    let backend = RusqliteBackend::new(conn);

    let gap = record_gap_backend(&backend, 1, "distributed_gap:timeout:1:4")
        .unwrap()
        .expect("distributed explicit gap should be recorded");

    assert_eq!(gap.seq_before, 1);
    assert_eq!(gap.seq_after, 4);
    assert_eq!(gap.reason, "distributed_gap:timeout:1:4");
}

#[test]
fn distributed_explicit_gap_reason_records_start_of_stream_gap() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "distributed:agent-a:prod", now_ms, now_ms, 1],
        )
        .unwrap();

    let backend = RusqliteBackend::new(conn);

    let gap = record_gap_backend(&backend, 1, "distributed_gap:startup_replay:0:3")
        .unwrap()
        .expect("explicit distributed gaps must not be dropped at stream start");

    assert_eq!(gap.seq_before, 0);
    assert_eq!(gap.seq_after, 3);
    assert_eq!(gap.reason, "distributed_gap:startup_replay:0:3");
}

#[test]
fn distributed_explicit_gap_reason_allows_colons_in_reason_text() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "distributed:agent-a:prod", now_ms, now_ms, 1],
        )
        .unwrap();

    let backend = RusqliteBackend::new(conn);

    let gap = record_gap_backend(&backend, 1, "distributed_gap:session:restart:4:9")
        .unwrap()
        .expect("colon-bearing distributed gap reason should still parse bounds");

    assert_eq!(gap.seq_before, 4);
    assert_eq!(gap.seq_after, 9);
    assert_eq!(gap.reason, "distributed_gap:session:restart:4:9");
}

// =========================================================================
// wa-4vx.3.3: Last-N Query Tests
// =========================================================================

#[test]
fn last_n_segments_returns_deterministic_order() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    // Insert pane
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    // Insert segments out of order (seq: 5, 2, 8, 1, 3)
    let insert_order = vec![5, 2, 8, 1, 3];
    for seq in insert_order {
        conn.execute(
                "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![1i64, seq, format!("segment-{}", seq), 10, now_ms + seq * 100],
            ).unwrap();
    }

    // Query last 3 segments
    let segments = query_segments(&conn, 1, 3).unwrap();

    // Should return in descending seq order: 8, 5, 3
    assert_eq!(segments.len(), 3);
    assert_eq!(segments[0].seq, 8);
    assert_eq!(segments[1].seq, 5);
    assert_eq!(segments[2].seq, 3);

    // Query all segments
    let all_segments = query_segments(&conn, 1, 100).unwrap();
    assert_eq!(all_segments.len(), 5);

    // Verify strictly descending order
    for window in all_segments.windows(2) {
        assert!(
            window[0].seq > window[1].seq,
            "Segments should be in strictly descending seq order"
        );
    }
}

#[test]
fn last_n_query_is_indexed() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Verify the index exists using EXPLAIN QUERY PLAN
    let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
                 FROM output_segments WHERE pane_id = 1 ORDER BY seq DESC LIMIT 10",
                [],
                |row| row.get(3),
            )
            .unwrap();

    // The query plan should use the idx_segments_pane_seq index
    assert!(
        plan.contains("idx_segments_pane_seq") || plan.contains("USING INDEX"),
        "Query should use the pane_seq index, got: {plan}"
    );
}

// =========================================================================
// wa-4vx.3.5: Agent Sessions Storage Tests
// =========================================================================

#[test]
fn can_insert_agent_session() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    let session = AgentSessionRecord {
        id: 0,
        pane_id: 1,
        agent_type: "claude_code".to_string(),
        session_id: Some("sess-123".to_string()),
        external_id: Some("ext-456".to_string()),
        external_meta: None,
        started_at: now_ms,
        ended_at: None,
        end_reason: None,
        total_tokens: None,
        input_tokens: None,
        output_tokens: None,
        cached_tokens: None,
        reasoning_tokens: None,
        model_name: Some("opus-4.5".to_string()),
        estimated_cost_usd: None,
    };

    let backend = crate::storage_backend_trait::RusqliteBackend::new(conn);
    let session_id = upsert_agent_session_backend(&backend, &session).unwrap();
    assert!(session_id > 0, "Session should have been assigned an ID");

    let retrieved = query_agent_session_backend(&backend, session_id)
        .unwrap()
        .unwrap();
    assert_eq!(retrieved.pane_id, 1);
    assert_eq!(retrieved.agent_type, "claude_code");
}

#[test]
fn can_update_agent_session() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    let backend = crate::storage_backend_trait::RusqliteBackend::new(conn);
    let session = AgentSessionRecord::new_start(1, "codex");
    let session_id = upsert_agent_session_backend(&backend, &session).unwrap();

    let mut updated = AgentSessionRecord::new_start(1, "codex");
    updated.id = session_id;
    updated.ended_at = Some(now_ms + 60_000);
    updated.total_tokens = Some(5000);

    upsert_agent_session_backend(&backend, &updated).unwrap();

    let retrieved = query_agent_session_backend(&backend, session_id)
        .unwrap()
        .unwrap();
    assert_eq!(retrieved.total_tokens, Some(5000));
}

#[test]
fn query_active_sessions_filters_ended() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    // Active session
    let backend = crate::storage_backend_trait::RusqliteBackend::new(conn);
    let active = AgentSessionRecord::new_start(1, "claude");
    upsert_agent_session_backend(&backend, &active).unwrap();

    // Ended session
    let mut ended = AgentSessionRecord::new_start(1, "codex");
    ended.ended_at = Some(now_ms);
    upsert_agent_session_backend(&backend, &ended).unwrap();

    let results = query_active_sessions_backend(&backend).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].agent_type, "claude");
}

#[test]
fn agent_sessions_table_exists() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_sessions'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

// ─── PooledReadConn Drop guard (eb61dcef) ─────────────────────────────
//
// The Drop impl on PooledReadConn discards a connection that left a
// transaction open instead of returning it to the per-`db_path` LIFO
// read pool. Pre-fix, a closure that panicked or returned mid-tx
// could leak its open transaction state into the next pool consumer
// (partial read view, locks held, unrolled-back state).
//
// These tests pin the contract:
//   - clean (autocommit) connections still recycle into the pool
//   - dirty (open-tx) connections are discarded on Drop, not pooled
//   - the next acquire() on the same path opens a fresh handle in
//     autocommit mode (proving the dirty conn never reached the pool)
//
// Each test uses a unique tempdir path so they cannot collide with
// each other or with any other pool-touching test running in parallel.

#[test]
fn pooled_read_conn_drop_returns_clean_connection_to_pool() {
    let temp = tempfile::tempdir().unwrap();
    let db_file = temp.path().join("clean.db");
    let db_path = db_file.to_string_lossy().into_owned();

    {
        let conn = PooledReadConn::acquire(&db_path).unwrap();
        assert!(conn.is_autocommit(), "fresh conn must start in autocommit");
        // No transaction opened — drop should recycle into the pool.
    }

    let pool = read_pool().lock().unwrap();
    let entry = pool.get(&db_path).expect("pool entry must exist for path");
    assert_eq!(
        entry.len(),
        1,
        "clean connection must be recycled to the pool"
    );
}

#[test]
fn pooled_read_conn_drop_discards_connection_with_open_tx() {
    let temp = tempfile::tempdir().unwrap();
    let db_file = temp.path().join("open_tx.db");
    let db_path = db_file.to_string_lossy().into_owned();

    {
        let conn = PooledReadConn::acquire(&db_path).unwrap();
        // Mimic a borrowing closure that BEGIN'd and then panicked or
        // returned early without COMMIT/ROLLBACK. After this, the
        // Connection's autocommit flag is false — exactly the state
        // the Drop guard must detect.
        conn.execute_batch("BEGIN").unwrap();
        assert!(
            !conn.is_autocommit(),
            "BEGIN must put the conn out of autocommit mode"
        );
        // Drop fires here.
    }

    let pool = read_pool().lock().unwrap();
    let pooled = pool.get(&db_path).map(|v| v.len()).unwrap_or(0);
    assert_eq!(
        pooled, 0,
        "connection with open transaction must NOT be returned to pool"
    );
}

#[test]
fn pooled_read_conn_acquire_after_dirty_drop_yields_fresh_autocommit_conn() {
    let temp = tempfile::tempdir().unwrap();
    let db_file = temp.path().join("reacquire.db");
    let db_path = db_file.to_string_lossy().into_owned();

    {
        let conn = PooledReadConn::acquire(&db_path).unwrap();
        conn.execute_batch("BEGIN").unwrap();
        // Drop with open tx → guard discards.
    }

    // Subsequent acquire must open a fresh connection (the dirty one
    // never reached the pool). The fresh handle must be in autocommit
    // mode — i.e. the open transaction state did not leak.
    let next = PooledReadConn::acquire(&db_path).unwrap();
    assert!(
        next.is_autocommit(),
        "post-discard re-acquire must yield a clean autocommit connection"
    );
}

// =========================================================================
// ft-0ctwe: pin redactor coverage on the `ft robot send` audit path
// =========================================================================
//
// The `ft robot send <pane> "<text>"` CLI handler in main.rs persists an
// AuditActionRecord through `record_audit_action_redacted_with_cx`, which
// calls `action.redact_fields(&Redactor::new())` before the storage write.
// This test pins the invariant directly on `redact_fields` so a refactor
// that removes the redactor call from the storage helper still fails this
// test: the contract is "every secret-bearing field on a send-path audit
// record gets scrubbed before persistence."
//
// Mirrors the redactor coverage shipped in ft-3se13 (decision-log scrub)
// and ft-3xek9 (newer-provider tokens). When a new provider lands in
// Redactor::new(), this test gains a row.
fn make_send_audit_record_with_secret(secret: &str) -> AuditActionRecord {
    AuditActionRecord {
        id: 0,
        ts: 1_700_000_000_000,
        actor_kind: "robot".to_string(),
        actor_id: Some("test-robot".to_string()),
        correlation_id: None,
        pane_id: Some(7),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: Some(format!("send authorized; payload begins with {secret}")),
        rule_id: Some("policy.allow.send_text".to_string()),
        input_summary: Some(format!(
            "ft robot send 7 'export OPENAI_KEY={secret} && run_agent'"
        )),
        verification_summary: Some(format!("post-send echo: {secret}")),
        decision_context: Some(format!("{{\"text_summary\":\"{secret}\"}}")),
        result: "ok".to_string(),
    }
}

fn assert_no_plaintext(record: &AuditActionRecord, secret: &str, label: &str) {
    for (field_name, value) in [
        ("decision_reason", record.decision_reason.as_deref()),
        ("input_summary", record.input_summary.as_deref()),
        (
            "verification_summary",
            record.verification_summary.as_deref(),
        ),
        ("decision_context", record.decision_context.as_deref()),
    ] {
        if let Some(v) = value {
            assert!(
                !v.contains(secret),
                "ft-0ctwe ({label}): field `{field_name}` still contains \
                     plaintext secret after redact_fields. value={v:?}"
            );
        }
    }
}

#[test]
fn ft_0ctwe_send_audit_redacts_anthropic_key() {
    let secret = "sk-ant-api03-DEADBEEFCAFEBABE0123456789ABCDEF";
    let mut record = make_send_audit_record_with_secret(secret);
    record.redact_fields(&Redactor::new());
    assert_no_plaintext(&record, secret, "anthropic");
    // At least one field should have been actively scrubbed (i.e. show
    // the [REDACTED] marker), not just absent — proves the redactor
    // fired rather than the secret simply not being present.
    let any_redacted = [
        &record.decision_reason,
        &record.input_summary,
        &record.verification_summary,
        &record.decision_context,
    ]
    .into_iter()
    .filter_map(|f| f.as_deref())
    .any(|s| s.contains("[REDACTED]"));
    assert!(
        any_redacted,
        "ft-0ctwe: redact_fields must mark at least one field [REDACTED]"
    );
}

#[test]
fn ft_0ctwe_send_audit_redacts_openai_key() {
    let secret = "sk-proj-1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
    let mut record = make_send_audit_record_with_secret(secret);
    record.redact_fields(&Redactor::new());
    assert_no_plaintext(&record, secret, "openai-proj");
}

#[test]
fn ft_0ctwe_send_audit_redacts_github_pat() {
    let secret = "github_pat_11ABCDEFG0aBcDeFg_HiJkLmNoPqRsTuVwXyZ1234567890ABCDE";
    let mut record = make_send_audit_record_with_secret(secret);
    record.redact_fields(&Redactor::new());
    assert_no_plaintext(&record, secret, "github-pat");
}

#[test]
fn ft_0ctwe_send_audit_preserves_clean_text() {
    // Negative case — a benign send must NOT be corrupted by the
    // redactor (no false positives). Pre-fix this would also have
    // worked; the assertion is a safety net against an over-eager
    // future redactor pattern.
    let mut record = AuditActionRecord {
        id: 0,
        ts: 1_700_000_000_000,
        actor_kind: "robot".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: Some(0),
        domain: None,
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: Some("send authorized".to_string()),
        rule_id: None,
        input_summary: Some("ft robot send 0 'ls -la'".to_string()),
        verification_summary: None,
        decision_context: None,
        result: "ok".to_string(),
    };
    record.redact_fields(&Redactor::new());
    assert_eq!(record.decision_reason.as_deref(), Some("send authorized"));
    assert_eq!(
        record.input_summary.as_deref(),
        Some("ft robot send 0 'ls -la'")
    );
}
