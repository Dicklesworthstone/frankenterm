//! ft-u6fba Phase 1b: extracted from storage.rs (mod db_check_repair_tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to
//! `crate::storage::*`.

    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DB_CHECK_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_path(prefix: &str) -> String {
        let id = DB_CHECK_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        format!("/tmp/wa_dbcheck_{prefix}_{pid}_{id}.db")
    }

    fn cleanup_db(path: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-shm"));
        // Also clean up any backup files
        if let Ok(entries) = std::fs::read_dir("/tmp") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&path.replace("/tmp/", "")) && name.contains(".bak.") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    #[test]
    fn check_nonexistent_db() {
        let path = temp_db_path("nonexist");
        let report = check_database_health(Path::new(&path));

        assert!(!report.db_exists);
        assert!(report.has_errors());
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].status, DbCheckStatus::Error);
    }

    // ── ft-oqfsx: database_stats Result propagation ──────────────────────
    //
    // Pre-fix: a corrupt DB silently presented as a green report
    // (events: 0 rows, top_panes: []) because every SQL query was
    // wrapped in `.unwrap_or_default()` / `.ok()`. ft doctor and
    // ft db stats lost the ability to detect corruption. Post-fix:
    // database_stats returns Result<DbStatsReport, StorageError>,
    // SQL failures propagate as StorageError::Database, and a
    // tracing::error event is emitted for ops surfaces.

    #[test]
    fn database_stats_returns_ok_on_healthy_db() {
        let path = temp_db_path("oqfsx_healthy");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report =
            database_stats(Path::new(&path), 30).expect("ft-oqfsx: healthy DB must return Ok");

        // Pre-fix this assertion existed implicitly; we now pin it
        // explicitly so a regression to .unwrap_or_default() cannot
        // silently swallow a future schema break.
        assert_eq!(report.tables.len(), 8);
        cleanup_db(&path);
    }

    #[test]
    fn database_stats_returns_open_failure_via_suggestion_for_missing_db() {
        // Connection::open on a path that does not exist is allowed
        // (sqlite creates the file). Use a path under a directory that
        // does not exist so open genuinely fails.
        let path = "/tmp/ft_oqfsx_does_not_exist/missing.db";
        let report = database_stats(Path::new(path), 30)
            .expect("ft-oqfsx: missing-DB path must return Ok with suggestion");

        // ft-oqfsx contract: open failure stays in the Ok arm so callers
        // can distinguish "DB not yet created" from "DB exists but
        // queries failed". The suggestion contains the canonical text.
        assert!(report.tables.is_empty());
        assert!(report.top_panes.is_empty());
        assert!(report.event_types.is_empty());
        assert!(
            report
                .suggestions
                .iter()
                .any(|s| s.contains("could not be opened")),
            "ft-oqfsx: open-failure suggestion missing; got {:?}",
            report.suggestions
        );
    }

    #[test]
    fn database_stats_returns_err_when_table_dropped_after_open() {
        // Construct a healthy DB, then drop one of the tables that
        // database_stats counts. The stats query for that table must
        // surface as StorageError::Database rather than silently
        // reporting 0 rows.
        let path = temp_db_path("oqfsx_corrupt");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
            conn.execute_batch("DROP TABLE events").unwrap();
        }

        let err = database_stats(Path::new(&path), 30)
            .expect_err("ft-oqfsx: dropped-events-table DB must return Err");

        // Match shape: StorageError::Database wrapping a message that
        // names the failing table or query phase.
        let msg = err.to_string();
        assert!(
            msg.contains("events") || msg.contains("event"),
            "ft-oqfsx: error message must reference the failing table; got {msg:?}"
        );
        cleanup_db(&path);
    }

    #[test]
    fn check_healthy_db() {
        let path = temp_db_path("healthy");
        // Create and initialize a valid database
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = check_database_health(Path::new(&path));

        assert!(report.db_exists);
        assert!(report.db_size_bytes.is_some());
        assert_eq!(report.schema_version, Some(SCHEMA_VERSION));
        assert!(!report.has_errors());
        assert_eq!(report.problem_count(), 0);

        // All checks should be OK
        for check in &report.checks {
            assert_eq!(
                check.status,
                DbCheckStatus::Ok,
                "Check '{}' was {:?}: {:?}",
                check.name,
                check.status,
                check.detail
            );
        }

        cleanup_db(&path);
    }

    #[test]
    fn check_old_schema_version() {
        let path = temp_db_path("oldschema");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
            // Set an older schema version
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
        }

        let report = check_database_health(Path::new(&path));

        assert!(report.db_exists);
        assert_eq!(report.schema_version, Some(1));
        assert!(report.has_warnings());

        let schema_check = report
            .checks
            .iter()
            .find(|c| c.name == "Schema version")
            .unwrap();
        assert_eq!(schema_check.status, DbCheckStatus::Warning);
        assert!(
            schema_check
                .detail
                .as_ref()
                .unwrap()
                .contains("needs migration")
        );

        cleanup_db(&path);
    }

    #[test]
    fn check_future_schema_version() {
        let path = temp_db_path("future");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
            conn.execute_batch("PRAGMA user_version = 999").unwrap();
        }

        let report = check_database_health(Path::new(&path));

        assert_eq!(report.schema_version, Some(999));
        assert!(report.has_errors());

        let schema_check = report
            .checks
            .iter()
            .find(|c| c.name == "Schema version")
            .unwrap();
        assert_eq!(schema_check.status, DbCheckStatus::Error);
        assert!(
            schema_check
                .detail
                .as_ref()
                .unwrap()
                .contains("newer than supported")
        );

        cleanup_db(&path);
    }

    #[test]
    fn check_report_serializes_to_json() {
        let path = temp_db_path("json");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = check_database_health(Path::new(&path));
        let json = serde_json::to_string(&report).unwrap();
        let parsed: DbCheckReport = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.db_exists, report.db_exists);
        assert_eq!(parsed.checks.len(), report.checks.len());

        cleanup_db(&path);
    }

    #[test]
    fn repair_nonexistent_db_fails() {
        let path = temp_db_path("repairne");
        let result = repair_database(Path::new(&path), false, true);
        assert!(result.is_err());
    }

    #[test]
    fn repair_dry_run_makes_no_changes() {
        let path = temp_db_path("dryrun");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let size_before = std::fs::metadata(&path).unwrap().len();
        let report = repair_database(Path::new(&path), true, true).unwrap();

        // Dry run should not create a backup
        assert!(report.backup_path.is_none());
        assert!(report.all_succeeded());

        // File should still exist and be similar size
        let size_after = std::fs::metadata(&path).unwrap().len();
        assert_eq!(size_before, size_after);

        cleanup_db(&path);
    }

    #[test]
    fn repair_creates_backup() {
        let path = temp_db_path("backup");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = repair_database(Path::new(&path), false, false).unwrap();

        assert!(report.backup_path.is_some());
        let backup = report.backup_path.as_ref().unwrap();
        assert!(Path::new(backup).exists(), "Backup file should exist");

        // Clean up backup
        let _ = std::fs::remove_file(backup);
        cleanup_db(&path);
    }

    #[test]
    fn repair_skips_backup_when_requested() {
        let path = temp_db_path("nobackup");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = repair_database(Path::new(&path), false, true).unwrap();

        assert!(report.backup_path.is_none());
        assert!(report.all_succeeded());

        cleanup_db(&path);
    }

    #[test]
    fn repair_healthy_db_reports_no_action_needed() {
        let path = temp_db_path("noaction");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = repair_database(Path::new(&path), false, true).unwrap();

        assert!(report.all_succeeded());
        // Each item should indicate no repair was needed
        let fts_item = report
            .repairs
            .iter()
            .find(|r| r.name == "FTS index")
            .unwrap();
        assert!(fts_item.detail.contains("healthy"));

        cleanup_db(&path);
    }

    #[test]
    fn repair_report_serializes_to_json() {
        let path = temp_db_path("repjson");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = repair_database(Path::new(&path), true, true).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        let parsed: DbRepairReport = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.repairs.len(), report.repairs.len());

        cleanup_db(&path);
    }

    #[test]
    fn check_report_methods() {
        let report = DbCheckReport {
            db_path: "/test".to_string(),
            db_exists: true,
            db_size_bytes: Some(1024),
            schema_version: Some(7),
            checks: vec![
                DbCheckItem {
                    name: "ok_check".to_string(),
                    status: DbCheckStatus::Ok,
                    detail: None,
                },
                DbCheckItem {
                    name: "warn_check".to_string(),
                    status: DbCheckStatus::Warning,
                    detail: Some("warning".to_string()),
                },
                DbCheckItem {
                    name: "err_check".to_string(),
                    status: DbCheckStatus::Error,
                    detail: Some("error".to_string()),
                },
            ],
        };

        assert!(report.has_errors());
        assert!(report.has_warnings());
        assert_eq!(report.problem_count(), 2);
    }

    #[test]
    fn check_status_display() {
        assert_eq!(DbCheckStatus::Ok.to_string(), "OK");
        assert_eq!(DbCheckStatus::Warning.to_string(), "WARNING");
        assert_eq!(DbCheckStatus::Error.to_string(), "ERROR");
    }
