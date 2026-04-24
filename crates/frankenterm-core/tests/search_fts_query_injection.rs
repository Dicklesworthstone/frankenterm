use frankenterm_core::StorageError;
use frankenterm_core::runtime_compat::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, SearchOptions, StorageHandle};
use tempfile::TempDir;

fn runtime() -> frankenterm_core::runtime_compat::Runtime {
    RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime")
}

fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("fts_injection_audit.db");
    (dir, path.to_string_lossy().to_string())
}

fn pane_record(pane_id: u64, now_ms: i64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: None,
        cwd: None,
        tty_name: None,
        first_seen_at: now_ms,
        last_seen_at: now_ms,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

async fn seed_search_corpus(storage: &StorageHandle) {
    let now_ms = 1_700_000_000_000i64;
    storage
        .upsert_pane(pane_record(1, now_ms))
        .await
        .expect("upsert pane");
    storage
        .append_segment(1, "needle alpha", None)
        .await
        .expect("append alpha");
    storage
        .append_segment(1, "needle beta", None)
        .await
        .expect("append beta");
}

#[test]
fn sqlish_payload_stays_in_structured_fts_error_path() {
    let (_dir, path) = temp_db();
    let rt = runtime();

    rt.block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        let err = storage
            .search_with_results("\" OR 1 --", SearchOptions::default())
            .await
            .expect_err("sql-looking payload must not execute as SQL");

        match err {
            frankenterm_core::Error::Storage(StorageError::FtsQueryError(msg)) => {
                assert!(
                    msg.contains("Invalid FTS5 query syntax")
                        || msg.contains("Query validation failed")
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    });
}

#[test]
fn unknown_column_selector_stays_in_structured_fts_error_path() {
    let (_dir, path) = temp_db();
    let rt = runtime();

    rt.block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        let err = storage
            .search_with_results("unknowncol:needle", SearchOptions::default())
            .await
            .expect_err("unknown FTS column selector must not widen into SQL");

        match err {
            frankenterm_core::Error::Storage(StorageError::FtsQueryError(msg)) => {
                assert!(
                    msg.contains("Invalid FTS5 query syntax")
                        || msg.contains("no such column")
                        || msg.contains("Query validation failed")
                );
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    });
}
