//! ft-u6fba Phase 1b: extracted from storage.rs (mod storage_handle_tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to
//! `crate::storage::*`.

use super::*;
use rusqlite::Connection;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

// Counter for unique temp DB paths

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    super::run_storage_async_test(future);
}

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique temp DB path
fn temp_db_path() -> String {
    let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir();
    dir.join(format!("wa_test_{counter}_{}.db", std::process::id()))
        .to_str()
        .unwrap()
        .to_string()
}

/// Helper to create a test pane record
fn test_pane(pane_id: u64) -> PaneRecord {
    let now = now_ms();
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: None,
        cwd: None,
        tty_name: None,
        first_seen_at: now,
        last_seen_at: now,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

#[cfg(unix)]
#[test]
fn storage_handle_sets_db_permissions() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        let mode = std::fs::metadata(&db_path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        for suffix in ["-wal", "-shm"] {
            let path = format!("{db_path}{suffix}");
            if std::path::Path::new(&path).exists() {
                let mode = std::fs::metadata(&path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600);
            }
        }

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path}-wal"));
        let _ = std::fs::remove_file(format!("{db_path}-shm"));
    });
}

#[test]
fn storage_handle_basic_write_read() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        // Create a pane
        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Append a segment
        let segment: Segment = handle
            .append_segment(1, "Hello, world!", None)
            .await
            .unwrap();

        assert_eq!(segment.pane_id, 1);
        assert_eq!(segment.seq, 0);
        assert_eq!(segment.content, "Hello, world!");

        // Append another segment
        let segment2: Segment = handle
            .append_segment(1, "Second segment", None)
            .await
            .unwrap();

        assert_eq!(segment2.seq, 1);

        // Query segments
        let recent: Vec<Segment> = handle.get_segments(1, 10).await.unwrap();
        assert_eq!(recent.len(), 2);
        // Returned in descending seq order
        assert_eq!(recent[0].seq, 1);
        assert_eq!(recent[1].seq, 0);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_embedding_roundtrip_and_unembedded_query() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let seg1 = handle.append_segment(1, "first", None).await.unwrap();
        let seg2 = handle.append_segment(1, "second", None).await.unwrap();

        handle
            .store_embedding(seg1.id, "hash", 2, &[1u8, 2u8])
            .await
            .unwrap();
        handle
            .store_embedding(seg1.id, "quality", 2, &[3u8, 4u8])
            .await
            .unwrap();

        let hash_vec = handle
            .get_embedding(seg1.id, "hash")
            .await
            .unwrap()
            .unwrap();
        let quality_vec = handle
            .get_embedding(seg1.id, "quality")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hash_vec, vec![1u8, 2u8]);
        assert_eq!(quality_vec, vec![3u8, 4u8]);

        let unembedded_hash = handle.get_unembedded_segments("hash", 10).await.unwrap();
        assert!(unembedded_hash.contains(&seg2.id));
        assert!(!unembedded_hash.contains(&seg1.id));

        let stats = handle.embedding_stats().await.unwrap();
        assert!(
            stats
                .iter()
                .any(|s| s.embedder_id == "hash" && s.count == 1 && s.dimension == 2)
        );
        assert!(
            stats
                .iter()
                .any(|s| s.embedder_id == "quality" && s.count == 1 && s.dimension == 2)
        );

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_semantic_search_ranks_and_respects_filters() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.upsert_pane(test_pane(2)).await.unwrap();

        let seg_a = handle
            .append_segment(1, "alpha output", None)
            .await
            .unwrap();
        let seg_b = handle.append_segment(1, "beta output", None).await.unwrap();
        let seg_c = handle
            .append_segment(2, "gamma output", None)
            .await
            .unwrap();

        handle
            .store_embedding_f32(seg_a.id, "hash", &[1.0, 0.0])
            .await
            .unwrap();
        handle
            .store_embedding_f32(seg_b.id, "hash", &[0.9, 0.1])
            .await
            .unwrap();
        handle
            .store_embedding_f32(seg_c.id, "hash", &[-1.0, 0.0])
            .await
            .unwrap();

        let options = SearchOptions {
            limit: Some(10),
            ..SearchOptions::default()
        };
        let hits = handle
            .semantic_search("hash", &[1.0, 0.0], options.clone())
            .await
            .unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].segment_id, seg_a.id);
        assert!(hits[0].score >= hits[1].score);
        assert!(hits[1].score >= hits[2].score);

        let pane_hits = handle
            .semantic_search(
                "hash",
                &[1.0, 0.0],
                SearchOptions {
                    pane_id: Some(1),
                    ..options
                },
            )
            .await
            .unwrap();
        assert_eq!(pane_hits.len(), 2);
        assert!(pane_hits.iter().all(|hit| hit.segment_id != seg_c.id));

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_hybrid_search_blends_lexical_and_semantic() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let seg_lexical_only = handle
            .append_segment(1, "needle appears in lexical lane", None)
            .await
            .unwrap();
        let seg_both = handle
            .append_segment(1, "needle appears in both lanes", None)
            .await
            .unwrap();
        let seg_semantic_only = handle
            .append_segment(1, "totally different wording", None)
            .await
            .unwrap();

        // Keep one lexical-only segment unembedded to verify fallback behavior.
        handle
            .store_embedding_f32(seg_both.id, "hash", &[0.9, 0.1])
            .await
            .unwrap();
        handle
            .store_embedding_f32(seg_semantic_only.id, "hash", &[1.0, 0.0])
            .await
            .unwrap();

        let bundle = handle
            .hybrid_search_with_results(
                "needle",
                SearchOptions {
                    limit: Some(3),
                    include_snippets: Some(false),
                    ..SearchOptions::default()
                },
                "hash",
                &[1.0, 0.0],
                crate::search::SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(crate::search::FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();

        assert_eq!(bundle.mode, "hybrid");
        assert_eq!(bundle.requested_mode, "hybrid");
        assert_eq!(bundle.fallback_reason, None);
        assert_eq!(bundle.rrf_k, 60);
        assert!((bundle.lexical_weight - 1.0).abs() < f32::EPSILON);
        assert!((bundle.semantic_weight - 1.0).abs() < f32::EPSILON);
        assert!(bundle.lexical_candidates >= 2);
        assert!(bundle.semantic_candidates >= 2);
        assert!(!bundle.results.is_empty());

        for (idx, hit) in bundle.results.iter().enumerate() {
            assert_eq!(hit.fusion_rank, idx);
            let expected =
                hit.lexical_contribution.unwrap_or(0.0) + hit.semantic_contribution.unwrap_or(0.0);
            assert!(
                (hit.fusion_score - expected).abs() < 1e-6,
                "fusion score should equal lane contributions"
            );
        }

        let ids: Vec<i64> = bundle.results.iter().map(|h| h.result.segment.id).collect();
        assert!(ids.contains(&seg_lexical_only.id));
        assert!(ids.contains(&seg_semantic_only.id));

        let lexical_only_hit = bundle
            .results
            .iter()
            .find(|h| h.result.segment.id == seg_lexical_only.id)
            .unwrap();
        assert!(lexical_only_hit.lexical_rank.is_some());
        assert!(lexical_only_hit.semantic_score.is_none());
        assert!(lexical_only_hit.lexical_contribution.is_some());
        assert!(lexical_only_hit.semantic_contribution.is_none());

        let semantic_only_hit = bundle
            .results
            .iter()
            .find(|h| h.result.segment.id == seg_semantic_only.id)
            .unwrap();
        assert!(semantic_only_hit.semantic_score.is_some());
        assert!(semantic_only_hit.lexical_rank.is_none());
        assert!(semantic_only_hit.semantic_contribution.is_some());
        assert!(semantic_only_hit.lexical_contribution.is_none());

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_streaming_redactor_redacts_secret_split_across_segments() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let secret_tail = "aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRs";
        let full_secret = format!("sk-ant-api03-{secret_tail}");
        handle
            .append_segment(1, "prefix sk-ant-api03-", None)
            .await
            .unwrap();
        handle
            .append_segment(1, &format!("{secret_tail} suffix"), None)
            .await
            .unwrap();
        handle
            .record_gap(1, "flush_streaming_redactor")
            .await
            .unwrap();

        let segments = handle.get_segments(1, 10).await.unwrap();
        let joined = segments
            .iter()
            .rev()
            .map(|segment| segment.content.as_str())
            .collect::<String>();
        assert!(
            joined.contains(crate::redactor::REDACTED_MARKER),
            "split secret should be replaced with a redaction marker: {joined:?}"
        );
        assert!(
            !joined.contains(&full_secret),
            "split secret leaked in stored segment content: {joined:?}"
        );
        assert!(
            !joined.contains("sk-ant-api03-"),
            "split secret prefix leaked in stored segment content: {joined:?}"
        );

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_hybrid_search_falls_back_to_lexical_when_semantic_degraded() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle
            .append_segment(1, "needle only in lexical lane", None)
            .await
            .unwrap();
        handle
            .append_segment(1, "another needle record", None)
            .await
            .unwrap();

        // Empty query vector degrades semantic lane deterministically.
        let bundle = handle
            .hybrid_search_with_results(
                "needle",
                SearchOptions {
                    limit: Some(3),
                    include_snippets: Some(false),
                    ..SearchOptions::default()
                },
                "hash",
                &[],
                crate::search::SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(crate::search::FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();

        assert_eq!(bundle.requested_mode, "hybrid");
        assert_eq!(bundle.mode, "lexical");
        assert_eq!(
            bundle.fallback_reason.as_deref(),
            Some("semantic_query_empty")
        );
        assert_eq!(bundle.semantic_candidates, 0);
        assert!(bundle.lexical_candidates >= 1);
        assert!(!bundle.results.is_empty());

        for hit in &bundle.results {
            assert!(hit.semantic_score.is_none());
            assert!(hit.semantic_rank.is_none());
            assert!(hit.semantic_contribution.is_none());
            assert!(hit.lexical_contribution.is_some());
            assert!((hit.fusion_score - hit.lexical_contribution.unwrap_or_default()).abs() < 1e-6);
        }

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_hybrid_search_sanitizes_invalid_weights() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        let segment = handle
            .append_segment(1, "needle semantic and lexical candidate", None)
            .await
            .unwrap();
        handle
            .store_embedding_f32(segment.id, "hash", &[1.0, 0.0])
            .await
            .unwrap();

        let options = SearchOptions {
            limit: Some(3),
            include_snippets: Some(false),
            ..SearchOptions::default()
        };

        let sanitized = handle
            .hybrid_search_with_results(
                "needle",
                options.clone(),
                "hash",
                &[1.0, 0.0],
                crate::search::SearchMode::Hybrid,
                60,
                f32::NAN,
                -1.0,
                Some(crate::search::FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();
        assert!((sanitized.lexical_weight - 1.0).abs() < f32::EPSILON);
        assert!((sanitized.semantic_weight - 0.0).abs() < f32::EPSILON);
        assert!(!sanitized.results.is_empty());

        let fallback = handle
            .hybrid_search_with_results(
                "needle",
                options,
                "hash",
                &[1.0, 0.0],
                crate::search::SearchMode::Hybrid,
                60,
                0.0,
                0.0,
                Some(crate::search::FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();
        assert!((fallback.lexical_weight - 1.0).abs() < f32::EPSILON);
        assert!((fallback.semantic_weight - 1.0).abs() < f32::EPSILON);
        assert!(!fallback.results.is_empty());

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_hybrid_search_uses_semantic_cache_and_invalidation() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();
        let mut config = SemanticBudgetConfig::default();
        config.max_semantic_latency_ms = u64::MAX;
        handle.set_semantic_budget_config(config);

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let seg_a = handle
            .append_segment(1, "needle from lexical and semantic lane", None)
            .await
            .unwrap();
        let seg_b = handle
            .append_segment(1, "another needle candidate", None)
            .await
            .unwrap();

        handle
            .store_embedding_f32(seg_a.id, "hash", &[1.0, 0.0])
            .await
            .unwrap();
        handle
            .store_embedding_f32(seg_b.id, "hash", &[0.9, 0.1])
            .await
            .unwrap();

        let options = SearchOptions {
            limit: Some(5),
            include_snippets: Some(false),
            ..SearchOptions::default()
        };

        let first = handle
            .hybrid_search_with_results(
                "needle",
                options.clone(),
                "hash",
                &[1.0, 0.0],
                crate::search::SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(crate::search::FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();
        assert!(!first.semantic_cache_hit);
        assert!(first.semantic_rows_scanned > 0);
        assert_eq!(first.semantic_budget_state, "active");

        let second = handle
            .hybrid_search_with_results(
                "needle",
                options.clone(),
                "hash",
                &[1.0, 0.0],
                crate::search::SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(crate::search::FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();
        assert!(second.semantic_cache_hit);
        assert_eq!(second.semantic_rows_scanned, 0);
        assert_eq!(second.semantic_budget_state, "cache_hit");

        // Storing a new embedding invalidates semantic cache generation.
        handle
            .store_embedding_f32(seg_b.id, "hash", &[0.0, 1.0])
            .await
            .unwrap();

        let third = handle
            .hybrid_search_with_results(
                "needle",
                options,
                "hash",
                &[1.0, 0.0],
                crate::search::SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(crate::search::FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();
        assert!(!third.semantic_cache_hit);
        assert!(third.semantic_rows_scanned > 0);

        let snapshot = handle.semantic_budget_snapshot();
        assert!(snapshot.metrics.semantic_cache_hits >= 1);
        assert!(snapshot.metrics.semantic_cache_invalidations >= 1);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_hybrid_search_applies_latency_backoff_budget() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.set_semantic_budget_config(SemanticBudgetConfig {
            max_semantic_latency_ms: 0,
            semantic_backoff_cooldown_ms: 60_000,
            max_semantic_queries_per_window: 100,
            rate_limit_window_ms: 60_000,
            cache_capacity: 32,
            cache_ttl_ms: 1,
            max_semantic_scan_rows: 1_000,
            latency_ewma_alpha: 0.5,
        });

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let seg_a = handle
            .append_segment(1, "needle baseline", None)
            .await
            .unwrap();
        let seg_b = handle
            .append_segment(1, "needle fallback target", None)
            .await
            .unwrap();
        handle
            .store_embedding_f32(seg_a.id, "hash", &[1.0, 0.0])
            .await
            .unwrap();
        handle
            .store_embedding_f32(seg_b.id, "hash", &[0.9, 0.1])
            .await
            .unwrap();

        let first = handle
            .hybrid_search_with_results(
                "needle",
                SearchOptions {
                    limit: Some(3),
                    include_snippets: Some(false),
                    ..SearchOptions::default()
                },
                "hash",
                &[1.0, 0.0],
                crate::search::SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(crate::search::FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();
        assert_eq!(first.mode, "hybrid");
        assert_eq!(first.semantic_budget_state, "active");

        // Use a different query vector key to bypass cache and trigger backoff skip.
        let second = handle
            .hybrid_search_with_results(
                "needle",
                SearchOptions {
                    limit: Some(3),
                    include_snippets: Some(false),
                    ..SearchOptions::default()
                },
                "hash",
                &[0.8, 0.2],
                crate::search::SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(crate::search::FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();

        assert_eq!(second.requested_mode, "hybrid");
        assert_eq!(second.mode, "lexical");
        assert_eq!(
            second.fallback_reason.as_deref(),
            Some("semantic_budget_backoff")
        );
        assert_eq!(second.semantic_budget_state, "backoff");
        assert_eq!(second.semantic_candidates, 0);
        assert!(!second.results.is_empty());

        let snapshot = handle.semantic_budget_snapshot();
        assert!(snapshot.metrics.semantic_backoff_activations >= 1);
        assert!(snapshot.metrics.semantic_skipped_backoff >= 1);
        assert!(snapshot.backoff_until_ms.is_some());

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_records_usage_metrics_batch() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        // Single insert
        let id1 = handle
            .record_usage_metric(UsageMetricRecord {
                id: 0,
                timestamp: 1_000,
                metric_type: MetricType::ApiCall,
                pane_id: Some(1),
                agent_type: Some("codex".to_string()),
                account_id: None,
                workflow_id: None,
                count: Some(1),
                amount: None,
                tokens: None,
                metadata: Some("{\"tool\":\"wa.robot.state\"}".to_string()),
                created_at: 1_000,
            })
            .await
            .unwrap();
        assert!(id1 > 0);

        // Batch insert
        let inserted = handle
            .record_usage_metrics_batch(vec![
                UsageMetricRecord {
                    id: 0,
                    timestamp: 2_000,
                    metric_type: MetricType::TokenUsage,
                    pane_id: Some(1),
                    agent_type: Some("codex".to_string()),
                    account_id: Some("acct-1".to_string()),
                    workflow_id: None,
                    count: None,
                    amount: None,
                    tokens: Some(123),
                    metadata: None,
                    created_at: 2_000,
                },
                UsageMetricRecord {
                    id: 0,
                    timestamp: 3_000,
                    metric_type: MetricType::ApiCost,
                    pane_id: Some(1),
                    agent_type: Some("codex".to_string()),
                    account_id: Some("acct-1".to_string()),
                    workflow_id: None,
                    count: None,
                    amount: Some(0.42),
                    tokens: None,
                    metadata: Some("{\"source\":\"test\"}".to_string()),
                    created_at: 3_000,
                },
            ])
            .await
            .unwrap();
        assert_eq!(inserted, 2);

        let rows = handle
            .query_usage_metrics(MetricQuery {
                metric_type: None,
                agent_type: Some("codex".to_string()),
                account_id: None,
                since: Some(0),
                until: None,
                limit: Some(10),
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);

        // Sorted DESC by timestamp
        assert_eq!(rows[0].timestamp, 3_000);
        assert_eq!(rows[1].timestamp, 2_000);
        assert_eq!(rows[2].timestamp, 1_000);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_shutdown_flushes_pending_writes() {
    run_async_test(async {
        let db_path = temp_db_path();

        {
            let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane(1)).await.unwrap();

            // Queue up multiple writes
            for i in 0..10 {
                handle
                    .append_segment(1, &format!("Segment {i}"), None)
                    .await
                    .unwrap();
            }

            // Shutdown should flush all pending writes
            handle.shutdown().await.unwrap();
        }

        // Reopen and verify all writes persisted
        {
            let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();
            let segments: Vec<Segment> = handle.get_segments(1, 100).await.unwrap();

            // All 10 segments should be present
            assert_eq!(segments.len(), 10);

            // Verify sequence numbers are correct (returned in descending order)
            let seqs: Vec<u64> = segments.iter().map(|s| s.seq).collect();
            assert_eq!(seqs, vec![9, 8, 7, 6, 5, 4, 3, 2, 1, 0]);

            handle.shutdown().await.unwrap();
        }

        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn writer_loop_does_not_dispatch_commands_queued_after_shutdown() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let (tx, mut rx) = mpsc::channel(8);
    let mut mmap_mirror = None;

    let (upsert_tx, _upsert_rx) = oneshot::channel();
    assert!(
        tx.try_send(WriteCommand::UpsertPane {
            pane: test_pane(1),
            respond: upsert_tx,
        })
        .is_ok()
    );

    let (shutdown_tx, _shutdown_rx) = oneshot::channel();
    assert!(
        tx.try_send(WriteCommand::Shutdown {
            respond: shutdown_tx,
        })
        .is_ok()
    );

    let (append_tx, _append_rx) = oneshot::channel();
    assert!(
        tx.try_send(WriteCommand::AppendSegment {
            pane_id: 1,
            content: "late".to_string(),
            content_hash: None,
            respond: append_tx,
        })
        .is_ok()
    );
    drop(tx);

    let backend = RusqliteBackend::new(conn);
    let queued_depth = AtomicUsize::new(3);
    writer_loop(&backend, &mut rx, &mut mmap_mirror, &queued_depth);
    let conn = backend.into_connection();

    let segment_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM output_segments WHERE pane_id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(segment_count, 0);
}

#[test]
fn storage_handle_concurrent_reads_during_writes() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Write segments
        for i in 0..5 {
            handle
                .append_segment(1, &format!("Content {i}"), None)
                .await
                .unwrap();
        }

        // Concurrent reads should work (WAL mode)
        let read1 = handle.get_segments(1, 10);
        let read2 = handle.get_segments(1, 10);
        let (result1, result2) = futures::future::join(read1, read2).await;

        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert_eq!(result1.unwrap().len(), 5);
        assert_eq!(result2.unwrap().len(), 5);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_workflow_step_logs() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        // Create pane first (required for foreign key constraint)
        handle.upsert_pane(test_pane(1)).await.unwrap();

        let workflow_id = "wf-test-123";
        let now = now_ms();

        // Create workflow execution
        let workflow = WorkflowRecord {
            id: workflow_id.to_string(),
            workflow_name: "test_workflow".to_string(),
            pane_id: 1,
            trigger_event_id: None,
            current_step: 0,
            status: "running".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: None,
            started_at: now,
            updated_at: now,
            completed_at: None,
        };

        handle.upsert_workflow(workflow).await.unwrap();

        // Insert step logs
        handle
            .insert_step_log(
                workflow_id,
                None,
                0,
                "init",
                None, // step_id
                None, // step_kind
                "success",
                Some(r#"{"message":"started"}"#.to_string()),
                None, // policy_summary
                None, // verification_refs
                None, // error_code
                now,
                now + 100,
            )
            .await
            .unwrap();

        handle
            .insert_step_log(
                workflow_id,
                None,
                1,
                "send_text",
                None, // step_id
                None, // step_kind
                "success",
                Some(r#"{"chars":42}"#.to_string()),
                None, // policy_summary
                None, // verification_refs
                None, // error_code
                now + 100,
                now + 200,
            )
            .await
            .unwrap();

        handle
            .insert_step_log(
                workflow_id,
                None,
                2,
                "wait_for",
                None, // step_id
                None, // step_kind
                "success",
                Some(r#"{"matched":true}"#.to_string()),
                None, // policy_summary
                None, // verification_refs
                None, // error_code
                now + 200,
                now + 500,
            )
            .await
            .unwrap();

        // Query step logs
        let steps: Vec<WorkflowStepLogRecord> = handle.get_step_logs(workflow_id).await.unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].step_name, "init");
        assert_eq!(steps[1].step_name, "send_text");
        assert_eq!(steps[2].step_name, "wait_for");

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_gap_recording() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Record some segments
        let _seg: Segment = handle.append_segment(1, "Before gap", None).await.unwrap();

        // Record a gap
        let gap: Gap = handle
            .record_gap(1, "connection_lost")
            .await
            .unwrap()
            .expect("should return gap");

        assert_eq!(gap.pane_id, 1);
        assert_eq!(gap.reason, "connection_lost");

        // Record more segments after gap
        let _seg2: Segment = handle.append_segment(1, "After gap", None).await.unwrap();

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_event_lifecycle() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        let now = now_ms();

        // Create pane first (foreign key constraint)
        handle.upsert_pane(test_pane(1)).await.unwrap();

        let event = StoredEvent {
            id: 0, // Will be assigned
            pane_id: 1,
            rule_id: "test.rule".to_string(),
            agent_type: "codex".to_string(),
            event_type: "usage".to_string(),
            severity: "warning".to_string(),
            confidence: 0.9,
            extracted: Some(serde_json::json!({"key":"value"})),
            matched_text: Some("match".to_string()),
            segment_id: None,
            detected_at: now,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };

        let event_id: i64 = handle.record_event(event).await.unwrap();
        assert!(event_id > 0);

        // Mark handled
        handle
            .mark_event_handled(event_id, Some("wf-123".to_string()), "completed")
            .await
            .unwrap();

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_event_annotations_roundtrip() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        let now = now_ms();

        // Create pane first (foreign key constraint)
        handle.upsert_pane(test_pane(1)).await.unwrap();

        let event = StoredEvent {
            id: 0,
            pane_id: 1,
            rule_id: "test.rule".to_string(),
            agent_type: "codex".to_string(),
            event_type: "usage".to_string(),
            severity: "warning".to_string(),
            confidence: 0.9,
            extracted: None,
            matched_text: Some("match".to_string()),
            segment_id: None,
            detected_at: now,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };

        let event_id: i64 = handle.record_event(event).await.unwrap();
        assert!(event_id > 0);

        // Triage state
        let changed = handle
            .set_event_triage_state(
                event_id,
                Some("new".to_string()),
                Some("tester".to_string()),
            )
            .await
            .unwrap();
        assert!(changed);

        // Labels (idempotent)
        let inserted = handle
            .add_event_label(
                event_id,
                "needs-attn".to_string(),
                Some("tester".to_string()),
            )
            .await
            .unwrap();
        assert!(inserted);
        let inserted_again = handle
            .add_event_label(
                event_id,
                "needs-attn".to_string(),
                Some("tester".to_string()),
            )
            .await
            .unwrap();
        assert!(!inserted_again);

        // Note (should be redacted at write time)
        let note = "token sk-abc123456789012345678901234567890123456789012345678901";
        handle
            .set_event_note(event_id, Some(note.to_string()), Some("tester".to_string()))
            .await
            .unwrap();

        let annotations = handle
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(annotations.triage_state.as_deref(), Some("new"));
        assert_eq!(annotations.triage_updated_by.as_deref(), Some("tester"));
        assert_eq!(annotations.labels, vec!["needs-attn".to_string()]);
        let stored_note = annotations.note.unwrap_or_default();
        assert!(stored_note.contains("[REDACTED]"));
        assert!(!stored_note.contains("sk-abc"));

        // Query filters should work (label + triage state)
        let events = handle
            .get_events(EventQuery {
                triage_state: Some("new".to_string()),
                label: Some("needs-attn".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event_id);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_event_stream_cursor_resume() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(7)).await.unwrap();
        let base = now_ms();
        let mut ids = Vec::new();
        for i in 0..3 {
            let id = handle
                .record_event(StoredEvent {
                    id: 0,
                    pane_id: 7,
                    rule_id: format!("test.rule.{i}"),
                    agent_type: "codex".to_string(),
                    event_type: "stream".to_string(),
                    severity: "info".to_string(),
                    confidence: 0.9,
                    extracted: None,
                    matched_text: Some(format!("m{i}")),
                    segment_id: None,
                    detected_at: base + i,
                    dedupe_key: None,
                    handled_at: None,
                    handled_by_workflow_id: None,
                    handled_status: None,
                })
                .await
                .unwrap();
            ids.push(id);
        }

        let page1 = handle
            .get_events_stream(EventStreamQuery {
                after_id: None,
                limit: Some(2),
                pane_id: Some(7),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0].id, ids[0]);
        assert_eq!(page1[1].id, ids[1]);

        let cursor = page1.last().map(|event| event.id).unwrap();
        let page2 = handle
            .get_events_stream(EventStreamQuery {
                after_id: Some(cursor),
                limit: Some(10),
                pane_id: Some(7),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].id, ids[2]);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_with_small_queue_handles_burst() {
    run_async_test(async {
        let db_path = temp_db_path();

        // Use a small queue to test bounded channel behavior
        let config = StorageConfig {
            write_queue_size: 4,
            defer_fts_triggers: false,
        };
        let handle: StorageHandle = StorageHandle::with_config(&db_path, config).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Write more items than queue size - should work because we await each write
        for i in 0..20 {
            handle
                .append_segment(1, &format!("Segment {i}"), None)
                .await
                .unwrap();
        }

        let segments: Vec<Segment> = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 20);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_reopen_preserves_synchronous_fts_indexing() {
    run_async_test(async {
        let db_path = temp_db_path();
        let config = StorageConfig {
            write_queue_size: 4,
            defer_fts_triggers: false,
        };
        let fts_trigger_count = |conn: &Connection| -> i64 {
            conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'trigger'
                       AND name IN ('output_segments_ai', 'output_segments_ad', 'output_segments_au')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
        };
        let fts_match_count = |conn: &Connection, match_token: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM output_segments_fts
                     WHERE output_segments_fts MATCH ?1",
                [match_token],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
        };

        let handle: StorageHandle = StorageHandle::with_config(&db_path, config.clone())
            .await
            .unwrap();
        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle
            .append_segment(1, "beforereopen", None)
            .await
            .unwrap();
        handle.shutdown().await.unwrap();

        let conn = Connection::open(&db_path).unwrap();
        assert_eq!(
            fts_trigger_count(&conn),
            3,
            "fresh default open should leave all three FTS triggers installed"
        );
        assert_eq!(
            fts_match_count(&conn, "beforereopen"),
            1,
            "default open should synchronously index the first write"
        );
        drop(conn);

        let reopened: StorageHandle = StorageHandle::with_config(&db_path, config).await.unwrap();
        reopened
            .append_segment(1, "afterreopen", None)
            .await
            .unwrap();
        reopened.shutdown().await.unwrap();

        let conn = Connection::open(&db_path).unwrap();
        assert_eq!(
            fts_trigger_count(&conn),
            3,
            "reopening with defer_fts_triggers=false must keep all three FTS triggers installed"
        );
        assert_eq!(
            fts_match_count(&conn, "beforereopen"),
            1,
            "reopening must not disturb previously indexed rows"
        );
        assert_eq!(
            fts_match_count(&conn, "afterreopen"),
            1,
            "reopened handle must still synchronously index new writes"
        );
        drop(conn);

        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path}-wal"));
        let _ = std::fs::remove_file(format!("{db_path}-shm"));
    });
}

#[test]
fn storage_handle_seq_is_monotonic_per_pane() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        // Create two panes
        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.upsert_pane(test_pane(2)).await.unwrap();

        // Interleave writes to both panes
        for i in 0..5 {
            handle
                .append_segment(1, &format!("Pane1 seg {i}"), None)
                .await
                .unwrap();
            handle
                .append_segment(2, &format!("Pane2 seg {i}"), None)
                .await
                .unwrap();
        }

        // Verify each pane has monotonic seqs starting at 0
        let pane1_segs: Vec<Segment> = handle.get_segments(1, 10).await.unwrap();
        let pane2_segs: Vec<Segment> = handle.get_segments(2, 10).await.unwrap();

        assert_eq!(pane1_segs.len(), 5);
        assert_eq!(pane2_segs.len(), 5);

        // Check monotonicity (returned in descending order)
        let pane1_seq_values: Vec<u64> = pane1_segs.iter().map(|s| s.seq).collect();
        let pane2_seq_values: Vec<u64> = pane2_segs.iter().map(|s| s.seq).collect();

        assert_eq!(pane1_seq_values, vec![4, 3, 2, 1, 0]);
        assert_eq!(pane2_seq_values, vec![4, 3, 2, 1, 0]);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_agent_sessions() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        let now = now_ms();

        // Create pane first (foreign key constraint)
        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        handle.upsert_pane(pane).await.unwrap();

        let mut session = AgentSessionRecord::new_start(1, "claude_code");
        session.started_at = now;
        session.total_tokens = Some(1000);
        session.model_name = Some("opus".to_string());

        let session_id: i64 = handle.upsert_agent_session(session).await.unwrap();
        assert!(session_id > 0);

        // Query back
        let retrieved: Option<AgentSessionRecord> =
            handle.get_agent_session(session_id).await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.agent_type, "claude_code");
        assert_eq!(retrieved.total_tokens, Some(1000));

        // Query active sessions
        let active: Vec<AgentSessionRecord> = handle.get_active_sessions().await.unwrap();
        assert!(!active.is_empty());

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

// =========================================================================
// Checkpoint Tests (wa-upg.5.3)
// =========================================================================

#[test]
fn checkpoint_returns_result() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        // Write some data so the WAL has pages
        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle
            .append_segment(1, "checkpoint test data", None)
            .await
            .unwrap();

        let result = handle.checkpoint().await.unwrap();
        // PASSIVE checkpoint may or may not move pages, but it should succeed
        assert!(result.wal_pages >= 0);
        assert!(result.optimized);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn checkpoint_is_idempotent() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Run checkpoint twice — both should succeed
        let r1 = handle.checkpoint().await.unwrap();
        let r2 = handle.checkpoint().await.unwrap();
        assert!(r1.optimized);
        assert!(r2.optimized);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn checkpoint_after_many_writes() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Generate WAL traffic
        for i in 0..50 {
            handle
                .append_segment(1, &format!("segment {i}"), None)
                .await
                .unwrap();
        }

        let result = handle.checkpoint().await.unwrap();
        assert!(result.wal_pages >= 0);
        assert!(result.optimized);

        // Data should still be readable after checkpoint
        let segments = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 50);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn vacuum_still_works() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.append_segment(1, "vacuum test", None).await.unwrap();

        // Vacuum should still work alongside checkpoint
        handle.vacuum().await.unwrap();

        let segments = handle.get_segments(1, 10).await.unwrap();
        assert_eq!(segments.len(), 1);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

// =========================================================================
// Write Batching Tests (wa-upg.5.3)
// =========================================================================

#[test]
fn concurrent_writes_are_batched() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Fire many writes concurrently — they should be batched
        let mut handles = Vec::new();
        for i in 0..20 {
            let h = handle.clone();
            handles.push(crate::runtime_async::task::spawn(async move {
                h.append_segment(1, &format!("batch-{i}"), None)
                    .await
                    .unwrap()
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // All segments should be persisted
        let segments = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 20);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn batched_writes_preserve_ordering() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Write segments sequentially — seq numbers should be monotonic
        for i in 0..10 {
            handle
                .append_segment(1, &format!("ordered-{i}"), None)
                .await
                .unwrap();
        }

        let segments = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 10);

        // Verify ordering by content (they should come back newest-first from get_segments)
        // but seq numbers should be monotonically increasing
        let mut seqs: Vec<u64> = segments.iter().map(|s| s.seq).collect();
        seqs.sort();
        for (idx, seq) in seqs.iter().enumerate() {
            assert_eq!(*seq, idx as u64, "seq should be monotonic");
        }

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn checkpoint_backend_works_directly() {
    run_async_test(async {
        // WAL mode requires a file-backed database.
        let db_path = temp_db_path();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
        initialize_schema(&conn).unwrap();

        // Insert some data
        conn.execute(
                "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (1, 'local', 0, 0, 1)",
                [],
            )
            .unwrap();

        let backend = RusqliteBackend::new(conn);
        let result = checkpoint_backend(&backend).unwrap();
        assert!(result.wal_pages >= 0);
        assert!(result.optimized);

        drop(backend);
        let _ = std::fs::remove_file(&db_path);
    });
}

// =========================================================================
// Indexing Progress Tracking Tests (wa-upg.5.2)
// =========================================================================

#[test]
fn indexing_stats_empty_database() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert!(stats.is_empty(), "No panes means no stats");

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn indexing_stats_pane_with_no_segments() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].pane_id, 1);
        assert_eq!(stats[0].segment_count, 0);
        assert_eq!(stats[0].total_bytes, 0);
        assert!(stats[0].max_seq.is_none());
        assert!(stats[0].last_segment_at.is_none());
        assert!(stats[0].fts_consistent);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn indexing_stats_tracks_segments() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.append_segment(1, "hello", None).await.unwrap();
        handle.append_segment(1, "world!", None).await.unwrap();
        handle.append_segment(1, "test data", None).await.unwrap();

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].pane_id, 1);
        assert_eq!(stats[0].segment_count, 3);
        assert_eq!(stats[0].total_bytes, 5 + 6 + 9); // hello + world! + test data
        assert_eq!(stats[0].max_seq, Some(2)); // 0, 1, 2
        assert!(stats[0].last_segment_at.is_some());
        assert!(stats[0].fts_consistent);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn indexing_stats_multiple_panes() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.upsert_pane(test_pane(2)).await.unwrap();

        handle.append_segment(1, "pane1-data", None).await.unwrap();
        handle
            .append_segment(2, "pane2-data-longer", None)
            .await
            .unwrap();
        handle.append_segment(2, "pane2-more", None).await.unwrap();

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert_eq!(stats.len(), 2);

        let p1 = stats.iter().find(|s| s.pane_id == 1).unwrap();
        assert_eq!(p1.segment_count, 1);
        assert_eq!(p1.total_bytes, 10);

        let p2 = stats.iter().find(|s| s.pane_id == 2).unwrap();
        assert_eq!(p2.segment_count, 2);
        assert_eq!(p2.total_bytes, 17 + 10);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn indexing_stats_seq_is_monotonic() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        for i in 0..10 {
            handle
                .append_segment(1, &format!("seg-{i}"), None)
                .await
                .unwrap();
        }

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert_eq!(stats[0].segment_count, 10);
        assert_eq!(stats[0].max_seq, Some(9));

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn indexing_stats_ignored_panes_excluded() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        // Create observed pane
        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.append_segment(1, "visible", None).await.unwrap();

        // Create ignored pane
        let mut ignored = test_pane(2);
        ignored.observed = false;
        ignored.ignore_reason = Some("test exclude".to_string());
        handle.upsert_pane(ignored).await.unwrap();

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert_eq!(stats.len(), 1, "Only observed panes appear in stats");
        assert_eq!(stats[0].pane_id, 1);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn indexing_health_report_healthy() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.append_segment(1, "hello world", None).await.unwrap();

        let report = handle.get_indexing_health().await.unwrap();
        assert!(report.healthy);
        assert_eq!(report.total_segments, 1);
        assert_eq!(report.total_bytes, 11);
        assert_eq!(report.inconsistent_panes, 0);
        assert_eq!(report.panes.len(), 1);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn indexing_health_report_aggregates() {
    run_async_test(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.upsert_pane(test_pane(2)).await.unwrap();
        handle.upsert_pane(test_pane(3)).await.unwrap();

        for pane in 1..=3u64 {
            for i in 0..5 {
                handle
                    .append_segment(pane, &format!("p{pane}-s{i}"), None)
                    .await
                    .unwrap();
            }
        }

        let report = handle.get_indexing_health().await.unwrap();
        assert!(report.healthy);
        assert_eq!(report.total_segments, 15);
        assert_eq!(report.panes.len(), 3);
        for p in &report.panes {
            assert_eq!(p.segment_count, 5);
            assert!(p.fts_consistent);
        }

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn fts_integrity_check_on_healthy_db() {
    let db_path = temp_db_path();
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
    initialize_schema(&conn).unwrap();

    // Insert some data via triggers
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (1, 'local', 0, 0, 1)",
            [],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (1, 0, 'test', 4, 0)",
            [],
        ).unwrap();

    let backend = RusqliteBackend::new(conn);
    let ok = check_fts_integrity_backend(&backend).unwrap();
    assert!(ok, "Healthy FTS should pass integrity check");

    let conn = backend.into_connection();
    drop(conn);
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn build_report_marks_healthy_when_fts_ok() {
    let stats = vec![PaneIndexingStats {
        pane_id: 1,
        segment_count: 10,
        total_bytes: 100,
        max_seq: Some(9),
        last_segment_at: Some(1000),
        fts_row_count: 10,
        fts_consistent: true,
    }];
    let report = build_indexing_health_report(stats, true);
    assert!(report.healthy);
    assert_eq!(report.inconsistent_panes, 0);
}

#[test]
fn build_report_marks_unhealthy_when_fts_corrupt() {
    let stats = vec![
        PaneIndexingStats {
            pane_id: 1,
            segment_count: 10,
            total_bytes: 100,
            max_seq: Some(9),
            last_segment_at: Some(1000),
            fts_row_count: 10,
            fts_consistent: true,
        },
        PaneIndexingStats {
            pane_id: 2,
            segment_count: 5,
            total_bytes: 50,
            max_seq: Some(4),
            last_segment_at: Some(2000),
            fts_row_count: 5,
            fts_consistent: true,
        },
    ];
    let report = build_indexing_health_report(stats, false);
    assert!(!report.healthy);
    assert_eq!(report.inconsistent_panes, 2); // All panes marked
    assert!(!report.panes[0].fts_consistent);
    assert!(!report.panes[1].fts_consistent);
}
