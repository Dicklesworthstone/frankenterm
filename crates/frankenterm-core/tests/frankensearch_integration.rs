//! FrankenSearch integration test suite (ft-dr6zv.1.7).
//!
//! Cross-module integration tests validating the full search stack:
//! - Indexing pipeline → SearchIndex → query
//! - RRF fusion correctness with known inputs
//! - Two-tier blending priority ordering
//! - Content-hash deduplication across ingest calls
//! - TTL expiration and size-bound eviction
//! - Pipeline state transitions (pause/resume/stop/restart)
//! - Concurrent RRF fusion safety
//! - Watermark progression across pipeline ticks

use std::collections::HashSet;

use frankenterm_core::search::{
    ContentIndexingPipeline, FusedResult, HybridSearchService, IndexableDocument, IndexingConfig,
    PipelineConfig, PipelineSkipReason, PipelineState, PipelineTickReport,
    ScrollbackLine, SearchDocumentSource, SearchIndex, SearchMode, TwoTierMetrics,
    blend_two_tier, kendall_tau, rrf_fuse,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_index_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

fn test_config(dir: &std::path::Path) -> IndexingConfig {
    IndexingConfig {
        index_dir: dir.to_path_buf(),
        max_index_size_bytes: 10 * 1024 * 1024,
        ttl_days: 30,
        flush_interval_secs: 1,
        flush_docs_threshold: 5,
        max_docs_per_second: 1000,
    }
}

fn make_scrollback_line(text: &str, ts: i64) -> ScrollbackLine {
    ScrollbackLine::new(text, ts)
}

fn make_doc(text: &str, ts: i64, pane_id: Option<u64>) -> IndexableDocument {
    IndexableDocument::text(SearchDocumentSource::Scrollback, text, ts, pane_id, None)
}

fn make_pane_content(
    pane_id: u64,
    lines: Vec<ScrollbackLine>,
) -> (u64, Option<String>, Vec<ScrollbackLine>) {
    (pane_id, None, lines)
}

// ---------------------------------------------------------------------------
// Integration: Full pipeline (index → search → verify)
// ---------------------------------------------------------------------------

#[test]
fn integration_search_full_pipeline() {
    let tmp = temp_index_dir();
    let config = test_config(tmp.path());
    let index = SearchIndex::open(config).unwrap();

    let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);
    assert_eq!(pipeline.state(), PipelineState::Running);

    // Ingest terminal content from two panes.
    let pane0_lines = vec![
        make_scrollback_line("$ cargo build --release", 1000),
        make_scrollback_line("   Compiling frankenterm v0.1.0", 1001),
        make_scrollback_line("   Finished release target(s) in 42.3s", 1002),
    ];
    let pane1_lines = vec![
        make_scrollback_line("error[E0308]: mismatched types", 2000),
        make_scrollback_line("  --> src/main.rs:42:5", 2001),
        make_scrollback_line("help: try wrapping the expression", 2002),
    ];

    let content = vec![
        make_pane_content(0, pane0_lines),
        make_pane_content(1, pane1_lines),
    ];

    let report = pipeline.tick(&content, 3000, false, None);

    // Both panes should have been processed.
    assert_eq!(report.panes_processed, 2);
    assert_eq!(report.panes_skipped, 0);
    assert!(report.skipped_reason.is_none());

    // Watermarks should be set for both panes.
    assert!(pipeline.watermark(0).is_some());
    assert!(pipeline.watermark(1).is_some());
    let wm0 = pipeline.watermark(0).unwrap();
    assert!(wm0.last_indexed_at_ms >= 1000);
    assert!(wm0.total_docs_indexed > 0);

    let wm1 = pipeline.watermark(1).unwrap();
    assert!(wm1.last_indexed_at_ms >= 2000);
    assert!(wm1.total_docs_indexed > 0);

    // Status snapshot should reflect indexed content.
    let status = pipeline.status(3000);
    assert_eq!(status.state, PipelineState::Running);
    assert!(status.total_docs_indexed > 0);
    assert!(status.total_lines_consumed > 0);
}

// ---------------------------------------------------------------------------
// Integration: Watermark progression across ticks
// ---------------------------------------------------------------------------

#[test]
fn integration_pipeline_watermark_progression() {
    let tmp = temp_index_dir();
    let config = test_config(tmp.path());
    let index = SearchIndex::open(config).unwrap();
    let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

    // Tick 1: initial content.
    let content1 = vec![make_pane_content(
        0,
        vec![
            make_scrollback_line("line A", 100),
            make_scrollback_line("line B", 200),
        ],
    )];
    let r1 = pipeline.tick(&content1, 300, false, None);
    assert_eq!(r1.panes_processed, 1);
    let wm_after_t1 = pipeline.watermark(0).unwrap().last_indexed_at_ms;

    // Tick 2: same content should be skipped.
    let r2 = pipeline.tick(&content1, 400, false, None);
    assert_eq!(r2.panes_skipped, 1);
    assert_eq!(r2.panes_processed, 0);

    // Tick 3: new content above watermark.
    let content3 = vec![make_pane_content(
        0,
        vec![
            make_scrollback_line("line A", 100), // old, should be skipped
            make_scrollback_line("line B", 200), // old, should be skipped
            make_scrollback_line("line C", 500), // new
        ],
    )];
    let r3 = pipeline.tick(&content3, 600, false, None);
    assert_eq!(r3.panes_processed, 1);
    let wm_after_t3 = pipeline.watermark(0).unwrap().last_indexed_at_ms;
    assert!(wm_after_t3 > wm_after_t1, "watermark should advance");
}

// ---------------------------------------------------------------------------
// Integration: RRF fusion correctness
// ---------------------------------------------------------------------------

#[test]
fn integration_rrf_fusion_correctness_known_inputs() {
    // Known lexical and semantic rankings.
    let lexical = vec![(1u64, 0.9f32), (2, 0.7), (3, 0.5), (4, 0.3)];
    let semantic = vec![(3u64, 0.95f32), (1, 0.8), (5, 0.6), (2, 0.4)];

    let results = rrf_fuse(&lexical, &semantic, 60);

    // All unique IDs should appear.
    let ids: HashSet<u64> = results.iter().map(|r| r.id).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
    assert!(ids.contains(&4));
    assert!(ids.contains(&5));
    assert_eq!(ids.len(), 5);

    // Results should be sorted by descending score.
    for window in results.windows(2) {
        assert!(
            window[0].score >= window[1].score,
            "results must be sorted by descending score: {} >= {}",
            window[0].score,
            window[1].score
        );
    }

    // Items in both lists should have higher RRF scores than items in only one.
    let in_both: Vec<&FusedResult> = results
        .iter()
        .filter(|r| r.lexical_rank.is_some() && r.semantic_rank.is_some())
        .collect();
    let in_one: Vec<&FusedResult> = results
        .iter()
        .filter(|r| r.lexical_rank.is_some() != r.semantic_rank.is_some())
        .collect();

    if !in_both.is_empty() && !in_one.is_empty() {
        let max_single = in_one.iter().map(|r| r.score).fold(0.0f32, f32::max);
        // With equal weights, items in both lists typically outscore single-list items.
        let max_dual = in_both.iter().map(|r| r.score).fold(0.0f32, f32::max);
        assert!(
            max_dual > max_single,
            "top dual-list item ({max_dual}) should outscore top single-list item ({max_single})"
        );
    }

    // Verify rank tracking.
    let item1 = results.iter().find(|r| r.id == 1).unwrap();
    assert_eq!(item1.lexical_rank, Some(0)); // first in lexical
    assert_eq!(item1.semantic_rank, Some(1)); // second in semantic
}

#[test]
fn integration_rrf_fusion_empty_lists() {
    let empty: Vec<(u64, f32)> = vec![];
    let results = rrf_fuse(&empty, &empty, 60);
    assert!(results.is_empty());

    let some = vec![(1u64, 0.5f32)];
    let results = rrf_fuse(&some, &empty, 60);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, 1);
    assert!(results[0].lexical_rank.is_some());
    assert!(results[0].semantic_rank.is_none());
}

#[test]
fn integration_rrf_fusion_duplicate_ids() {
    // Same ID appears multiple times in one list.
    let lexical = vec![(1u64, 0.9f32), (1, 0.8), (2, 0.7)];
    let semantic = vec![(2u64, 0.5f32)];

    let results = rrf_fuse(&lexical, &semantic, 60);
    let ids: Vec<u64> = results.iter().map(|r| r.id).collect();
    // Should deduplicate within the list.
    let unique_ids: HashSet<u64> = ids.iter().copied().collect();
    assert_eq!(unique_ids.len(), ids.len(), "results should have unique IDs");
}

// ---------------------------------------------------------------------------
// Integration: Two-tier blending priority
// ---------------------------------------------------------------------------

#[test]
fn integration_two_tier_blending_priority() {
    let tier1 = vec![
        FusedResult {
            id: 10,
            score: 0.9,
            lexical_rank: Some(0),
            semantic_rank: None,
        },
        FusedResult {
            id: 20,
            score: 0.8,
            lexical_rank: Some(1),
            semantic_rank: None,
        },
    ];

    let tier2 = vec![
        FusedResult {
            id: 30,
            score: 0.95,
            lexical_rank: None,
            semantic_rank: Some(0),
        },
        FusedResult {
            id: 40,
            score: 0.85,
            lexical_rank: None,
            semantic_rank: Some(1),
        },
    ];

    let (results, metrics) = blend_two_tier(&tier1, &tier2, 4, 1.0);

    // With alpha=1.0, tier1 items should appear first.
    assert_eq!(results.len(), 4);
    assert_eq!(results[0].id, 10, "tier1 first item should be first");
    assert_eq!(results[1].id, 20, "tier1 second item should be second");
    assert_eq!(metrics.tier1_count, 2);
    assert_eq!(metrics.tier2_count, 2);
    assert_eq!(metrics.overlap_count, 0);
}

#[test]
fn integration_two_tier_blending_with_overlap() {
    let tier1 = vec![
        FusedResult {
            id: 1,
            score: 0.9,
            lexical_rank: Some(0),
            semantic_rank: Some(0),
        },
        FusedResult {
            id: 2,
            score: 0.8,
            lexical_rank: Some(1),
            semantic_rank: None,
        },
    ];

    let tier2 = vec![
        FusedResult {
            id: 1,
            score: 0.95,
            lexical_rank: None,
            semantic_rank: Some(0),
        },
        FusedResult {
            id: 3,
            score: 0.7,
            lexical_rank: None,
            semantic_rank: Some(1),
        },
    ];

    let (results, metrics) = blend_two_tier(&tier1, &tier2, 3, 0.8);

    // ID 1 appears in both tiers — should appear only once.
    let id_counts: Vec<u64> = results.iter().map(|r| r.id).collect();
    let unique: HashSet<u64> = id_counts.iter().copied().collect();
    assert_eq!(unique.len(), id_counts.len(), "no duplicate IDs in results");
    assert_eq!(metrics.overlap_count, 1);
}

#[test]
fn integration_two_tier_alpha_zero() {
    let tier1 = vec![FusedResult {
        id: 1,
        score: 0.9,
        lexical_rank: Some(0),
        semantic_rank: None,
    }];
    let tier2 = vec![FusedResult {
        id: 2,
        score: 0.8,
        lexical_rank: None,
        semantic_rank: Some(0),
    }];

    let (results, _) = blend_two_tier(&tier1, &tier2, 2, 0.0);
    // alpha=0 means tier1 scores multiplied by 0, tier2 by 1.
    assert_eq!(results.len(), 2);
    let item1 = results.iter().find(|r| r.id == 1).unwrap();
    assert!((item1.score - 0.0).abs() < f32::EPSILON, "tier1 score should be ~0 with alpha=0");
}

// ---------------------------------------------------------------------------
// Integration: Content hash deduplication
// ---------------------------------------------------------------------------

#[test]
fn integration_index_dedup_across_ingests() {
    let tmp = temp_index_dir();
    let config = test_config(tmp.path());
    let mut index = SearchIndex::open(config).unwrap();

    let doc1 = make_doc("Hello world from pane 0", 1000, Some(0));
    let doc2 = make_doc("Different content", 1001, Some(0));

    // First ingest: both accepted.
    let r1 = index.ingest_documents(&[doc1.clone(), doc2.clone()], 2000, false, None).unwrap();
    assert_eq!(r1.accepted_docs, 2);
    assert_eq!(r1.skipped_duplicate_docs, 0);

    // Second ingest with same content: both should be deduped.
    let r2 = index.ingest_documents(&[doc1, doc2], 3000, false, None).unwrap();
    assert_eq!(r2.accepted_docs, 0);
    assert_eq!(r2.skipped_duplicate_docs, 2);
}

#[test]
fn integration_index_dedup_with_cass() {
    let tmp = temp_index_dir();
    let config = test_config(tmp.path());
    let mut index = SearchIndex::open(config).unwrap();

    // Simulate cass having already indexed a document.
    let mut cass_hashes: HashSet<String> = HashSet::new();

    // We need to compute the hash the same way the index does. Use a known string.
    // Since we can't easily replicate the exact hash, test with empty cass set first.
    let doc = make_doc("Unique content not in cass", 1000, Some(0));
    let r1 = index.ingest_documents(std::slice::from_ref(&doc), 2000, false, Some(&cass_hashes)).unwrap();
    assert_eq!(r1.accepted_docs, 1);
    assert_eq!(r1.skipped_cass_docs, 0);

    // Now add a hash to cass (won't match our doc since hashes are implementation-specific).
    cass_hashes.insert("not_a_matching_hash".to_string());
    let doc2 = make_doc("Another unique doc", 1001, Some(0));
    let r2 = index.ingest_documents(&[doc2], 3000, false, Some(&cass_hashes)).unwrap();
    assert_eq!(r2.accepted_docs, 1); // Different hash, should be accepted.
}

// ---------------------------------------------------------------------------
// Integration: TTL expiration
// ---------------------------------------------------------------------------

#[test]
fn integration_index_ttl_expiration() {
    let tmp = temp_index_dir();
    let config = IndexingConfig {
        index_dir: tmp.path().to_path_buf(),
        max_index_size_bytes: 10 * 1024 * 1024,
        ttl_days: 1, // 1 day TTL
        flush_interval_secs: 1,
        flush_docs_threshold: 1, // flush every doc
        max_docs_per_second: 1000,
    };
    let mut index = SearchIndex::open(config).unwrap();

    let one_day_ms: i64 = 86_400_000;

    // Insert a doc at time 1000ms (within first rate window).
    let old_doc = make_doc("Old content from yesterday", 1000, Some(0));
    let r = index
        .ingest_documents(&[old_doc], 1000, false, None)
        .unwrap();
    assert_eq!(r.accepted_docs, 1);

    // Tick to flush.
    let _ = index.tick(2000, false);

    // Insert a fresh doc at time = 2 days later.
    let new_doc = make_doc("Fresh content from today", 2 * one_day_ms, Some(0));
    let r2 = index
        .ingest_documents(&[new_doc], 2 * one_day_ms, false, None)
        .unwrap();
    assert_eq!(r2.accepted_docs, 1);

    // Tick at 2 days to trigger TTL cleanup.
    let tick = index.tick(2 * one_day_ms + 2000, false).unwrap();
    // The old doc should have been expired during flush/tick.
    assert!(
        tick.expired_docs > 0 || r2.expired_docs > 0,
        "old doc should expire: tick.expired={}, ingest.expired={}",
        tick.expired_docs,
        r2.expired_docs
    );
}

// ---------------------------------------------------------------------------
// Integration: Index size limit eviction
// ---------------------------------------------------------------------------

#[test]
fn integration_index_size_limit() {
    let tmp = temp_index_dir();
    let config = IndexingConfig {
        index_dir: tmp.path().to_path_buf(),
        max_index_size_bytes: 500, // Very small: ~500 bytes
        ttl_days: 365,
        flush_interval_secs: 1,
        flush_docs_threshold: 1,
        max_docs_per_second: 1000,
    };
    let mut index = SearchIndex::open(config).unwrap();

    // Insert many large-ish documents to exceed the size limit.
    let mut total_accepted = 0usize;
    let mut total_evicted = 0usize;
    for i in 0..50 {
        let text = format!("Document number {i} with some padding content to take up space in the index");
        let doc = make_doc(&text, i * 1000 + 1000, Some(0));
        let r = index
            .ingest_documents(&[doc], i * 1000 + 1000, false, None)
            .unwrap();
        total_accepted += r.accepted_docs;
        total_evicted += r.evicted_docs;
        // Tick to flush after each insert.
        let tick = index.tick(i * 1000 + 1500, false).unwrap();
        total_evicted += tick.evicted_docs;
    }

    // Some documents should have been accepted.
    assert!(total_accepted > 0, "should have accepted some docs");
    // With a 500 byte limit and many docs, eviction should have occurred.
    assert!(
        total_evicted > 0,
        "should have evicted some docs to stay within size budget (accepted={total_accepted}, evicted={total_evicted})"
    );
}

// ---------------------------------------------------------------------------
// Integration: Pipeline state transitions
// ---------------------------------------------------------------------------

#[test]
fn integration_pipeline_pause_resume() {
    let tmp = temp_index_dir();
    let config = test_config(tmp.path());
    let index = SearchIndex::open(config).unwrap();
    let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

    let content = vec![make_pane_content(
        0,
        vec![make_scrollback_line("test line", 100)],
    )];

    // Running → tick processes content.
    assert_eq!(pipeline.state(), PipelineState::Running);
    let r = pipeline.tick(&content, 200, false, None);
    assert_eq!(r.panes_processed, 1);
    assert!(r.skipped_reason.is_none());

    // Pause → tick skipped.
    pipeline.pause();
    assert_eq!(pipeline.state(), PipelineState::Paused);
    let r = pipeline.tick(&content, 300, false, None);
    assert_eq!(r.skipped_reason, Some(PipelineSkipReason::Paused));
    assert_eq!(r.panes_processed, 0);

    // Resume → tick processes again.
    pipeline.resume();
    assert_eq!(pipeline.state(), PipelineState::Running);

    // Stop → tick skipped.
    pipeline.stop();
    assert_eq!(pipeline.state(), PipelineState::Stopped);
    let r = pipeline.tick(&content, 400, false, None);
    assert_eq!(r.skipped_reason, Some(PipelineSkipReason::Stopped));

    // Restart → processing again.
    pipeline.restart();
    assert_eq!(pipeline.state(), PipelineState::Running);
}

#[test]
fn integration_pipeline_resize_storm_pause() {
    let tmp = temp_index_dir();
    let config = test_config(tmp.path());
    let index = SearchIndex::open(config).unwrap();
    let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

    let content = vec![make_pane_content(
        0,
        vec![make_scrollback_line("storm line", 100)],
    )];

    // Resize storm active → tick skipped.
    let r = pipeline.tick(&content, 200, true, None);
    assert_eq!(r.skipped_reason, Some(PipelineSkipReason::ResizeStorm));
    assert_eq!(r.panes_processed, 0);

    // Storm subsides → processes normally.
    let r = pipeline.tick(&content, 300, false, None);
    assert_eq!(r.panes_processed, 1);
}

#[test]
fn integration_pipeline_empty_panes() {
    let tmp = temp_index_dir();
    let config = test_config(tmp.path());
    let index = SearchIndex::open(config).unwrap();
    let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

    let r = pipeline.tick(&[], 100, false, None);
    assert_eq!(r.skipped_reason, Some(PipelineSkipReason::NoPanes));
}

// ---------------------------------------------------------------------------
// Integration: Concurrent RRF fusion safety
// ---------------------------------------------------------------------------

#[test]
fn integration_concurrent_rrf_fusion() {
    use std::sync::Arc;
    use std::thread;

    let lexical = Arc::new(vec![
        (1u64, 0.9f32),
        (2, 0.7),
        (3, 0.5),
        (4, 0.3),
        (5, 0.1),
    ]);
    let semantic = Arc::new(vec![
        (3u64, 0.95f32),
        (1, 0.8),
        (5, 0.6),
        (2, 0.4),
        (6, 0.2),
    ]);

    #[allow(clippy::needless_collect)] // collect is needed to spawn all threads before joining
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let lex = Arc::clone(&lexical);
            let sem = Arc::clone(&semantic);
            thread::spawn(move || rrf_fuse(&lex, &sem, 60))
        })
        .collect();

    let results: Vec<Vec<FusedResult>> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // All 10 threads should produce identical results.
    let reference = &results[0];
    for (i, result) in results.iter().enumerate().skip(1) {
        assert_eq!(
            reference.len(),
            result.len(),
            "thread {i} result count differs"
        );
        for (j, (a, b)) in reference.iter().zip(result.iter()).enumerate() {
            assert_eq!(a.id, b.id, "thread {i} item {j} id differs");
            assert!(
                (a.score - b.score).abs() < f32::EPSILON,
                "thread {i} item {j} score differs: {} vs {}",
                a.score,
                b.score
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Integration: Kendall Tau correctness
// ---------------------------------------------------------------------------

#[test]
fn integration_kendall_tau_identical_rankings() {
    let ranking = vec![1u64, 2, 3, 4, 5];
    let tau = kendall_tau(&ranking, &ranking);
    assert!(
        (tau - 1.0).abs() < f32::EPSILON,
        "identical rankings should have tau=1.0, got {tau}"
    );
}

#[test]
fn integration_kendall_tau_reversed_rankings() {
    let ranking_a = vec![1u64, 2, 3, 4, 5];
    let ranking_b = vec![5u64, 4, 3, 2, 1];
    let tau = kendall_tau(&ranking_a, &ranking_b);
    assert!(
        (tau - (-1.0)).abs() < f32::EPSILON,
        "reversed rankings should have tau=-1.0, got {tau}"
    );
}

#[test]
fn integration_kendall_tau_partial_overlap() {
    let ranking_a = vec![1u64, 2, 3];
    let ranking_b = vec![2u64, 3, 4]; // shares 2,3 with a
    let tau = kendall_tau(&ranking_a, &ranking_b);
    // With only 2 common items in same order: tau should be 1.0.
    assert!(
        (tau - 1.0).abs() < f32::EPSILON,
        "partial overlap same order: tau={tau}"
    );
}

#[test]
fn integration_kendall_tau_no_overlap() {
    let ranking_a = vec![1u64, 2, 3];
    let ranking_b = vec![4u64, 5, 6];
    let tau = kendall_tau(&ranking_a, &ranking_b);
    assert!(
        tau.abs() < f32::EPSILON,
        "no overlap should have tau=0.0, got {tau}"
    );
}

// ---------------------------------------------------------------------------
// Integration: HybridSearchService builder API
// ---------------------------------------------------------------------------

#[test]
fn integration_hybrid_search_service_defaults() {
    let svc = HybridSearchService::new();
    // Default mode should be Hybrid.
    assert_eq!(svc.mode(), SearchMode::Hybrid);
    assert_eq!(svc.rrf_k(), 60);
}

#[test]
fn integration_hybrid_search_service_mode_override() {
    let svc = HybridSearchService::new().with_mode(SearchMode::Lexical);
    assert_eq!(svc.mode(), SearchMode::Lexical);

    let svc = HybridSearchService::new().with_mode(SearchMode::Semantic);
    assert_eq!(svc.mode(), SearchMode::Semantic);
}

#[test]
fn integration_hybrid_search_service_rrf_k() {
    let svc = HybridSearchService::new().with_rrf_k(100);
    assert_eq!(svc.rrf_k(), 100);
}

// ---------------------------------------------------------------------------
// Integration: Pipeline tick report aggregation
// ---------------------------------------------------------------------------

#[test]
fn integration_pipeline_multi_pane_report() {
    let tmp = temp_index_dir();
    let config = test_config(tmp.path());
    let index = SearchIndex::open(config).unwrap();
    let mut pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

    let content = vec![
        make_pane_content(
            0,
            vec![
                make_scrollback_line("pane 0 line 1", 100),
                make_scrollback_line("pane 0 line 2", 200),
            ],
        ),
        make_pane_content(
            1,
            vec![make_scrollback_line("pane 1 line 1", 150)],
        ),
        make_pane_content(2, vec![]), // empty pane
    ];

    let report = pipeline.tick(&content, 300, false, None);

    // 2 panes with content processed, 1 with empty content skipped.
    assert_eq!(report.panes_processed, 2);
    assert_eq!(report.panes_skipped, 1); // empty pane
    assert!(report.total_lines_consumed > 0);
    assert!(report.ingest_report.submitted_docs > 0);
}

// ---------------------------------------------------------------------------
// Integration: Index persistence across open/close
// ---------------------------------------------------------------------------

#[test]
fn integration_index_persistence() {
    let tmp = temp_index_dir();

    // Open, ingest, flush.
    {
        let config = IndexingConfig {
            index_dir: tmp.path().to_path_buf(),
            max_index_size_bytes: 10 * 1024 * 1024,
            ttl_days: 30,
            flush_interval_secs: 1,
            flush_docs_threshold: 1,
            max_docs_per_second: 1000,
        };
        let mut index = SearchIndex::open(config).unwrap();
        let doc = make_doc("Persistent content", 1000, Some(0));
        let r = index.ingest_documents(&[doc], 1000, false, None).unwrap();
        assert_eq!(r.accepted_docs, 1);
        // Trigger flush.
        let _ = index.tick(2000, false);
    }

    // Reopen — docs should still be there.
    {
        let config = IndexingConfig {
            index_dir: tmp.path().to_path_buf(),
            max_index_size_bytes: 10 * 1024 * 1024,
            ttl_days: 30,
            flush_interval_secs: 1,
            flush_docs_threshold: 1,
            max_docs_per_second: 1000,
        };
        let index = SearchIndex::open(config).unwrap();
        assert!(
            !index.documents().is_empty(),
            "documents should survive reopen"
        );
        assert!(
            index
                .documents()
                .iter()
                .any(|d| d.text.contains("Persistent")),
            "should find persistent content"
        );
    }
}

// ---------------------------------------------------------------------------
// Integration: Rate limiting
// ---------------------------------------------------------------------------

#[test]
fn integration_index_rate_limiting() {
    let tmp = temp_index_dir();
    let config = IndexingConfig {
        index_dir: tmp.path().to_path_buf(),
        max_index_size_bytes: 10 * 1024 * 1024,
        ttl_days: 30,
        flush_interval_secs: 5,
        flush_docs_threshold: 100,
        max_docs_per_second: 3, // very low rate limit
    };
    let mut index = SearchIndex::open(config).unwrap();

    let docs: Vec<IndexableDocument> = (0..10)
        .map(|i| make_doc(&format!("Rate limited doc {i}"), 1000 + i as i64, Some(0)))
        .collect();

    // All docs within a 1-second window — should hit the rate limit.
    // Use now_ms=1000 so the rate window starts at 1000 and all 10 docs are in the same window.
    let r = index.ingest_documents(&docs, 1000, false, None).unwrap();
    assert_eq!(r.submitted_docs, 10);
    // Due to rate limiting (max 3/sec), only 3 should be accepted.
    assert!(
        r.deferred_rate_limited_docs > 0,
        "some docs should be rate limited (accepted={}, deferred={})",
        r.accepted_docs,
        r.deferred_rate_limited_docs
    );
    assert_eq!(
        r.accepted_docs, 3,
        "only max_docs_per_second should be accepted"
    );
}

// ---------------------------------------------------------------------------
// Integration: Structured logging format verification
// ---------------------------------------------------------------------------

#[test]
fn integration_pipeline_tick_report_serde() {
    let report = PipelineTickReport {
        panes_processed: 3,
        panes_skipped: 1,
        panes_truncated: 0,
        total_lines_consumed: 42,
        ingest_report: Default::default(),
        skipped_reason: None,
    };

    let json = serde_json::to_string(&report).unwrap();
    let parsed: PipelineTickReport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.panes_processed, 3);
    assert_eq!(parsed.total_lines_consumed, 42);
}

#[test]
fn integration_pipeline_status_serde() {
    let tmp = temp_index_dir();
    let config = test_config(tmp.path());
    let index = SearchIndex::open(config).unwrap();
    let pipeline = ContentIndexingPipeline::new(PipelineConfig::default(), index);

    let status = pipeline.status(1000);
    let json = serde_json::to_string(&status).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert!(parsed["state"].is_string());
    assert!(parsed["total_ticks"].is_number());
    assert!(parsed["total_docs_indexed"].is_number());
    assert!(parsed["index_stats"].is_object());
}

#[test]
fn integration_two_tier_metrics_serde() {
    let metrics = TwoTierMetrics {
        tier1_count: 5,
        tier2_count: 3,
        overlap_count: 2,
        rank_correlation: 0.75,
    };
    let json = serde_json::to_string(&metrics).unwrap();
    let parsed: TwoTierMetrics = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.tier1_count, 5);
    assert_eq!(parsed.tier2_count, 3);
    assert_eq!(parsed.overlap_count, 2);
    assert!((parsed.rank_correlation - 0.75).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// Integration: Pipeline config limits
// ---------------------------------------------------------------------------

#[test]
fn integration_pipeline_max_panes_per_tick() {
    let tmp = temp_index_dir();
    let config = test_config(tmp.path());
    let index = SearchIndex::open(config).unwrap();

    let mut pc = PipelineConfig::default();
    pc.max_panes_per_tick = 2; // Limit to 2 panes per tick.
    let mut pipeline = ContentIndexingPipeline::new(pc, index);

    let content: Vec<_> = (0..5)
        .map(|i| {
            make_pane_content(
                i,
                vec![make_scrollback_line(&format!("pane {i} content"), i as i64 * 100 + 100)],
            )
        })
        .collect();

    let report = pipeline.tick(&content, 1000, false, None);
    // Only 2 panes should be processed due to the limit.
    assert_eq!(report.panes_processed + report.panes_skipped, 2);
}

#[test]
fn integration_pipeline_max_lines_per_pane_tick() {
    let tmp = temp_index_dir();
    let config = test_config(tmp.path());
    let index = SearchIndex::open(config).unwrap();

    let mut pc = PipelineConfig::default();
    pc.max_lines_per_pane_tick = 2; // Limit to 2 lines per pane per tick.
    let mut pipeline = ContentIndexingPipeline::new(pc, index);

    let content = vec![make_pane_content(
        0,
        vec![
            make_scrollback_line("line 1", 100),
            make_scrollback_line("line 2", 200),
            make_scrollback_line("line 3", 300),
            make_scrollback_line("line 4", 400),
        ],
    )];

    let report = pipeline.tick(&content, 500, false, None);
    assert_eq!(report.panes_processed, 1);
    assert_eq!(report.panes_truncated, 1, "should report truncation");
}
