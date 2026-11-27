//! Search results with capture provenance from the same SQL read as their text.
//!
//! Ranking may precede hydration, as in the existing lexical search. The epoch
//! is read with the returned segment and snippet, never by a later query that
//! could label an older row with a replacement database's identity.

use std::collections::HashMap;
use std::sync::Arc;

use super::types::CaptureSearchResult;
use super::{
    SearchOptions, SearchResult, StorageHandle, append_search_filter_backend_params,
    ensure_fts_search_ready_backend, search_fts_rank_stage_backend,
    search_result_from_backend_cells, segment_from_backend_cells, storage_backend_error,
    usize_to_i64, validate_fts_query_backend,
};
use crate::error::{Result, StorageError};
use crate::storage_backend_row_helpers::CellRowReader;
use crate::storage_backend_trait::{SqlCell, StorageBackend, ToSqlValue};

/// The most results one capture search returns. A larger limit is an error
/// here, so callers that take a limit from a request clamp it to this.
pub const CAPTURE_SEARCH_MAX_RESULTS: usize = 1_000;
const CAPTURE_AUTHORITY_COLUMNS: &str =
    "(SELECT cursor_epoch FROM segment_stream_state WHERE singleton = 1),
     (SELECT max_segment_id FROM segment_stream_state WHERE singleton = 1)";

/// Exact address of a detection's retained output. Decimal strings preserve
/// all SQLite integer bits in browser clients.
#[cfg(feature = "web")]
#[derive(serde::Serialize)]
pub(crate) struct DetectionCapture {
    event_id: String,
    event_epoch: String,
    pane_id: String,
    pane_uuid: String,
    segment_id: String,
    captured_at: String,
    content_len: String,
    capture_cursor: String,
}

#[cfg(feature = "web")]
impl StorageHandle {
    pub(crate) async fn detection_capture_with_cx(
        &self,
        cx: &crate::cx::Cx,
        epoch: String,
        event_id: i64,
        pane_id: u64,
        pane_uuid: String,
    ) -> Result<Option<DetectionCapture>> {
        Self::checkpoint_storage_operation(cx, "detection_capture")?;
        let db_path = Arc::clone(&self.db_path);
        let provider = Arc::clone(&self.backend_provider);
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            super::with_provider_fresh_read_backend(
                provider.as_ref(),
                db_path.as_str(),
                |backend| detection_capture_backend(backend, &epoch, event_id, pane_id, &pane_uuid),
            )
        })
        .await
    }
}

#[cfg(feature = "web")]
fn detection_capture_backend(
    backend: &dyn StorageBackend,
    epoch: &str,
    event_id: i64,
    pane_id: u64,
    pane_uuid: &str,
) -> Result<Option<DetectionCapture>> {
    let Ok(sql_pane) = i64::try_from(pane_id) else {
        return Ok(None);
    };
    // Bind both independent epochs, the event and its source in a single
    // read. The current panes table may already describe another terminal.
    let row = backend
        .query_row_cells(
            "SELECT s.id, s.captured_at, s.content_len, stream.cursor_epoch
         FROM events e
         JOIN output_segments s ON s.id = e.segment_id AND s.pane_id = e.pane_id
         JOIN output_segment_captures c ON c.segment_id = s.id
         JOIN pane_capture_bindings b ON b.capture_uuid = c.capture_uuid AND b.pane_id = s.pane_id
         JOIN event_retention_state ev ON ev.singleton = 1 AND ev.cursor_epoch = ?1
             AND e.id <= ev.max_event_id
         JOIN segment_stream_state stream ON stream.singleton = 1 AND s.id <= stream.max_segment_id
         WHERE e.id = ?2 AND e.pane_id = ?3 AND b.capture_uuid = ?4",
            &[
                ToSqlValue::Text(epoch),
                ToSqlValue::Integer(event_id),
                ToSqlValue::Integer(sql_pane),
                ToSqlValue::Text(pane_uuid),
            ],
        )
        .map_err(|err| storage_backend_error("Resolve detection capture", err))?;
    let Some(row) = row else { return Ok(None) };
    let reader = CellRowReader::new(&row);
    let number = |index| {
        reader
            .i64(index)
            .map_err(|err| storage_backend_error("Read detection capture", err))
    };
    let id = number(0)?;
    let captured_at = number(1)?;
    let content_len = number(2)?;
    let stream_epoch = reader
        .string(3)
        .map_err(|err| storage_backend_error("Read capture epoch", err))?;
    if id <= 0
        || captured_at < 0
        || content_len < 0
        || stream_epoch.len() != 32
        || !stream_epoch
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(StorageError::Database("invalid detection capture metadata".into()).into());
    }
    Ok(Some(DetectionCapture {
        event_id: event_id.to_string(),
        event_epoch: epoch.to_owned(),
        pane_id: pane_id.to_string(),
        pane_uuid: pane_uuid.to_owned(),
        segment_id: id.to_string(),
        captured_at: captured_at.to_string(),
        content_len: content_len.to_string(),
        capture_cursor: format!("delta-v2:{stream_epoch}:pane:{pane_id}:{}", id - 1),
    }))
}

impl StorageHandle {
    /// Search retained output with each hit bound to its database epoch.
    ///
    /// The result limit is at most 1,000. Each epoch is read in the same SQL
    /// statement as the returned segment and snippet. This identifies retained
    /// capture lineage, not immutable original bytes or complete history.
    pub async fn search_captures_with_cx(
        &self,
        cx: &crate::cx::Cx,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<CaptureSearchResult>> {
        Self::checkpoint_storage_operation(cx, "search_captures")?;
        let db_path = Arc::clone(&self.db_path);
        let provider = Arc::clone(&self.backend_provider);
        let query = query.to_string();
        Self::spawn_blocking_storage_with_cx_with_join_error(cx, "Task join error", move || {
            super::with_provider_fresh_read_backend(
                provider.as_ref(),
                db_path.as_str(),
                |backend| search_captures_backend(backend, &query, &options),
            )
        })
        .await
    }
}

fn search_captures_backend(
    backend: &dyn StorageBackend,
    query: &str,
    options: &SearchOptions,
) -> Result<Vec<CaptureSearchResult>> {
    options.validate_pane_scope()?;
    let limit = options.limit.unwrap_or(100);
    if limit > CAPTURE_SEARCH_MAX_RESULTS {
        return Err(StorageError::FtsQueryError(
            "capture search result limit exceeds 1000".to_string(),
        )
        .into());
    }
    ensure_fts_search_ready_backend(backend)?;
    validate_fts_query_backend(backend, query)?;

    if !options.include_snippets.unwrap_or(true) {
        return search_captures_rank_only_backend(backend, query, options, limit);
    }
    let ranked = search_fts_rank_stage_backend(backend, query, options, limit)?;
    if ranked.is_empty() {
        return Ok(Vec::new());
    }
    if ranked.len() > limit
        || ranked
            .iter()
            .any(|(id, score)| *id <= 0 || !score.is_finite())
    {
        return Err(StorageError::Database("invalid capture search rank page".to_string()).into());
    }
    let mut hydrated = hydrate_captures_backend(backend, query, options, &ranked)?;
    let mut results = Vec::with_capacity(ranked.len());
    for (id, score) in ranked {
        if let Some(mut hit) = hydrated.remove(&id) {
            hit.result.score = score;
            results.push(hit);
        }
    }
    if !hydrated.is_empty() {
        return Err(StorageError::Database("unranked capture search row".to_string()).into());
    }
    Ok(results)
}

fn hydrate_captures_backend(
    backend: &dyn StorageBackend,
    query: &str,
    options: &SearchOptions,
    ranked: &[(i64, f64)],
) -> Result<HashMap<i64, CaptureSearchResult>> {
    let placeholders = std::iter::repeat_n("?", ranked.len())
        .collect::<Vec<_>>()
        .join(",");
    let prefix = options.highlight_prefix.as_deref().unwrap_or(">>>");
    let suffix = options.highlight_suffix.as_deref().unwrap_or("<<<");
    let include_highlights = options.include_highlights.unwrap_or(true);
    let highlight_column = if include_highlights {
        "highlight(output_segments_fts, 0, ?, ?) AS highlight"
    } else {
        "NULL AS highlight"
    };
    let mut sql = format!(
        "SELECT s.id, s.pane_id, s.seq, s.content, s.content_len, s.content_hash, s.captured_at,
                snippet(output_segments_fts, 0, ?, ?, '...', ?) AS snippet,
                {highlight_column}, {CAPTURE_AUTHORITY_COLUMNS}
         FROM output_segments s
         JOIN output_segments_fts fts ON s.id = fts.rowid
         WHERE output_segments_fts MATCH ? AND s.id IN ({placeholders})"
    );
    let mut params = vec![
        ToSqlValue::Text(prefix),
        ToSqlValue::Text(suffix),
        ToSqlValue::Integer(usize_to_i64(
            options.snippet_max_tokens.unwrap_or(64),
            "max_tokens",
        )?),
    ];
    if include_highlights {
        params.push(ToSqlValue::Text(prefix));
        params.push(ToSqlValue::Text(suffix));
    }
    params.push(ToSqlValue::Text(query));
    for (id, _) in ranked {
        params.push(ToSqlValue::Integer(*id));
    }
    // Ranking and hydration are distinct reads. Recheck the requested scope
    // against the actual row returned with its epoch, including mutable zones.
    append_search_filter_backend_params(&mut sql, &mut params, options)?;
    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| storage_backend_error("Hydrate capture search rows", err))?;
    let mut results = HashMap::with_capacity(ranked.len());
    for row in rows {
        let reader = CellRowReader::new(&row);
        let result = SearchResult {
            segment: segment_from_backend_cells(&row)?,
            snippet: reader
                .optional_string(7)
                .map_err(|err| storage_backend_error("Read capture search snippet", err))?,
            highlight: reader
                .optional_string(8)
                .map_err(|err| storage_backend_error("Read capture search highlight", err))?,
            score: 0.0,
        };
        let hit = bind_capture_result(result, &row, 9)?;
        if results.insert(hit.result.segment.id, hit).is_some() {
            return Err(StorageError::Database("duplicate capture search row".to_string()).into());
        }
    }
    Ok(results)
}

fn search_captures_rank_only_backend(
    backend: &dyn StorageBackend,
    query: &str,
    options: &SearchOptions,
    limit: usize,
) -> Result<Vec<CaptureSearchResult>> {
    let mut sql = format!(
        "SELECT s.id, s.pane_id, s.seq, s.content, s.content_len, s.content_hash, s.captured_at,
                NULL AS snippet, NULL AS highlight, bm25(output_segments_fts) AS score,
                {CAPTURE_AUTHORITY_COLUMNS}
         FROM output_segments s
         JOIN output_segments_fts fts ON s.id = fts.rowid
         WHERE output_segments_fts MATCH ?1"
    );
    let mut params = vec![ToSqlValue::Text(query)];
    append_search_filter_backend_params(&mut sql, &mut params, options)?;
    sql.push_str(" ORDER BY score ASC, s.captured_at ASC, s.id ASC LIMIT ?");
    params.push(ToSqlValue::Integer(usize_to_i64(limit, "limit")?));
    let rows = backend
        .query_map_cells(&sql, &params)
        .map_err(|err| storage_backend_error("Query capture search rows", err))?;
    if rows.len() > limit {
        return Err(StorageError::Database("oversized capture search page".to_string()).into());
    }
    rows.into_iter()
        .map(|row| {
            let result = search_result_from_backend_cells(&row)?;
            bind_capture_result(result, &row, 10)
        })
        .collect()
}

fn bind_capture_result(
    result: SearchResult,
    row: &[SqlCell],
    authority_start: usize,
) -> Result<CaptureSearchResult> {
    let reader = CellRowReader::new(row);
    // Scalar authority columns stay NULL if the singleton is absent. A cross
    // join would instead hide otherwise matching rows as an empty search.
    let cursor_epoch = reader
        .string(authority_start)
        .map_err(|err| storage_backend_error("Read capture search epoch", err))?;
    let high_water = reader
        .i64(authority_start + 1)
        .map_err(|err| storage_backend_error("Read capture search high-water mark", err))?;
    if cursor_epoch.len() != 32
        || !cursor_epoch
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || result.segment.id <= 0
        || result.segment.id > high_water
        || !result.score.is_finite()
    {
        return Err(StorageError::Database("invalid capture search authority".to_string()).into());
    }
    Ok(CaptureSearchResult {
        result,
        cursor_epoch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_backend_helpers::execute_typed;
    use crate::storage_backend_trait::RusqliteBackend;

    fn fixture(content: &str) -> RusqliteBackend {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::migrations::initialize_schema(&conn).unwrap();
        let backend = RusqliteBackend::new(conn).unwrap();
        backend
            .execute_batch(
                "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed)
                 VALUES (7, 'local', 1000, 1000, 1);",
            )
            .unwrap();
        execute_typed(
            &backend,
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at)
             VALUES (7, 0, ?1, ?2, 1000)",
            &[
                ToSqlValue::Text(content),
                ToSqlValue::Integer(i64::try_from(content.len()).unwrap()),
            ],
        )
        .unwrap();
        backend
    }

    #[test]
    fn colliding_segment_metadata_is_bound_to_distinct_database_epochs() {
        let original = fixture("needle original A");
        let replacement = fixture("needle replaced B");
        for snippets in [false, true] {
            let options = SearchOptions {
                include_snippets: Some(snippets),
                include_highlights: Some(false),
                ..SearchOptions::default()
            };
            let first = search_captures_backend(&original, "needle", &options).unwrap();
            let second = search_captures_backend(&replacement, "needle", &options).unwrap();
            assert_eq!(first.len(), 1);
            assert_eq!(second.len(), 1);
            assert_eq!(first[0].result.segment.id, second[0].result.segment.id);
            assert_eq!(
                first[0].result.segment.pane_id,
                second[0].result.segment.pane_id
            );
            assert_eq!(
                first[0].result.segment.captured_at,
                second[0].result.segment.captured_at
            );
            assert_eq!(
                first[0].result.segment.content_len,
                second[0].result.segment.content_len
            );
            assert_ne!(first[0].cursor_epoch, second[0].cursor_epoch);
            assert_eq!(first[0].result.segment.content, "needle original A");
            assert_eq!(second[0].result.segment.content, "needle replaced B");
            assert_eq!(first[0].result.snippet.is_some(), snippets);
            assert!(first[0].result.highlight.is_none());
        }
    }

    #[cfg(feature = "web")]
    #[test]
    fn detection_resolves_only_its_retained_capture_after_pane_reuse() {
        let backend = fixture("needle retained");
        let original = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let replacement = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        backend
            .execute_batch(
                "INSERT INTO pane_capture_bindings (capture_uuid, pane_id, created_at)
                 VALUES ('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 7, 1000),
                        ('bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 7, 2000);
                 INSERT INTO output_segment_captures (segment_id, capture_uuid)
                 VALUES (1, 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa');
                 INSERT INTO events (id, pane_id, rule_id, agent_type, event_type,
                                     severity, confidence, segment_id, detected_at)
                 VALUES (10, 7, 'demo', 'codex', 'match', 'info', 1.0, 1, 1000);
                 UPDATE panes SET pane_uuid = 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb' WHERE pane_id = 7;",
            )
            .unwrap();
        let epoch = crate::storage::event_retention_snapshot_backend(&backend)
            .unwrap()
            .cursor_epoch;
        let stream_epoch = crate::storage::query_segment_stream_snapshot_backend(&backend, Some(7))
            .unwrap()
            .cursor_epoch;
        let lookup = |epoch: &str, pane, uuid: &str| {
            detection_capture_backend(&backend, epoch, 10, pane, uuid).unwrap()
        };
        let target = lookup(&epoch, 7, original).expect("retained original capture");
        assert_eq!(target.pane_uuid, original);
        assert_eq!(target.segment_id, "1");
        assert_eq!(
            target.capture_cursor,
            format!("delta-v2:{stream_epoch}:pane:7:0")
        );
        assert!(lookup(&epoch, 7, replacement).is_none());
        assert!(lookup(&epoch, 8, original).is_none());
        let other = fixture("different database");
        let other_epoch = crate::storage::event_retention_snapshot_backend(&other)
            .unwrap()
            .cursor_epoch;
        assert!(lookup(&other_epoch, 7, original).is_none());

        // A regressed allocation watermark cannot authorize an otherwise
        // exact event and retained segment in the same database epoch.
        backend
            .execute_batch(
                "SAVEPOINT bad_event_watermark;
                 DROP TRIGGER event_retention_state_monotonic_guard;
                 UPDATE event_retention_state SET max_event_id = 9 WHERE singleton = 1;",
            )
            .unwrap();
        assert!(lookup(&epoch, 7, original).is_none());
        backend
            .execute_batch("ROLLBACK TO bad_event_watermark; RELEASE bad_event_watermark;")
            .unwrap();

        backend
            .execute("UPDATE events SET segment_id = NULL WHERE id = 10")
            .unwrap();
        backend
            .execute("DELETE FROM output_segments WHERE id = 1")
            .unwrap();
        assert!(lookup(&epoch, 7, original).is_none());
    }

    #[test]
    fn capture_search_preserves_lexical_filters_order_and_large_ids() {
        let backend = fixture("needle first");
        execute_typed(
            &backend,
            "INSERT INTO output_segments (id, pane_id, seq, content, content_len, captured_at)
             VALUES (?1, 7, 1, 'needle second', 13, 2000)",
            &[ToSqlValue::Integer(i64::MAX)],
        )
        .unwrap();
        for snippets in [false, true] {
            let options = SearchOptions {
                pane_id: Some(7),
                since: Some(1500),
                until: Some(2500),
                limit: Some(1),
                include_snippets: Some(snippets),
                ..SearchOptions::default()
            };
            let hits = search_captures_backend(&backend, "needle", &options).unwrap();
            let legacy =
                crate::storage::search_fts_with_snippets_backend(&backend, "needle", &options)
                    .unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].result.segment.id, i64::MAX);
            assert_eq!(
                serde_json::to_value(&hits[0].result).unwrap(),
                serde_json::to_value(&legacy[0]).unwrap()
            );
            let empty = SearchOptions {
                pane_id: Some(8),
                ..options
            };
            assert!(
                search_captures_backend(&backend, "needle", &empty)
                    .unwrap()
                    .is_empty()
            );
        }
        let oversized = SearchOptions {
            limit: Some(CAPTURE_SEARCH_MAX_RESULTS + 1),
            ..SearchOptions::default()
        };
        assert!(search_captures_backend(&backend, "needle", &oversized).is_err());
    }

    #[test]
    fn hydration_rechecks_zone_scope_after_ranking() {
        let backend = fixture("needle changes zone");
        backend
            .execute("UPDATE output_segments SET zone_type = 'output'")
            .unwrap();
        let options = SearchOptions {
            zone_type: Some("output".to_string()),
            ..SearchOptions::default()
        };
        let ranked = search_fts_rank_stage_backend(&backend, "needle", &options, 10).unwrap();
        assert_eq!(ranked.len(), 1);
        backend
            .execute("UPDATE output_segments SET zone_type = 'prompt'")
            .unwrap();
        let hydrated = hydrate_captures_backend(&backend, "needle", &options, &ranked).unwrap();
        assert!(
            hydrated.is_empty(),
            "a ranked row that left the scope cannot become a hit"
        );
    }

    #[test]
    fn missing_or_invalid_authority_never_returns_an_actionable_capture() {
        for fault in [
            "DROP TRIGGER segment_stream_state_delete_guard;
             DELETE FROM segment_stream_state;",
            "DROP TRIGGER segment_stream_state_update_guard;
             PRAGMA ignore_check_constraints = ON;
             UPDATE segment_stream_state SET cursor_epoch = 'invalid';",
            "DROP TRIGGER segment_stream_state_update_guard;
             UPDATE segment_stream_state SET max_segment_id = 0;",
        ] {
            for snippets in [false, true] {
                let backend = fixture("needle retained");
                backend.execute_batch(fault).unwrap();
                let options = SearchOptions {
                    include_snippets: Some(snippets),
                    ..SearchOptions::default()
                };
                assert!(search_captures_backend(&backend, "needle", &options).is_err());
            }
        }
    }
}
