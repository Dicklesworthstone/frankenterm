use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, SearchOptions, StorageHandle};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("build e2e runtime");
    runtime.block_on(future);
}

fn emit_diag(event: &str, payload: serde_json::Value) {
    eprintln!(
        "{}",
        json!({
            "test": "e2e_capture_search",
            "event": event,
            "payload": payload,
        })
    );
}

fn pane_record(pane_id: u64, now_ms: i64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some(format!("pane-{pane_id}")),
        cwd: Some(format!("/tmp/pane-{pane_id}")),
        tty_name: None,
        first_seen_at: now_ms,
        last_seen_at: now_ms,
        observed: true,
        ignore_reason: None,
        last_decision_at: Some(now_ms),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SearchToonEnvelope {
    ok: bool,
    query: String,
    hit_count: usize,
    segment_ids: Vec<i64>,
    snippets: Vec<String>,
}

fn sqlite_count(conn: &Connection, sql: &str, param: &str) -> i64 {
    conn.query_row(sql, [param], |row| row.get::<_, i64>(0))
        .expect("sqlite count query")
}

fn sqlite_table_count(conn: &Connection, table: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .expect("sqlite table count")
}

fn collect_matching_rowids(conn: &Connection, query: &str) -> Vec<i64> {
    let mut stmt = conn
        .prepare(
            "SELECT rowid
             FROM output_segments_fts
             WHERE output_segments_fts MATCH ?1
             ORDER BY rowid ASC",
        )
        .expect("prepare rowid query");
    let rows = stmt
        .query_map([query], |row| row.get::<_, i64>(0))
        .expect("run rowid query");
    rows.collect::<Result<Vec<_>, _>>().expect("collect rowids")
}

fn decode_toon_to_json(text: &str) -> serde_json::Value {
    let decoded = toon_rust::try_decode(text, None).expect("decode TOON");
    let json_text = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    serde_json::from_str(&json_text).expect("parse decoded TOON as json")
}

async fn seed_capture_records(storage: &StorageHandle) {
    let base_ts = 1_710_000_000_000i64;
    storage
        .upsert_pane(pane_record(7, base_ts))
        .await
        .expect("upsert pane 7");
    storage
        .upsert_pane(pane_record(9, base_ts))
        .await
        .expect("upsert pane 9");

    for i in 0..100usize {
        let pane_id = if i % 2 == 0 { 7 } else { 9 };
        let content = if i % 10 == 0 {
            format!("capture_target_{i:03} build passed pane={pane_id}")
        } else {
            format!("noise_{i:03} regular pane output pane={pane_id}")
        };
        storage
            .append_segment(pane_id, &content, None)
            .await
            .expect("append segment");
    }
}

#[test]
fn e2e_capture_search_real_sqlite_roundtrip() {
    let temp = TempDir::new().expect("create temp dir");
    let db_path = temp.path().join("e2e_capture_search.sqlite");
    let db_path_str = db_path.to_string_lossy().to_string();

    emit_diag(
        "temp_db_ready",
        json!({
            "db_path": db_path_str.clone(),
            "temp_root": temp.path().display().to_string(),
        }),
    );

    run_async_test(async {
        let storage = StorageHandle::new(&db_path_str)
            .await
            .expect("create storage");
        seed_capture_records(&storage).await;

        let conn = Connection::open(&db_path).expect("open sqlite connection");
        let segment_count = sqlite_table_count(&conn, "output_segments");
        let fts_count = sqlite_table_count(&conn, "output_segments_fts");
        let direct_match_count = sqlite_count(
            &conn,
            "SELECT COUNT(*) FROM output_segments_fts WHERE output_segments_fts MATCH ?1",
            "capture_target*",
        );
        let rowids = collect_matching_rowids(&conn, "capture_target*");

        emit_diag(
            "sqlite_counts",
            json!({
                "segment_count": segment_count,
                "fts_count": fts_count,
                "direct_match_count": direct_match_count,
                "match_rowids_sample": rowids.iter().take(5).collect::<Vec<_>>(),
            }),
        );

        assert_eq!(segment_count, 100);
        assert_eq!(fts_count, 100);
        assert_eq!(direct_match_count, 10);
        assert_eq!(rowids.len(), 10);

        let results = storage
            .search_with_results(
                "capture_target*",
                SearchOptions {
                    limit: Some(20),
                    pane_id: None,
                    since: None,
                    until: None,
                    include_snippets: Some(true),
                    include_highlights: Some(true),
                    snippet_max_tokens: Some(16),
                    highlight_prefix: Some("[[".to_string()),
                    highlight_suffix: Some("]]".to_string()),
                },
            )
            .await
            .expect("search capture target");

        emit_diag(
            "search_results",
            json!({
                "result_count": results.len(),
                "first_segment_id": results.first().map(|r| r.segment.id),
                "first_snippet": results.first().and_then(|r| r.snippet.clone()),
            }),
        );

        assert_eq!(results.len(), 10);
        assert!(
            results
                .iter()
                .all(|result| result.segment.content.contains("capture_target_"))
        );
        assert!(results.iter().all(|result| {
            result
                .snippet
                .as_deref()
                .is_some_and(|s| s.contains("[[capture_target"))
        }));

        let envelope = SearchToonEnvelope {
            ok: true,
            query: "capture_target*".to_string(),
            hit_count: results.len(),
            segment_ids: results.iter().map(|result| result.segment.id).collect(),
            snippets: results
                .iter()
                .map(|result| result.snippet.clone().expect("snippet present"))
                .collect(),
        };

        let toon = toon_rust::encode(
            serde_json::to_value(&envelope).expect("envelope json"),
            None,
        );
        let decoded_json = decode_toon_to_json(&toon);

        emit_diag(
            "toon_roundtrip",
            json!({
                "toon_len": toon.len(),
                "decoded_hit_count": decoded_json["hit_count"],
                "decoded_first_segment_id": decoded_json["segment_ids"][0],
            }),
        );

        assert_eq!(decoded_json["ok"], true);
        assert_eq!(decoded_json["query"], "capture_target*");
        assert_eq!(decoded_json["hit_count"], 10);
        assert_eq!(
            decoded_json["segment_ids"].as_array().map_or(0, Vec::len),
            10
        );
        assert_eq!(decoded_json["snippets"].as_array().map_or(0, Vec::len), 10);
    });

    assert!(db_path.exists(), "db file should exist before tempdir drop");
    let cleanup_path: PathBuf = db_path.clone();
    let cleanup_root = temp.path().to_path_buf();
    drop(temp);
    emit_diag(
        "cleanup",
        json!({
            "db_exists_after_drop": Path::new(&cleanup_path).exists(),
            "temp_root": cleanup_root.display().to_string(),
            "temp_root_exists_after_drop": cleanup_root.exists(),
        }),
    );
    assert!(!cleanup_path.exists(), "temp db should be cleaned up");
}
