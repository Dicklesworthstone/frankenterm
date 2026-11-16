//! [ft-nsb8c / ft-dn2tu Phase 5] Export Query Functions
//!
//! Read-only query helpers that export segments / gaps / workflows /
//! sessions / reservations to callers like `ft export ...` and the
//! audit/forensics surface. Lifted out of `storage.rs` (~340 LOC) so
//! the storage facade keeps its writer-thread + handle responsibilities
//! and the read-side query surface lives next to its siblings under
//! `storage/`.
//!
//! Visibility: each function is `pub(super)` so `StorageHandle::export_*`
//! methods in `storage.rs` can keep calling them directly via
//! `export::query_export_*`. Export is a data-faithfulness boundary:
//! native SQL cells preserve NULL versus empty TEXT and reject wrong storage
//! classes. Malformed JSON and non-finite costs fail the query rather than
//! silently changing a present value into an absent one.

use super::{
    AgentSessionRecord, ExportQuery, Gap, PaneReservation, Segment, StorageError, WorkflowRecord,
};
use crate::error::Result;
use crate::storage_backend_row_helpers::CellRowReader;
use crate::storage_backend_trait::{BackendError, SqlCell, StorageBackend, ToSqlValue};

/// Build a dynamic WHERE clause and params from an ExportQuery.
/// `time_column` is the column name used for since/until filtering.
pub(super) fn build_export_where(
    query: &ExportQuery,
    time_column: &str,
) -> Result<(String, Vec<ToSqlValue<'static>>)> {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<ToSqlValue<'static>> = Vec::new();

    if let Some(pane_id) = query.pane_id {
        let pane_id = i64::try_from(pane_id)
            .map_err(|_| StorageError::Database("export pane_id exceeds i64 range".to_string()))?;
        clauses.push(format!("pane_id = ?{}", params.len() + 1));
        params.push(ToSqlValue::Integer(pane_id));
    }
    if let Some(since) = query.since {
        clauses.push(format!("{time_column} >= ?{}", params.len() + 1));
        params.push(ToSqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        clauses.push(format!("{time_column} <= ?{}", params.len() + 1));
        params.push(ToSqlValue::Integer(until));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    Ok((where_clause, params))
}

fn export_backend_error(context: &str, err: BackendError) -> StorageError {
    StorageError::Database(format!("{context}: {err}"))
}

fn export_i64_to_u64(value: i64, label: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| {
        StorageError::Database(format!("{label} value {value} is out of u64 range")).into()
    })
}

fn export_i64_to_usize(value: i64, label: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        StorageError::Database(format!("{label} value {value} is out of usize range")).into()
    })
}

fn export_limit_value(limit: usize) -> Result<ToSqlValue<'static>> {
    i64::try_from(limit).map(ToSqlValue::Integer).map_err(|_| {
        StorageError::Database(format!("export limit {limit} exceeds i64 range")).into()
    })
}

// ============================================================================
// br-ft-l3u5k: export JSON-column parse-drop observability
// ============================================================================

/// br-ft-l3u5k: cumulative count of `optional_json_value` parse
/// failures observed since process load. Pre-fix `.ok()` silently
/// turned a corrupted JSON column into `Ok(None)` — indistinguishable
/// from "column was actually NULL". A backup/audit export then had
/// missing fields that looked intentional. Export is operator-facing
/// and meant to be a faithful snapshot, so silent drops corrupt the
/// downstream forensic record.
///
/// NULL column is NOT counted (legitimate absence — handled by the
/// early return on `optional_string -> None`); only column-present-
/// but-fails-to-parse bumps the counter.
///
/// Same observability defect family as ft-yygus, ft-k6uwb, ft-zhnaw,
/// ft-94cdu, ft-lqj5g, ft-4ymqn, ft-bn6qi, ft-crpvd, ft-0n4nx.
static STORAGE_EXPORT_JSON_PARSE_DROP_COUNT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// br-ft-l3u5k: cumulative count of export JSON-column parse failures.
/// The historical name is retained for callers; failures now reject the export
/// instead of dropping the malformed field.
#[must_use]
pub(super) fn storage_export_json_parse_drop_count() -> u64 {
    STORAGE_EXPORT_JSON_PARSE_DROP_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// br-ft-l3u5k: test helper to reset the parse-drop counter.
#[cfg(test)]
pub fn reset_storage_export_json_parse_drop_count_for_test() {
    STORAGE_EXPORT_JSON_PARSE_DROP_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Parse a present JSON column, preserving the parse-failure counter but never
/// returning a fabricated absence. Diagnostics identify the field and position,
/// not the potentially sensitive workflow or session payload.
fn parse_export_json_column(raw: &str, label: &str, column: usize) -> Result<serde_json::Value> {
    match serde_json::from_str(raw) {
        Ok(value) => Ok(value),
        Err(err) => {
            STORAGE_EXPORT_JSON_PARSE_DROP_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                target: "ft.storage.export",
                event = "storage_export_json_parse_drop",
                label = label,
                column = column,
                raw_len = raw.len(),
                json_line = err.line(),
                json_column = err.column(),
                "export JSON column failed to parse; export rejected"
            );
            Err(StorageError::Database(format!(
                "{label}: malformed JSON at line {} column {}",
                err.line(),
                err.column()
            ))
            .into())
        }
    }
}

fn optional_json_value(
    reader: &CellRowReader<'_>,
    column: usize,
    label: &str,
) -> Result<Option<serde_json::Value>> {
    let Some(raw) = reader
        .optional_string(column)
        .map_err(|err| export_backend_error(label, err))?
    else {
        return Ok(None);
    };
    parse_export_json_column(&raw, label, column).map(Some)
}

fn optional_f64(reader: &CellRowReader<'_>, column: usize, label: &str) -> Result<Option<f64>> {
    if reader.is_null(column) {
        return Ok(None);
    }
    let value = reader
        .f64(column)
        .map_err(|err| export_backend_error(label, err))?;
    if !value.is_finite() {
        return Err(StorageError::Database(format!("{label}: non-finite cost")).into());
    }
    Ok(Some(value))
}

fn query_map_export<T>(
    backend: &dyn StorageBackend,
    sql: &str,
    params: &[ToSqlValue<'_>],
    query_context: &str,
    decode: fn(&[SqlCell]) -> Result<T>,
) -> Result<Vec<T>> {
    // The trait's compatibility fallback reconstructs cells from strings and
    // cannot recover NULL versus empty TEXT. An export must not use that path.
    if !backend.supports_lossless_cells() {
        return Err(StorageError::Database(format!(
            "{query_context}: backend does not support lossless SQL cells"
        ))
        .into());
    }
    backend
        .query_map_cells(sql, params)
        .map_err(|err| export_backend_error(query_context, err))?
        .into_iter()
        .map(|row| decode(&row))
        .collect()
}

fn segment_from_backend_row(row: &[SqlCell]) -> Result<Segment> {
    let reader = CellRowReader::new(row);
    Ok(Segment {
        id: reader
            .i64(0)
            .map_err(|err| export_backend_error("output_segments.id", err))?,
        pane_id: export_i64_to_u64(
            reader
                .i64(1)
                .map_err(|err| export_backend_error("output_segments.pane_id", err))?,
            "output_segments.pane_id",
        )?,
        seq: export_i64_to_u64(
            reader
                .i64(2)
                .map_err(|err| export_backend_error("output_segments.seq", err))?,
            "output_segments.seq",
        )?,
        content: reader
            .string(3)
            .map_err(|err| export_backend_error("output_segments.content", err))?,
        content_len: export_i64_to_usize(
            reader
                .i64(4)
                .map_err(|err| export_backend_error("output_segments.content_len", err))?,
            "output_segments.content_len",
        )?,
        content_hash: reader
            .optional_string(5)
            .map_err(|err| export_backend_error("output_segments.content_hash", err))?,
        captured_at: reader
            .i64(6)
            .map_err(|err| export_backend_error("output_segments.captured_at", err))?,
    })
}

fn gap_from_backend_row(row: &[SqlCell]) -> Result<Gap> {
    let reader = CellRowReader::new(row);
    Ok(Gap {
        id: reader
            .i64(0)
            .map_err(|err| export_backend_error("output_gaps.id", err))?,
        pane_id: export_i64_to_u64(
            reader
                .i64(1)
                .map_err(|err| export_backend_error("output_gaps.pane_id", err))?,
            "output_gaps.pane_id",
        )?,
        seq_before: export_i64_to_u64(
            reader
                .i64(2)
                .map_err(|err| export_backend_error("output_gaps.seq_before", err))?,
            "output_gaps.seq_before",
        )?,
        seq_after: export_i64_to_u64(
            reader
                .i64(3)
                .map_err(|err| export_backend_error("output_gaps.seq_after", err))?,
            "output_gaps.seq_after",
        )?,
        reason: reader
            .string(4)
            .map_err(|err| export_backend_error("output_gaps.reason", err))?,
        detected_at: reader
            .i64(5)
            .map_err(|err| export_backend_error("output_gaps.detected_at", err))?,
    })
}

fn workflow_from_backend_row(row: &[SqlCell]) -> Result<WorkflowRecord> {
    let reader = CellRowReader::new(row);
    Ok(WorkflowRecord {
        id: reader
            .string(0)
            .map_err(|err| export_backend_error("workflow_executions.id", err))?,
        workflow_name: reader
            .string(1)
            .map_err(|err| export_backend_error("workflow_executions.workflow_name", err))?,
        pane_id: export_i64_to_u64(
            reader
                .i64(2)
                .map_err(|err| export_backend_error("workflow_executions.pane_id", err))?,
            "workflow_executions.pane_id",
        )?,
        trigger_event_id: reader
            .optional_i64(3)
            .map_err(|err| export_backend_error("workflow_executions.trigger_event_id", err))?,
        current_step: export_i64_to_usize(
            reader
                .i64(4)
                .map_err(|err| export_backend_error("workflow_executions.current_step", err))?,
            "workflow_executions.current_step",
        )?,
        status: reader
            .string(5)
            .map_err(|err| export_backend_error("workflow_executions.status", err))?,
        wait_condition: optional_json_value(&reader, 6, "workflow_executions.wait_condition")?,
        context: optional_json_value(&reader, 7, "workflow_executions.context")?,
        result: optional_json_value(&reader, 8, "workflow_executions.result")?,
        error: reader
            .optional_string(9)
            .map_err(|err| export_backend_error("workflow_executions.error", err))?,
        started_at: reader
            .i64(10)
            .map_err(|err| export_backend_error("workflow_executions.started_at", err))?,
        updated_at: reader
            .i64(11)
            .map_err(|err| export_backend_error("workflow_executions.updated_at", err))?,
        completed_at: reader
            .optional_i64(12)
            .map_err(|err| export_backend_error("workflow_executions.completed_at", err))?,
    })
}

fn agent_session_from_backend_row(row: &[SqlCell]) -> Result<AgentSessionRecord> {
    let reader = CellRowReader::new(row);
    Ok(AgentSessionRecord {
        id: reader
            .i64(0)
            .map_err(|err| export_backend_error("agent_sessions.id", err))?,
        pane_id: export_i64_to_u64(
            reader
                .i64(1)
                .map_err(|err| export_backend_error("agent_sessions.pane_id", err))?,
            "agent_sessions.pane_id",
        )?,
        agent_type: reader
            .string(2)
            .map_err(|err| export_backend_error("agent_sessions.agent_type", err))?,
        session_id: reader
            .optional_string(3)
            .map_err(|err| export_backend_error("agent_sessions.session_id", err))?,
        external_id: reader
            .optional_string(4)
            .map_err(|err| export_backend_error("agent_sessions.external_id", err))?,
        external_meta: optional_json_value(&reader, 5, "agent_sessions.external_meta")?,
        started_at: reader
            .i64(6)
            .map_err(|err| export_backend_error("agent_sessions.started_at", err))?,
        ended_at: reader
            .optional_i64(7)
            .map_err(|err| export_backend_error("agent_sessions.ended_at", err))?,
        end_reason: reader
            .optional_string(8)
            .map_err(|err| export_backend_error("agent_sessions.end_reason", err))?,
        total_tokens: reader
            .optional_i64(9)
            .map_err(|err| export_backend_error("agent_sessions.total_tokens", err))?,
        input_tokens: reader
            .optional_i64(10)
            .map_err(|err| export_backend_error("agent_sessions.input_tokens", err))?,
        output_tokens: reader
            .optional_i64(11)
            .map_err(|err| export_backend_error("agent_sessions.output_tokens", err))?,
        cached_tokens: reader
            .optional_i64(12)
            .map_err(|err| export_backend_error("agent_sessions.cached_tokens", err))?,
        reasoning_tokens: reader
            .optional_i64(13)
            .map_err(|err| export_backend_error("agent_sessions.reasoning_tokens", err))?,
        model_name: reader
            .optional_string(14)
            .map_err(|err| export_backend_error("agent_sessions.model_name", err))?,
        estimated_cost_usd: optional_f64(&reader, 15, "agent_sessions.estimated_cost_usd")?,
    })
}

fn pane_reservation_from_backend_row(row: &[SqlCell]) -> Result<PaneReservation> {
    let reader = CellRowReader::new(row);
    Ok(PaneReservation {
        id: reader
            .i64(0)
            .map_err(|err| export_backend_error("pane_reservations.id", err))?,
        pane_id: export_i64_to_u64(
            reader
                .i64(1)
                .map_err(|err| export_backend_error("pane_reservations.pane_id", err))?,
            "pane_reservations.pane_id",
        )?,
        owner_kind: reader
            .string(2)
            .map_err(|err| export_backend_error("pane_reservations.owner_kind", err))?,
        owner_id: reader
            .string(3)
            .map_err(|err| export_backend_error("pane_reservations.owner_id", err))?,
        reason: reader
            .optional_string(4)
            .map_err(|err| export_backend_error("pane_reservations.reason", err))?,
        created_at: reader
            .i64(5)
            .map_err(|err| export_backend_error("pane_reservations.created_at", err))?,
        expires_at: reader
            .i64(6)
            .map_err(|err| export_backend_error("pane_reservations.expires_at", err))?,
        released_at: reader
            .optional_i64(7)
            .map_err(|err| export_backend_error("pane_reservations.released_at", err))?,
        status: reader
            .string(8)
            .map_err(|err| export_backend_error("pane_reservations.status", err))?,
    })
}

pub(super) fn query_export_segments(
    backend: &dyn StorageBackend,
    query: &ExportQuery,
) -> Result<Vec<Segment>> {
    let (where_clause, params) = build_export_where(query, "captured_at")?;
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
         FROM output_segments{where_clause}
         ORDER BY captured_at ASC, id ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(export_limit_value(limit)?);
    query_map_export(
        backend,
        &sql,
        &all_params,
        "Query export segments",
        segment_from_backend_row,
    )
}

pub(super) fn query_export_gaps(
    backend: &dyn StorageBackend,
    query: &ExportQuery,
) -> Result<Vec<Gap>> {
    let (where_clause, params) = build_export_where(query, "detected_at")?;
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, seq_before, seq_after, reason, detected_at
         FROM output_gaps{where_clause}
         ORDER BY detected_at ASC, id ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(export_limit_value(limit)?);
    query_map_export(
        backend,
        &sql,
        &all_params,
        "Query export gaps",
        gap_from_backend_row,
    )
}

pub(super) fn query_export_workflows(
    backend: &dyn StorageBackend,
    query: &ExportQuery,
) -> Result<Vec<WorkflowRecord>> {
    let (where_clause, params) = build_export_where(query, "started_at")?;
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, workflow_name, pane_id, trigger_event_id, current_step,
                status, wait_condition, context, result, error, started_at, updated_at, completed_at
         FROM workflow_executions{where_clause}
         ORDER BY started_at ASC, id ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(export_limit_value(limit)?);
    query_map_export(
        backend,
        &sql,
        &all_params,
        "Query export workflows",
        workflow_from_backend_row,
    )
}

pub(super) fn query_export_sessions(
    backend: &dyn StorageBackend,
    query: &ExportQuery,
) -> Result<Vec<AgentSessionRecord>> {
    let (where_clause, params) = build_export_where(query, "started_at")?;
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, agent_type, session_id, external_id, external_meta,
                started_at, ended_at, end_reason,
                total_tokens, input_tokens, output_tokens, cached_tokens, reasoning_tokens,
                model_name, estimated_cost_usd
         FROM agent_sessions{where_clause}
         ORDER BY started_at ASC, id ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(export_limit_value(limit)?);
    query_map_export(
        backend,
        &sql,
        &all_params,
        "Query export sessions",
        agent_session_from_backend_row,
    )
}

pub(super) fn query_export_reservations(
    backend: &dyn StorageBackend,
    query: &ExportQuery,
) -> Result<Vec<PaneReservation>> {
    let (where_clause, params) = build_export_where(query, "created_at")?;
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, owner_kind, owner_id, reason,
                created_at, expires_at, released_at, status
         FROM pane_reservations{where_clause}
         ORDER BY created_at ASC, id ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(export_limit_value(limit)?);
    query_map_export(
        backend,
        &sql,
        &all_params,
        "Query export reservations",
        pane_reservation_from_backend_row,
    )
}

#[cfg(test)]
mod export_json_parse_drop_tests {
    use super::*;
    use crate::storage_backend_trait::{MockBackend, OpenConfig, RusqliteBackend};

    fn parse_drop_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .expect("export JSON parse-drop test lock should not be poisoned")
    }

    #[test]
    fn well_formed_does_not_bump_ft_l3u5k() {
        let _guard = parse_drop_test_lock();
        reset_storage_export_json_parse_drop_count_for_test();
        let v = parse_export_json_column(r#"{"k":"v"}"#, "test", 0);
        assert_eq!(v.unwrap(), serde_json::json!({"k": "v"}));
        assert_eq!(crate::storage::storage_export_json_parse_drop_count(), 0);
    }

    #[test]
    fn malformed_bumps_counter_ft_l3u5k() {
        let _guard = parse_drop_test_lock();
        // br-ft-l3u5k: a malformed JSON payload bumps the counter
        // exactly once.
        reset_storage_export_json_parse_drop_count_for_test();
        let v = parse_export_json_column("{not json", "test", 5);
        assert!(v.is_err());
        assert_eq!(crate::storage::storage_export_json_parse_drop_count(), 1);

        // Second malformed call bumps again — every event observable.
        let _ = parse_export_json_column("[unclosed", "another", 7);
        assert_eq!(crate::storage::storage_export_json_parse_drop_count(), 2);
    }

    #[test]
    fn null_input_path_does_not_bump_ft_l3u5k() {
        let _guard = parse_drop_test_lock();
        // The NULL path (optional_string returning None) doesn't
        // even reach this helper — the early return at
        // optional_json_value handles it. Pin the contract via the
        // helper directly: an empty-but-valid JSON shouldn't bump.
        reset_storage_export_json_parse_drop_count_for_test();
        let v = parse_export_json_column("null", "test", 0);
        assert!(v.is_ok());
        assert_eq!(v.unwrap(), serde_json::Value::Null);
        assert_eq!(crate::storage::storage_export_json_parse_drop_count(), 0);
    }

    // Real SQL execution exercises the parameter binding and the typed return
    // path together. The schemas deliberately permit malformed storage classes
    // so these tests can also represent old or damaged databases.
    fn fresh_db() -> RusqliteBackend {
        let backend = RusqliteBackend::open(
            ":memory:",
            &OpenConfig {
                wal_mode: false,
                ..OpenConfig::default()
            },
        )
        .expect("open export test database");
        backend
            .execute_batch(
                "CREATE TABLE output_segments (
                    id INTEGER PRIMARY KEY, pane_id INTEGER, seq INTEGER,
                    content TEXT, content_len INTEGER, content_hash TEXT, captured_at INTEGER
                );
                CREATE TABLE output_gaps (
                    id INTEGER PRIMARY KEY, pane_id INTEGER, seq_before INTEGER,
                    seq_after INTEGER, reason TEXT, detected_at INTEGER
                );
                CREATE TABLE workflow_executions (
                    id TEXT PRIMARY KEY, workflow_name TEXT, pane_id INTEGER,
                    trigger_event_id INTEGER, current_step INTEGER, status TEXT,
                    wait_condition TEXT, context TEXT, result TEXT, error TEXT,
                    started_at INTEGER, updated_at INTEGER, completed_at INTEGER
                );
                CREATE TABLE agent_sessions (
                    id INTEGER PRIMARY KEY, pane_id INTEGER, agent_type TEXT,
                    session_id TEXT, external_id TEXT, external_meta TEXT,
                    started_at INTEGER, ended_at INTEGER, end_reason TEXT,
                    total_tokens INTEGER, input_tokens INTEGER, output_tokens INTEGER,
                    cached_tokens INTEGER, reasoning_tokens INTEGER, model_name TEXT,
                    estimated_cost_usd REAL
                );
                CREATE TABLE pane_reservations (
                    id INTEGER PRIMARY KEY, pane_id INTEGER, owner_kind TEXT,
                    owner_id TEXT, reason TEXT, created_at INTEGER, expires_at INTEGER,
                    released_at INTEGER, status TEXT
                );
                INSERT INTO output_segments VALUES
                    (1, 7, 0, '', 0, '', 100),
                    (2, 7, 1, '42', 2, NULL, 100);
                INSERT INTO output_gaps VALUES
                    (1, 7, 0, 2, '', 100),
                    (2, 7, 2, 4, '<blob:4 bytes>', 100);
                INSERT INTO workflow_executions VALUES
                    ('w-1', '42', 7, NULL, 0, 'done', 'null', '{}', '[]', '', 100, 101, NULL),
                    ('w-2', '1.5', 7, 1, 2, 'done', NULL, NULL, NULL, NULL, 100, 101, 102);
                INSERT INTO agent_sessions VALUES
                    (1, 7, 'claude', '', '42', 'null', 100, NULL, '', 0, NULL, 0, NULL, NULL, '', 0.125),
                    (2, 7, 'codex', NULL, NULL, NULL, 100, 200, NULL, NULL, 1, 2, 3, 4, '1.5', NULL);
                INSERT INTO pane_reservations VALUES
                    (1, 7, 'agent', '', '', 100, 200, NULL, 'active'),
                    (2, 7, 'agent', '42', NULL, 100, 200, 150, 'released');",
            )
            .expect("create and populate export tables");
        backend
    }

    #[test]
    fn export_preserves_empty_text_null_and_numeric_looking_text() {
        let backend = fresh_db();
        let query = ExportQuery::default();
        let segments = query_export_segments(&backend, &query).unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].content, "");
        assert_eq!(segments[0].content_hash.as_deref(), Some(""));
        assert_eq!(segments[1].content, "42");
        assert_eq!(segments[1].content_hash, None);

        let gaps = query_export_gaps(&backend, &query).unwrap();
        assert_eq!(gaps[0].reason, "");
        assert_eq!(gaps[1].reason, "<blob:4 bytes>");

        let workflows = query_export_workflows(&backend, &query).unwrap();
        assert_eq!(workflows[0].workflow_name, "42");
        assert_eq!(workflows[0].wait_condition, Some(serde_json::Value::Null));
        assert_eq!(workflows[0].context, Some(serde_json::json!({})));
        assert_eq!(workflows[0].result, Some(serde_json::json!([])));
        assert_eq!(workflows[0].error.as_deref(), Some(""));
        assert_eq!(workflows[1].workflow_name, "1.5");
        assert_eq!(workflows[1].wait_condition, None);
        assert_eq!(workflows[1].context, None);
        assert_eq!(workflows[1].result, None);
        assert_eq!(workflows[1].error, None);

        let sessions = query_export_sessions(&backend, &query).unwrap();
        assert_eq!(sessions[0].session_id.as_deref(), Some(""));
        assert_eq!(sessions[0].external_id.as_deref(), Some("42"));
        assert_eq!(sessions[0].external_meta, Some(serde_json::Value::Null));
        assert_eq!(sessions[0].end_reason.as_deref(), Some(""));
        assert_eq!(sessions[0].model_name.as_deref(), Some(""));
        assert_eq!(sessions[0].estimated_cost_usd, Some(0.125));
        assert_eq!(sessions[1].session_id, None);
        assert_eq!(sessions[1].external_meta, None);
        assert_eq!(sessions[1].estimated_cost_usd, None);
        assert_eq!(sessions[1].model_name.as_deref(), Some("1.5"));

        let reservations = query_export_reservations(&backend, &query).unwrap();
        assert_eq!(reservations[0].owner_id, "");
        assert_eq!(reservations[0].reason.as_deref(), Some(""));
        assert_eq!(reservations[1].owner_id, "42");
        assert_eq!(reservations[1].reason, None);
    }

    #[test]
    fn corrupt_workflow_json_rejects_export_instead_of_returning_a_prefix() {
        let _guard = parse_drop_test_lock();
        for column in ["wait_condition", "context", "result"] {
            for raw in ["", "   ", "{not json", "{\"EXPORT_PRIVATE_SENTINEL\":"] {
                let backend = fresh_db();
                // w-1 is valid and precedes the corrupt w-2. The query must
                // return an error, not that prefix or a row with missing JSON.
                backend
                    .execute_batch(&format!(
                        "UPDATE workflow_executions SET {column} = '{raw}' WHERE id = 'w-2'"
                    ))
                    .unwrap();
                let error = query_export_workflows(&backend, &ExportQuery::default())
                    .expect_err("malformed present workflow JSON must fail export")
                    .to_string();
                assert!(error.contains(&format!("workflow_executions.{column}")));
                assert!(!error.contains("EXPORT_PRIVATE_SENTINEL"));
            }
        }
    }

    #[test]
    fn corrupt_session_metadata_rejects_export() {
        let _guard = parse_drop_test_lock();
        for raw in ["", "[unclosed", "null trailing"] {
            let backend = fresh_db();
            backend
                .execute_batch(&format!(
                    "UPDATE agent_sessions SET external_meta = '{raw}' WHERE id = 2"
                ))
                .unwrap();
            let error = query_export_sessions(&backend, &ExportQuery::default())
                .expect_err("malformed session metadata must fail export")
                .to_string();
            assert!(error.contains("agent_sessions.external_meta"));
        }
    }

    #[test]
    fn wrong_storage_classes_are_not_fabricated_as_text() {
        let backend = fresh_db();
        backend
            .execute_batch(
                "UPDATE output_segments SET content = X'00ff' WHERE id = 2;
                UPDATE output_gaps SET reason = X'00ff' WHERE id = 2;
                UPDATE workflow_executions SET workflow_name = X'00ff' WHERE id = 'w-2';
                UPDATE agent_sessions SET session_id = X'00ff' WHERE id = 2;
                UPDATE pane_reservations SET reason = X'00ff' WHERE id = 2;",
            )
            .unwrap();
        let query = ExportQuery::default();
        assert!(query_export_segments(&backend, &query).is_err());
        assert!(query_export_gaps(&backend, &query).is_err());
        assert!(query_export_workflows(&backend, &query).is_err());
        assert!(query_export_sessions(&backend, &query).is_err());
        assert!(query_export_reservations(&backend, &query).is_err());
    }

    #[test]
    fn required_null_text_and_negative_sequence_are_rejected() {
        let backend = fresh_db();
        backend
            .execute_batch("UPDATE output_segments SET content = NULL WHERE id = 2")
            .unwrap();
        assert!(query_export_segments(&backend, &ExportQuery::default()).is_err());
        backend
            .execute_batch("UPDATE output_segments SET content = '', seq = -1 WHERE id = 2")
            .unwrap();
        assert!(query_export_segments(&backend, &ExportQuery::default()).is_err());
        // An optional INTEGER column containing empty TEXT is corruption, not NULL.
        backend
            .execute_batch("UPDATE agent_sessions SET total_tokens = '' WHERE id = 2")
            .unwrap();
        assert!(query_export_sessions(&backend, &ExportQuery::default()).is_err());
    }

    #[test]
    fn non_finite_and_wrong_type_costs_are_rejected() {
        for value in ["1e999", "-1e999", "'NaN'", "''", "X'00ff'"] {
            let backend = fresh_db();
            backend
                .execute_batch(&format!(
                    "UPDATE agent_sessions SET estimated_cost_usd = {value} WHERE id = 2"
                ))
                .unwrap();
            assert!(query_export_sessions(&backend, &ExportQuery::default()).is_err());
        }
        // SQLite normalizes NaN to NULL at insertion; a backend that exposes a
        // real NaN must still be rejected before JSON can normalize it to null.
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let row = [SqlCell::Real(value)];
            assert!(optional_f64(&CellRowReader::new(&row), 0, "cost").is_err());
        }
        let row = [SqlCell::Null, SqlCell::Integer(2), SqlCell::Real(0.125)];
        let reader = CellRowReader::new(&row);
        assert_eq!(optional_f64(&reader, 0, "cost").unwrap(), None);
        assert_eq!(optional_f64(&reader, 1, "cost").unwrap(), Some(2.0));
        assert_eq!(optional_f64(&reader, 2, "cost").unwrap(), Some(0.125));
        assert!(optional_f64(&reader, 3, "cost").is_err());
    }

    #[test]
    fn limited_exports_break_timestamp_ties_by_id_on_every_table() {
        let backend = fresh_db();
        // Each covering order index offers the opposite tie order. Timestamp-
        // only sorting used to let the chosen index determine which row won.
        backend
            .execute_batch(
                "CREATE INDEX segments_export_order ON output_segments(captured_at ASC, id DESC);
                CREATE INDEX gaps_export_order ON output_gaps(detected_at ASC, id DESC);
                CREATE INDEX workflows_export_order ON workflow_executions(started_at ASC, id DESC);
                CREATE INDEX sessions_export_order ON agent_sessions(started_at ASC, id DESC);
                CREATE INDEX reservations_export_order ON pane_reservations(created_at ASC, id DESC);",
            )
            .unwrap();
        let query = ExportQuery {
            since: Some(100),
            until: Some(100),
            limit: Some(1),
            ..ExportQuery::default()
        };
        assert_eq!(query_export_segments(&backend, &query).unwrap()[0].id, 1);
        assert_eq!(query_export_gaps(&backend, &query).unwrap()[0].id, 1);
        assert_eq!(
            query_export_workflows(&backend, &query).unwrap()[0].id,
            "w-1"
        );
        assert_eq!(query_export_sessions(&backend, &query).unwrap()[0].id, 1);
        assert_eq!(
            query_export_reservations(&backend, &query).unwrap()[0].id,
            1
        );
    }

    #[test]
    fn export_filters_keep_inclusive_time_bounds_and_zero_limit() {
        let backend = fresh_db();
        let mut query = ExportQuery {
            pane_id: Some(7),
            since: Some(100),
            until: Some(100),
            ..ExportQuery::default()
        };
        assert_eq!(query_export_segments(&backend, &query).unwrap().len(), 2);
        query.pane_id = Some(8);
        assert!(query_export_segments(&backend, &query).unwrap().is_empty());
        query.pane_id = Some(7);
        query.since = Some(101);
        assert!(query_export_segments(&backend, &query).unwrap().is_empty());
        query.since = Some(100);
        query.limit = Some(0);
        assert!(query_export_segments(&backend, &query).unwrap().is_empty());
        assert!(query_export_gaps(&backend, &query).unwrap().is_empty());
        assert!(query_export_workflows(&backend, &query).unwrap().is_empty());
        assert!(query_export_sessions(&backend, &query).unwrap().is_empty());
        assert!(
            query_export_reservations(&backend, &query)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn out_of_range_pane_filter_never_wraps_to_a_negative_pane() {
        let backend = fresh_db();
        backend
            .execute_batch("UPDATE output_segments SET pane_id = -1 WHERE id = 2")
            .unwrap();
        let query = ExportQuery {
            pane_id: Some(u64::MAX),
            ..ExportQuery::default()
        };
        assert!(query_export_segments(&backend, &query).is_err());
        assert!(query_export_gaps(&backend, &query).is_err());
        assert!(query_export_workflows(&backend, &query).is_err());
        assert!(query_export_sessions(&backend, &query).is_err());
        assert!(query_export_reservations(&backend, &query).is_err());
        let maximum = u64::try_from(i64::MAX).unwrap();
        let query = ExportQuery {
            pane_id: Some(maximum),
            ..ExportQuery::default()
        };
        assert!(build_export_where(&query, "captured_at").is_ok());
        assert!(export_limit_value(0).is_ok());
        if let Ok(too_large) = usize::try_from(maximum + 1) {
            assert!(export_limit_value(too_large).is_err());
        }
    }

    #[test]
    fn export_refuses_a_lossy_backend_instead_of_guessing_cell_types() {
        let backend = MockBackend::new();
        assert!(!backend.supports_lossless_cells());
        let error = query_export_segments(&backend, &ExportQuery::default())
            .expect_err("the legacy string-cell fallback cannot produce faithful exports")
            .to_string();
        assert!(error.contains("lossless SQL cells"));
    }

    #[test]
    fn truncated_rows_are_rejected_by_every_decoder() {
        assert!(segment_from_backend_row(&[]).is_err());
        assert!(gap_from_backend_row(&[]).is_err());
        assert!(workflow_from_backend_row(&[]).is_err());
        assert!(agent_session_from_backend_row(&[]).is_err());
        assert!(pane_reservation_from_backend_row(&[]).is_err());
    }
}
