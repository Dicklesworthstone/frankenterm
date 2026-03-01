//! LabRuntime-ported storage tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` tests from `storage.rs` to asupersync-based
//! `RuntimeFixture`, gaining deterministic scheduling for StorageHandle I/O.
//!
//! Covers: storage_handle_tests (29), queue_depth_tests (4),
//! backpressure_integration_tests (3), timeline_integration_tests (17).
//!
//! Skipped tests:
//! - `concurrent_writes_are_batched` — uses `task::spawn` for concurrent writes
//! - `checkpoint_sync_function_works_directly` — uses private `checkpoint_sync` fn + raw Connection
//! - `fts_integrity_check_on_healthy_db` — uses private `check_fts_integrity_sync` + raw Connection
//! - `build_report_marks_healthy_when_fts_ok` — uses private `build_indexing_health_report`
//! - `build_report_marks_unhealthy_when_fts_corrupt` — uses private `build_indexing_health_report`
//! - `write_queue_depth_rises_under_concurrent_writes` — uses `task::spawn` + `sleep`
//! - `write_queue_bounded_under_heavy_load` — uses `task::spawn` + `sleep`
//! - `capture_channel_backpressure_detected` — uses `timeout`
//! - `capture_channel_drains_when_consumer_resumes` — uses `sleep`
//! - `storage_concurrent_writers_dont_deadlock` — uses `task::spawn` + `timeout`
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::storage::{
    AgentSessionRecord, Correlation, CorrelationRef, CorrelationType, EventQuery, Gap,
    IndexingHealthReport, MetricQuery, MetricType, PaneIndexingStats, PaneInfo, PaneRecord,
    SearchOptions, Segment, SemanticBudgetConfig, StorageConfig, StorageHandle, StoredEvent,
    Timeline, TimelineEvent, TimelineQuery, UsageMetricRecord, WorkflowRecord,
    WorkflowStepLogRecord, now_ms,
};
use frankenterm_core::search::{FusionBackend, SearchMode};
use std::sync::atomic::{AtomicU64, Ordering};

// ===========================================================================
// Helpers
// ===========================================================================

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> String {
    let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir();
    dir.join(format!(
        "wa_labrt_test_{counter}_{}.db",
        std::process::id()
    ))
    .to_str()
    .unwrap()
    .to_string()
}

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

fn make_timeline_pane(pane_id: u64, now: i64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: Some(format!("uuid-{pane_id}")),
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some(format!("pane-{pane_id}")),
        cwd: Some("/tmp/test".to_string()),
        tty_name: None,
        first_seen_at: now,
        last_seen_at: now,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

fn make_event(
    pane_id: u64,
    rule_id: &str,
    event_type: &str,
    severity: &str,
    detected_at: i64,
) -> StoredEvent {
    StoredEvent {
        id: 0,
        pane_id,
        rule_id: rule_id.to_string(),
        agent_type: "claude_code".to_string(),
        event_type: event_type.to_string(),
        severity: severity.to_string(),
        confidence: 0.9,
        extracted: None,
        matched_text: None,
        segment_id: None,
        detected_at,
        dedupe_key: None,
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

// ===========================================================================
// Section 1: storage_handle_tests ported from tokio::test
// ===========================================================================

#[cfg(unix)]
#[test]
fn storage_handle_sets_db_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let segment: Segment = handle
            .append_segment(1, "Hello, world!", None)
            .await
            .unwrap();
        assert_eq!(segment.pane_id, 1);
        assert_eq!(segment.seq, 0);
        assert_eq!(segment.content, "Hello, world!");

        let segment2: Segment = handle
            .append_segment(1, "Second segment", None)
            .await
            .unwrap();
        assert_eq!(segment2.seq, 1);

        let recent: Vec<Segment> = handle.get_segments(1, 10).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].seq, 1);
        assert_eq!(recent[1].seq, 0);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_embedding_roundtrip_and_unembedded_query() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.upsert_pane(test_pane(2)).await.unwrap();

        let seg_a = handle
            .append_segment(1, "alpha output", None)
            .await
            .unwrap();
        let seg_b = handle
            .append_segment(1, "beta output", None)
            .await
            .unwrap();
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

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
                SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(FusionBackend::FrankenSearchRrf),
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
fn storage_handle_hybrid_search_falls_back_to_lexical_when_semantic_degraded() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle
            .append_segment(1, "needle only in lexical lane", None)
            .await
            .unwrap();
        handle
            .append_segment(1, "another needle record", None)
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
                &[],
                SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(FusionBackend::FrankenSearchRrf),
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
            assert!(
                (hit.fusion_score - hit.lexical_contribution.unwrap_or_default()).abs() < 1e-6
            );
        }

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_hybrid_search_sanitizes_invalid_weights() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

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
                SearchMode::Hybrid,
                60,
                f32::NAN,
                -1.0,
                Some(FusionBackend::FrankenSearchRrf),
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
                SearchMode::Hybrid,
                60,
                0.0,
                0.0,
                Some(FusionBackend::FrankenSearchRrf),
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();
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
                SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(FusionBackend::FrankenSearchRrf),
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
                SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();
        assert!(second.semantic_cache_hit);
        assert_eq!(second.semantic_rows_scanned, 0);
        assert_eq!(second.semantic_budget_state, "cache_hit");

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
                SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(FusionBackend::FrankenSearchRrf),
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

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
                SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(FusionBackend::FrankenSearchRrf),
            )
            .await
            .unwrap();
        assert_eq!(first.mode, "hybrid");
        assert_eq!(first.semantic_budget_state, "active");

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
                SearchMode::Hybrid,
                60,
                1.0,
                1.0,
                Some(FusionBackend::FrankenSearchRrf),
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

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
        assert_eq!(rows[0].timestamp, 3_000);
        assert_eq!(rows[1].timestamp, 2_000);
        assert_eq!(rows[2].timestamp, 1_000);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_shutdown_flushes_pending_writes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();

        {
            let handle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane(1)).await.unwrap();

            for i in 0..10 {
                handle
                    .append_segment(1, &format!("Segment {i}"), None)
                    .await
                    .unwrap();
            }

            handle.shutdown().await.unwrap();
        }

        {
            let handle = StorageHandle::new(&db_path).await.unwrap();
            let segments: Vec<Segment> = handle.get_segments(1, 100).await.unwrap();
            assert_eq!(segments.len(), 10);

            let seqs: Vec<u64> = segments.iter().map(|s| s.seq).collect();
            assert_eq!(seqs, vec![9, 8, 7, 6, 5, 4, 3, 2, 1, 0]);

            handle.shutdown().await.unwrap();
        }

        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_concurrent_reads_during_writes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        for i in 0..5 {
            handle
                .append_segment(1, &format!("Content {i}"), None)
                .await
                .unwrap();
        }

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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let workflow_id = "wf-test-123";
        let now = now_ms();

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

        handle
            .insert_step_log(
                workflow_id,
                None,
                0,
                "init",
                None,
                None,
                "success",
                Some(r#"{"message":"started"}"#.to_string()),
                None,
                None,
                None,
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
                None,
                None,
                "success",
                Some(r#"{"chars":42}"#.to_string()),
                None,
                None,
                None,
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
                None,
                None,
                "success",
                Some(r#"{"matched":true}"#.to_string()),
                None,
                None,
                None,
                now + 200,
                now + 500,
            )
            .await
            .unwrap();

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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let _seg: Segment = handle
            .append_segment(1, "Before gap", None)
            .await
            .unwrap();

        let gap: Gap = handle
            .record_gap(1, "connection_lost")
            .await
            .unwrap()
            .expect("should return gap");

        assert_eq!(gap.pane_id, 1);
        assert_eq!(gap.reason, "connection_lost");

        let _seg2: Segment = handle
            .append_segment(1, "After gap", None)
            .await
            .unwrap();

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn storage_handle_event_lifecycle() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        let now = now_ms();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let event = StoredEvent {
            id: 0,
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        let now = now_ms();

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

        let changed = handle
            .set_event_triage_state(
                event_id,
                Some("new".to_string()),
                Some("tester".to_string()),
            )
            .await
            .unwrap();
        assert!(changed);

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
fn storage_handle_with_small_queue_handles_burst() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();

        let config = StorageConfig {
            write_queue_size: 4,
        };
        let handle = StorageHandle::with_config(&db_path, config).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

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
fn storage_handle_seq_is_monotonic_per_pane() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.upsert_pane(test_pane(2)).await.unwrap();

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

        let pane1_segs: Vec<Segment> = handle.get_segments(1, 10).await.unwrap();
        let pane2_segs: Vec<Segment> = handle.get_segments(2, 10).await.unwrap();

        assert_eq!(pane1_segs.len(), 5);
        assert_eq!(pane2_segs.len(), 5);

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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        let now = now_ms();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let mut session = AgentSessionRecord::new_start(1, "claude_code");
        session.started_at = now;
        session.total_tokens = Some(1000);
        session.model_name = Some("opus".to_string());

        let session_id: i64 = handle.upsert_agent_session(session).await.unwrap();
        assert!(session_id > 0);

        let retrieved: Option<AgentSessionRecord> =
            handle.get_agent_session(session_id).await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.agent_type, "claude_code");
        assert_eq!(retrieved.total_tokens, Some(1000));

        let active: Vec<AgentSessionRecord> = handle.get_active_sessions().await.unwrap();
        assert!(!active.is_empty());

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

// ── Checkpoint tests ──────────────────────────────────────────────

#[test]
fn checkpoint_returns_result() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle
            .append_segment(1, "checkpoint test data", None)
            .await
            .unwrap();

        let result = handle.checkpoint().await.unwrap();
        assert!(result.wal_pages >= 0);
        assert!(result.optimized);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn checkpoint_is_idempotent() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        for i in 0..50 {
            handle
                .append_segment(1, &format!("segment {i}"), None)
                .await
                .unwrap();
        }

        let result = handle.checkpoint().await.unwrap();
        assert!(result.wal_pages >= 0);
        assert!(result.optimized);

        let segments = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 50);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn vacuum_still_works() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle
            .append_segment(1, "vacuum test", None)
            .await
            .unwrap();

        handle.vacuum().await.unwrap();

        let segments = handle.get_segments(1, 10).await.unwrap();
        assert_eq!(segments.len(), 1);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

// Note: `concurrent_writes_are_batched` omitted — uses `crate::runtime_compat::task::spawn`

#[test]
fn batched_writes_preserve_ordering() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        for i in 0..10 {
            handle
                .append_segment(1, &format!("ordered-{i}"), None)
                .await
                .unwrap();
        }

        let segments = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 10);

        let mut seqs: Vec<u64> = segments.iter().map(|s| s.seq).collect();
        seqs.sort();
        for (idx, seq) in seqs.iter().enumerate() {
            assert_eq!(*seq, idx as u64, "seq should be monotonic");
        }

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

// Note: `checkpoint_sync_function_works_directly` omitted — uses private `checkpoint_sync`
//       and raw rusqlite `Connection` not available from integration tests.

// ── Indexing stats tests ──────────────────────────────────────────

#[test]
fn indexing_stats_empty_database() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.append_segment(1, "hello", None).await.unwrap();
        handle.append_segment(1, "world!", None).await.unwrap();
        handle
            .append_segment(1, "test data", None)
            .await
            .unwrap();

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].pane_id, 1);
        assert_eq!(stats[0].segment_count, 3);
        assert_eq!(stats[0].total_bytes, 5 + 6 + 9);
        assert_eq!(stats[0].max_seq, Some(2));
        assert!(stats[0].last_segment_at.is_some());
        assert!(stats[0].fts_consistent);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn indexing_stats_multiple_panes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.upsert_pane(test_pane(2)).await.unwrap();

        handle
            .append_segment(1, "pane1-data", None)
            .await
            .unwrap();
        handle
            .append_segment(2, "pane2-data-longer", None)
            .await
            .unwrap();
        handle
            .append_segment(2, "pane2-more", None)
            .await
            .unwrap();

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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.append_segment(1, "visible", None).await.unwrap();

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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle
            .append_segment(1, "hello world", None)
            .await
            .unwrap();

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
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
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

// Note: `fts_integrity_check_on_healthy_db`, `build_report_marks_healthy_when_fts_ok`,
//       and `build_report_marks_unhealthy_when_fts_corrupt` omitted — use private functions
//       and raw rusqlite Connection not available from integration tests.

// ===========================================================================
// Section 2: queue_depth_tests ported from tokio::test
// ===========================================================================

#[test]
fn write_queue_depth_starts_at_zero() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        assert_eq!(handle.write_queue_depth(), 0);
        assert!(handle.write_queue_capacity() > 0);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn write_queue_capacity_matches_config() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let mut config = StorageConfig::default();
        config.write_queue_size = 64;
        let handle = StorageHandle::with_config(&db_path, config).await.unwrap();

        assert_eq!(handle.write_queue_capacity(), 64);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn write_queue_depth_is_bounded() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        let depth = handle.write_queue_depth();
        let cap = handle.write_queue_capacity();
        assert!(
            depth <= cap,
            "depth ({depth}) should be <= capacity ({cap})"
        );

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

// Note: `write_queue_depth_rises_under_concurrent_writes` omitted — uses task::spawn + sleep
// Note: `write_queue_bounded_under_heavy_load` omitted — uses task::spawn + sleep

#[test]
fn write_queue_depth_returns_to_zero_after_drain() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        for i in 0..5 {
            handle
                .append_segment(1, &format!("sequential-{i}"), None)
                .await
                .unwrap();
        }

        assert_eq!(handle.write_queue_depth(), 0);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

// ===========================================================================
// Section 3: backpressure_integration_tests ported from tokio::test
// ===========================================================================

// Note: `capture_channel_backpressure_detected` omitted — uses timeout
// Note: `capture_channel_drains_when_consumer_resumes` omitted — uses sleep
// Note: `storage_concurrent_writers_dont_deadlock` omitted — uses task::spawn + timeout

#[test]
fn gap_recording_works_under_backpressure() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let mut config = StorageConfig::default();
        config.write_queue_size = 4;
        let handle = StorageHandle::with_config(&db_path, config).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let seg_before = handle
            .append_segment(1, "before-gap", None)
            .await
            .unwrap();

        let gap = handle
            .record_gap(1, "backpressure_overflow")
            .await
            .unwrap();
        assert!(
            gap.is_some(),
            "GAP should be recorded after existing segment"
        );
        let gap = gap.unwrap();
        assert_eq!(gap.pane_id, 1);
        assert_eq!(gap.reason, "backpressure_overflow");
        assert_eq!(gap.seq_before, seg_before.seq);
        assert_eq!(gap.seq_after, seg_before.seq + 1);

        let seg_after = handle
            .append_segment(1, "after-gap", None)
            .await
            .unwrap();

        let segments = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].content, "after-gap");
        assert_eq!(segments[1].content, "before-gap");
        assert!(seg_after.seq > seg_before.seq);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn health_warning_threshold_generates_warnings() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use frankenterm_core::crash::HealthSnapshot;

        let snapshot = HealthSnapshot {
            timestamp: 0,
            observed_panes: 2,
            capture_queue_depth: 820,
            write_queue_depth: 10,
            last_seq_by_pane: vec![],
            warnings: vec!["Capture queue backpressure: 820/1024 (80%)".to_string()],
            ingest_lag_avg_ms: 100.0,
            ingest_lag_max_ms: 500,
            db_writable: true,
            db_last_write_at: Some(1000),
            pane_priority_overrides: vec![],
            scheduler: None,
            backpressure_tier: None,
            last_activity_by_pane: vec![],
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
        };

        assert!(!snapshot.warnings.is_empty());
        assert!(snapshot.warnings[0].contains("backpressure"));
        assert!(snapshot.warnings[0].contains("80%"));
    });
}

#[test]
fn event_bus_detects_subscriber_lag() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use frankenterm_core::events::{Event, EventBus};

        let bus = EventBus::new(4);

        let mut sub = bus.subscribe();

        for i in 0..8 {
            let _ = bus.publish(Event::SegmentCaptured {
                pane_id: 1,
                seq: i,
                content_len: 100,
            });
        }

        let result = sub.recv().await;
        match result {
            Err(frankenterm_core::events::RecvError::Lagged { missed_count }) => {
                assert!(missed_count > 0, "Should report missed events due to lag");
            }
            Ok(_) => {
                // Some events may still be in buffer, that's also valid
            }
            Err(e) => panic!("Unexpected error: {e:?}"),
        }

        let stats = bus.stats();
        assert_eq!(stats.capacity, 4);
    });
}

// ===========================================================================
// Section 4: timeline_integration_tests ported from tokio::test
// ===========================================================================

#[test]
fn timeline_empty_db_returns_empty() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let query = TimelineQuery::new();
        let timeline = handle.get_timeline(query).await.unwrap();

        assert!(timeline.events.is_empty());
        assert!(timeline.correlations.is_empty());
        assert_eq!(timeline.total_count, 0);
        assert!(!timeline.has_more);

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_single_event_no_correlations() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 1);
        assert!(timeline.correlations.is_empty());
        assert_eq!(timeline.total_count, 1);

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_temporal_correlation_across_panes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();
        handle
            .upsert_pane(make_timeline_pane(2, now))
            .await
            .unwrap();

        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "warning", "warning", now + 3000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 2);
        let temporal = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Temporal)
            .count();
        assert!(temporal > 0, "Should detect temporal correlation");

        let event_with_refs = timeline
            .events
            .iter()
            .filter(|e| !e.correlations.is_empty())
            .count();
        assert!(event_with_refs > 0, "Events should have correlation refs");

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_failover_correlation_integration() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();
        handle
            .upsert_pane(make_timeline_pane(2, now))
            .await
            .unwrap();

        handle
            .record_event(make_event(
                1,
                "usage_limit",
                "usage.reached",
                "warning",
                now,
            ))
            .await
            .unwrap();
        handle
            .record_event(make_event(
                2,
                "session_start",
                "session.start",
                "info",
                now + 120_000,
            ))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        let failover = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Failover)
            .count();
        assert_eq!(failover, 1, "Should detect failover correlation");

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_pagination_offset_limit() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();

        for i in 0..10 {
            handle
                .record_event(make_event(
                    1,
                    &format!("rule_{i}"),
                    "info",
                    "info",
                    now + i * 1000,
                ))
                .await
                .unwrap();
        }

        let query = TimelineQuery {
            limit: 3,
            offset: 0,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let page1 = handle.get_timeline(query).await.unwrap();
        assert_eq!(page1.events.len(), 3);
        assert_eq!(page1.total_count, 10);
        assert!(page1.has_more);

        let query = TimelineQuery {
            limit: 3,
            offset: 3,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let page2 = handle.get_timeline(query).await.unwrap();
        assert_eq!(page2.events.len(), 3);
        assert!(page2.has_more);

        let query = TimelineQuery {
            limit: 3,
            offset: 9,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let page4 = handle.get_timeline(query).await.unwrap();
        assert_eq!(page4.events.len(), 1);
        assert!(!page4.has_more);

        let query = TimelineQuery {
            limit: 3,
            offset: 15,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let beyond = handle.get_timeline(query).await.unwrap();
        assert!(beyond.events.is_empty());
        assert!(!beyond.has_more);

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_filter_by_severity() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();

        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_b", "warning", "warning", now + 1000))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_c", "info", "info", now + 2000))
            .await
            .unwrap();

        let query = TimelineQuery {
            severities: Some(vec!["error".to_string()]),
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 1);
        assert_eq!(timeline.events[0].severity, "error");

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_filter_by_pane_id() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();
        handle
            .upsert_pane(make_timeline_pane(2, now))
            .await
            .unwrap();

        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "error", "error", now + 1000))
            .await
            .unwrap();

        let query = TimelineQuery {
            pane_ids: Some(vec![1]),
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 1);
        assert_eq!(timeline.events[0].pane_info.pane_id, 1);

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_filter_by_time_range() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();

        for i in 0..5 {
            handle
                .record_event(make_event(
                    1,
                    &format!("rule_{i}"),
                    "info",
                    "info",
                    now + i * 60_000,
                ))
                .await
                .unwrap();
        }

        let query = TimelineQuery {
            start: Some(now),
            end: Some(now + 120_000),
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 3);

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_unhandled_only_filter() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();

        let event_id = handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .mark_event_handled(event_id, Some("wf-1".to_string()), "handled")
            .await
            .unwrap();

        handle
            .record_event(make_event(1, "rule_b", "warning", "warning", now + 5000))
            .await
            .unwrap();

        let query = TimelineQuery {
            unhandled_only: true,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 1);
        assert!(timeline.events[0].handled.is_none());

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_events_same_timestamp_handled_gracefully() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();
        handle
            .upsert_pane(make_timeline_pane(2, now))
            .await
            .unwrap();

        for i in 0..3 {
            handle
                .record_event(make_event(
                    (i % 2) as u64 + 1,
                    &format!("rule_{i}"),
                    "error",
                    "error",
                    now,
                ))
                .await
                .unwrap();
        }

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 3);
        let temporal = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Temporal)
            .count();
        assert!(
            temporal > 0,
            "Same-timestamp cross-pane events should correlate"
        );

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_correlation_refs_attached_to_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();
        handle
            .upsert_pane(make_timeline_pane(2, now))
            .await
            .unwrap();

        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "warning", "warning", now + 2000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        let has_refs = timeline
            .events
            .iter()
            .any(|e| !e.correlations.is_empty());
        assert!(
            has_refs,
            "Correlated events should have CorrelationRef attached"
        );

        for event in &timeline.events {
            for cref in &event.correlations {
                assert!(
                    timeline.correlations.iter().any(|c| c.id == cref.id),
                    "Event correlation ref ID should match a top-level correlation"
                );
            }
        }

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_correlations_disabled_returns_empty() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();
        handle
            .upsert_pane(make_timeline_pane(2, now))
            .await
            .unwrap();

        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "error", "error", now + 1000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 2);
        assert!(
            timeline.correlations.is_empty(),
            "Correlations should be empty when disabled"
        );

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_query_performance_many_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        for p in 1..=5 {
            handle
                .upsert_pane(make_timeline_pane(p, now))
                .await
                .unwrap();
        }

        for i in 0..200 {
            let pane = (i % 5) as u64 + 1;
            handle
                .record_event(make_event(
                    pane,
                    &format!("rule_{}", i % 10),
                    "detection",
                    if i % 3 == 0 { "error" } else { "warning" },
                    now + i * 500,
                ))
                .await
                .unwrap();
        }

        let start = std::time::Instant::now();
        let query = TimelineQuery {
            include_correlations: true,
            limit: 100,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(timeline.events.len(), 100);
        assert_eq!(timeline.total_count, 200);
        assert!(timeline.has_more);
        assert!(
            !timeline.correlations.is_empty(),
            "Should find correlations among 200 events"
        );
        assert!(
            elapsed.as_millis() < 500,
            "Timeline query took {}ms, expected <500ms",
            elapsed.as_millis()
        );

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_workflow_group_integration() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();
        handle
            .upsert_pane(make_timeline_pane(2, now))
            .await
            .unwrap();

        let eid1 = handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .mark_event_handled(eid1, Some("wf-test-1".to_string()), "handled")
            .await
            .unwrap();

        let eid2 = handle
            .record_event(make_event(2, "rule_b", "error", "error", now + 5000))
            .await
            .unwrap();
        handle
            .mark_event_handled(eid2, Some("wf-test-1".to_string()), "handled")
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        let workflow = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::WorkflowGroup)
            .collect::<Vec<_>>();
        assert_eq!(workflow.len(), 1, "Should detect workflow group");
        assert_eq!(workflow[0].event_ids.len(), 2);
        assert!(
            (workflow[0].confidence - 0.95).abs() < 0.01,
            "Workflow confidence should be 0.95"
        );

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_serde_roundtrip() {
    // This test is pure sync (no DB), but included for completeness
    let timeline = Timeline {
        start: 1000,
        end: 5000,
        events: vec![TimelineEvent {
            id: 1,
            timestamp: 1000,
            pane_info: PaneInfo {
                pane_id: 1,
                pane_uuid: Some("uuid-1".to_string()),
                agent_type: Some("claude_code".to_string()),
                domain: "local".to_string(),
                cwd: Some("/tmp".to_string()),
                title: Some("test".to_string()),
            },
            rule_id: "rule_a".to_string(),
            event_type: "error".to_string(),
            severity: "error".to_string(),
            confidence: 0.9,
            handled: None,
            correlations: vec![CorrelationRef {
                id: "corr-1".to_string(),
                correlation_type: CorrelationType::Temporal,
            }],
            summary: Some("Test event".to_string()),
        }],
        correlations: vec![Correlation {
            id: "corr-1".to_string(),
            event_ids: vec![1, 2],
            correlation_type: CorrelationType::Temporal,
            confidence: 0.6,
            description: "Test correlation".to_string(),
        }],
        total_count: 1,
        has_more: false,
    };

    let json = serde_json::to_string(&timeline).unwrap();
    let deserialized: Timeline = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.events.len(), 1);
    assert_eq!(deserialized.correlations.len(), 1);
    assert_eq!(deserialized.total_count, 1);
    assert!(!deserialized.has_more);
    assert_eq!(deserialized.events[0].correlations.len(), 1);
    assert_eq!(
        deserialized.correlations[0].correlation_type,
        CorrelationType::Temporal
    );
}

#[test]
fn timeline_dedupe_group_integration() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();
        handle
            .upsert_pane(make_timeline_pane(2, now))
            .await
            .unwrap();
        handle
            .upsert_pane(make_timeline_pane(3, now))
            .await
            .unwrap();

        handle
            .record_event(make_event(
                1,
                "claude_code.usage.reached",
                "usage",
                "warning",
                now,
            ))
            .await
            .unwrap();
        handle
            .record_event(make_event(
                2,
                "claude_code.usage.reached",
                "usage",
                "warning",
                now + 10_000,
            ))
            .await
            .unwrap();
        handle
            .record_event(make_event(
                3,
                "claude_code.usage.reached",
                "usage",
                "warning",
                now + 20_000,
            ))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        let dedupe = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::DedupeGroup)
            .collect::<Vec<_>>();
        assert_eq!(dedupe.len(), 1, "Should detect dedupe group across 3 panes");
        assert_eq!(dedupe[0].event_ids.len(), 3);

        handle.shutdown().await.unwrap();
    });
}

#[test]
fn timeline_events_ordered_chronologically() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ft.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle
            .upsert_pane(make_timeline_pane(1, now))
            .await
            .unwrap();

        handle
            .record_event(make_event(1, "rule_c", "info", "info", now + 5000))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_a", "info", "info", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_b", "info", "info", now + 2000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 3);
        assert!(
            timeline.events[0].timestamp <= timeline.events[1].timestamp
                && timeline.events[1].timestamp <= timeline.events[2].timestamp,
            "Events should be in chronological order"
        );

        handle.shutdown().await.unwrap();
    });
}

// ===========================================================================
// Note: LabRuntime sections (2-5) omitted for storage tests.
//
// The StorageHandle uses `tokio::task::spawn_blocking` for SQLite I/O,
// which requires a tokio reactor. Under the LabRuntime's deterministic
// scheduler, tokio-backed spawn_blocking calls would not resolve properly.
// The RuntimeFixture ports above (Sections 1-4) use the full asupersync
// runtime where tokio interop works correctly.
// ===========================================================================
