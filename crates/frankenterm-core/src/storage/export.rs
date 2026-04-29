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

use rusqlite::Connection;

use super::{
    AgentSessionRecord, ExportQuery, Gap, PaneReservation, Segment, StorageError, WorkflowRecord,
    i64_to_usize, u64_to_i64_unchecked,
};
use crate::error::Result;


/// Build a dynamic WHERE clause and params from an ExportQuery.
/// `time_column` is the column name used for since/until filtering.
pub(super) fn build_export_where(
    query: &ExportQuery,
    time_column: &str,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(pane_id) = query.pane_id {
        clauses.push(format!("pane_id = ?{}", params.len() + 1));
        params.push(Box::new(u64_to_i64_unchecked(pane_id)));
    }
    if let Some(since) = query.since {
        clauses.push(format!("{time_column} >= ?{}", params.len() + 1));
        params.push(Box::new(since));
    }
    if let Some(until) = query.until {
        clauses.push(format!("{time_column} <= ?{}", params.len() + 1));
        params.push(Box::new(until));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    (where_clause, params)
}

pub(super) fn query_export_segments(conn: &Connection, query: &ExportQuery) -> Result<Vec<Segment>> {
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
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(Segment {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                content: row.get(3)?,
                content_len: {
                    let val: i64 = row.get(4)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as usize
                    }
                },
                content_hash: row.get(5)?,
                captured_at: row.get(6)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

pub(super) fn query_export_gaps(conn: &Connection, query: &ExportQuery) -> Result<Vec<Gap>> {
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
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(Gap {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq_before: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq_after: {
                    let val: i64 = row.get(3)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                reason: row.get(4)?,
                detected_at: row.get(5)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

pub(super) fn query_export_workflows(conn: &Connection, query: &ExportQuery) -> Result<Vec<WorkflowRecord>> {
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
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let wait_condition: Option<String> = row.get(6)?;
            let wait_condition = wait_condition.and_then(|s| serde_json::from_str(&s).ok());
            let context: Option<String> = row.get(7)?;
            let context = context.and_then(|s| serde_json::from_str(&s).ok());
            let result: Option<String> = row.get(8)?;
            let result = result.and_then(|s| serde_json::from_str(&s).ok());

            Ok(WorkflowRecord {
                id: row.get(0)?,
                workflow_name: row.get(1)?,
                pane_id: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                trigger_event_id: row.get(3)?,
                current_step: {
                    let val: i64 = row.get(4)?;
                    i64_to_usize(val)?
                },
                status: row.get(5)?,
                wait_condition,
                context,
                result,
                error: row.get(9)?,
                started_at: row.get(10)?,
                updated_at: row.get(11)?,
                completed_at: row.get(12)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

pub(super) fn query_export_sessions(
    conn: &Connection,
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
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(AgentSessionRecord {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                agent_type: row.get(2)?,
                session_id: row.get(3)?,
                external_id: row.get(4)?,
                external_meta: row
                    .get::<_, Option<String>>(5)?
                    .as_ref()
                    .and_then(|value| serde_json::from_str(value).ok()),
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                end_reason: row.get(8)?,
                total_tokens: row.get(9)?,
                input_tokens: row.get(10)?,
                output_tokens: row.get(11)?,
                cached_tokens: row.get(12)?,
                reasoning_tokens: row.get(13)?,
                model_name: row.get(14)?,
                estimated_cost_usd: row.get(15)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

pub(super) fn query_export_reservations(
    conn: &Connection,
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
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(PaneReservation {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                owner_kind: row.get(2)?,
                owner_id: row.get(3)?,
                reason: row.get(4)?,
                created_at: row.get(5)?,
                expires_at: row.get(6)?,
                released_at: row.get(7)?,
                status: row.get(8)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}
