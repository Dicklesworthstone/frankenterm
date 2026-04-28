//! Database Health Check & Repair
//!
//! [ft-fxymo / ft-dn2tu Phase 3] Extracted from `storage.rs` so the health/
//! repair surface lives next to its siblings (`mmap_store`, future
//! `types`, etc.). Public API is preserved via `pub use` re-exports in
//! `storage.rs`; existing callers (`ft doctor`, `ft db stats`,
//! `ft db repair`, proptest_storage) need no changes.
//!
//! Public surface:
//! - [`database_stats`] — collect per-table counts, top panes, event-type
//!   distribution, and cleanup suggestions.
//! - [`check_database_health`] — run the 5-check health report (integrity,
//!   schema version, foreign keys, FTS index, WAL).
//! - [`repair_database`] — rebuild FTS / checkpoint WAL / VACUUM, with
//!   optional dry-run and backup.
//!
//! Phase 2 (ft-6qkx1) will move the `Db*Report` types into
//! `storage/types.rs`; until that lands they live here alongside the
//! functions that produce them.
//!
//! Note: `WAL_RECOVERY_THRESHOLD` is owned by `storage.rs` (used by
//! `check_and_recover_wal`) and exposed to this module via `pub(crate)`.

use std::path::Path;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::{SCHEMA_VERSION, WAL_RECOVERY_THRESHOLD, get_user_version};
use crate::error::{Result, StorageError};

/// Status of a single database health check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbCheckStatus {
    Ok,
    Warning,
    Error,
}

impl std::fmt::Display for DbCheckStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "OK"),
            Self::Warning => write!(f, "WARNING"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

/// Result of a single health check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbCheckItem {
    pub name: String,
    pub status: DbCheckStatus,
    pub detail: Option<String>,
}

/// Full database health report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbCheckReport {
    pub db_path: String,
    pub db_exists: bool,
    pub db_size_bytes: Option<u64>,
    pub schema_version: Option<i32>,
    pub checks: Vec<DbCheckItem>,
}

impl DbCheckReport {
    /// Whether any check has error status.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.checks.iter().any(|c| c.status == DbCheckStatus::Error)
    }

    /// Whether any check has warning status.
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        self.checks
            .iter()
            .any(|c| c.status == DbCheckStatus::Warning)
    }

    /// Count of problems (errors + warnings).
    #[must_use]
    pub fn problem_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status != DbCheckStatus::Ok)
            .count()
    }
}

/// Result of a single repair operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbRepairItem {
    pub name: String,
    pub success: bool,
    pub detail: String,
}

/// Full repair report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbRepairReport {
    pub backup_path: Option<String>,
    pub repairs: Vec<DbRepairItem>,
}

impl DbRepairReport {
    /// Whether all repairs succeeded.
    #[must_use]
    pub fn all_succeeded(&self) -> bool {
        self.repairs.iter().all(|r| r.success)
    }
}

/// Per-table row count for the stats report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableStats {
    pub name: String,
    pub row_count: u64,
}

/// Per-pane storage summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneStats {
    pub pane_id: u64,
    pub title: Option<String>,
    pub segment_count: u64,
    pub segment_bytes: u64,
    pub event_count: u64,
}

/// Event type distribution entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventTypeStats {
    pub event_type: String,
    pub count: u64,
}

/// Full database statistics report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbStatsReport {
    pub db_path: String,
    pub db_size_bytes: Option<u64>,
    pub tables: Vec<TableStats>,
    pub top_panes: Vec<PaneStats>,
    pub event_types: Vec<EventTypeStats>,
    pub suggestions: Vec<String>,
}

/// Collect storage statistics for `db_path`.
///
/// Returns row counts per table, top panes by data volume, event type
/// distribution, and cleanup suggestions referencing dry-run commands.
///
/// # Errors
///
/// ft-oqfsx: previous implementation swallowed every SQL error via
/// `.unwrap_or(0)` / `.ok()` chains, so a corrupt DB still rendered
/// `ft db stats` as a green report (`events: 0 rows`, `top_panes: []`).
/// The fix propagates SQL errors as [`StorageError::Database`] so
/// callers — including `ft doctor` and `ft db stats` — fail loud.
/// The pre-flight `Connection::open` failure path is preserved
/// inside the `Ok` arm with a `Database could not be opened.`
/// suggestion so non-existent DB files still report sensibly.
pub fn database_stats(db_path: &Path, retention_days: u32) -> Result<DbStatsReport> {
    let path_str = db_path.display().to_string();
    let db_size_bytes = std::fs::metadata(db_path).ok().map(|m| m.len());

    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(err) => {
            // Open failure remains a non-fatal "report-only" branch so
            // callers can still distinguish "DB not yet created" from
            // "DB exists but query failed". Logged as warn so doctor /
            // ops surfaces have something to grep for.
            tracing::warn!(
                db_path = %path_str,
                error = %err,
                "database_stats: open failed; returning empty report"
            );
            return Ok(DbStatsReport {
                db_path: path_str,
                db_size_bytes,
                tables: vec![],
                top_panes: vec![],
                event_types: vec![],
                suggestions: vec!["Database could not be opened.".to_string()],
            });
        }
    };
    // database_stats reads the live DB while writers may be active;
    // without busy_timeout, SELECT COUNT(*) returns SQLITE_BUSY immediately
    // and every table row reports as -1 even though the DB is healthy.
    if let Err(err) = conn.busy_timeout(std::time::Duration::from_secs(5)) {
        tracing::warn!(
            db_path = %path_str,
            error = %err,
            "database_stats: busy_timeout could not be set; counts may report SQLITE_BUSY"
        );
    }

    // Table row counts.
    //
    // ft-oqfsx: any per-table failure here previously coerced to 0,
    // so a corrupt `events` table looked indistinguishable from an
    // empty one. Now propagate the first failure with table context.
    let table_names = [
        "panes",
        "output_segments",
        "events",
        "audit_actions",
        "usage_metrics",
        "notification_history",
        "workflow_executions",
        "maintenance_log",
    ];
    let mut tables = Vec::with_capacity(table_names.len());
    for name in &table_names {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |row| {
                row.get(0)
            })
            .map_err(|err| {
                tracing::error!(
                    db_path = %path_str,
                    table = %name,
                    error = %err,
                    "database_stats: count query failed"
                );
                StorageError::Database(format!(
                    "database_stats: count query failed for table {name}: {err}"
                ))
            })?;
        tables.push(TableStats {
            name: (*name).to_string(),
            row_count: count as u64,
        });
    }

    // Top panes by segment volume (count + total content_len). ft-oqfsx:
    // prepare/query_map errors previously degraded silently to an empty
    // list; now they propagate so a corrupt output_segments table fails
    // loud instead of pretending no panes exist.
    let mut top_panes_stmt = conn
        .prepare(
            "SELECT s.pane_id, p.title,
                    COUNT(*) as seg_count,
                    COALESCE(SUM(s.content_len), 0) as seg_bytes,
                    (SELECT COUNT(*) FROM events e WHERE e.pane_id = s.pane_id) as evt_count
             FROM output_segments s
             LEFT JOIN panes p ON p.pane_id = s.pane_id
             GROUP BY s.pane_id
             ORDER BY seg_bytes DESC
             LIMIT 10",
        )
        .map_err(|err| {
            tracing::error!(
                db_path = %path_str,
                error = %err,
                "database_stats: top-panes prepare failed"
            );
            StorageError::Database(format!("database_stats: top-panes prepare failed: {err}"))
        })?;

    let top_panes: Vec<PaneStats> = top_panes_stmt
        .query_map([], |row| {
            Ok(PaneStats {
                pane_id: row.get::<_, i64>(0)? as u64,
                title: row.get(1)?,
                segment_count: row.get::<_, i64>(2)? as u64,
                segment_bytes: row.get::<_, i64>(3)? as u64,
                event_count: row.get::<_, i64>(4)? as u64,
            })
        })
        .map_err(|err| {
            tracing::error!(
                db_path = %path_str,
                error = %err,
                "database_stats: top-panes query_map failed"
            );
            StorageError::Database(format!("database_stats: top-panes query failed: {err}"))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|err| {
            tracing::error!(
                db_path = %path_str,
                error = %err,
                "database_stats: top-panes row mapping failed"
            );
            StorageError::Database(format!(
                "database_stats: top-panes row mapping failed: {err}"
            ))
        })?;

    // Event type distribution. Same propagation discipline as top_panes.
    let mut event_types_stmt = conn
        .prepare(
            "SELECT event_type, COUNT(*) as cnt
             FROM events
             GROUP BY event_type
             ORDER BY cnt DESC",
        )
        .map_err(|err| {
            tracing::error!(
                db_path = %path_str,
                error = %err,
                "database_stats: event-types prepare failed"
            );
            StorageError::Database(format!(
                "database_stats: event-types prepare failed: {err}"
            ))
        })?;

    let event_types: Vec<EventTypeStats> = event_types_stmt
        .query_map([], |row| {
            Ok(EventTypeStats {
                event_type: row.get(0)?,
                count: row.get::<_, i64>(1)? as u64,
            })
        })
        .map_err(|err| {
            tracing::error!(
                db_path = %path_str,
                error = %err,
                "database_stats: event-types query_map failed"
            );
            StorageError::Database(format!(
                "database_stats: event-types query failed: {err}"
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|err| {
            tracing::error!(
                db_path = %path_str,
                error = %err,
                "database_stats: event-types row mapping failed"
            );
            StorageError::Database(format!(
                "database_stats: event-types row mapping failed: {err}"
            ))
        })?;

    // Cleanup suggestions
    let mut suggestions = Vec::new();

    let total_events: u64 = tables
        .iter()
        .find(|t| t.name == "events")
        .map_or(0, |t| t.row_count);
    let total_segments: u64 = tables
        .iter()
        .find(|t| t.name == "output_segments")
        .map_or(0, |t| t.row_count);

    if total_events > 10_000 {
        suggestions.push(format!(
            "{total_events} events stored. Preview cleanup: wa cleanup --dry-run"
        ));
    }
    if total_segments > 50_000 {
        suggestions.push(format!(
            "{total_segments} segments stored. Preview cleanup: wa cleanup --dry-run"
        ));
    }
    if let Some(size) = db_size_bytes {
        if size > 100 * 1024 * 1024 {
            suggestions.push(format!(
                "Database is {:.1} MB. Consider: wa db repair --dry-run (includes VACUUM)",
                size as f64 / 1_048_576.0
            ));
        }
    }
    if retention_days > 0 {
        suggestions.push(format!(
            "Retention policy: {retention_days} days. Configure in ft.toml [storage] retention_days"
        ));
    }
    if suggestions.is_empty() {
        suggestions.push("Database looks healthy. No cleanup actions needed.".to_string());
    }

    Ok(DbStatsReport {
        db_path: path_str,
        db_size_bytes,
        tables,
        top_panes,
        event_types,
        suggestions,
    })
}

/// Run health checks on the database at `db_path`.
///
/// Checks performed:
/// 1. SQLite quick integrity check
/// 2. Schema version validation
/// 3. Foreign key consistency
/// 4. FTS index integrity
/// 5. WAL status
#[must_use]
pub fn check_database_health(db_path: &Path) -> DbCheckReport {
    let path_str = db_path.display().to_string();
    let db_exists = db_path.exists();

    if !db_exists {
        return DbCheckReport {
            db_path: path_str,
            db_exists: false,
            db_size_bytes: None,
            schema_version: None,
            checks: vec![DbCheckItem {
                name: "Database file".to_string(),
                status: DbCheckStatus::Error,
                detail: Some("Database file does not exist".to_string()),
            }],
        };
    }

    let db_size_bytes = std::fs::metadata(db_path).ok().map(|m| m.len());

    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            return DbCheckReport {
                db_path: path_str,
                db_exists: true,
                db_size_bytes,
                schema_version: None,
                checks: vec![DbCheckItem {
                    name: "Database open".to_string(),
                    status: DbCheckStatus::Error,
                    detail: Some(format!("Failed to open database: {e}")),
                }],
            };
        }
    };
    // db_check runs PRAGMA queries against the live DB; without
    // busy_timeout, integrity_check + foreign_key_check return SQLITE_BUSY
    // and the report falsely flags a healthy DB as broken.
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));

    let mut checks = Vec::new();
    let mut schema_version = None;

    // 1. SQLite integrity check
    match conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0)) {
        Ok(result) if result == "ok" => {
            checks.push(DbCheckItem {
                name: "SQLite integrity".to_string(),
                status: DbCheckStatus::Ok,
                detail: None,
            });
        }
        Ok(result) => {
            checks.push(DbCheckItem {
                name: "SQLite integrity".to_string(),
                status: DbCheckStatus::Error,
                detail: Some(format!("CORRUPT: {result}")),
            });
        }
        Err(e) => {
            checks.push(DbCheckItem {
                name: "SQLite integrity".to_string(),
                status: DbCheckStatus::Error,
                detail: Some(format!("Check failed: {e}")),
            });
        }
    }

    // 2. Schema version
    match get_user_version(&conn) {
        Ok(v) => {
            schema_version = Some(v);
            let (status, detail) = match v.cmp(&SCHEMA_VERSION) {
                std::cmp::Ordering::Equal => (DbCheckStatus::Ok, format!("{v} (current)")),
                std::cmp::Ordering::Less => (
                    DbCheckStatus::Warning,
                    format!("{v} (needs migration to {SCHEMA_VERSION})"),
                ),
                std::cmp::Ordering::Greater => (
                    DbCheckStatus::Error,
                    format!("{v} (newer than supported {SCHEMA_VERSION})"),
                ),
            };
            checks.push(DbCheckItem {
                name: "Schema version".to_string(),
                status,
                detail: Some(detail),
            });
        }
        Err(e) => {
            checks.push(DbCheckItem {
                name: "Schema version".to_string(),
                status: DbCheckStatus::Error,
                detail: Some(format!("Failed to read: {e}")),
            });
        }
    }

    // 3. Foreign key check
    match conn.query_row("PRAGMA foreign_key_check", [], |row| {
        row.get::<_, String>(0)
    }) {
        Ok(table) => {
            // If a row is returned, there's a violation
            checks.push(DbCheckItem {
                name: "Foreign keys".to_string(),
                status: DbCheckStatus::Warning,
                detail: Some(format!("Violation in table: {table}")),
            });
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            checks.push(DbCheckItem {
                name: "Foreign keys".to_string(),
                status: DbCheckStatus::Ok,
                detail: None,
            });
        }
        Err(e) => {
            checks.push(DbCheckItem {
                name: "Foreign keys".to_string(),
                status: DbCheckStatus::Error,
                detail: Some(format!("Check failed: {e}")),
            });
        }
    }

    // 4. FTS index integrity
    match conn.execute(
        "INSERT INTO output_segments_fts(output_segments_fts) VALUES('integrity-check')",
        [],
    ) {
        Ok(_) => {
            checks.push(DbCheckItem {
                name: "FTS index".to_string(),
                status: DbCheckStatus::Ok,
                detail: None,
            });
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("no such table") {
                checks.push(DbCheckItem {
                    name: "FTS index".to_string(),
                    status: DbCheckStatus::Error,
                    detail: Some("FTS table missing".to_string()),
                });
            } else {
                checks.push(DbCheckItem {
                    name: "FTS index".to_string(),
                    status: DbCheckStatus::Error,
                    detail: Some(format!("CORRUPT: {msg}")),
                });
            }
        }
    }

    // 5. WAL checkpoint status
    let wal_path = format!("{}-wal", db_path.display());
    let wal_exists = Path::new(&wal_path).exists();
    match conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    }) {
        Ok((_busy, wal_frames, checkpointed)) => {
            if wal_frames > WAL_RECOVERY_THRESHOLD {
                checks.push(DbCheckItem {
                    name: "WAL checkpoint".to_string(),
                    status: DbCheckStatus::Warning,
                    detail: Some(format!(
                        "Large WAL: {wal_frames} frames ({checkpointed} checkpointed)"
                    )),
                });
            } else if wal_exists && wal_frames > 0 {
                checks.push(DbCheckItem {
                    name: "WAL checkpoint".to_string(),
                    status: DbCheckStatus::Ok,
                    detail: Some(format!("{wal_frames} frames pending")),
                });
            } else {
                checks.push(DbCheckItem {
                    name: "WAL checkpoint".to_string(),
                    status: DbCheckStatus::Ok,
                    detail: None,
                });
            }
        }
        Err(e) => {
            checks.push(DbCheckItem {
                name: "WAL checkpoint".to_string(),
                status: DbCheckStatus::Error,
                detail: Some(format!("Check failed: {e}")),
            });
        }
    }

    DbCheckReport {
        db_path: path_str,
        db_exists,
        db_size_bytes,
        schema_version,
        checks,
    }
}

/// Repair the database at `db_path`.
///
/// Repairs performed based on detected issues:
/// 1. Rebuild FTS index from source table
/// 2. Checkpoint and truncate WAL
/// 3. VACUUM to reclaim space
///
/// Creates a backup before any modifications unless `skip_backup` is true.
pub fn repair_database(db_path: &Path, dry_run: bool, skip_backup: bool) -> Result<DbRepairReport> {
    if !db_path.exists() {
        return Err(
            StorageError::Database(format!("Database not found: {}", db_path.display())).into(),
        );
    }

    // Create backup unless skipped
    let backup_path = if !dry_run && !skip_backup {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let backup = format!("{}.bak.{ts}", db_path.display());
        match std::fs::copy(db_path, &backup) {
            Ok(_) => Some(backup),
            Err(e) => {
                return Err(StorageError::Database(format!("Failed to create backup: {e}")).into());
            }
        }
    } else {
        None
    };

    let mut repairs = Vec::new();

    let conn = Connection::open(db_path)
        .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;
    // repair_database performs FTS rebuild + WAL checkpoint + VACUUM, all
    // of which need the write lock. Without busy_timeout, any active reader
    // makes the entire repair fail at the first PRAGMA. Match the standard
    // 5s recipe so SQLite retries internally.
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));

    // 1. Rebuild FTS index
    let fts_needs_rebuild = conn
        .execute(
            "INSERT INTO output_segments_fts(output_segments_fts) VALUES('integrity-check')",
            [],
        )
        .is_err();

    if fts_needs_rebuild {
        if dry_run {
            repairs.push(DbRepairItem {
                name: "FTS index rebuild".to_string(),
                success: true,
                detail: "Would rebuild FTS index from output_segments table".to_string(),
            });
        } else {
            match conn.execute(
                "INSERT INTO output_segments_fts(output_segments_fts) VALUES('rebuild')",
                [],
            ) {
                Ok(_) => {
                    let count: i64 = conn
                        .query_row("SELECT COUNT(*) FROM output_segments", [], |row| row.get(0))
                        .unwrap_or(0);
                    repairs.push(DbRepairItem {
                        name: "FTS index rebuild".to_string(),
                        success: true,
                        detail: format!("Rebuilt FTS index ({count} segments indexed)"),
                    });
                }
                Err(e) => {
                    repairs.push(DbRepairItem {
                        name: "FTS index rebuild".to_string(),
                        success: false,
                        detail: format!("Failed to rebuild FTS index: {e}"),
                    });
                }
            }
        }
    } else {
        repairs.push(DbRepairItem {
            name: "FTS index".to_string(),
            success: true,
            detail: "FTS index is healthy, no rebuild needed".to_string(),
        });
    }

    // 2. WAL checkpoint + truncate
    match conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        row.get::<_, i64>(1)
    }) {
        Ok(wal_frames) if wal_frames > 0 => {
            if dry_run {
                repairs.push(DbRepairItem {
                    name: "WAL checkpoint".to_string(),
                    success: true,
                    detail: format!("Would checkpoint and truncate WAL ({wal_frames} frames)"),
                });
            } else {
                match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok::<_, rusqlite::Error>((row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
                }) {
                    Ok((frames, checkpointed)) => {
                        repairs.push(DbRepairItem {
                            name: "WAL checkpoint".to_string(),
                            success: true,
                            detail: format!("Checkpointed WAL ({checkpointed}/{frames} frames)"),
                        });
                    }
                    Err(e) => {
                        repairs.push(DbRepairItem {
                            name: "WAL checkpoint".to_string(),
                            success: false,
                            detail: format!("WAL checkpoint failed: {e}"),
                        });
                    }
                }
            }
        }
        Ok(_) => {
            repairs.push(DbRepairItem {
                name: "WAL checkpoint".to_string(),
                success: true,
                detail: "WAL is clean, no checkpoint needed".to_string(),
            });
        }
        Err(e) => {
            repairs.push(DbRepairItem {
                name: "WAL checkpoint".to_string(),
                success: false,
                detail: format!("WAL status check failed: {e}"),
            });
        }
    }

    // 3. VACUUM
    if dry_run {
        repairs.push(DbRepairItem {
            name: "Vacuum".to_string(),
            success: true,
            detail: "Would vacuum database to reclaim space".to_string(),
        });
    } else {
        match conn.execute_batch("VACUUM") {
            Ok(()) => {
                repairs.push(DbRepairItem {
                    name: "Vacuum".to_string(),
                    success: true,
                    detail: "Database vacuumed".to_string(),
                });
            }
            Err(e) => {
                repairs.push(DbRepairItem {
                    name: "Vacuum".to_string(),
                    success: false,
                    detail: format!("Vacuum failed: {e}"),
                });
            }
        }
    }

    Ok(DbRepairReport {
        backup_path,
        repairs,
    })
}
