//! [ft-nsb8c / ft-dn2tu Phase 5] Export Query Functions
//!
//! Read-only query helpers that export segments / gaps / workflows /
//! sessions / reservations to callers like `wa export ...` and the
//! audit/forensics surface. Lifted out of `storage.rs` (~340 LOC) so
//! the storage facade keeps its writer-thread + handle responsibilities
//! and the read-side query surface lives next to its siblings under
//! `storage/`. No behavior change — call sites just gain the
//! `export::` qualifier.
//!
//! Visibility: each function is `pub(super)` so `StorageHandle::export_*`
//! methods in `storage.rs` can keep calling them directly via
//! `export::query_export_*`. The shared `u64_to_i64_unchecked`
//! helper stays in `storage.rs` because two non-export builders
//! (audit and stream-page where-clauses) also use it.

use super::{
    AgentSessionRecord, ExportQuery, Gap, PaneReservation, Segment, StorageError, WorkflowRecord,
    u64_to_i64_unchecked,
};
use crate::error::Result;
use crate::storage_backend_row_helpers::RowReader;
use crate::storage_backend_trait::{BackendError, StorageBackend, ToSqlValue};

/// Build a dynamic WHERE clause and params from an ExportQuery.
/// `time_column` is the column name used for since/until filtering.
pub(super) fn build_export_where(
    query: &ExportQuery,
    time_column: &str,
) -> (String, Vec<ToSqlValue<'static>>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<ToSqlValue<'static>> = Vec::new();

    if let Some(pane_id) = query.pane_id {
        clauses.push(format!("pane_id = ?{}", params.len() + 1));
        params.push(ToSqlValue::Integer(u64_to_i64_unchecked(pane_id)));
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

    (where_clause, params)
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
#[must_use]
pub fn storage_export_json_parse_drop_count() -> u64 {
    STORAGE_EXPORT_JSON_PARSE_DROP_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// br-ft-l3u5k: test helper to reset the parse-drop counter.
#[cfg(test)]
pub fn reset_storage_export_json_parse_drop_count_for_test() {
    STORAGE_EXPORT_JSON_PARSE_DROP_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// br-ft-l3u5k: parse a JSON column value and bump the parse-drop
/// counter on failure. Extracted so unit tests can exercise the
/// counter contract without constructing a `RowReader` over a
/// real storage backend. Returns `Some(Value)` on Ok,
/// `None` on Err (the calling helper turns that into `Ok(None)`).
fn parse_export_json_column(raw: &str, label: &str, column: usize) -> Option<serde_json::Value> {
    match serde_json::from_str(raw) {
        Ok(value) => Some(value),
        Err(err) => {
            STORAGE_EXPORT_JSON_PARSE_DROP_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                target: "ft.storage.export",
                event = "storage_export_json_parse_drop",
                label = label,
                column = column,
                raw_len = raw.len(),
                error = %err,
                "export JSON column failed to parse; consumer will see None (br-ft-l3u5k)"
            );
            None
        }
    }
}

fn optional_json_value(
    reader: &RowReader<'_>,
    column: usize,
    label: &str,
) -> Result<Option<serde_json::Value>> {
    let Some(raw) = reader
        .optional_string(column)
        .map_err(|err| export_backend_error(label, err))?
    else {
        return Ok(None);
    };
    Ok(parse_export_json_column(&raw, label, column))
}

#[cfg(test)]
mod export_json_parse_drop_tests {
    use super::*;

    #[test]
    fn well_formed_does_not_bump_ft_l3u5k() {
        reset_storage_export_json_parse_drop_count_for_test();
        let v = parse_export_json_column(r#"{"k":"v"}"#, "test", 0);
        assert!(v.is_some());
        assert_eq!(storage_export_json_parse_drop_count(), 0);
    }

    #[test]
    fn malformed_bumps_counter_ft_l3u5k() {
        // br-ft-l3u5k: a malformed JSON payload bumps the counter
        // exactly once.
        reset_storage_export_json_parse_drop_count_for_test();
        let v = parse_export_json_column("{not json", "test", 5);
        assert!(v.is_none());
        assert_eq!(storage_export_json_parse_drop_count(), 1);

        // Second malformed call bumps again — every event observable.
        let _ = parse_export_json_column("[unclosed", "another", 7);
        assert_eq!(storage_export_json_parse_drop_count(), 2);
    }

    #[test]
    fn null_input_path_does_not_bump_ft_l3u5k() {
        // The NULL path (optional_string returning None) doesn't
        // even reach this helper — the early return at
        // optional_json_value handles it. Pin the contract via the
        // helper directly: an empty-but-valid JSON shouldn't bump.
        reset_storage_export_json_parse_drop_count_for_test();
        let v = parse_export_json_column("null", "test", 0);
        assert!(v.is_some());
        assert_eq!(v.unwrap(), serde_json::Value::Null);
        assert_eq!(storage_export_json_parse_drop_count(), 0);
    }
}

fn optional_f64(reader: &RowReader<'_>, column: usize, label: &str) -> Result<Option<f64>> {
    let Some(raw) = reader
        .optional_string(column)
        .map_err(|err| export_backend_error(label, err))?
    else {
        return Ok(None);
    };
    raw.parse::<f64>().map(Some).map_err(|err| {
        StorageError::Database(format!("{label}: f64 parse failed for `{raw}`: {err}")).into()
    })
}

fn query_map_export<T>(
    backend: &dyn StorageBackend,
    sql: &str,
    params: &[ToSqlValue<'_>],
    query_context: &str,
    decode: fn(&[String]) -> Result<T>,
) -> Result<Vec<T>> {
    backend
        .query_map_typed(sql, params)
        .map_err(|err| export_backend_error(query_context, err))?
        .iter()
        .map(|row| decode(row))
        .collect()
}

fn segment_from_backend_row(row: &[String]) -> Result<Segment> {
    let reader = RowReader::new(row);
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

fn gap_from_backend_row(row: &[String]) -> Result<Gap> {
    let reader = RowReader::new(row);
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

fn workflow_from_backend_row(row: &[String]) -> Result<WorkflowRecord> {
    let reader = RowReader::new(row);
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

fn agent_session_from_backend_row(row: &[String]) -> Result<AgentSessionRecord> {
    let reader = RowReader::new(row);
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

fn pane_reservation_from_backend_row(row: &[String]) -> Result<PaneReservation> {
    let reader = RowReader::new(row);
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
    let (where_clause, params) = build_export_where(query, "captured_at");
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
         FROM output_segments{where_clause}
         ORDER BY captured_at ASC
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
    let (where_clause, params) = build_export_where(query, "detected_at");
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, seq_before, seq_after, reason, detected_at
         FROM output_gaps{where_clause}
         ORDER BY detected_at ASC
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
    let (where_clause, params) = build_export_where(query, "started_at");
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, workflow_name, pane_id, trigger_event_id, current_step,
                status, wait_condition, context, result, error, started_at, updated_at, completed_at
         FROM workflow_executions{where_clause}
         ORDER BY started_at ASC
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
    let (where_clause, params) = build_export_where(query, "started_at");
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, agent_type, session_id, external_id, external_meta,
                started_at, ended_at, end_reason,
                total_tokens, input_tokens, output_tokens, cached_tokens, reasoning_tokens,
                model_name, estimated_cost_usd
         FROM agent_sessions{where_clause}
         ORDER BY started_at ASC
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
    let (where_clause, params) = build_export_where(query, "created_at");
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, owner_kind, owner_id, reason,
                created_at, expires_at, released_at, status
         FROM pane_reservations{where_clause}
         ORDER BY created_at ASC
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
