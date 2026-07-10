//! Golden + e2e for the storage/search write -> index/embed -> search ->
//! retention pipeline (bead ft-j9seg). Locks in this session's storage/search
//! changes against a REAL `StorageHandle` over a temp DB:
//!
//!   * ft-xx5cl — appending a segment auto-writes a semantic embedding under the
//!     SAME embedder the query path uses (`HashEmbedder::default()` =>
//!     "fnv1a-hash-128", 128-dim); `segment_embeddings` finally has a production
//!     writer, so `semantic_search` returns hits instead of being hollow over an
//!     empty table.
//!   * ft-rrqhm — `enforce_size_limit` honors `storage.retention_max_mb`: a `0`
//!     cap is a no-op; a small cap evicts the oldest segments, shrinks live size,
//!     and retains the newest. Evicted segments' embeddings cascade-delete via
//!     the `segment_embeddings` FK (`ON DELETE CASCADE`; the writer connection
//!     runs `PRAGMA foreign_keys = ON`) — no orphans.
//!   * ft-vyg9y — `Config::validate` fail-closes `search.reranker_enabled=true`.
//!
//! Ungated so the default `cargo test` exercises it; the runtime harness mirrors
//! `tests/web.rs`.

use frankenterm_core::storage::{PaneRecord, SearchOptions, StorageConfig, StorageHandle};

const QUERY_EMBEDDER_ID: &str = "fnv1a-hash-128";

fn run_async<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    use frankenterm_core::runtime_async::CompatRuntime;
    let runtime = frankenterm_core::runtime_async::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future);
}

fn temp_db_path(slug: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "ft_storage_pipe_{slug}_{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path.to_string_lossy().to_string()
}

async fn open_storage(db_path: &str) -> StorageHandle {
    StorageHandle::with_config(db_path, StorageConfig::default())
        .await
        .expect("open storage")
}

/// Segments carry a foreign key to their pane row, so register the pane before
/// appending (matches the live capture path: discovery upserts the pane, then
/// captures append segments).
async fn register_pane_7(storage: &StorageHandle) {
    storage
        .upsert_pane(PaneRecord {
            pane_id: 7,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("pane-7".to_string()),
            cwd: Some("/tmp/pane-7".to_string()),
            tty_name: None,
            first_seen_at: 1_710_000_000_000,
            last_seen_at: 1_710_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: Some(1_710_000_000_000),
        })
        .await
        .expect("upsert pane 7");
}

async fn cleanup(storage: StorageHandle, db_path: &str) {
    storage.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
}

/// Number of embeddings stored under `embedder_id`.
async fn embedding_count(storage: &StorageHandle, embedder_id: &str) -> usize {
    storage
        .embedding_stats()
        .await
        .expect("embedding_stats")
        .iter()
        .find(|s| s.embedder_id == embedder_id)
        .map_or(0, |s| usize::try_from(s.count).unwrap_or(0))
}

async fn total_segments(storage: &StorageHandle) -> usize {
    storage
        .count_segments_before(i64::MAX)
        .await
        .expect("count segments")
}

// ===========================================================================
// write -> embed -> lexical + semantic search
// ===========================================================================

#[test]
fn write_embed_search_pipeline_e2e() {
    run_async(async {
        let db = temp_db_path("pipeline");
        let storage = open_storage(&db).await;
        register_pane_7(&storage).await;

        let texts = [
            "error: compilation failed in module auth",
            "warning: unused variable in request handler",
            "test result ok 42 passed 0 failed",
        ];
        let mut ids = Vec::new();
        for text in texts {
            let seg = storage
                .append_segment(7, text, None)
                .await
                .expect("append segment");
            ids.push(seg.id);
        }
        storage.checkpoint().await.expect("checkpoint");

        // INDEX/EMBED (ft-xx5cl): every appended segment has a fnv1a-hash-128
        // embedding — segment_embeddings has a production writer now.
        let stats = storage.embedding_stats().await.expect("embedding_stats");
        let group = stats
            .iter()
            .find(|s| s.embedder_id == QUERY_EMBEDDER_ID)
            .expect("fnv1a-hash-128 embedding group present");
        assert_eq!(
            usize::try_from(group.count).unwrap(),
            ids.len(),
            "exactly one embedding per appended segment"
        );
        assert_eq!(group.dimension, 128, "embedding dimension matches the query embedder");
        assert!(
            storage
                .get_embedding(ids[0], QUERY_EMBEDDER_ID)
                .await
                .expect("get_embedding")
                .is_some(),
            "the first segment has a stored embedding"
        );

        // SEARCH (lexical FTS): a distinctive token finds exactly its segment.
        let lexical = storage.search("compilation").await.expect("lexical search");
        assert!(
            lexical.iter().any(|seg| seg.id == ids[0]),
            "lexical search surfaces the 'compilation' segment"
        );
        assert!(
            !lexical.iter().any(|seg| seg.id == ids[2]),
            "lexical search does not return unrelated segments"
        );

        // SEARCH (semantic): non-empty now that embeddings are written — the
        // ft-xx5cl payoff (previously semantic_search ran over an empty table and
        // hybrid silently degraded to lexical).
        use frankenterm_core::search::{Embedder, HashEmbedder};
        let query_vec = HashEmbedder::default()
            .embed("compilation failed")
            .expect("embed query");
        let hits = storage
            .semantic_search(QUERY_EMBEDDER_ID, &query_vec, SearchOptions::default())
            .await
            .expect("semantic search");
        assert!(
            !hits.is_empty(),
            "semantic search returns hits over the written embeddings (not hollow)"
        );
        assert!(
            hits.iter().all(|h| ids.contains(&h.segment_id)),
            "every semantic hit is one of our written segments"
        );
        assert!(
            hits.iter().any(|h| h.segment_id == ids[0]),
            "semantic search surfaces the most relevant segment"
        );

        cleanup(storage, &db).await;
    });
}

// ===========================================================================
// retention: size eviction honors config + cascades embeddings (no orphans)
// ===========================================================================

#[test]
fn retention_size_eviction_honors_config_and_cascades_embeddings_e2e() {
    run_async(async {
        let db = temp_db_path("retention");
        let storage = open_storage(&db).await;
        register_pane_7(&storage).await;

        // ~4 MiB of low-cardinality content (so the FTS index stays tiny and the
        // segment content dominates the live size), comfortably over a 1 MiB cap.
        let big = "x".repeat(16 * 1024);
        for i in 0..256u64 {
            storage
                .append_segment(7, &format!("seg-{i}-{big}"), None)
                .await
                .expect("append segment");
        }
        storage.checkpoint().await.expect("checkpoint");

        assert_eq!(total_segments(&storage).await, 256, "all segments written");
        assert_eq!(
            embedding_count(&storage, QUERY_EMBEDDER_ID).await,
            256,
            "one embedding per segment before eviction"
        );

        // Config honoring: retention_max_mb = 0 (the unset/no-limit default) must
        // evict nothing.
        let disabled = storage.enforce_size_limit(0).await.expect("enforce 0");
        assert_eq!(disabled.deleted_segments, 0, "cap=0 must be a no-op");
        assert_eq!(total_segments(&storage).await, 256);

        // Enforce a 1 MiB cap: evicts the oldest segments and shrinks live size.
        let outcome = storage.enforce_size_limit(1).await.expect("enforce 1");
        assert!(
            outcome.deleted_segments > 0,
            "a 1 MiB cap must evict over-cap segments: {outcome:?}"
        );
        assert!(
            outcome.used_bytes_after < outcome.used_bytes_before,
            "live database size must shrink after eviction: {outcome:?}"
        );

        // Retention retains correctly: partial, oldest-first — some newest survive.
        let remaining = total_segments(&storage).await;
        assert!(
            remaining > 0 && remaining < 256,
            "expected partial oldest-first eviction, {remaining} segments remain"
        );

        // FK cascade: each evicted segment's embedding is gone too (no orphans),
        // so the embedding count tracks the surviving segment count exactly.
        let remaining_embeddings = embedding_count(&storage, QUERY_EMBEDDER_ID).await;
        assert_eq!(
            remaining_embeddings, remaining,
            "evicted segments' embeddings must cascade-delete (no orphans): \
             {remaining_embeddings} embeddings vs {remaining} segments"
        );

        cleanup(storage, &db).await;
    });
}

// ===========================================================================
// config honoring: reranker_enabled fail-closed (ft-vyg9y)
// ===========================================================================

#[test]
fn config_honoring_reranker_enabled_fails_closed() {
    use frankenterm_core::config::Config;

    let mut config = Config::default();
    config.search.reranker_enabled = true;
    let err = config
        .validate()
        .expect_err("reranker_enabled=true must be rejected — no production reranker exists");
    assert!(
        err.to_string().contains("search.reranker_enabled"),
        "the fail-closed rejection must name the key: {err}"
    );
}
