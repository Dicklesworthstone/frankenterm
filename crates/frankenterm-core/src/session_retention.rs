//! Session data retention and cleanup.
//!
//! Implements retention policies for session persistence data to prevent
//! unbounded growth. Cleans up old sessions by age, count, and total size.
//!
//! # Cleanup order
//!
//! 1. Delete sessions older than `max_age_days` (skip active sessions)
//! 2. Delete excess closed sessions beyond `max_closed_sessions` (oldest first)
//! 3. If total size exceeds `max_total_size_mb`, delete oldest closed sessions
//! 4. Clean orphaned data (pane_state without checkpoint, checkpoint without session)
//!
//! Cascade: session deletion cascades to checkpoints -> pane_state via FK.

use std::sync::Arc;

use rusqlite::Connection;
use tracing::{debug, info, warn};

use crate::config::SessionRetentionConfig;

/// Result of a session cleanup operation.
#[derive(Debug, Clone, Default)]
pub struct CleanupResult {
    /// Sessions deleted by age policy.
    pub deleted_by_age: usize,
    /// Sessions deleted by count limit.
    pub deleted_by_count: usize,
    /// Sessions deleted by size budget.
    pub deleted_by_size: usize,
    /// Orphaned checkpoint rows cleaned.
    pub orphaned_checkpoints: usize,
    /// Orphaned pane_state rows cleaned.
    pub orphaned_pane_states: usize,
    /// Whether VACUUM was run.
    pub vacuumed: bool,
}

impl CleanupResult {
    /// Total number of sessions deleted.
    #[must_use]
    pub fn total_sessions_deleted(&self) -> usize {
        self.deleted_by_age + self.deleted_by_count + self.deleted_by_size
    }

    /// Whether any cleanup was performed.
    #[must_use]
    pub fn any_work_done(&self) -> bool {
        self.total_sessions_deleted() > 0
            || self.orphaned_checkpoints > 0
            || self.orphaned_pane_states > 0
    }
}

/// Run the full session cleanup pipeline.
///
/// Designed to be called from `tokio::task::spawn_blocking` since all
/// operations are synchronous SQLite calls.
///
/// # Errors
/// Returns error if database operations fail.
pub fn cleanup_sessions(
    conn: &Connection,
    config: &SessionRetentionConfig,
) -> Result<CleanupResult, rusqlite::Error> {
    let mut result = CleanupResult::default();

    // 1. Delete sessions older than max_age_days
    if config.max_age_days > 0 {
        result.deleted_by_age = delete_sessions_by_age(conn, config.max_age_days)?;
        if result.deleted_by_age > 0 {
            info!(
                deleted = result.deleted_by_age,
                max_age_days = config.max_age_days,
                "Cleaned up old sessions by age"
            );
        }
    }

    // 2. Delete excess closed sessions
    if config.max_closed_sessions > 0 {
        result.deleted_by_count =
            delete_excess_closed_sessions(conn, config.max_closed_sessions)?;
        if result.deleted_by_count > 0 {
            info!(
                deleted = result.deleted_by_count,
                max = config.max_closed_sessions,
                "Cleaned up excess closed sessions"
            );
        }
    }

    // 3. Delete by size budget
    if config.max_total_size_mb > 0 {
        result.deleted_by_size =
            delete_sessions_by_size(conn, config.max_total_size_mb)?;
        if result.deleted_by_size > 0 {
            info!(
                deleted = result.deleted_by_size,
                max_mb = config.max_total_size_mb,
                "Cleaned up sessions by size budget"
            );
        }
    }

    // 4. Clean orphaned data
    let (orphan_cp, orphan_ps) = cleanup_orphaned_data(conn)?;
    result.orphaned_checkpoints = orphan_cp;
    result.orphaned_pane_states = orphan_ps;
    if orphan_cp > 0 || orphan_ps > 0 {
        warn!(
            orphaned_checkpoints = orphan_cp,
            orphaned_pane_states = orphan_ps,
            "Cleaned up orphaned session data"
        );
    }

    // 5. VACUUM only if significant cleanup was performed
    let total_deleted = result.total_sessions_deleted();
    if total_deleted >= 10 {
        debug!("Running VACUUM after significant cleanup ({total_deleted} sessions deleted)");
        // VACUUM can be expensive; only run if we freed a meaningful amount
        if let Err(e) = conn.execute_batch("VACUUM") {
            warn!(error = %e, "VACUUM failed (non-critical)");
        } else {
            result.vacuumed = true;
        }
    }

    Ok(result)
}

/// Delete closed sessions older than `max_age_days`.
///
/// Active sessions (shutdown_clean = 0 with recent checkpoints) are preserved.
fn delete_sessions_by_age(
    conn: &Connection,
    max_age_days: u64,
) -> Result<usize, rusqlite::Error> {
    let cutoff_ms = epoch_ms().saturating_sub(max_age_days * 86_400_000);

    let deleted = conn.execute(
        "DELETE FROM mux_sessions
         WHERE created_at < ?1
         AND shutdown_clean = 1",
        [cutoff_ms as i64],
    )?;

    Ok(deleted)
}

/// Delete excess closed sessions, keeping the most recent `max_count`.
fn delete_excess_closed_sessions(
    conn: &Connection,
    max_count: usize,
) -> Result<usize, rusqlite::Error> {
    let deleted = conn.execute(
        "DELETE FROM mux_sessions
         WHERE shutdown_clean = 1
         AND session_id NOT IN (
             SELECT session_id FROM mux_sessions
             WHERE shutdown_clean = 1
             ORDER BY COALESCE(last_checkpoint_at, created_at) DESC
             LIMIT ?1
         )",
        [max_count as i64],
    )?;

    Ok(deleted)
}

/// Delete oldest closed sessions until total session data size is under budget.
fn delete_sessions_by_size(
    conn: &Connection,
    max_total_mb: u64,
) -> Result<usize, rusqlite::Error> {
    let max_bytes = max_total_mb * 1_024 * 1_024;

    // Get total size of session data
    let total_bytes: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(total_bytes), 0) FROM session_checkpoints",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if (total_bytes as u64) <= max_bytes {
        return Ok(0);
    }

    // Need to free: total_bytes - max_bytes
    let to_free = (total_bytes as u64).saturating_sub(max_bytes);
    let mut freed: u64 = 0;
    let mut deleted = 0;

    // Get closed sessions ordered oldest first, with their checkpoint sizes
    let mut stmt = conn.prepare(
        "SELECT s.session_id, COALESCE(SUM(c.total_bytes), 0) as session_bytes
         FROM mux_sessions s
         LEFT JOIN session_checkpoints c ON c.session_id = s.session_id
         WHERE s.shutdown_clean = 1
         GROUP BY s.session_id
         ORDER BY COALESCE(s.last_checkpoint_at, s.created_at) ASC",
    )?;

    let sessions: Vec<(String, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    for (session_id, session_bytes) in sessions {
        if freed >= to_free {
            break;
        }

        conn.execute(
            "DELETE FROM mux_sessions WHERE session_id = ?1",
            [&session_id],
        )?;

        freed += session_bytes as u64;
        deleted += 1;

        debug!(
            session_id = %session_id,
            freed_bytes = session_bytes,
            total_freed = freed,
            target = to_free,
            "Deleted session for size budget"
        );
    }

    Ok(deleted)
}

/// Clean orphaned data that lost its parent reference.
///
/// Returns (orphaned_checkpoints, orphaned_pane_states).
fn cleanup_orphaned_data(conn: &Connection) -> Result<(usize, usize), rusqlite::Error> {
    // Orphaned pane_state rows (checkpoint_id references deleted checkpoint)
    let orphan_ps = conn.execute(
        "DELETE FROM mux_pane_state
         WHERE checkpoint_id NOT IN (
             SELECT id FROM session_checkpoints
         )",
        [],
    )?;

    // Orphaned checkpoint rows (session_id references deleted session)
    let orphan_cp = conn.execute(
        "DELETE FROM session_checkpoints
         WHERE session_id NOT IN (
             SELECT session_id FROM mux_sessions
         )",
        [],
    )?;

    Ok((orphan_cp, orphan_ps))
}

/// Run cleanup asynchronously via spawn_blocking.
///
/// Convenience wrapper for use from async contexts.
pub async fn cleanup_sessions_async(
    db_path: Arc<String>,
    config: SessionRetentionConfig,
) -> Result<CleanupResult, String> {
    tokio::task::spawn_blocking(move || {
        let conn = Connection::open(db_path.as_str())
            .map_err(|e| format!("Failed to open database: {e}"))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")
            .map_err(|e| format!("Failed to set PRAGMAs: {e}"))?;
        cleanup_sessions(&conn, &config).map_err(|e| format!("Cleanup failed: {e}"))
    })
    .await
    .map_err(|e| format!("Task join error: {e}"))?
}

/// Get current epoch time in milliseconds.
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(crate::storage::SCHEMA_SQL).unwrap();
        conn
    }

    fn insert_session(conn: &Connection, id: &str, created_at: i64, shutdown_clean: bool) {
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, shutdown_clean, topology_json, ft_version)
             VALUES (?1, ?2, ?3, '{}', '0.1.0')",
            rusqlite::params![id, created_at, shutdown_clean as i64],
        )
        .unwrap();
    }

    fn insert_checkpoint(
        conn: &Connection,
        session_id: &str,
        checkpoint_at: i64,
        total_bytes: i64,
    ) -> i64 {
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
             VALUES (?1, ?2, 'periodic', 'hash', 1, ?3)",
            rusqlite::params![session_id, checkpoint_at, total_bytes],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_pane_state(conn: &Connection, checkpoint_id: i64, pane_id: u64) {
        conn.execute(
            "INSERT INTO mux_pane_state
             (checkpoint_id, pane_id, terminal_state_json)
             VALUES (?1, ?2, '{}')",
            rusqlite::params![checkpoint_id, pane_id as i64],
        )
        .unwrap();
    }

    fn count_sessions(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))
            .unwrap()
    }

    fn count_checkpoints(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM session_checkpoints",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn count_pane_states(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
            .unwrap()
    }

    // ---- Age-based cleanup ----

    #[test]
    fn delete_old_closed_sessions() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        let old = now - 31 * 86_400_000; // 31 days ago

        insert_session(&conn, "old-closed", old, true);
        insert_session(&conn, "recent-closed", now - 1000, true);
        insert_session(&conn, "old-active", old, false); // active: should NOT be deleted

        let deleted = delete_sessions_by_age(&conn, 30).unwrap();
        assert_eq!(deleted, 1); // only old-closed
        assert_eq!(count_sessions(&conn), 2);
    }

    #[test]
    fn age_cleanup_preserves_active_sessions() {
        let conn = make_test_db();
        let old = (epoch_ms() as i64) - 90 * 86_400_000; // 90 days ago

        insert_session(&conn, "active-old", old, false);

        let deleted = delete_sessions_by_age(&conn, 30).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(count_sessions(&conn), 1);
    }

    // ---- Count-based cleanup ----

    #[test]
    fn delete_excess_closed_sessions_keeps_newest() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        for i in 0..5 {
            insert_session(&conn, &format!("sess-{i}"), now + i * 1000, true);
        }

        let deleted = delete_excess_closed_sessions(&conn, 3).unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(count_sessions(&conn), 3);

        // Verify the 3 newest were kept
        let kept: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT session_id FROM mux_sessions ORDER BY created_at DESC")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert_eq!(kept, vec!["sess-4", "sess-3", "sess-2"]);
    }

    // ---- Size-based cleanup ----

    #[test]
    fn delete_sessions_by_size_frees_space() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Create 3 sessions, each with 400KB of checkpoint data
        for i in 0..3 {
            let id = format!("sess-{i}");
            insert_session(&conn, &id, now + i * 1000, true);
            insert_checkpoint(&conn, &id, now + i * 1000, 400 * 1024); // 400KB each
        }

        // Total: 1200KB. Budget: 1MB (1024KB). Need to free 176KB → delete oldest.
        let deleted = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(deleted, 1); // Deletes oldest (400KB > 176KB needed)
        assert_eq!(count_sessions(&conn), 2);
    }

    #[test]
    fn size_cleanup_noop_when_under_budget() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        insert_session(&conn, "small", now, true);
        insert_checkpoint(&conn, "small", now, 1024); // 1KB

        let deleted = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(deleted, 0);
    }

    // ---- Cascade delete ----

    #[test]
    fn session_delete_cascades_to_checkpoints_and_pane_state() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        let old = now - 31 * 86_400_000;

        insert_session(&conn, "old-sess", old, true);
        let cp_id = insert_checkpoint(&conn, "old-sess", old, 1024);
        insert_pane_state(&conn, cp_id, 1);
        insert_pane_state(&conn, cp_id, 2);

        assert_eq!(count_checkpoints(&conn), 1);
        assert_eq!(count_pane_states(&conn), 2);

        delete_sessions_by_age(&conn, 30).unwrap();

        assert_eq!(count_sessions(&conn), 0);
        assert_eq!(count_checkpoints(&conn), 0);
        assert_eq!(count_pane_states(&conn), 0);
    }

    // ---- Orphaned data cleanup ----

    #[test]
    fn cleanup_orphaned_checkpoints() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Create a session and checkpoint normally
        insert_session(&conn, "valid", now, true);
        insert_checkpoint(&conn, "valid", now, 1024);

        // Temporarily disable FK to insert orphaned checkpoint (simulates corruption)
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
             VALUES ('orphan-sess', ?1, 'periodic', 'hash', 0, 0)",
            [now],
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

        assert_eq!(count_checkpoints(&conn), 2);

        let (orphan_cp, _) = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(orphan_cp, 1);
        assert_eq!(count_checkpoints(&conn), 1);
    }

    #[test]
    fn cleanup_orphaned_pane_states() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        insert_session(&conn, "valid", now, true);
        let cp_id = insert_checkpoint(&conn, "valid", now, 1024);
        insert_pane_state(&conn, cp_id, 1);

        // Temporarily disable FK to insert orphaned pane_state (simulates corruption)
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute(
            "INSERT INTO mux_pane_state
             (checkpoint_id, pane_id, terminal_state_json)
             VALUES (99999, 42, '{}')",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

        assert_eq!(count_pane_states(&conn), 2);

        let (_, orphan_ps) = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(orphan_ps, 1);
        assert_eq!(count_pane_states(&conn), 1);
    }

    // ---- Full cleanup pipeline ----

    #[test]
    fn full_cleanup_with_defaults() {
        let conn = make_test_db();
        let config = SessionRetentionConfig::default();
        let now = epoch_ms() as i64;

        // Insert some sessions within retention period
        insert_session(&conn, "recent", now, true);
        insert_checkpoint(&conn, "recent", now, 1024);

        let result = cleanup_sessions(&conn, &config).unwrap();
        assert_eq!(result.total_sessions_deleted(), 0);
        assert!(!result.vacuumed);
    }

    #[test]
    fn full_cleanup_disabled_policies() {
        let conn = make_test_db();
        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 0,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();
        assert_eq!(result.total_sessions_deleted(), 0);
    }

    // ---- Config defaults ----

    #[test]
    fn config_defaults_sensible() {
        let config = SessionRetentionConfig::default();
        assert_eq!(config.max_age_days, 30);
        assert_eq!(config.max_closed_sessions, 50);
        assert_eq!(config.max_total_size_mb, 500);
        assert_eq!(config.cleanup_interval_hours, 24);
    }

    // ---- CleanupResult helpers ----

    #[test]
    fn cleanup_result_total() {
        let result = CleanupResult {
            deleted_by_age: 3,
            deleted_by_count: 2,
            deleted_by_size: 1,
            ..Default::default()
        };
        assert_eq!(result.total_sessions_deleted(), 6);
        assert!(result.any_work_done());
    }

    #[test]
    fn cleanup_result_empty() {
        let result = CleanupResult::default();
        assert_eq!(result.total_sessions_deleted(), 0);
        assert!(!result.any_work_done());
    }
}
