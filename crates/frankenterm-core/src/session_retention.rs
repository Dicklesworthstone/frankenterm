//! Session data retention and cleanup.
//!
//! Implements retention policies for session persistence data to prevent
//! unbounded growth. Cleans up old sessions by age, count, and total size.
//!
//! # Cleanup order
//!
//! 1. Delete sessions older than `max_age_days` (skip active sessions)
//! 2. Delete excess closed sessions beyond `max_closed_sessions` (oldest first)
//! 3. If the schema-v40 exact logical retained-payload authority exceeds
//!    `max_total_size_mb`, delete oldest eligible closed sessions
//! 4. Clean orphaned data (lifecycle/checkpoint rows without a session and
//!    pane-state rows without a checkpoint)
//!
//! Cascade: session deletion cascades to checkpoints -> pane_state via FK.

#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use tracing::{debug, info, warn};

use crate::checkpoint_witness::{MAX_CHECKPOINT_METADATA_BYTES, MAX_CHECKPOINT_SESSION_ID_BYTES};
use crate::config::SessionRetentionConfig;

// [ft-xcsm0 / ft-8nqx0 Phase 4] CleanupResult lifted to the audit-types
// leaf crate so the cleanup summary contract can be reviewed
// independently from the operational SQL pipeline below. Re-exported
// here so existing `crate::session_retention::CleanupResult` and
// `frankenterm_core::session_retention::CleanupResult` callers (and
// the proptest_session_retention.rs proptest) need zero edits.
pub use frankenterm_core_audit_types::session_retention_types::CleanupResult;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SizeCleanupOutcome {
    deleted: usize,
    measured_bytes: u64,
    deleted_bytes: u64,
    retained_bytes: u64,
    ineligible_shortfall_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct OrphanCleanupOutcome {
    orphaned_restore_lifecycle_rows: usize,
    orphaned_checkpoints: usize,
    orphaned_pane_states: usize,
}

/// Finite phase in which session-cleanup completion became unobservable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCleanupIndeterminatePhase {
    /// The blocking cleanup task was admitted, but its terminal result was not
    /// observed. The closure may still be running or may already have committed.
    BlockingTaskSettlement,
    /// The cleanup SQL pipeline returned an error after execution began. Earlier
    /// independently committed phases may already be durable.
    CleanupExecution,
}

impl SessionCleanupIndeterminatePhase {
    const fn label(self) -> &'static str {
        match self {
            Self::BlockingTaskSettlement => "blocking_task_settlement",
            Self::CleanupExecution => "cleanup_execution",
        }
    }
}

impl std::fmt::Display for SessionCleanupIndeterminatePhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Finite asynchronous session-cleanup failure contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionCleanupError {
    /// The caller was already cancelled, proving no blocking cleanup handoff
    /// was admitted.
    #[error("session cleanup cancelled before blocking handoff")]
    CancelledBeforeHandoff,
    /// SQLite could not open the cleanup connection, so no cleanup SQL ran.
    #[error("session cleanup database open failed")]
    DatabaseOpen,
    /// The cleanup connection could not establish its required PRAGMAs, so the
    /// cleanup pipeline was not invoked.
    #[error("session cleanup database preparation failed")]
    DatabasePreparation,
    /// Cleanup may have durably changed SQLite state, but no authoritative
    /// completion receipt is available. Callers must reconcile and must not
    /// automatically retry.
    #[error(
        "session cleanup outcome is indeterminate during {phase}; reconcile durable state before retrying"
    )]
    IndeterminateCleanup {
        /// Finite observation-loss phase; never a database path or SQL string.
        phase: SessionCleanupIndeterminatePhase,
    },
}

impl SessionCleanupError {
    /// Whether durable state must be reconciled before any retry decision.
    #[must_use]
    pub const fn requires_reconciliation(self) -> bool {
        matches!(self, Self::IndeterminateCleanup { .. })
    }
}

/// Run the full session cleanup pipeline.
///
/// Designed to be called from `runtime_async::spawn_blocking` since all
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
        result.deleted_by_count = delete_excess_closed_sessions(conn, config.max_closed_sessions)?;
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
        let size_outcome = delete_sessions_by_size(conn, config.max_total_size_mb)?;
        result.deleted_by_size = size_outcome.deleted;
        result.size_measured_bytes = size_outcome.measured_bytes;
        result.size_deleted_bytes = size_outcome.deleted_bytes;
        result.size_retained_bytes = size_outcome.retained_bytes;
        result.size_ineligible_shortfall_bytes = size_outcome.ineligible_shortfall_bytes;
        if size_outcome.ineligible_shortfall_bytes > 0 {
            warn!(
                deleted = size_outcome.deleted,
                measured_bytes = size_outcome.measured_bytes,
                deleted_bytes = size_outcome.deleted_bytes,
                retained_bytes = size_outcome.retained_bytes,
                shortfall_bytes = size_outcome.ineligible_shortfall_bytes,
                max_mb = config.max_total_size_mb,
                "Session size budget remains above its configured limit because no more closed sessions are eligible"
            );
        } else if size_outcome.deleted > 0 {
            info!(
                deleted = size_outcome.deleted,
                measured_bytes = size_outcome.measured_bytes,
                deleted_bytes = size_outcome.deleted_bytes,
                retained_bytes = size_outcome.retained_bytes,
                max_mb = config.max_total_size_mb,
                "Applied session size budget"
            );
        }
    }

    // 4. Clean orphaned data
    let orphaned = cleanup_orphaned_data(conn)?;
    result.orphaned_restore_lifecycle_rows = orphaned.orphaned_restore_lifecycle_rows;
    result.orphaned_checkpoints = orphaned.orphaned_checkpoints;
    result.orphaned_pane_states = orphaned.orphaned_pane_states;
    if orphaned != OrphanCleanupOutcome::default() {
        warn!(
            orphaned_restore_lifecycle_rows = orphaned.orphaned_restore_lifecycle_rows,
            orphaned_checkpoints = orphaned.orphaned_checkpoints,
            orphaned_pane_states = orphaned.orphaned_pane_states,
            "Cleaned up orphaned session data"
        );
    }

    // Deliberately issue no VACUUM or incremental_vacuum operation here. Under
    // FrankenTerm's normal `auto_vacuum=NONE` database policy, freed pages stay
    // on SQLite's freelist for reuse. An externally-created database configured
    // with `auto_vacuum=FULL` may still relocate and truncate pages as part of a
    // committing DELETE; absence of an explicit VACUUM is not a universal
    // no-compaction guarantee for that non-default mode. The runtime's ordinary
    // online-maintenance lane owns PASSIVE WAL checkpointing and PRAGMA optimize.
    // Explicit operator-requested physical compaction remains a separate,
    // disruptive maintenance operation.
    Ok(result)
}

fn u64_to_sqlite_integer(value: u64) -> Result<i64, rusqlite::Error> {
    i64::try_from(value).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn clean_authority_error(error: crate::session_restore::RestoreError) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(error.to_string())))
}

fn begin_retention_transaction(conn: &Connection) -> rusqlite::Result<Transaction<'_>> {
    Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
}

/// Return whether a session has any restore attempt that lacks an explicit
/// durable `resolved` lifecycle state. The metadata branch preserves fail-safe
/// behavior for v37-era intent rows written under the overloaded
/// `restore_receipt` role before the schema gained that type. Neither a linked
/// outcome nor a later snapshot implies resolution: retrying external mux
/// mutations without reconciliation can duplicate tabs, panes, or processes.
pub(crate) fn has_unresolved_restore_intent(
    conn: &Connection,
    session_id: &str,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT EXISTS (
             SELECT 1
             FROM restore_attempt_lifecycle AS lifecycle
             WHERE lifecycle.session_id = ?1
               AND CASE
                       WHEN typeof(lifecycle.status) = 'text'
                        AND lifecycle.status = 'resolved'
                       THEN 0
                       ELSE 1
                   END = 1
         ) OR EXISTS (
             SELECT 1
             FROM session_checkpoints AS intent
             WHERE intent.session_id = ?1
               AND intent.checkpoint_role = 'restore_intent'
               AND NOT EXISTS (
                   SELECT 1
                   FROM restore_attempt_lifecycle AS lifecycle
                   WHERE lifecycle.intent_checkpoint_id = intent.id
                     AND lifecycle.session_id = intent.session_id
               )
         ) OR EXISTS (
             SELECT 1
             FROM session_checkpoints AS intent
             WHERE intent.session_id = ?1
               AND intent.checkpoint_role = 'restore_receipt'
               AND CASE
                       WHEN typeof(intent.metadata_json) != 'text'
                         OR length(CAST(intent.metadata_json AS BLOB)) > ?2
                       THEN 1
                       WHEN NOT json_valid(intent.metadata_json)
                       THEN 1
                       WHEN json_extract(
                           intent.metadata_json,
                           '$.restore_attempt.phase'
                       ) = 'outcome'
                       THEN 0
                       WHEN json_type(
                               intent.metadata_json,
                               '$.restore_attempt'
                            ) IS NULL
                        AND json_type(
                               intent.metadata_json,
                               '$.old_to_new'
                            ) = 'object'
                       THEN 0
                       ELSE 1
                   END = 1
               AND NOT EXISTS (
                   SELECT 1
                   FROM restore_attempt_lifecycle AS lifecycle
                   WHERE lifecycle.intent_checkpoint_id = intent.id
                     AND lifecycle.session_id = intent.session_id
               )
         )",
        rusqlite::params![
            session_id,
            i64::try_from(MAX_CHECKPOINT_METADATA_BYTES).unwrap_or(i64::MAX),
        ],
        |row| row.get::<_, bool>(0),
    )
}

/// Fail closed when unaudited authority-table triggers could invalidate direct
/// SQLite row-count receipts. Schema-v40 retained-size triggers are the sole
/// persistent allowlist: migration validation pins their bodies, and cleanup
/// depends on them to keep byte deletion receipts exact.
///
/// SQLite identifiers are case-insensitive while both schema catalogs preserve
/// the spelling used by `CREATE TRIGGER`, so both the persistent and TEMP
/// catalog comparisons must use `NOCASE`.
///
/// # Errors
///
/// Returns the underlying catalog-query error, or a fail-closed conversion
/// error when any unaudited trigger targets an authority table.
pub(crate) fn ensure_session_authority_tables_have_no_unaudited_triggers(
    conn: &Connection,
) -> Result<(), rusqlite::Error> {
    let trigger_count: i64 = conn.query_row(
        "SELECT
             (SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'trigger'
                AND tbl_name COLLATE NOCASE
                    IN ('mux_sessions', 'session_checkpoints', 'mux_pane_state',
                        'restore_attempt_lifecycle')
                AND name COLLATE NOCASE NOT IN (
                    'mux_sessions_retained_size_ai',
                    'mux_sessions_retained_size_au',
                    'mux_sessions_retained_size_ad',
                    'session_checkpoints_retained_size_ai',
                    'session_checkpoints_retained_size_au',
                    'session_checkpoints_retained_size_bd',
                    'mux_pane_state_retained_size_ai',
                    'mux_pane_state_retained_size_au',
                    'mux_pane_state_retained_size_ad',
                    'restore_attempt_lifecycle_retained_size_ai',
                    'restore_attempt_lifecycle_retained_size_au',
                    'restore_attempt_lifecycle_retained_size_ad'
                ))
           + (SELECT COUNT(*) FROM sqlite_temp_schema
              WHERE type = 'trigger'
                AND tbl_name COLLATE NOCASE
                    IN ('mux_sessions', 'session_checkpoints', 'mux_pane_state',
                        'restore_attempt_lifecycle'))",
        [],
        |row| row.get(0),
    )?;
    if trigger_count == 0 {
        Ok(())
    } else {
        Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
            std::io::Error::other(
                "session authority mutation refuses unaudited triggers on authoritative tables",
            ),
        )))
    }
}

/// Delete closed sessions older than `max_age_days`.
///
/// Active sessions (`shutdown_clean = 0`) are preserved. Closed-session age is
/// measured from its latest checkpoint when present, so a long-running session
/// is not deleted immediately after a recent clean shutdown merely because it
/// was originally created before the age cutoff.
fn delete_sessions_by_age(conn: &Connection, max_age_days: u64) -> Result<usize, rusqlite::Error> {
    let cutoff_ms = epoch_ms().saturating_sub(max_age_days.saturating_mul(86_400_000));
    // An unrepresentable future cutoff must fail without deleting anything;
    // clamping to i64::MAX would make nearly every closed session eligible.
    let cutoff_ms = u64_to_sqlite_integer(cutoff_ms)?;

    let tx = begin_retention_transaction(conn)?;
    ensure_session_authority_tables_have_no_unaudited_triggers(&tx)?;
    let candidates: Vec<(String, i64, Option<i64>)> = {
        let mut stmt = tx.prepare(
            "SELECT session_id, shutdown_clean, clean_checkpoint_id
             FROM mux_sessions
             WHERE MAX(
                       created_at,
                       COALESCE(last_checkpoint_at, created_at),
                       COALESCE(
                           (SELECT MAX(c.checkpoint_at)
                            FROM session_checkpoints c
                            WHERE c.session_id = mux_sessions.session_id),
                           created_at
                       )
                   ) < ?1
               AND shutdown_clean = 1
               AND typeof(session_id) = 'text'
               AND length(CAST(session_id AS BLOB)) BETWEEN 1 AND ?2
             ORDER BY session_id ASC",
        )?;
        stmt.query_map(
            rusqlite::params![
                cutoff_ms,
                i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?
        .collect::<Result<_, _>>()?
    };
    let mut deleted = 0usize;
    for (session_id, shutdown_clean, clean_checkpoint_id) in candidates {
        if has_unresolved_restore_intent(&tx, &session_id)? {
            continue;
        }
        if !crate::session_restore::assess_clean_authority(
            &tx,
            &session_id,
            shutdown_clean,
            clean_checkpoint_id,
        )
        .map_err(clean_authority_error)?
        {
            continue;
        }
        let affected = tx.execute(
            "DELETE FROM mux_sessions WHERE session_id = ?1",
            [&session_id],
        )?;
        if affected != 1 {
            return Err(rusqlite::Error::StatementChangedRows(affected));
        }
        deleted = deleted.saturating_add(1);
    }
    tx.commit()?;
    Ok(deleted)
}

/// Delete excess closed sessions, keeping the most recent `max_count`.
fn delete_excess_closed_sessions(
    conn: &Connection,
    max_count: usize,
) -> Result<usize, rusqlite::Error> {
    let tx = begin_retention_transaction(conn)?;
    ensure_session_authority_tables_have_no_unaudited_triggers(&tx)?;
    let candidates: Vec<(String, i64, Option<i64>)> = {
        let mut stmt = tx.prepare(
            "SELECT session_id, shutdown_clean, clean_checkpoint_id
             FROM mux_sessions
             WHERE shutdown_clean = 1
               AND typeof(session_id) = 'text'
               AND length(CAST(session_id AS BLOB)) BETWEEN 1 AND ?1
             ORDER BY MAX(
                          created_at,
                          COALESCE(last_checkpoint_at, created_at),
                          COALESCE(
                              (SELECT MAX(c.checkpoint_at)
                               FROM session_checkpoints c
                               WHERE c.session_id = mux_sessions.session_id),
                              created_at
                          )
                      ) DESC,
                      session_id DESC",
        )?;
        stmt.query_map(
            [i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?
        .collect::<Result<_, _>>()?
    };
    let mut retained = 0usize;
    let mut deleted = 0usize;
    for (session_id, shutdown_clean, clean_checkpoint_id) in candidates {
        if has_unresolved_restore_intent(&tx, &session_id)? {
            continue;
        }
        if !crate::session_restore::assess_clean_authority(
            &tx,
            &session_id,
            shutdown_clean,
            clean_checkpoint_id,
        )
        .map_err(clean_authority_error)?
        {
            continue;
        }
        if retained < max_count {
            retained = retained.saturating_add(1);
            continue;
        }
        let affected = tx.execute(
            "DELETE FROM mux_sessions WHERE session_id = ?1",
            [&session_id],
        )?;
        if affected != 1 {
            return Err(rusqlite::Error::StatementChangedRows(affected));
        }
        deleted = deleted.saturating_add(1);
    }
    tx.commit()?;
    Ok(deleted)
}

fn retained_size_contract_error(message: &'static str) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(message)))
}

/// Recompute the complete logical retained-payload contract and require exact
/// agreement with the trigger-maintained O(sessions) authority before cleanup
/// trusts a byte. This scan is intentionally confined to the infrequent
/// retention transaction; normal mutation and decision paths remain bounded.
fn validate_session_retained_size_authority(conn: &Connection) -> Result<(), rusqlite::Error> {
    let invalid_row: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM mux_sessions s
             WHERE typeof(s.session_id) != 'text'
                OR length(CAST(s.session_id AS BLOB)) NOT BETWEEN 1 AND ?1
                OR typeof(s.created_at) != 'integer' OR s.created_at < 0
                OR (s.last_checkpoint_at IS NOT NULL AND
                    (typeof(s.last_checkpoint_at) != 'integer' OR s.last_checkpoint_at < 0))
                OR typeof(s.shutdown_clean) != 'integer' OR s.shutdown_clean NOT IN (0, 1)
                OR typeof(s.topology_json) != 'text'
                OR (s.window_metadata_json IS NOT NULL AND
                    typeof(s.window_metadata_json) != 'text')
                OR typeof(s.ft_version) != 'text'
                OR (s.host_id IS NOT NULL AND typeof(s.host_id) != 'text')
                OR (s.clean_checkpoint_id IS NOT NULL AND
                    (typeof(s.clean_checkpoint_id) != 'integer' OR s.clean_checkpoint_id <= 0))
             UNION ALL
             SELECT 1 FROM session_checkpoints c
             INNER JOIN mux_sessions s ON s.session_id = c.session_id
             WHERE typeof(c.id) != 'integer' OR c.id <= 0
                OR typeof(c.session_id) != 'text'
                OR typeof(c.checkpoint_at) != 'integer' OR c.checkpoint_at < 0
                OR typeof(c.checkpoint_type) != 'text'
                OR typeof(c.state_hash) != 'text'
                OR typeof(c.pane_count) != 'integer' OR c.pane_count < 0
                OR typeof(c.total_bytes) != 'integer' OR c.total_bytes < 0
                OR (c.metadata_json IS NOT NULL AND typeof(c.metadata_json) != 'text')
                OR typeof(c.checkpoint_role) != 'text'
                OR (c.topology_json IS NOT NULL AND typeof(c.topology_json) != 'text')
                OR (c.restore_intent_checkpoint_id IS NOT NULL AND
                    (typeof(c.restore_intent_checkpoint_id) != 'integer' OR
                     c.restore_intent_checkpoint_id <= 0))
             UNION ALL
             SELECT 1 FROM mux_pane_state p
             INNER JOIN session_checkpoints c ON c.id = p.checkpoint_id
             INNER JOIN mux_sessions s ON s.session_id = c.session_id
             WHERE typeof(p.id) != 'integer' OR p.id <= 0
                OR typeof(p.checkpoint_id) != 'integer' OR p.checkpoint_id <= 0
                OR typeof(p.pane_id) != 'integer' OR p.pane_id < 0
                OR (p.cwd IS NOT NULL AND typeof(p.cwd) != 'text')
                OR (p.command IS NOT NULL AND typeof(p.command) != 'text')
                OR (p.env_json IS NOT NULL AND typeof(p.env_json) != 'text')
                OR typeof(p.terminal_state_json) != 'text'
                OR (p.agent_metadata_json IS NOT NULL AND
                    typeof(p.agent_metadata_json) != 'text')
                OR (p.scrollback_checkpoint_seq IS NOT NULL AND
                    (typeof(p.scrollback_checkpoint_seq) != 'integer' OR
                     p.scrollback_checkpoint_seq < 0))
                OR (p.last_output_at IS NOT NULL AND
                    (typeof(p.last_output_at) != 'integer' OR p.last_output_at < 0))
             UNION ALL
             SELECT 1 FROM restore_attempt_lifecycle r
             INNER JOIN mux_sessions s ON s.session_id = r.session_id
             WHERE typeof(r.intent_checkpoint_id) != 'integer' OR r.intent_checkpoint_id <= 0
                OR typeof(r.session_id) != 'text'
                OR typeof(r.source_checkpoint_id) != 'integer' OR r.source_checkpoint_id <= 0
                OR (r.outcome_checkpoint_id IS NOT NULL AND
                    (typeof(r.outcome_checkpoint_id) != 'integer' OR
                     r.outcome_checkpoint_id <= 0))
                OR typeof(r.status) != 'text'
                OR typeof(r.created_at) != 'integer' OR r.created_at < 0
                OR (r.resolved_at IS NOT NULL AND
                    (typeof(r.resolved_at) != 'integer' OR r.resolved_at < r.created_at))
         )",
        [i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX)],
        |row| row.get(0),
    )?;
    if invalid_row {
        return Err(retained_size_contract_error(
            "session retained-size authority rejected invalid persisted row metadata",
        ));
    }

    let drift: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM mux_sessions s
             LEFT JOIN session_retained_size z ON z.session_id = s.session_id
             LEFT JOIN session_retained_size_recomputed r ON r.session_id = s.session_id
             WHERE z.session_id IS NULL OR r.session_id IS NULL
                OR z.session_row_bytes != r.session_row_bytes
                OR z.checkpoint_row_bytes != r.checkpoint_row_bytes
                OR z.pane_state_row_bytes != r.pane_state_row_bytes
                OR z.restore_lifecycle_row_bytes != r.restore_lifecycle_row_bytes
                OR z.retained_bytes !=
                   r.session_row_bytes + r.checkpoint_row_bytes
                     + r.pane_state_row_bytes + r.restore_lifecycle_row_bytes
             UNION ALL
             SELECT 1
             FROM session_retained_size z
             LEFT JOIN mux_sessions s ON s.session_id = z.session_id
             WHERE s.session_id IS NULL
         )",
        [],
        |row| row.get(0),
    )?;
    if drift {
        return Err(retained_size_contract_error(
            "session retained-size authority drifted from stored payload rows",
        ));
    }
    Ok(())
}

/// Delete oldest eligible closed sessions until exact logical retained-payload
/// bytes are under budget. Every stored field is charged once by schema-v40
/// `session_retained_size`; SQLite page, index, freelist, and WAL bytes remain
/// outside this explicitly logical budget.
fn delete_sessions_by_size(
    conn: &Connection,
    max_total_mb: u64,
) -> Result<SizeCleanupOutcome, rusqlite::Error> {
    let max_bytes = max_total_mb
        .checked_mul(1_024)
        .and_then(|bytes| bytes.checked_mul(1_024))
        .ok_or_else(|| {
            retained_size_contract_error("session retained-size budget conversion overflow")
        })?;

    // Keep the measurement, candidate set, and deletions in one transaction.
    // Without this boundary, a later DELETE failure left earlier session
    // deletions committed even though this stage returned no accounting
    // result, and a concurrent cleanup could make us claim bytes for a row our
    // connection did not delete.
    let tx = begin_retention_transaction(conn)?;
    ensure_session_authority_tables_have_no_unaudited_triggers(&tx)?;
    validate_session_retained_size_authority(&tx)?;

    // The generated retained_bytes column is checked non-negative and each
    // summary row belongs to exactly one live mux session. SUM overflow is a
    // SQLite error, so an unrepresentable total fails before any deletion.
    let (total_bytes, minimum_bytes): (i64, i64) = tx.query_row(
        "SELECT COALESCE(SUM(retained_bytes), 0),
                COALESCE(MIN(retained_bytes), 0)
         FROM session_retained_size",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if minimum_bytes < 0 {
        return Err(rusqlite::Error::IntegralValueOutOfRange(1, minimum_bytes));
    }
    let total_bytes = u64::try_from(total_bytes)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, total_bytes))?;

    if total_bytes <= max_bytes {
        return Ok(SizeCleanupOutcome {
            measured_bytes: total_bytes,
            retained_bytes: total_bytes,
            ..SizeCleanupOutcome::default()
        });
    }

    let to_free = total_bytes.checked_sub(max_bytes).ok_or_else(|| {
        retained_size_contract_error("session retained-size budget subtraction underflow")
    })?;
    let mut freed: u64 = 0;
    let mut deleted = 0_usize;

    // Get closed sessions ordered oldest first. Session-level payload bytes are
    // read once from the summary row; no join can multiply topology or other
    // session fields by checkpoint/pane cardinality.
    let candidate_rows: Vec<(String, u64, i64, Option<i64>)> = {
        let mut stmt = tx.prepare(
            "SELECT s.session_id,
                    z.retained_bytes,
                    s.shutdown_clean,
                    s.clean_checkpoint_id
             FROM mux_sessions s
             INNER JOIN session_retained_size z ON z.session_id = s.session_id
             WHERE s.shutdown_clean = 1
               AND typeof(s.session_id) = 'text'
               AND length(CAST(s.session_id AS BLOB)) BETWEEN 1 AND ?1
               AND z.retained_bytes > 0
             ORDER BY MAX(
                          s.created_at,
                          COALESCE(s.last_checkpoint_at, s.created_at),
                          COALESCE(
                              (SELECT MAX(c.checkpoint_at)
                               FROM session_checkpoints c
                               WHERE c.session_id = s.session_id),
                              s.created_at
                          )
                      ) ASC,
                      s.session_id ASC",
        )?;

        let sessions = stmt
            .query_map(
                [i64::try_from(MAX_CHECKPOINT_SESSION_ID_BYTES).unwrap_or(i64::MAX)],
                |row| {
                    let session_id = row.get(0)?;
                    let session_bytes: i64 = row.get(1)?;
                    let session_bytes = u64::try_from(session_bytes)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, session_bytes))?;
                    Ok((session_id, session_bytes, row.get(2)?, row.get(3)?))
                },
            )?
            .collect::<Result<_, _>>()?;
        sessions
    };

    let mut sessions = Vec::with_capacity(candidate_rows.len());
    for (session_id, session_bytes, shutdown_clean, clean_checkpoint_id) in candidate_rows {
        if has_unresolved_restore_intent(&tx, &session_id)? {
            continue;
        }
        if crate::session_restore::assess_clean_authority(
            &tx,
            &session_id,
            shutdown_clean,
            clean_checkpoint_id,
        )
        .map_err(clean_authority_error)?
        {
            sessions.push((session_id, session_bytes));
        }
    }

    for (session_id, session_bytes) in sessions {
        if freed >= to_free {
            break;
        }

        let affected = tx.execute(
            "DELETE FROM mux_sessions WHERE session_id = ?1",
            [&session_id],
        )?;

        // The candidate query names one primary-key row. A trigger that ignores
        // it (or any other non-exact effect) means the size target was not
        // enforced as measured, so roll back rather than return false cleanup
        // accounting.
        if affected != 1 {
            return Err(rusqlite::Error::StatementChangedRows(affected));
        }

        freed = freed.checked_add(session_bytes).ok_or_else(|| {
            retained_size_contract_error("session retained-size deletion receipt overflow")
        })?;
        deleted = deleted.saturating_add(1);
    }

    let retained_bytes = total_bytes.checked_sub(freed).ok_or_else(|| {
        retained_size_contract_error("session retained-size deletion exceeded measurement")
    })?;
    let stored_retained_bytes: i64 = tx.query_row(
        "SELECT COALESCE(SUM(retained_bytes), 0) FROM session_retained_size",
        [],
        |row| row.get(0),
    )?;
    let stored_retained_bytes = u64::try_from(stored_retained_bytes)
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, stored_retained_bytes))?;
    if stored_retained_bytes != retained_bytes {
        return Err(retained_size_contract_error(
            "session retained-size deletion receipt disagrees with durable summary",
        ));
    }

    tx.commit()?;
    if deleted > 0 {
        debug!(
            deleted,
            measured_bytes = total_bytes,
            deleted_bytes = freed,
            retained_bytes,
            target_bytes = to_free,
            "Committed bounded session set for size budget"
        );
    }
    Ok(SizeCleanupOutcome {
        deleted,
        measured_bytes: total_bytes,
        deleted_bytes: freed,
        retained_bytes,
        ineligible_shortfall_bytes: retained_bytes.saturating_sub(max_bytes),
    })
}

/// Clean orphaned data that lost its parent reference.
///
/// Returns exact direct-row counts for every orphan authority shape removed by
/// this transaction. Foreign-key cascade effects are excluded from SQLite's
/// direct change counts, so children are deleted explicitly before parents.
fn cleanup_orphaned_data(conn: &Connection) -> Result<OrphanCleanupOutcome, rusqlite::Error> {
    // ft-rt6ol + ft-kccj8: delete pane_state CHILDREN first, with a predicate
    // that names both orphan shapes explicitly — rows whose checkpoint is
    // already gone, and rows whose checkpoint is about to be removed as a
    // session-orphan below. The previous checkpoint-first ordering relied on
    // the second DELETE to sweep the linked shape, which is correct with
    // `foreign_keys` OFF but count-false with it ON: the checkpoint DELETE
    // cascades the children away and sqlite3_changes() excludes rows removed
    // by FK actions, so the reported pane_state count came back 0 while the
    // data was in fact collected. Naming both shapes in one child DELETE is
    // correct AND correctly counted under either FK setting. The transaction
    // makes both deletes commit atomically.
    let tx = begin_retention_transaction(conn)?;
    ensure_session_authority_tables_have_no_unaudited_triggers(&tx)?;

    // If an external writer disabled FK enforcement before deleting a session,
    // remove its lifecycle rows first. Otherwise the deferred intent FK can
    // correctly prevent the following checkpoint cleanup from erasing durable
    // evidence whose parent session is already irretrievably absent.
    let orphaned_restore_lifecycle_rows = tx.execute(
        "DELETE FROM restore_attempt_lifecycle
         WHERE session_id NOT IN (
             SELECT session_id FROM mux_sessions
         )",
        [],
    )?;

    // Orphaned pane_state rows: checkpoint already deleted, OR checkpoint is
    // itself a session-orphan that the next statement removes.
    let orphan_ps = tx.execute(
        "DELETE FROM mux_pane_state
         WHERE checkpoint_id NOT IN (
             SELECT id FROM session_checkpoints
         )
         OR checkpoint_id IN (
             SELECT id FROM session_checkpoints
             WHERE session_id NOT IN (SELECT session_id FROM mux_sessions)
         )",
        [],
    )?;

    // Orphaned checkpoint rows (session_id references a deleted session).
    let orphan_cp = tx.execute(
        "DELETE FROM session_checkpoints
         WHERE session_id NOT IN (
             SELECT session_id FROM mux_sessions
         )",
        [],
    )?;

    tx.commit()?;
    Ok(OrphanCleanupOutcome {
        orphaned_restore_lifecycle_rows,
        orphaned_checkpoints: orphan_cp,
        orphaned_pane_states: orphan_ps,
    })
}

/// Test-only cleanup under an explicit `&Cx` (ft-xbnl0.2.2).
///
/// Cx-first entry point: caller-supplied cancellation is honored before the
/// blocking handoff. After admission, cancellation, executor failure, or result
/// loss is reported as [`SessionCleanupError::IndeterminateCleanup`], because
/// the SQLite pipeline may still be running or may already have committed.
///
/// # Errors
///
/// Returns a finite [`SessionCleanupError`]. In particular, an indeterminate
/// result requires durable-state reconciliation and is never retry-safe merely
/// because the async join did not produce a receipt.
#[cfg(test)]
pub(crate) async fn cleanup_sessions_async_cx(
    cx: &crate::cx::Cx,
    db_path: Arc<String>,
    config: SessionRetentionConfig,
) -> Result<CleanupResult, SessionCleanupError> {
    // Honor caller cancellation before we hand work off to the blocking pool.
    cx.checkpoint()
        .map_err(|_| SessionCleanupError::CancelledBeforeHandoff)?;

    let outcome = crate::runtime_async::spawn_blocking_with_cx(cx, move || {
        cleanup_sessions_from_path(db_path.as_str(), &config)
    })
    .await;

    match outcome {
        Ok(result) => result,
        Err(error) => Err(classify_session_cleanup_blocking_failure(error)),
    }
}

/// Synchronous path-owned cleanup entry used when an outer authority
/// coordinator owns the exact queued-vs-started blocking handoff. Keeping
/// connection setup here also gives the test-only async seam the same finite
/// failure classification without nesting a second blocking task.
pub(crate) fn cleanup_sessions_from_path(
    db_path: &str,
    config: &SessionRetentionConfig,
) -> Result<CleanupResult, SessionCleanupError> {
    #[cfg(unix)]
    const SESSION_RETENTION_SQLITE_DEFAULT_VFS: &str = "unix";
    #[cfg(windows)]
    const SESSION_RETENTION_SQLITE_DEFAULT_VFS: &str = "win32";

    #[cfg(any(unix, windows))]
    let connection = Connection::open_with_flags_and_vfs(
        db_path,
        OpenFlags::default() | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
        SESSION_RETENTION_SQLITE_DEFAULT_VFS,
    );
    #[cfg(not(any(unix, windows)))]
    let connection = Connection::open_with_flags(
        db_path,
        OpenFlags::default() | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE,
    );
    let conn = connection.map_err(|error| {
        warn!(
            error_class = session_cleanup_database_error_class(&error),
            "session cleanup database open failed"
        );
        SessionCleanupError::DatabaseOpen
    })?;
    conn.busy_timeout(Duration::from_secs(5)).map_err(|error| {
        warn!(
            error_class = session_cleanup_database_error_class(&error),
            "session cleanup busy policy setup failed"
        );
        SessionCleanupError::DatabasePreparation
    })?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .map_err(|error| {
            warn!(
                error_class = session_cleanup_database_error_class(&error),
                "session cleanup database preparation failed"
            );
            SessionCleanupError::DatabasePreparation
        })?;
    cleanup_sessions(&conn, config).map_err(|error| {
        // The cleanup pipeline currently commits policy phases independently.
        // Any earlier phase may be durable when a later phase fails, so a
        // generic database error would fabricate retry safety until exact
        // partial receipts/continuations land.
        warn!(
            error_class = session_cleanup_database_error_class(&error),
            "session cleanup execution outcome is indeterminate"
        );
        SessionCleanupError::IndeterminateCleanup {
            phase: SessionCleanupIndeterminatePhase::CleanupExecution,
        }
    })
}

/// Collapse SQLite/rusqlite failures into a finite, content-free telemetry
/// class. Raw error displays are deliberately excluded because SQLite error
/// messages can contain database paths, schema fragments, or persisted values.
fn session_cleanup_database_error_class(error: &rusqlite::Error) -> &'static str {
    use rusqlite::ffi::ErrorCode;

    match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => "contention",
        Some(ErrorCode::OperationAborted | ErrorCode::OperationInterrupted) => "interrupted",
        Some(ErrorCode::PermissionDenied | ErrorCode::ReadOnly | ErrorCode::CannotOpen) => "access",
        Some(ErrorCode::SystemIoFailure | ErrorCode::DiskFull) => "storage_io",
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => "invalid_database",
        Some(
            ErrorCode::TooBig
            | ErrorCode::ConstraintViolation
            | ErrorCode::TypeMismatch
            | ErrorCode::ParameterOutOfRange,
        ) => "data_contract",
        Some(ErrorCode::OutOfMemory) => "resource_exhausted",
        Some(_) => "database_failure",
        None => "rusqlite_contract",
    }
}

#[cfg(test)]
fn classify_session_cleanup_blocking_failure(
    error: crate::runtime_async::SpawnBlockingWithCxError,
) -> SessionCleanupError {
    match error {
        crate::runtime_async::SpawnBlockingWithCxError::CancelledBeforeSpawn { .. } => {
            SessionCleanupError::CancelledBeforeHandoff
        }
        crate::runtime_async::SpawnBlockingWithCxError::CancelledMidFlight { .. }
        | crate::runtime_async::SpawnBlockingWithCxError::RuntimeFailure
        | crate::runtime_async::SpawnBlockingWithCxError::CancellationWatcherTimerFailure => {
            SessionCleanupError::IndeterminateCleanup {
                phase: SessionCleanupIndeterminatePhase::BlockingTaskSettlement,
            }
        }
    }
}

/// Get current epoch time in milliseconds.
fn epoch_ms() -> u64 {
    crate::clock_anomaly::epoch_ms_u64("ft.session_retention.clock")
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// LabRuntime-based determinism test (ft-xbnl0.2.2): prove that the
    /// Cx-first `cleanup_sessions_async_cx` path respects the caller's
    /// checkpoint boundary under seed-locked virtual-time scheduling.
    /// We point it at a path that will fail fast at `Connection::open`
    /// so no real SQLite work is done and no real time elapses; the
    /// test asserts the spawn_blocking handoff + result plumbing run
    /// under the LabRuntime scheduler without wall-clock dependence.
    #[test]
    fn cleanup_sessions_async_cx_runs_under_labruntime() {
        use std::sync::atomic::{AtomicBool, Ordering};

        const SEED: u64 = 0x5E55_1022_C410_D0BE;
        let wall_start = std::time::Instant::now();
        let observed_error = Arc::new(AtomicBool::new(false));
        let observed_error_task = Arc::clone(&observed_error);

        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(SEED)
                .with_auto_advance()
                .worker_count(1)
                .max_steps(50_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                let cx = crate::cx::for_request();
                // Path that cannot exist / cannot open as a SQLite DB.
                let bad_db = Arc::new("/ft-xbnl0-2-2/does-not-exist/bogus.sqlite".to_string());
                let config = SessionRetentionConfig::default();
                let result = cleanup_sessions_async_cx(&cx, bad_db, config).await;
                match result {
                    Err(SessionCleanupError::DatabaseOpen) => {
                        // The finite open failure proves the Cx-first plumbing
                        // ran end to end without deadlocking or burning real
                        // time, while keeping the path out of the error surface.
                        observed_error_task.store(true, Ordering::SeqCst);
                    }
                    Err(other) => panic!("unexpected error surface: {other}"),
                    Ok(_) => panic!("expected an error from nonexistent DB"),
                }
            })
            .expect("spawn session_retention task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.step_for_test();
        let _ = runtime.run_with_auto_advance();
        let report = runtime.run_until_quiescent_with_report();

        assert!(
            observed_error.load(Ordering::SeqCst),
            "cleanup_sessions_async_cx must surface an error for a bogus DB path"
        );
        assert!(
            report.oracle_report.all_passed(),
            "LabRuntime oracles must all pass: {report:?}"
        );
        assert!(
            wall_start.elapsed() < std::time::Duration::from_secs(2),
            "Cx-first cleanup must not burn real time; elapsed {:?}",
            wall_start.elapsed()
        );
    }

    #[test]
    fn blocking_settlement_classifier_never_fabricates_retry_safety() {
        use crate::runtime_async::SpawnBlockingWithCxError;

        let not_admitted = classify_session_cleanup_blocking_failure(
            SpawnBlockingWithCxError::CancelledBeforeSpawn { kind: None },
        );
        assert_eq!(not_admitted, SessionCleanupError::CancelledBeforeHandoff);
        assert!(!not_admitted.requires_reconciliation());

        let observation_losses = [
            SpawnBlockingWithCxError::CancelledMidFlight { kind: None },
            SpawnBlockingWithCxError::RuntimeFailure,
            SpawnBlockingWithCxError::CancellationWatcherTimerFailure,
        ];
        for failure in observation_losses {
            let classified = classify_session_cleanup_blocking_failure(failure);
            assert_eq!(
                classified,
                SessionCleanupError::IndeterminateCleanup {
                    phase: SessionCleanupIndeterminatePhase::BlockingTaskSettlement,
                }
            );
            assert!(classified.requires_reconciliation());
            let message = classified.to_string();
            assert!(message.contains("reconcile durable state"));
            assert!(!message.contains("None"));
        }
    }

    #[test]
    fn cleanup_execution_failure_is_content_free_and_requires_reconciliation() {
        let error = SessionCleanupError::IndeterminateCleanup {
            phase: SessionCleanupIndeterminatePhase::CleanupExecution,
        };
        assert!(error.requires_reconciliation());
        assert_eq!(
            error.to_string(),
            "session cleanup outcome is indeterminate during cleanup_execution; reconcile durable state before retrying"
        );
    }

    #[test]
    fn database_error_telemetry_class_never_exposes_sqlite_message_content() {
        let canary = "credential-canary /private/database/path";
        let error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::CannotOpen,
                extended_code: rusqlite::ffi::SQLITE_CANTOPEN,
            },
            Some(canary.to_owned()),
        );

        assert!(error.to_string().contains(canary));
        let class = session_cleanup_database_error_class(&error);
        assert_eq!(class, "access");
        assert!(!class.contains(canary));

        let wrapped_content =
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(canary)));
        assert!(wrapped_content.to_string().contains(canary));
        let wrapped_class = session_cleanup_database_error_class(&wrapped_content);
        assert_eq!(wrapped_class, "rusqlite_contract");
        assert!(!wrapped_class.contains(canary));
    }

    fn make_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // Pin the physical-reclamation proof contract rather than relying on a
        // platform SQLite build's compile-time auto-vacuum default.
        conn.execute_batch("PRAGMA auto_vacuum = NONE; PRAGMA foreign_keys = ON;")
            .unwrap();
        conn.execute_batch(crate::storage::SCHEMA_SQL).unwrap();
        conn
    }

    #[derive(Debug, Clone, Copy)]
    enum LegacyOrphanInsertSurface {
        Checkpoint,
        PaneState,
        RestoreLifecycle,
    }

    fn seed_legacy_orphan<T>(
        conn: &Connection,
        surface: LegacyOrphanInsertSurface,
        seed: impl FnOnce(&Connection) -> T,
    ) -> T {
        let drop_trigger_sql = match surface {
            LegacyOrphanInsertSurface::Checkpoint => {
                "DROP TRIGGER session_checkpoints_retained_size_ai;"
            }
            LegacyOrphanInsertSurface::PaneState => "DROP TRIGGER mux_pane_state_retained_size_ai;",
            LegacyOrphanInsertSurface::RestoreLifecycle => {
                "DROP TRIGGER restore_attempt_lifecycle_retained_size_ai;"
            }
        };
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute_batch(drop_trigger_sql).unwrap();
        let output = seed(conn);
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(crate::storage::SCHEMA_SQL).unwrap();
        output
    }

    fn insert_session(conn: &Connection, id: &str, created_at: i64, shutdown_clean: bool) {
        conn.execute(
            "INSERT INTO mux_sessions (session_id, created_at, shutdown_clean, topology_json, ft_version)
             VALUES (?1, ?2, ?3, '{}', '0.1.0')",
            rusqlite::params![id, created_at, shutdown_clean as i64],
        )
        .unwrap();
        if shutdown_clean {
            insert_v2_clean_receipt(conn, id, created_at);
        }
    }

    fn insert_checkpoint(
        conn: &Connection,
        session_id: &str,
        checkpoint_at: i64,
        total_bytes: i64,
    ) -> i64 {
        let logical_payload = usize::try_from(total_bytes)
            .ok()
            .map(|bytes| "x".repeat(bytes));
        conn.execute(
            "DELETE FROM session_checkpoints
             WHERE id = (
                 SELECT clean_checkpoint_id
                 FROM mux_sessions
                 WHERE session_id = ?1
             )
               AND checkpoint_role = 'restore_receipt'
               AND total_bytes = 0",
            [session_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, metadata_json)
             VALUES (?1, ?2, 'periodic', '0123456789abcdef', 1, ?3, ?4)",
            rusqlite::params![session_id, checkpoint_at, total_bytes, logical_payload],
        )
        .unwrap();
        let checkpoint_id = conn.last_insert_rowid();
        let shutdown_clean: bool = conn
            .query_row(
                "SELECT shutdown_clean FROM mux_sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE mux_sessions
             SET last_checkpoint_at = ?2,
                 clean_checkpoint_id = NULL
             WHERE session_id = ?1",
            rusqlite::params![session_id, checkpoint_at],
        )
        .unwrap();
        if shutdown_clean {
            insert_v2_clean_receipt(conn, session_id, checkpoint_at);
        }
        checkpoint_id
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

    fn insert_v2_clean_receipt(conn: &Connection, session_id: &str, checkpoint_at: i64) -> i64 {
        let metadata_json = r#"{"old_to_new":{}}"#;
        conn.execute(
            "INSERT INTO session_checkpoints
             (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count,
              total_bytes, metadata_json, checkpoint_role, topology_json)
             VALUES (?1, ?2, 'startup', 'pending:rst2', 0, 0, ?3,
                     'restore_receipt', NULL)",
            rusqlite::params![session_id, checkpoint_at, metadata_json],
        )
        .unwrap();
        let checkpoint_id = conn.last_insert_rowid();
        let state_hash = crate::checkpoint_witness::checkpoint_witness(
            crate::checkpoint_witness::CHECKPOINT_ROLE_RESTORE_RECEIPT,
            session_id,
            checkpoint_id,
            checkpoint_at,
            "startup",
            0,
            0,
            Some(metadata_json),
            None,
            &[],
        )
        .expect("compute v2 clean receipt witness");
        conn.execute(
            "UPDATE session_checkpoints SET state_hash = ?1 WHERE id = ?2",
            rusqlite::params![state_hash, checkpoint_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE mux_sessions
             SET shutdown_clean = 1,
                 last_checkpoint_at = ?2,
                 clean_checkpoint_id = ?3
             WHERE session_id = ?1",
            rusqlite::params![session_id, checkpoint_at, checkpoint_id],
        )
        .unwrap();
        checkpoint_id
    }

    fn count_sessions(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))
            .unwrap()
    }

    fn count_checkpoints(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM session_checkpoints
             WHERE checkpoint_role = 'snapshot'",
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

    #[test]
    fn cleanup_candidate_queries_retain_oversized_session_identity() {
        let conn = make_test_db();
        let old =
            i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer") - 90 * 86_400_000;
        let oversized_session_id = "s".repeat(MAX_CHECKPOINT_SESSION_ID_BYTES + 1);
        conn.execute(
            "INSERT INTO mux_sessions (
                 session_id, created_at, last_checkpoint_at, shutdown_clean,
                 topology_json, ft_version
             ) VALUES (?1, ?2, ?2, 1, '{}', '0.1.0')",
            rusqlite::params![oversized_session_id, old],
        )
        .expect("insert oversized session identity fixture");
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role
             ) VALUES (?1, ?2, 'periodic', 'legacy:oversized-session',
                       0, 1048576, 'snapshot')",
            rusqlite::params![oversized_session_id, old],
        )
        .expect("insert oversized session checkpoint fixture");

        assert_eq!(delete_sessions_by_age(&conn, 30).unwrap(), 0);
        assert_eq!(delete_excess_closed_sessions(&conn, 0).unwrap(), 0);
        delete_sessions_by_size(&conn, 0)
            .expect_err("oversized session identity must fail exact byte accounting closed");
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn age_cleanup_never_deletes_session_with_corrupt_v2_clean_receipt() {
        let conn = make_test_db();
        let old =
            i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer") - 90 * 86_400_000;
        insert_session(&conn, "corrupt-v2-clean", old, false);
        let receipt_id = insert_v2_clean_receipt(&conn, "corrupt-v2-clean", old);
        conn.execute(
            "UPDATE session_checkpoints
             SET metadata_json = '{\"old_to_new\":{\"1\":9}}'
             WHERE id = ?1",
            [receipt_id],
        )
        .expect("tamper clean receipt without recomputing witness");

        assert_eq!(delete_sessions_by_age(&conn, 30).unwrap(), 0);
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn unresolved_restore_intent_remains_blocking_after_a_later_snapshot() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let old = now - 90 * 86_400_000;
        insert_session(&conn, "unresolved-restore", old, true);
        let source_id = insert_checkpoint(&conn, "unresolved-restore", old, 2 * 1_024 * 1_024);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role,
                 topology_json
             ) VALUES (
                 'unresolved-restore', ?1, 'startup', 'pending:rsi2',
                 0, 0,
                 '{\"old_to_new\":{},\"restore_attempt\":{\"phase\":\"intent\"}}',
                 'restore_intent', NULL
             )",
            [old + 1],
        )
        .expect("insert explicit unresolved restore intent");
        let intent_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 status, created_at
             ) VALUES (?1, 'unresolved-restore', ?2, 'intent', ?3)",
            rusqlite::params![intent_id, source_id, old + 1],
        )
        .expect("bind authoritative unresolved lifecycle");

        // The later snapshot and clean receipt make the ordinary clean-session
        // authority valid again, but cannot resolve the older external-effect
        // intent merely by displacing it from the latest-row position.
        insert_checkpoint(&conn, "unresolved-restore", old + 2, 2 * 1_024 * 1_024);
        assert!(
            has_unresolved_restore_intent(&conn, "unresolved-restore")
                .expect("query unresolved intent"),
            "intent {intent_id} must remain visible after later rows"
        );

        assert_eq!(delete_sessions_by_age(&conn, 30).unwrap(), 0);
        assert_eq!(delete_excess_closed_sessions(&conn, 0).unwrap(), 0);
        let size = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(size.deleted, 0);
        assert!(size.ineligible_shortfall_bytes > 0);
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn corrupt_or_oversized_legacy_receipt_metadata_fails_closed() {
        let conn = make_test_db();
        let cases = [
            ("corrupt-legacy-receipt", "{".to_string()),
            (
                "oversized-legacy-receipt",
                "x".repeat(MAX_CHECKPOINT_METADATA_BYTES + 1),
            ),
        ];

        for (session_id, metadata_json) in cases {
            insert_session(&conn, session_id, 1_000, false);
            conn.execute(
                "INSERT INTO session_checkpoints (
                     session_id, checkpoint_at, checkpoint_type, state_hash,
                     pane_count, total_bytes, metadata_json, checkpoint_role
                 ) VALUES (?1, 1001, 'startup', 'corrupt:legacy-receipt',
                           0, 0, ?2, 'restore_receipt')",
                rusqlite::params![session_id, metadata_json],
            )
            .expect("insert corrupt legacy receipt fixture");
            assert!(
                has_unresolved_restore_intent(&conn, session_id)
                    .expect("classify corrupt legacy receipt"),
                "{session_id} must remain unresolved"
            );
        }
    }

    #[test]
    fn finite_invalid_lifecycle_status_fails_closed_as_unresolved() {
        let conn = make_test_db();
        insert_session(&conn, "invalid-lifecycle-status", 1_000, false);
        let source_id = insert_checkpoint(&conn, "invalid-lifecycle-status", 1_001, 0);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role
             ) VALUES (
                 'invalid-lifecycle-status', 1002, 'startup', 'pending:rsi2',
                 0, 0,
                 '{\"old_to_new\":{},\"restore_attempt\":{\"phase\":\"intent\"}}',
                 'restore_intent'
             )",
            [],
        )
        .expect("insert intent fixture");
        let intent_id = conn.last_insert_rowid();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .expect("permit lifecycle corruption fixture");
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 status, created_at, resolved_at
             ) VALUES (?1, 'invalid-lifecycle-status', ?2,
                       'corrupt-status', 1002, 1002)",
            rusqlite::params![intent_id, source_id],
        )
        .expect("insert invalid lifecycle status fixture");
        conn.execute_batch("PRAGMA ignore_check_constraints = OFF;")
            .expect("restore constraint enforcement");

        assert!(
            has_unresolved_restore_intent(&conn, "invalid-lifecycle-status")
                .expect("classify invalid lifecycle status")
        );
    }

    #[test]
    fn linked_outcome_cannot_resolve_an_intent_without_lifecycle_authority() {
        let conn = make_test_db();
        let old =
            i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer") - 90 * 86_400_000;
        insert_session(&conn, "missing-lifecycle", old, true);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role
             ) VALUES (
                 'missing-lifecycle', ?1, 'startup', 'pending:rsi2',
                 0, 0, '{\"restore_attempt\":{\"phase\":\"intent\"}}',
                 'restore_intent'
             )",
            [old + 1],
        )
        .expect("insert intent without lifecycle authority");
        let intent_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, metadata_json, checkpoint_role,
                 restore_intent_checkpoint_id
             ) VALUES (
                 'missing-lifecycle', ?1, 'startup', 'pending:rst2',
                 0, 0, '{\"old_to_new\":{},\"restore_attempt\":{\"phase\":\"outcome\"}}',
                 'restore_receipt', ?2
             )",
            rusqlite::params![old + 2, intent_id],
        )
        .expect("insert linked outcome without lifecycle transition");
        insert_session(&conn, "foreign-lifecycle-owner", old, false);
        conn.execute(
            "INSERT INTO restore_attempt_lifecycle (
                 intent_checkpoint_id, session_id, source_checkpoint_id,
                 status, created_at, resolved_at
             ) VALUES (?1, 'foreign-lifecycle-owner', ?2,
                       'resolved', ?3, ?3)",
            rusqlite::params![intent_id, intent_id + 100, old],
        )
        .expect("seed cross-session lifecycle corruption");
        insert_v2_clean_receipt(&conn, "missing-lifecycle", old + 3);

        assert!(
            has_unresolved_restore_intent(&conn, "missing-lifecycle")
                .expect("query missing lifecycle authority")
        );
        assert_eq!(delete_sessions_by_age(&conn, 30).unwrap(), 0);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM mux_sessions WHERE session_id = 'missing-lifecycle'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count protected missing-lifecycle session"),
            1
        );
    }

    #[test]
    fn age_cleanup_uses_recent_checkpoint_for_long_lived_closed_session() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let created_at = now - 90 * 86_400_000;
        insert_session(&conn, "long-lived-recently-closed", created_at, true);
        insert_checkpoint(&conn, "long-lived-recently-closed", now - 1_000, 1_024);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = ?1 WHERE session_id = ?2",
            rusqlite::params![now - 1_000, "long-lived-recently-closed"],
        )
        .unwrap();

        let deleted = delete_sessions_by_age(&conn, 30).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn age_cleanup_uses_authoritative_recency_when_cached_metadata_is_stale() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let old = now - 90 * 86_400_000;

        insert_session(&conn, "recent-created-corrupt-cache", now - 1_000, true);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = -1 WHERE session_id = ?1",
            ["recent-created-corrupt-cache"],
        )
        .unwrap();

        insert_session(&conn, "recent-checkpoint-stale-cache", old, true);
        insert_checkpoint(&conn, "recent-checkpoint-stale-cache", now - 500, 1_024);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = ?1 WHERE session_id = ?2",
            rusqlite::params![old, "recent-checkpoint-stale-cache"],
        )
        .unwrap();

        let deleted = delete_sessions_by_age(&conn, 30).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(count_sessions(&conn), 2);
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

    #[test]
    fn count_cleanup_breaks_equal_timestamps_deterministically() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        for id in ["sess-a", "sess-c", "sess-b"] {
            insert_session(&conn, id, now, true);
        }

        let deleted = delete_excess_closed_sessions(&conn, 1).unwrap();
        assert_eq!(deleted, 2);
        let retained: String = conn
            .query_row("SELECT session_id FROM mux_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained, "sess-c");
    }

    #[test]
    fn count_cleanup_orders_by_authoritative_checkpoint_recency() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let old = now - 90 * 86_400_000;
        insert_session(&conn, "recent-by-created", now - 10_000, true);
        insert_session(&conn, "recent-by-checkpoint", old, true);
        insert_checkpoint(&conn, "recent-by-checkpoint", now - 1_000, 1_024);
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = -1 WHERE session_id = ?1",
            ["recent-by-checkpoint"],
        )
        .unwrap();

        let deleted = delete_excess_closed_sessions(&conn, 1).unwrap();
        assert_eq!(deleted, 1);
        let retained: String = conn
            .query_row("SELECT session_id FROM mux_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained, "recent-by-checkpoint");
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
        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 1); // Deletes oldest (400KB > 176KB needed)
        assert_eq!(outcome.ineligible_shortfall_bytes, 0);
        assert_eq!(count_sessions(&conn), 2);
    }

    #[test]
    fn size_cleanup_noop_when_under_budget() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        insert_session(&conn, "small", now, true);
        insert_checkpoint(&conn, "small", now, 1024); // 1KB

        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 0);
        assert_eq!(outcome.deleted_bytes, 0);
        assert_eq!(outcome.measured_bytes, outcome.retained_bytes);
        assert!(outcome.measured_bytes > 0);
        assert_eq!(outcome.ineligible_shortfall_bytes, 0);
    }

    #[test]
    fn size_cleanup_honors_exact_budget_and_plus_one_boundary() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "boundary", now, true);
        let checkpoint_id = insert_checkpoint(&conn, "boundary", now, 0);
        let baseline: i64 = conn
            .query_row(
                "SELECT retained_bytes FROM session_retained_size
                 WHERE session_id = 'boundary'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let budget = 1_024_u64 * 1_024;
        let padding_len = usize::try_from(budget - u64::try_from(baseline).unwrap()).unwrap();
        let exact_budget_payload = format!("\"{}\"", "x".repeat(padding_len - 2));
        conn.execute(
            "UPDATE session_checkpoints SET topology_json = ?1 WHERE id = ?2",
            rusqlite::params![exact_budget_payload, checkpoint_id],
        )
        .unwrap();

        let at_budget = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(at_budget.measured_bytes, budget);
        assert_eq!(at_budget.deleted, 0);
        assert_eq!(at_budget.deleted_bytes, 0);
        assert_eq!(at_budget.retained_bytes, budget);
        assert_eq!(at_budget.ineligible_shortfall_bytes, 0);

        let plus_one_payload = format!("\"{}\"", "x".repeat(padding_len - 1));
        conn.execute(
            "UPDATE session_checkpoints SET topology_json = ?1 WHERE id = ?2",
            rusqlite::params![plus_one_payload, checkpoint_id],
        )
        .unwrap();
        let over_budget = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(over_budget.measured_bytes, budget + 1);
        assert_eq!(over_budget.deleted, 1);
        assert_eq!(over_budget.deleted_bytes, budget + 1);
        assert_eq!(over_budget.retained_bytes, 0);
        assert_eq!(over_budget.ineligible_shortfall_bytes, 0);
    }

    #[test]
    fn size_cleanup_rejects_summary_drift_before_any_deletion() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "drifted", now, true);
        insert_checkpoint(&conn, "drifted", now, 2 * 1_024 * 1_024);
        conn.execute(
            "UPDATE session_retained_size
             SET checkpoint_row_bytes = checkpoint_row_bytes + 1
             WHERE session_id = 'drifted'",
            [],
        )
        .unwrap();

        let error = delete_sessions_by_size(&conn, 1)
            .expect_err("trigger authority drift must fail before eviction");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 1);
    }

    #[test]
    fn retained_size_trigger_overflow_and_underflow_are_atomic() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "arithmetic", now, false);
        let session_bytes: i64 = conn
            .query_row(
                "SELECT session_row_bytes FROM session_retained_size
                 WHERE session_id = 'arithmetic'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE session_retained_size
             SET checkpoint_row_bytes = ?1
             WHERE session_id = 'arithmetic'",
            [i64::MAX - session_bytes],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT OR IGNORE INTO session_checkpoints (
                     session_id, checkpoint_at, checkpoint_type, state_hash,
                     pane_count, total_bytes
                 ) VALUES ('arithmetic', ?1, 'periodic', 'hash', 0, 0)",
                [now],
            )
            .is_err(),
            "RAISE(ABORT) must defeat outer OR IGNORE at the i64 boundary"
        );
        assert_eq!(count_checkpoints(&conn), 0);

        conn.execute(
            "UPDATE session_retained_size
             SET checkpoint_row_bytes = 0
             WHERE session_id = 'arithmetic'",
            [],
        )
        .unwrap();
        let checkpoint_id = insert_checkpoint(&conn, "arithmetic", now, 0);
        conn.execute(
            "UPDATE session_retained_size
             SET checkpoint_row_bytes = 0
             WHERE session_id = 'arithmetic'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "DELETE FROM session_checkpoints WHERE id = ?1",
                [checkpoint_id],
            )
            .is_err(),
            "trigger subtraction below zero must roll back the deletion"
        );
        assert_eq!(count_checkpoints(&conn), 1);
    }

    #[test]
    fn pane_and_lifecycle_overflow_guards_defeat_outer_or_ignore() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "additive-guards", now, false);
        let source_id = insert_checkpoint(&conn, "additive-guards", now, 0);
        conn.execute(
            "INSERT INTO session_checkpoints (
                 session_id, checkpoint_at, checkpoint_type, state_hash,
                 pane_count, total_bytes, checkpoint_role
             ) VALUES ('additive-guards', ?1, 'startup', 'intent', 0, 0,
                       'restore_intent')",
            [now + 1],
        )
        .unwrap();
        let intent_id = conn.last_insert_rowid();

        conn.execute(
            "UPDATE session_retained_size
             SET pane_state_row_bytes = 9223372036854775807 - retained_bytes
             WHERE session_id = 'additive-guards'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT OR IGNORE INTO mux_pane_state (
                     checkpoint_id, pane_id, terminal_state_json
                 ) VALUES (?1, 7, '{}')",
                [source_id],
            )
            .is_err()
        );
        assert_eq!(count_pane_states(&conn), 0);

        conn.execute(
            "UPDATE session_retained_size
             SET pane_state_row_bytes = 0,
                 restore_lifecycle_row_bytes =
                     9223372036854775807
                     - (session_row_bytes + checkpoint_row_bytes)
             WHERE session_id = 'additive-guards'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT OR IGNORE INTO restore_attempt_lifecycle (
                     intent_checkpoint_id, session_id, source_checkpoint_id,
                     status, created_at
                 ) VALUES (?1, 'additive-guards', ?2, 'intent', ?3)",
                rusqlite::params![intent_id, source_id, now + 2],
            )
            .is_err()
        );
        let lifecycle_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM restore_attempt_lifecycle",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lifecycle_rows, 0);
    }

    #[test]
    fn size_cleanup_evicts_by_authoritative_checkpoint_recency() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let old = now - 90 * 86_400_000;
        insert_session(&conn, "genuinely-old", old, true);
        insert_checkpoint(&conn, "genuinely-old", old, 700 * 1_024);
        insert_session(&conn, "recent-checkpoint-stale-cache", old - 1, true);
        insert_checkpoint(
            &conn,
            "recent-checkpoint-stale-cache",
            now - 1_000,
            700 * 1_024,
        );
        conn.execute(
            "UPDATE mux_sessions SET last_checkpoint_at = ?1 WHERE session_id = ?2",
            rusqlite::params![old - 2, "recent-checkpoint-stale-cache"],
        )
        .unwrap();

        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 1);
        let retained: String = conn
            .query_row("SELECT session_id FROM mux_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained, "recent-checkpoint-stale-cache");
    }

    #[test]
    fn size_cleanup_rejects_negative_checkpoint_accounting_without_deleting() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "corrupt-size", now, true);
        insert_checkpoint(&conn, "corrupt-size", now, -1);

        let error = delete_sessions_by_size(&conn, 1)
            .expect_err("negative byte accounting must fail closed");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn sqlite_timestamp_conversion_rejects_values_that_would_wrap_or_overdelete() {
        assert_eq!(
            u64_to_sqlite_integer(u64::try_from(i64::MAX).unwrap()).unwrap(),
            i64::MAX
        );
        assert!(u64_to_sqlite_integer(u64::MAX).is_err());
    }

    #[test]
    fn size_cleanup_rolls_back_partial_deletes_when_a_later_delete_fails() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        for i in 0..3 {
            let id = format!("sess-{i}");
            insert_session(&conn, &id, now + i * 1000, true);
            insert_checkpoint(&conn, &id, now + i * 1000, 700 * 1024);
        }
        conn.execute_batch(
            "CREATE TABLE synthetic_delete_guard (
                 session_id TEXT PRIMARY KEY
                     REFERENCES mux_sessions(session_id) ON DELETE RESTRICT
             );
             INSERT INTO synthetic_delete_guard(session_id) VALUES ('sess-1');",
        )
        .unwrap();

        delete_sessions_by_size(&conn, 1)
            .expect_err("the second candidate must abort the size-cleanup transaction");
        assert_eq!(count_sessions(&conn), 3);
        assert_eq!(count_checkpoints(&conn), 3);
    }

    #[test]
    fn size_cleanup_rejects_unaudited_ignore_trigger_before_mutation() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        for i in 0..2 {
            let id = format!("sess-{i}");
            insert_session(&conn, &id, now + i * 1000, true);
            insert_checkpoint(&conn, &id, now + i * 1000, 700 * 1024);
        }
        conn.execute_batch(
            "CREATE TRIGGER ignore_first_size_delete
             BEFORE DELETE ON mux_sessions
             WHEN OLD.session_id = 'sess-0'
             BEGIN
                 SELECT RAISE(IGNORE);
             END;",
        )
        .unwrap();

        let error = delete_sessions_by_size(&conn, 1)
            .expect_err("an unaudited ignore trigger must fail before any deletion");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));
        assert_eq!(count_sessions(&conn), 2);
        assert_eq!(count_checkpoints(&conn), 2);
    }

    #[test]
    fn size_cleanup_rejects_temp_schema_triggers_before_mutation() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "sess-temp", now, true);
        insert_checkpoint(&conn, "sess-temp", now, 2 * 1024 * 1024);
        conn.execute_batch(
            "CREATE TEMP TRIGGER ignore_temp_size_delete
             BEFORE DELETE ON mux_sessions
             BEGIN
                 SELECT RAISE(IGNORE);
             END;",
        )
        .unwrap();

        delete_sessions_by_size(&conn, 1)
            .expect_err("TEMP triggers on authoritative tables must fail closed");
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 1);
    }

    #[test]
    fn retention_trigger_guard_is_identifier_case_insensitive() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "sess-case", now, true);
        insert_checkpoint(&conn, "sess-case", now, 2 * 1024 * 1024);
        conn.execute_batch(
            "CREATE TRIGGER ignore_uppercase_size_delete
             BEFORE DELETE ON MUX_SESSIONS
             BEGIN
                 SELECT RAISE(IGNORE);
             END;",
        )
        .unwrap();

        delete_sessions_by_size(&conn, 1)
            .expect_err("identifier casing must not bypass the persistent trigger guard");
        assert_eq!(count_sessions(&conn), 1);

        conn.execute_batch(
            "DROP TRIGGER ignore_uppercase_size_delete;
             CREATE TEMP TRIGGER ignore_mixed_case_size_delete
             BEFORE DELETE ON MuX_sEsSiOnS
             BEGIN
                 SELECT RAISE(IGNORE);
             END;",
        )
        .unwrap();
        delete_sessions_by_size(&conn, 1)
            .expect_err("identifier casing must not bypass the TEMP trigger guard");
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 1);
    }

    #[test]
    fn size_cleanup_reports_shortfall_when_active_data_alone_exceeds_budget() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "active-large", now, false);
        insert_checkpoint(&conn, "active-large", now, 2 * 1024 * 1024);
        insert_session(&conn, "closed-small", now - 1, true);
        insert_checkpoint(&conn, "closed-small", now - 1, 400 * 1024);

        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 1);
        assert_eq!(
            outcome.ineligible_shortfall_bytes,
            outcome.retained_bytes - 1024 * 1024
        );
        assert_eq!(
            outcome.measured_bytes,
            outcome.deleted_bytes + outcome.retained_bytes
        );
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn size_receipt_and_active_shortfall_remain_consistent_across_restart() {
        let file = tempfile::NamedTempFile::new().expect("temporary retention database");
        let path = file.path().to_path_buf();
        let conn = Connection::open(&path).expect("open retention database");
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(crate::storage::SCHEMA_SQL).unwrap();
        let now = epoch_ms() as i64;
        insert_session(&conn, "restart-active", now, false);
        insert_checkpoint(&conn, "restart-active", now, 2 * 1_024 * 1_024);
        insert_session(&conn, "restart-closed", now - 1, true);
        insert_checkpoint(&conn, "restart-closed", now - 1, 400 * 1_024);
        drop(conn);

        let reopened = Connection::open(&path).expect("reopen before cleanup");
        reopened
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        let outcome = delete_sessions_by_size(&reopened, 1).unwrap();
        assert_eq!(outcome.deleted, 1);
        assert_eq!(
            outcome.measured_bytes,
            outcome.deleted_bytes + outcome.retained_bytes
        );
        assert_eq!(
            outcome.ineligible_shortfall_bytes,
            outcome.retained_bytes - 1_024 * 1_024
        );
        drop(reopened);

        let restarted = Connection::open(&path).expect("reopen after cleanup");
        let retained: i64 = restarted
            .query_row(
                "SELECT COALESCE(SUM(retained_bytes), 0)
                 FROM session_retained_size",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(u64::try_from(retained).unwrap(), outcome.retained_bytes);
        assert_eq!(
            outcome.ineligible_shortfall_bytes,
            outcome.retained_bytes - 1_024 * 1_024
        );
    }

    #[test]
    fn size_cleanup_charges_and_deletes_closed_session_without_payload_checkpoint() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "active-large", now, false);
        insert_checkpoint(&conn, "active-large", now, 2 * 1024 * 1024);
        insert_session(&conn, "closed-without-checkpoint", now - 1, true);

        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 1);
        assert!(outcome.deleted_bytes > 0);
        assert_eq!(
            outcome.ineligible_shortfall_bytes,
            outcome.retained_bytes - 1024 * 1024
        );
        assert_eq!(count_sessions(&conn), 1);
    }

    #[test]
    fn size_cleanup_excludes_orphan_bytes_from_eviction_and_shortfall() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        insert_session(&conn, "valid-closed", now, true);
        insert_checkpoint(&conn, "valid-closed", now, 400 * 1024);

        // Simulate legacy/corrupt orphan data. Its 2 MiB is reclaimed by the
        // orphan phase, not by evicting an unrelated valid session.
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::Checkpoint, |conn| {
            conn.execute(
                "INSERT INTO session_checkpoints
                 (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
                 VALUES ('orphan-session', ?1, 'periodic', 'orphan', 0, ?2)",
                rusqlite::params![now, 2 * 1024 * 1024],
            )
            .unwrap();
        });

        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 0);
        assert_eq!(outcome.deleted_bytes, 0);
        assert_eq!(outcome.measured_bytes, outcome.retained_bytes);
        assert!(outcome.measured_bytes > 0);
        assert_eq!(outcome.ineligible_shortfall_bytes, 0);
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 2);

        let orphaned = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(orphaned.orphaned_restore_lifecycle_rows, 0);
        assert_eq!(orphaned.orphaned_checkpoints, 1);
        assert_eq!(orphaned.orphaned_pane_states, 0);
        assert_eq!(count_sessions(&conn), 1);
        assert_eq!(count_checkpoints(&conn), 1);
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
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::Checkpoint, |conn| {
            conn.execute(
                "INSERT INTO session_checkpoints
                 (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
                 VALUES ('orphan-sess', ?1, 'periodic', '0123456789abcdef', 0, 0)",
                [now],
            )
            .unwrap();
        });

        assert_eq!(count_checkpoints(&conn), 2);

        let orphaned = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(orphaned.orphaned_checkpoints, 1);
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
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::PaneState, |conn| {
            conn.execute(
                "INSERT INTO mux_pane_state
                 (checkpoint_id, pane_id, terminal_state_json)
                 VALUES (99999, 42, '{}')",
                [],
            )
            .unwrap();
        });

        assert_eq!(count_pane_states(&conn), 2);

        let orphaned = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(orphaned.orphaned_pane_states, 1);
        assert_eq!(count_pane_states(&conn), 1);
    }

    #[test]
    fn cleanup_collects_pane_state_children_of_orphan_checkpoint() {
        // ft-rt6ol: a checkpoint orphaned from mux_sessions that STILL has
        // mux_pane_state children must be fully collected in ONE pass. The old
        // pane_state-first ordering left the children behind (the child's
        // checkpoint still existed at the first DELETE, then was removed by the
        // second DELETE, orphaning the child).
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Orphan checkpoint (ghost session) WITH a pane_state child, inserted
        // with FK enforcement off to simulate an older/corrupt DB.
        let orphan_cp_id = seed_legacy_orphan(
            &conn,
            LegacyOrphanInsertSurface::Checkpoint,
            |conn| {
                conn.execute(
                    "INSERT INTO session_checkpoints
                     (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
                     VALUES ('orphan-sess', ?1, 'periodic', '0123456789abcdef', 1, 0)",
                    [now],
                )
                .unwrap();
                conn.last_insert_rowid()
            },
        );
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::PaneState, |conn| {
            conn.execute(
                "INSERT INTO mux_pane_state (checkpoint_id, pane_id, terminal_state_json)
                 VALUES (?1, 7, '{}')",
                [orphan_cp_id],
            )
            .unwrap();
        });

        assert_eq!(count_checkpoints(&conn), 1);
        assert_eq!(count_pane_states(&conn), 1);

        // One pass must remove BOTH the orphan checkpoint and its child.
        let orphaned = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(
            orphaned.orphaned_checkpoints, 1,
            "orphan checkpoint removed"
        );
        assert_eq!(
            orphaned.orphaned_pane_states, 1,
            "its pane_state child collected in the same pass"
        );
        assert_eq!(count_checkpoints(&conn), 0);
        assert_eq!(
            count_pane_states(&conn),
            0,
            "no orphan pane_state may remain after one cleanup pass (ft-rt6ol)"
        );
    }

    #[test]
    fn cleanup_removes_orphan_lifecycle_before_its_intent_checkpoint() {
        let conn = make_test_db();
        let now = i64::try_from(epoch_ms()).expect("test epoch fits SQLite integer");
        let intent_id = seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::Checkpoint, |conn| {
            conn.execute(
                "INSERT INTO session_checkpoints (
                         session_id, checkpoint_at, checkpoint_type, state_hash,
                         pane_count, total_bytes, metadata_json, checkpoint_role
                     ) VALUES (
                         'orphan-attempt', ?1, 'startup', 'pending:rsi2',
                         0, 0, '{\"restore_attempt\":{\"phase\":\"intent\"}}',
                         'restore_intent'
                     )",
                [now],
            )
            .expect("seed orphan restore intent");
            conn.last_insert_rowid()
        });
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::RestoreLifecycle, |conn| {
            conn.execute(
                "INSERT INTO restore_attempt_lifecycle (
                         intent_checkpoint_id, session_id, source_checkpoint_id,
                         status, created_at
                     ) VALUES (?1, 'orphan-attempt', ?2,
                               'reconciliation_required', ?3)",
                rusqlite::params![intent_id, intent_id + 1, now],
            )
            .expect("seed orphan restore lifecycle");
        });

        let orphaned = cleanup_orphaned_data(&conn).unwrap();
        assert_eq!(orphaned.orphaned_restore_lifecycle_rows, 1);
        assert_eq!(orphaned.orphaned_checkpoints, 1);
        assert_eq!(orphaned.orphaned_pane_states, 0);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM restore_attempt_lifecycle",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count retained lifecycle rows"),
            0
        );
    }

    #[test]
    fn full_cleanup_reports_lifecycle_only_orphan_as_completed_work() {
        let conn = make_test_db();
        seed_legacy_orphan(&conn, LegacyOrphanInsertSurface::RestoreLifecycle, |conn| {
            conn.execute(
                "INSERT INTO restore_attempt_lifecycle (
                         intent_checkpoint_id, session_id, source_checkpoint_id,
                         status, created_at
                     ) VALUES (999, 'missing-session', 998, 'intent', 1000)",
                [],
            )
            .expect("seed lifecycle-only orphan");
        });

        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 0,
        };
        let result = cleanup_sessions(&conn, &config).expect("clean lifecycle-only orphan");

        assert_eq!(result.total_sessions_deleted(), 0);
        assert_eq!(result.orphaned_restore_lifecycle_rows, 1);
        assert_eq!(result.orphaned_checkpoints, 0);
        assert_eq!(result.orphaned_pane_states, 0);
        assert!(result.any_work_done());
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM restore_attempt_lifecycle",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count remaining lifecycle rows"),
            0
        );
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

    // ====================================================================
    // CleanupResult additional tests
    // ====================================================================

    #[test]
    fn cleanup_result_any_work_done_orphan_checkpoints_only() {
        let result = CleanupResult {
            orphaned_checkpoints: 5,
            ..Default::default()
        };
        assert!(result.any_work_done());
        assert_eq!(result.total_sessions_deleted(), 0);
    }

    #[test]
    fn cleanup_result_any_work_done_orphan_pane_states_only() {
        let result = CleanupResult {
            orphaned_pane_states: 3,
            ..Default::default()
        };
        assert!(result.any_work_done());
    }

    #[test]
    fn cleanup_result_any_work_done_orphan_restore_lifecycle_only() {
        let result = CleanupResult {
            orphaned_restore_lifecycle_rows: 2,
            ..Default::default()
        };
        assert!(result.any_work_done());
        assert_eq!(result.total_sessions_deleted(), 0);
    }

    #[test]
    fn cleanup_result_debug() {
        let result = CleanupResult {
            deleted_by_age: 1,
            deleted_by_count: 2,
            deleted_by_size: 3,
            orphaned_restore_lifecycle_rows: 6,
            orphaned_checkpoints: 4,
            orphaned_pane_states: 5,
            ..Default::default()
        };
        let dbg = format!("{result:?}");
        assert!(dbg.contains("CleanupResult"));
        assert!(dbg.contains("orphaned_restore_lifecycle_rows"));
    }

    #[test]
    fn cleanup_result_clone() {
        let result = CleanupResult {
            deleted_by_age: 10,
            ..Default::default()
        };
        let result2 = result.clone();
        assert_eq!(result2.deleted_by_age, 10);
        assert_eq!(result2.total_sessions_deleted(), 10);
    }

    #[test]
    fn cleanup_result_total_combines_all_sources() {
        let result = CleanupResult {
            deleted_by_age: 5,
            deleted_by_count: 3,
            deleted_by_size: 2,
            ..Default::default()
        };
        assert_eq!(result.total_sessions_deleted(), 10);
    }

    #[test]
    fn cleanup_result_any_work_done_all_zero_sessions_but_orphans() {
        let result = CleanupResult {
            deleted_by_age: 0,
            deleted_by_count: 0,
            deleted_by_size: 0,
            orphaned_restore_lifecycle_rows: 0,
            orphaned_checkpoints: 1,
            orphaned_pane_states: 0,
            ..Default::default()
        };
        assert!(result.any_work_done());
    }

    // ====================================================================
    // epoch_ms test
    // ====================================================================

    #[test]
    fn epoch_ms_returns_positive() {
        let ms = epoch_ms();
        assert!(ms > 0);
    }

    #[test]
    fn epoch_ms_reasonable_range() {
        // Should be after 2024-01-01 (1704067200000ms)
        let ms = epoch_ms();
        assert!(ms > 1_704_067_200_000);
    }

    // ====================================================================
    // Config serde
    // ====================================================================

    #[test]
    fn config_serde_roundtrip() {
        let config = SessionRetentionConfig {
            max_age_days: 60,
            max_closed_sessions: 100,
            max_total_size_mb: 1000,
            cleanup_interval_hours: 12,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SessionRetentionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_age_days, 60);
        assert_eq!(back.max_closed_sessions, 100);
        assert_eq!(back.max_total_size_mb, 1000);
        assert_eq!(back.cleanup_interval_hours, 12);
    }

    // ====================================================================
    // Edge case: empty database cleanup
    // ====================================================================

    #[test]
    fn cleanup_empty_database() {
        let conn = make_test_db();
        let config = SessionRetentionConfig {
            max_age_days: 30,
            max_closed_sessions: 50,
            max_total_size_mb: 500,
            cleanup_interval_hours: 24,
        };
        let result = cleanup_sessions(&conn, &config).unwrap();
        assert_eq!(result.total_sessions_deleted(), 0);
        assert!(!result.any_work_done());
    }

    // ====================================================================
    // Edge case: only active sessions
    // ====================================================================

    #[test]
    fn cleanup_only_active_sessions_deletes_nothing() {
        let conn = make_test_db();
        let old = (epoch_ms() as i64) - 90 * 86_400_000; // 90 days ago

        // All sessions are active (shutdown_clean = false)
        for i in 0..5 {
            insert_session(&conn, &format!("active-{i}"), old, false);
        }

        let config = SessionRetentionConfig {
            max_age_days: 30,
            max_closed_sessions: 2,
            max_total_size_mb: 0, // disabled
            cleanup_interval_hours: 24,
        };
        let result = cleanup_sessions(&conn, &config).unwrap();
        assert_eq!(result.deleted_by_age, 0);
        // excess closed sessions check also skips active
        assert_eq!(count_sessions(&conn), 5);
    }

    // ====================================================================
    // Online cleanup issues no explicit compaction; this fixture pins NONE
    // ====================================================================

    #[test]
    fn cleanup_leaves_reclaimable_pages_for_sqlite_reuse_after_many_deletions() {
        let conn = make_test_db();
        let auto_vacuum: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            auto_vacuum, 0,
            "freelist-reuse proof requires auto_vacuum=NONE"
        );
        let now = epoch_ms() as i64;
        let old = now - 31 * 86_400_000;

        // Large inline topology values make the deleted rows occupy enough
        // pages for freelist_count to be a deterministic physical-compaction
        // witness. Full VACUUM would drive this count back to zero.
        let payload = "x".repeat(32 * 1024);
        for i in 0..12 {
            conn.execute(
                "INSERT INTO mux_sessions
                 (session_id, created_at, shutdown_clean, topology_json, ft_version)
                 VALUES (?1, ?2, 1, ?3, '0.1.0')",
                rusqlite::params![format!("old-{i}"), old, &payload],
            )
            .unwrap();
        }

        let config = SessionRetentionConfig {
            max_age_days: 30,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 24,
        };
        let result = cleanup_sessions(&conn, &config).unwrap();
        assert_eq!(result.deleted_by_age, 12);
        let reusable_pages: i64 = conn
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))
            .unwrap();
        assert!(
            reusable_pages > 0,
            "retention cleanup must preserve freed pages for SQLite reuse"
        );

        // A sentinel interactive write must remain available immediately after
        // cleanup; under this pinned NONE policy, cleanup does not compact.
        insert_session(&conn, "interactive-sentinel", now, false);
        assert_eq!(count_sessions(&conn), 1);
    }

    // ── Batch: DarkBadger wa-1u90p.7.1 ───────────────────────────────────

    // ── CleanupResult trait coverage ──

    #[test]
    fn cleanup_result_default_all_zeros() {
        let r = CleanupResult::default();
        assert_eq!(r.deleted_by_age, 0);
        assert_eq!(r.deleted_by_count, 0);
        assert_eq!(r.deleted_by_size, 0);
        assert_eq!(r.size_measured_bytes, 0);
        assert_eq!(r.size_deleted_bytes, 0);
        assert_eq!(r.size_retained_bytes, 0);
        assert_eq!(r.size_ineligible_shortfall_bytes, 0);
        assert_eq!(r.orphaned_restore_lifecycle_rows, 0);
        assert_eq!(r.orphaned_checkpoints, 0);
        assert_eq!(r.orphaned_pane_states, 0);
        assert_eq!(r.total_sessions_deleted(), 0);
        assert!(!r.any_work_done());
    }

    #[test]
    fn cleanup_result_total_single_source_age() {
        let r = CleanupResult {
            deleted_by_age: 7,
            ..Default::default()
        };
        assert_eq!(r.total_sessions_deleted(), 7);
        assert!(r.any_work_done());
    }

    #[test]
    fn cleanup_result_total_single_source_count() {
        let r = CleanupResult {
            deleted_by_count: 4,
            ..Default::default()
        };
        assert_eq!(r.total_sessions_deleted(), 4);
        assert!(r.any_work_done());
    }

    #[test]
    fn cleanup_result_total_single_source_size() {
        let r = CleanupResult {
            deleted_by_size: 2,
            ..Default::default()
        };
        assert_eq!(r.total_sessions_deleted(), 2);
        assert!(r.any_work_done());
    }

    // ── Multi-phase cleanup ──

    #[test]
    fn full_cleanup_all_phases_fire() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        let old = now - 31 * 86_400_000; // 31 days ago

        // 1 old closed session → age cleanup
        insert_session(&conn, "age-victim", old, true);

        // 5 recent closed sessions → count cleanup (keep only 2)
        for i in 0..5 {
            let id = format!("recent-{i}");
            insert_session(&conn, &id, now + i * 1000, true);
            // give each 600KB of checkpoint data → total 3000KB > 2MB budget for last 2
            insert_checkpoint(&conn, &id, now + i * 1000, 600 * 1024);
        }

        let config = SessionRetentionConfig {
            max_age_days: 30,
            max_closed_sessions: 2,
            max_total_size_mb: 2,
            cleanup_interval_hours: 24,
        };
        let result = cleanup_sessions(&conn, &config).unwrap();

        // age should delete "age-victim"
        assert!(result.deleted_by_age >= 1);
        // count should delete excess beyond 2
        // After age delete: 5 remain, keep 2 → delete 3
        assert!(result.deleted_by_count >= 1);
    }

    // ── Size cleanup: multiple deletions needed ──

    #[test]
    fn size_cleanup_deletes_multiple_sessions() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // 4 sessions × 400 KiB = 1600 KiB.
        for i in 0..4 {
            let id = format!("big-{i}");
            insert_session(&conn, &id, now + i * 1000, true);
            insert_checkpoint(&conn, &id, now + i * 1000, 400 * 1024);
        }

        // Budget: 1 MiB = 1024 KiB. Total is 1600 KiB, so freeing 576 KiB
        // deterministically deletes the two oldest 400 KiB sessions.
        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 2);
        assert_eq!(outcome.ineligible_shortfall_bytes, 0);
        assert_eq!(count_sessions(&conn), 2);
    }

    // ── Excess closed: mixed active and closed ──

    #[test]
    fn excess_closed_ignores_active_sessions() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // 3 active sessions (should NOT be counted or deleted)
        for i in 0..3 {
            insert_session(&conn, &format!("active-{i}"), now + i * 1000, false);
        }

        // 4 closed sessions → keep 2 → delete 2
        for i in 0..4 {
            insert_session(&conn, &format!("closed-{i}"), now + (i + 3) * 1000, true);
        }

        let deleted = delete_excess_closed_sessions(&conn, 2).unwrap();
        assert_eq!(deleted, 2);
        // All 3 active remain + 2 closed = 5
        assert_eq!(count_sessions(&conn), 5);
    }

    #[test]
    fn excess_closed_noop_when_under_limit() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        insert_session(&conn, "s1", now, true);
        insert_session(&conn, "s2", now + 1000, true);

        let deleted = delete_excess_closed_sessions(&conn, 10).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(count_sessions(&conn), 2);
    }

    // ── Age boundary ──

    #[test]
    fn age_cleanup_boundary_exactly_at_cutoff() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        // Slightly newer than 30 days ago (+100ms) to account for time elapsed
        // between this epoch_ms() call and the one inside delete_sessions_by_age.
        // The query uses `created_at < cutoff`, so anything at or after the cutoff
        // should NOT be deleted.
        let just_inside_30_days = now - 30 * 86_400_000 + 100;

        insert_session(&conn, "boundary", just_inside_30_days, true);

        let deleted = delete_sessions_by_age(&conn, 30).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn age_cleanup_one_ms_past_cutoff() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        // 1ms past the 30-day cutoff
        let just_past = now - 30 * 86_400_000 - 1;

        insert_session(&conn, "just-past", just_past, true);

        let deleted = delete_sessions_by_age(&conn, 30).unwrap();
        assert_eq!(deleted, 1);
    }

    // ── Cascade: multiple checkpoints per session ──

    #[test]
    fn cascade_deletes_multiple_checkpoints_and_pane_states() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        let old = now - 31 * 86_400_000;

        insert_session(&conn, "multi-cp", old, true);
        let cp1 = insert_checkpoint(&conn, "multi-cp", old, 1024);
        let cp2 = insert_checkpoint(&conn, "multi-cp", old + 1000, 2048);
        let cp3 = insert_checkpoint(&conn, "multi-cp", old + 2000, 512);
        insert_pane_state(&conn, cp1, 1);
        insert_pane_state(&conn, cp1, 2);
        insert_pane_state(&conn, cp2, 3);
        insert_pane_state(&conn, cp3, 4);
        insert_pane_state(&conn, cp3, 5);

        assert_eq!(count_checkpoints(&conn), 3);
        assert_eq!(count_pane_states(&conn), 5);

        delete_sessions_by_age(&conn, 30).unwrap();

        assert_eq!(count_sessions(&conn), 0);
        assert_eq!(count_checkpoints(&conn), 0);
        assert_eq!(count_pane_states(&conn), 0);
    }

    // ── Orphan cleanup noop ──

    #[test]
    fn orphan_cleanup_noop_when_no_orphans() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        insert_session(&conn, "valid", now, true);
        let cp = insert_checkpoint(&conn, "valid", now, 1024);
        insert_pane_state(&conn, cp, 1);

        assert_eq!(
            cleanup_orphaned_data(&conn).unwrap(),
            OrphanCleanupOutcome::default()
        );
    }

    #[test]
    fn orphan_cleanup_on_empty_db() {
        let conn = make_test_db();
        assert_eq!(
            cleanup_orphaned_data(&conn).unwrap(),
            OrphanCleanupOutcome::default()
        );
    }

    // ── Size cleanup: sessions with only clean-receipt checkpoints ──

    #[test]
    fn size_cleanup_with_only_session_and_clean_receipt_payloads() {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Session rows and clean receipts are charged even without a snapshot.
        for i in 0..5 {
            insert_session(&conn, &format!("nochk-{i}"), now + i * 1000, true);
        }

        let outcome = delete_sessions_by_size(&conn, 1).unwrap();
        assert_eq!(outcome.deleted, 0);
        assert_eq!(outcome.deleted_bytes, 0);
        assert_eq!(outcome.measured_bytes, outcome.retained_bytes);
        assert!(outcome.measured_bytes > 0);
        assert_eq!(outcome.ineligible_shortfall_bytes, 0);
        assert_eq!(count_sessions(&conn), 5);
    }

    // ── Config edge cases ──

    #[test]
    fn overflowing_size_budget_fails_closed_even_on_an_empty_database() {
        let config = SessionRetentionConfig {
            max_age_days: u64::MAX,
            max_closed_sessions: usize::MAX,
            max_total_size_mb: u64::MAX,
            cleanup_interval_hours: u64::MAX,
        };
        let conn = make_test_db();
        let error = cleanup_sessions(&conn, &config)
            .expect_err("MiB-to-byte overflow must not silently become an unlimited budget");
        assert!(matches!(error, rusqlite::Error::ToSqlConversionFailure(_)));
        assert_eq!(count_sessions(&conn), 0);
    }

    #[test]
    fn config_serde_preserves_zero_values() {
        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 0,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SessionRetentionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_age_days, 0);
        assert_eq!(back.max_closed_sessions, 0);
        assert_eq!(back.max_total_size_mb, 0);
        assert_eq!(back.cleanup_interval_hours, 0);
    }

    #[test]
    fn config_debug_impl() {
        let config = SessionRetentionConfig::default();
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("SessionRetentionConfig"));
    }

    #[test]
    fn config_clone_preserves_all_fields() {
        let config = SessionRetentionConfig {
            max_age_days: 7,
            max_closed_sessions: 10,
            max_total_size_mb: 100,
            cleanup_interval_hours: 6,
        };
        let cloned = config.clone();
        assert_eq!(cloned.max_age_days, 7);
        assert_eq!(cloned.max_closed_sessions, 10);
        assert_eq!(cloned.max_total_size_mb, 100);
        assert_eq!(cloned.cleanup_interval_hours, 6);
    }

    // ── epoch_ms additional ──

    #[test]
    fn epoch_ms_is_a_saturating_system_time_conversion() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let observed = u128::from(epoch_ms());
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        // Wall time is allowed to step in either direction. Only assert an
        // ordinary-range conversion when both surrounding observations form
        // an ordered interval; otherwise the clock moved and there is no
        // monotonic contract to test.
        if before <= after && after <= u128::from(u64::MAX) {
            assert!((before..=after).contains(&observed));
        } else {
            assert!(observed <= u128::from(u64::MAX));
        }
    }
}
