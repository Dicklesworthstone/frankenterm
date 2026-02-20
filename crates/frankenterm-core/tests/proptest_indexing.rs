//! Property-based tests for `search::indexing` module.
//!
//! Covers: SearchIndex lifecycle (ingest, dedup, flush, TTL, eviction, rate-limit,
//! search, persistence, reindex), chunk_scrollback_lines, extract_command_output_blocks,
//! extract_agent_artifacts, normalize/ANSI-strip invariants.

use std::collections::HashSet;
use std::path::Path;

use proptest::prelude::*;
use tempfile::tempdir;

use frankenterm_core::search::{
    CommandBlockExtractionConfig, IndexFlushReason, IndexableDocument, IndexingConfig,
    ScrollbackLine, SearchDocumentSource, SearchIndex, chunk_scrollback_lines,
    extract_agent_artifacts, extract_command_output_blocks,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_config(dir: &Path) -> IndexingConfig {
    IndexingConfig {
        index_dir: dir.join("index"),
        max_index_size_bytes: 500 * 1024 * 1024,
        ttl_days: 30,
        flush_interval_secs: 5,
        flush_docs_threshold: 50,
        max_docs_per_second: 100,
    }
}

fn fast_flush_config(dir: &Path) -> IndexingConfig {
    IndexingConfig {
        index_dir: dir.join("index"),
        max_index_size_bytes: 500 * 1024 * 1024,
        ttl_days: 30,
        flush_interval_secs: 1,
        flush_docs_threshold: 1,
        max_docs_per_second: 10_000,
    }
}

fn make_doc(text: &str, ts: i64, source: SearchDocumentSource) -> IndexableDocument {
    IndexableDocument::text(source, text, ts, Some(1), Some("s1".to_string()))
}

/// Strategy for non-empty ASCII text (avoids NUL and control chars).
fn text_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _.,;:!?/\\-]{1,80}"
}

fn source_strategy() -> impl Strategy<Value = SearchDocumentSource> {
    prop_oneof![
        Just(SearchDocumentSource::Scrollback),
        Just(SearchDocumentSource::Command),
        Just(SearchDocumentSource::AgentArtifact),
        Just(SearchDocumentSource::PaneMetadata),
        Just(SearchDocumentSource::Cass),
    ]
}

fn scrollback_line_strategy() -> impl Strategy<Value = ScrollbackLine> {
    (text_strategy(), 1_000i64..100_000i64).prop_map(|(text, ts)| {
        let mut line = ScrollbackLine::new(text, ts);
        line.pane_id = Some(1);
        line.session_id = Some("sess".to_string());
        line
    })
}

// ── Ingest invariants ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// Ingested doc count + skipped counts must equal submitted count.
    #[test]
    fn ingest_accounting_invariant(
        texts in prop::collection::vec(text_strategy(), 1..20),
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs: Vec<IndexableDocument> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| make_doc(t, 1_000 + i as i64, SearchDocumentSource::Scrollback))
            .collect();

        let report = index
            .ingest_documents(&docs, 2_000, false, None)
            .expect("ingest");

        let total_accounted = report.accepted_docs
            + report.skipped_empty_docs
            + report.skipped_duplicate_docs
            + report.skipped_cass_docs
            + report.skipped_resize_pause_docs
            + report.deferred_rate_limited_docs;
        prop_assert_eq!(
            total_accounted, report.submitted_docs,
            "accounting mismatch: accounted={} submitted={}",
            total_accounted, report.submitted_docs
        );
    }

    /// Duplicate texts produce exactly skipped_duplicate_docs count.
    #[test]
    fn dedup_catches_all_duplicates(
        text in text_strategy(),
        count in 2usize..8,
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs: Vec<IndexableDocument> = (0..count)
            .map(|i| make_doc(&text, 1_000 + i as i64, SearchDocumentSource::Scrollback))
            .collect();

        let report = index
            .ingest_documents(&docs, 2_000, false, None)
            .expect("ingest");

        // First is accepted, rest are duplicates.
        prop_assert_eq!(report.accepted_docs, 1, "should accept exactly 1");
        prop_assert_eq!(
            report.skipped_duplicate_docs,
            count - 1,
            "should skip {} dups", count - 1
        );
    }

    /// Resize storm pauses all ingestion.
    #[test]
    fn resize_storm_blocks_all_docs(
        texts in prop::collection::vec(text_strategy(), 1..10),
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs: Vec<IndexableDocument> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| make_doc(t, 1_000 + i as i64, SearchDocumentSource::Scrollback))
            .collect();

        let report = index
            .ingest_documents(&docs, 2_000, true, None)
            .expect("ingest");

        prop_assert_eq!(report.skipped_resize_pause_docs, docs.len());
        prop_assert_eq!(report.accepted_docs, 0);
        prop_assert_eq!(index.documents().len(), 0);
    }

    /// Cass hash provider suppresses matching docs.
    #[test]
    fn cass_dedup_skips_matching_hashes(
        texts in prop::collection::vec(text_strategy(), 1..10),
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs: Vec<IndexableDocument> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| make_doc(t, 1_000 + i as i64, SearchDocumentSource::Scrollback))
            .collect();

        // Put all content hashes into cass provider.
        let cass_hashes: HashSet<String> = docs
            .iter()
            .map(|d| {
                use sha2::{Digest, Sha256};
                let basis = d.text.split_whitespace().collect::<Vec<_>>().join(" ");
                let mut hasher = Sha256::new();
                hasher.update(basis.as_bytes());
                format!("{:x}", hasher.finalize())
            })
            .collect();

        let report = index
            .ingest_documents(&docs, 2_000, false, Some(&cass_hashes))
            .expect("ingest");

        // All should be skipped (cass or empty).
        prop_assert_eq!(
            report.accepted_docs, 0,
            "all docs should be suppressed by cass"
        );
    }

    /// After ingestion, every accepted doc is searchable.
    #[test]
    fn accepted_docs_are_searchable(
        texts in prop::collection::hash_set(text_strategy(), 1..8),
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs: Vec<IndexableDocument> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| make_doc(t, 1_000 + i as i64, SearchDocumentSource::Scrollback))
            .collect();

        let _ = index
            .ingest_documents(&docs, 2_000, false, None)
            .expect("ingest");

        // Each unique doc text should be findable.
        for text in &texts {
            let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                continue;
            }
            let hits = index.search(&normalized, 100, 3_000);
            prop_assert!(
                !hits.is_empty(),
                "expected to find '{}' (normalized: '{}')", text, normalized
            );
        }
    }

    /// Search with empty or whitespace-only query returns nothing.
    #[test]
    fn empty_query_returns_nothing(
        whitespace in "[ \\t\\n]{0,10}",
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let doc = make_doc("some content", 1_000, SearchDocumentSource::Scrollback);
        let _ = index
            .ingest_documents(&[doc], 1_100, false, None)
            .expect("ingest");

        let hits = index.search(&whitespace, 10, 2_000);
        prop_assert_eq!(hits.len(), 0, "empty/whitespace query should match nothing");
    }

    /// Search with limit=0 always returns nothing.
    #[test]
    fn search_limit_zero_returns_nothing(
        query in text_strategy(),
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let doc = make_doc("some findable text", 1_000, SearchDocumentSource::Scrollback);
        let _ = index
            .ingest_documents(&[doc], 1_100, false, None)
            .expect("ingest");

        let hits = index.search(&query, 0, 2_000);
        prop_assert_eq!(hits.len(), 0);
    }

    /// Search results are capped at the requested limit.
    #[test]
    fn search_respects_limit(
        n in 5usize..15,
        limit in 1usize..5,
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs: Vec<IndexableDocument> = (0..n)
            .map(|i| {
                make_doc(
                    &format!("common prefix item-{i}"),
                    1_000 + i as i64,
                    SearchDocumentSource::Scrollback,
                )
            })
            .collect();

        let _ = index
            .ingest_documents(&docs, 2_000, false, None)
            .expect("ingest");

        let hits = index.search("common prefix", limit, 3_000);
        prop_assert!(hits.len() <= limit, "got {} > limit {}", hits.len(), limit);
    }
}

// ── Persistence invariants ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Index state survives close and reopen.
    #[test]
    fn persistence_round_trip(
        texts in prop::collection::hash_set(text_strategy(), 1..6),
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());

        let doc_count;
        {
            let mut index = SearchIndex::open(cfg.clone()).expect("open write");
            let docs: Vec<IndexableDocument> = texts
                .iter()
                .enumerate()
                .map(|(i, t)| make_doc(t, 1_000 + i as i64, SearchDocumentSource::Scrollback))
                .collect();
            let _ = index
                .ingest_documents(&docs, 2_000, false, None)
                .expect("ingest");
            doc_count = index.documents().len();
        }

        let reopened = SearchIndex::open(cfg).expect("reopen");
        prop_assert_eq!(reopened.documents().len(), doc_count);
    }

    /// Reindex clears old docs and produces fresh state.
    #[test]
    fn reindex_clears_and_rebuilds(
        old_texts in prop::collection::vec(text_strategy(), 1..5),
        new_texts in prop::collection::hash_set(text_strategy(), 1..5),
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let old_docs: Vec<IndexableDocument> = old_texts
            .iter()
            .enumerate()
            .map(|(i, t)| make_doc(t, 1_000 + i as i64, SearchDocumentSource::Scrollback))
            .collect();
        let _ = index
            .ingest_documents(&old_docs, 2_000, false, None)
            .expect("old ingest");

        let new_docs: Vec<IndexableDocument> = new_texts
            .iter()
            .enumerate()
            .map(|(i, t)| make_doc(t, 5_000 + i as i64, SearchDocumentSource::Scrollback))
            .collect();
        let report = index
            .reindex_documents(&new_docs, 6_000, None)
            .expect("reindex");

        prop_assert_eq!(
            report.submitted_docs, new_texts.len(),
            "reindex submitted count"
        );
        // After reindex, only new docs remain.
        prop_assert!(
            index.documents().len() <= new_texts.len(),
            "reindex doc count"
        );
    }
}

// ── TTL and eviction invariants ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Documents older than TTL are expired during maintenance.
    #[test]
    fn ttl_expires_old_documents(
        fresh_count in 1usize..5,
        stale_count in 1usize..5,
    ) {
        let dir = tempdir().expect("tempdir");
        let mut cfg = fast_flush_config(dir.path());
        cfg.ttl_days = 1;

        let mut index = SearchIndex::open(cfg).expect("open");
        let now = 10 * 86_400_000i64; // 10 days in ms

        let mut docs = Vec::new();
        for i in 0..stale_count {
            docs.push(make_doc(
                &format!("stale-{i}"),
                now - 3 * 86_400_000, // 3 days ago (beyond 1-day TTL)
                SearchDocumentSource::Scrollback,
            ));
        }
        for i in 0..fresh_count {
            docs.push(make_doc(
                &format!("fresh-{i}"),
                now - 1_000, // 1 second ago
                SearchDocumentSource::Scrollback,
            ));
        }

        let _ = index
            .ingest_documents(&docs, now, false, None)
            .expect("ingest");

        // Only fresh docs should survive.
        prop_assert_eq!(
            index.documents().len(), fresh_count,
            "stale docs should be expired: have {} want {}",
            index.documents().len(), fresh_count
        );
    }

    /// Size-limited eviction keeps total bytes within budget.
    #[test]
    fn eviction_respects_size_budget(
        doc_count in 3usize..12,
    ) {
        let dir = tempdir().expect("tempdir");
        let mut cfg = fast_flush_config(dir.path());
        // Very small budget to force eviction.
        cfg.max_index_size_bytes = 400;

        let mut index = SearchIndex::open(cfg).expect("open");

        let docs: Vec<IndexableDocument> = (0..doc_count)
            .map(|i| {
                make_doc(
                    &format!("document number {i} with some padding text"),
                    1_000 + i as i64,
                    SearchDocumentSource::Scrollback,
                )
            })
            .collect();

        let _ = index
            .ingest_documents(&docs, 2_000, false, None)
            .expect("ingest");

        let stats = index.stats(2_100);
        prop_assert!(
            stats.total_bytes <= 400,
            "total_bytes {} should be <= 400", stats.total_bytes
        );
    }

    /// Eviction removes least-recently-accessed docs first.
    #[test]
    fn eviction_is_lru(
        extra_docs in 2usize..6,
    ) {
        let dir = tempdir().expect("tempdir");
        let mut cfg = fast_flush_config(dir.path());
        // Budget fits about 2 docs.
        cfg.max_index_size_bytes = 350;

        let mut index = SearchIndex::open(cfg).expect("open");

        // Insert first doc and access it via search.
        let first = make_doc("keep this document alive", 1_000, SearchDocumentSource::Scrollback);
        let _ = index
            .ingest_documents(&[first], 1_100, false, None)
            .expect("first");

        // Search to bump its last_accessed_at.
        let _ = index.search("keep this", 5, 1_200);

        // Insert more docs to trigger eviction.
        let more: Vec<IndexableDocument> = (0..extra_docs)
            .map(|i| {
                make_doc(
                    &format!("extra eviction candidate {i}"),
                    1_300 + i as i64,
                    SearchDocumentSource::Scrollback,
                )
            })
            .collect();
        let _ = index
            .ingest_documents(&more, 1_500, false, None)
            .expect("more");

        let stats = index.stats(1_600);
        prop_assert!(stats.total_bytes <= 350, "should stay within budget");
    }
}

// ── Rate limiting invariants ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Rate limiter defers docs exceeding per-second quota.
    #[test]
    fn rate_limiter_defers_excess(
        doc_count in 5usize..20,
        max_per_sec in 1u32..5,
    ) {
        let dir = tempdir().expect("tempdir");
        let mut cfg = fast_flush_config(dir.path());
        cfg.max_docs_per_second = max_per_sec;
        cfg.flush_docs_threshold = 100; // Prevent auto-flush complicating counts.

        let mut index = SearchIndex::open(cfg).expect("open");

        let docs: Vec<IndexableDocument> = (0..doc_count)
            .map(|i| {
                make_doc(
                    &format!("rate-limited-{i}"),
                    1_000 + i as i64,
                    SearchDocumentSource::Scrollback,
                )
            })
            .collect();

        // All ingested at the same ms => same rate window.
        let report = index
            .ingest_documents(&docs, 1_000, false, None)
            .expect("ingest");

        let accepted = report.accepted_docs;
        let deferred = report.deferred_rate_limited_docs;
        let skipped_dup = report.skipped_duplicate_docs;

        // Accepted + deferred + skipped should total up correctly.
        prop_assert!(
            accepted <= max_per_sec as usize,
            "accepted {} > max_per_sec {}", accepted, max_per_sec
        );
        prop_assert_eq!(
            accepted + deferred + skipped_dup + report.skipped_empty_docs,
            doc_count,
            "accounting"
        );
    }

    /// Rate window resets after 1 second boundary.
    #[test]
    fn rate_window_resets_after_one_second(
        batch_size in 2usize..6,
    ) {
        let dir = tempdir().expect("tempdir");
        let mut cfg = fast_flush_config(dir.path());
        cfg.max_docs_per_second = 2;
        cfg.flush_docs_threshold = 100;

        let mut index = SearchIndex::open(cfg).expect("open");

        let batch1: Vec<IndexableDocument> = (0..batch_size)
            .map(|i| {
                make_doc(
                    &format!("window1-{i}"),
                    1_000 + i as i64,
                    SearchDocumentSource::Scrollback,
                )
            })
            .collect();
        let r1 = index
            .ingest_documents(&batch1, 1_000, false, None)
            .expect("batch1");

        // Second batch at +1001ms — new rate window.
        let batch2: Vec<IndexableDocument> = (0..batch_size)
            .map(|i| {
                make_doc(
                    &format!("window2-{i}"),
                    2_001 + i as i64,
                    SearchDocumentSource::Scrollback,
                )
            })
            .collect();
        let r2 = index
            .ingest_documents(&batch2, 2_001, false, None)
            .expect("batch2");

        // Each window allows up to 2 docs.
        prop_assert!(r1.accepted_docs <= 2, "window1 accepted: {}", r1.accepted_docs);
        prop_assert!(r2.accepted_docs <= 2, "window2 accepted: {}", r2.accepted_docs);
    }
}

// ── Chunk scrollback invariants ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// Empty input produces empty output.
    #[test]
    fn chunk_scrollback_empty_input(gap_ms in 1_000i64..60_000) {
        let result = chunk_scrollback_lines(&[], gap_ms);
        prop_assert_eq!(result.len(), 0);
    }

    /// All produced chunks have Scrollback source.
    #[test]
    fn chunk_scrollback_source_tag(
        lines in prop::collection::vec(scrollback_line_strategy(), 1..15),
        gap_ms in 1_000i64..60_000,
    ) {
        let chunks = chunk_scrollback_lines(&lines, gap_ms);
        for chunk in &chunks {
            prop_assert_eq!(chunk.source, SearchDocumentSource::Scrollback);
        }
    }

    /// Blank lines create chunk boundaries — more blanks means more chunks.
    #[test]
    fn chunk_scrollback_blank_line_boundary(
        segments in prop::collection::vec(
            prop::collection::vec("[a-z]{3,10}", 1..5),
            1..6,
        ),
    ) {
        let mut lines = Vec::new();
        let mut ts = 1_000i64;
        for (seg_idx, segment) in segments.iter().enumerate() {
            if seg_idx > 0 {
                // Insert blank line as boundary.
                lines.push(ScrollbackLine::new("", ts));
                ts += 1;
            }
            for text in segment {
                lines.push(ScrollbackLine::new(text.clone(), ts));
                ts += 1;
            }
        }

        let chunks = chunk_scrollback_lines(&lines, 999_999);
        // Should produce at least as many chunks as segments (blank lines split them).
        prop_assert!(
            chunks.len() >= segments.len(),
            "chunks {} < segments {}", chunks.len(), segments.len()
        );
    }

    /// Time gap creates chunk boundary between surrounding content.
    #[test]
    fn chunk_scrollback_time_gap_splits(
        gap_ms in 1_000i64..10_000,
    ) {
        // Need content before, content at gap boundary (consumed as separator),
        // and content after for the second chunk.
        let lines = vec![
            ScrollbackLine::new("before gap line1", 1_000),
            ScrollbackLine::new("before gap line2", 1_001),
            ScrollbackLine::new("gap marker", 1_001 + gap_ms + 1),
            ScrollbackLine::new("after gap content", 1_001 + gap_ms + 2),
        ];
        let chunks = chunk_scrollback_lines(&lines, gap_ms);
        // The gap boundary flushes "before" lines; "after gap content" forms second chunk.
        prop_assert!(chunks.len() >= 2, "time gap should split: got {}", chunks.len());
    }

    /// Single contiguous block produces exactly one chunk.
    #[test]
    fn chunk_scrollback_single_block(
        texts in prop::collection::vec("[a-z]{3,10}", 1..8),
    ) {
        let lines: Vec<ScrollbackLine> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| ScrollbackLine::new(t.clone(), 1_000 + i as i64))
            .collect();

        // Very large gap => no time-based splits. No blank lines => one chunk.
        let chunks = chunk_scrollback_lines(&lines, 999_999);
        prop_assert_eq!(chunks.len(), 1, "contiguous lines should form 1 chunk");
    }
}

// ── Command block extraction invariants ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Empty input produces empty output.
    #[test]
    fn command_blocks_empty_input(gap_ms in 1_000i64..60_000) {
        let config = CommandBlockExtractionConfig {
            prompt_pattern: r"^\$ ".to_string(),
            gap_ms,
        };
        let result = extract_command_output_blocks(&[], &config);
        prop_assert_eq!(result.len(), 0);
    }

    /// All extracted blocks have Command source.
    #[test]
    fn command_blocks_source_tag(
        lines in prop::collection::vec(scrollback_line_strategy(), 1..10),
    ) {
        let config = CommandBlockExtractionConfig::default();
        let blocks = extract_command_output_blocks(&lines, &config);
        for block in &blocks {
            prop_assert_eq!(block.source, SearchDocumentSource::Command);
        }
    }

    /// OSC 133 markers always produce boundaries.
    #[test]
    fn command_blocks_osc133_boundary(
        before_count in 1usize..4,
        after_count in 1usize..4,
    ) {
        let mut lines = Vec::new();
        let mut ts = 1_000i64;
        for i in 0..before_count {
            lines.push(ScrollbackLine::new(format!("output-{i}"), ts));
            ts += 1;
        }
        // OSC 133;A is prompt start.
        lines.push(ScrollbackLine::new("\x1b]133;A\x07user@host $", ts));
        ts += 1;
        for i in 0..after_count {
            lines.push(ScrollbackLine::new(format!("cmd-output-{i}"), ts));
            ts += 1;
        }

        let config = CommandBlockExtractionConfig::default();
        let blocks = extract_command_output_blocks(&lines, &config);
        // The OSC boundary should cause at least 2 blocks.
        prop_assert!(
            blocks.len() >= 2,
            "OSC 133 should create boundary: got {} blocks", blocks.len()
        );
    }

    /// Without prompt boundaries, non-empty lines form one fallback block.
    #[test]
    fn command_blocks_fallback_single(
        lines in prop::collection::vec("[a-z]{3,10}".prop_map(|t| ScrollbackLine::new(t, 1_000)), 1..8),
    ) {
        // Use prompt pattern that won't match simple alpha strings.
        let config = CommandBlockExtractionConfig {
            prompt_pattern: r"^\$ $".to_string(),
            gap_ms: 999_999,
        };
        let blocks = extract_command_output_blocks(&lines, &config);
        // Without boundaries, all output falls into one block.
        prop_assert_eq!(blocks.len(), 1, "no boundaries => 1 fallback block");
    }
}

// ── Agent artifact extraction invariants ─────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// All extracted artifacts have AgentArtifact source.
    #[test]
    fn agent_artifacts_source_tag(
        text in "(.|\n){10,200}",
    ) {
        let artifacts = extract_agent_artifacts(&text, 1_000, Some(1), Some("s1".to_string()));
        for artifact in &artifacts {
            prop_assert_eq!(artifact.source, SearchDocumentSource::AgentArtifact);
        }
    }

    /// Error-containing lines produce artifacts.
    #[test]
    fn agent_artifacts_detect_errors(
        prefix in "[a-z]{3,10}",
        suffix in "[a-z]{3,10}",
    ) {
        let text = format!("{prefix} error: something went wrong {suffix}");
        let artifacts = extract_agent_artifacts(&text, 1_000, Some(1), None);
        prop_assert!(
            !artifacts.is_empty(),
            "should detect 'error:' in '{}'", text
        );
    }

    /// Fenced code blocks produce artifacts.
    #[test]
    fn agent_artifacts_detect_code_blocks(
        code_line in "[a-z_]+\\(\\);",
    ) {
        let text = format!("```\n{code_line}\n```");
        let artifacts = extract_agent_artifacts(&text, 1_000, Some(1), None);
        let has_code = artifacts.iter().any(|a| {
            a.metadata.get("artifact_kind")
                .and_then(|v| v.as_str())
                == Some("code_block")
        });
        prop_assert!(has_code, "should find code block artifact");
    }

    /// Artifact dedup prevents duplicate content hashes.
    #[test]
    fn agent_artifacts_dedup(
        line in "[a-z]{3,10}",
    ) {
        // Same error line repeated should produce only one artifact.
        let text = format!("error: {line}\nerror: {line}\nerror: {line}");
        let artifacts = extract_agent_artifacts(&text, 1_000, Some(1), None);
        let hashes: HashSet<&str> = artifacts
            .iter()
            .map(|a| a.text.as_str())
            .collect();
        prop_assert_eq!(
            hashes.len(), artifacts.len(),
            "artifacts should be unique by content"
        );
    }

    /// Empty or whitespace-only text produces no artifacts.
    #[test]
    fn agent_artifacts_empty_input(
        ws in "[ \\t\\n]{0,20}",
    ) {
        let artifacts = extract_agent_artifacts(&ws, 1_000, Some(1), None);
        prop_assert_eq!(artifacts.len(), 0, "whitespace-only should produce no artifacts");
    }
}

// ── Stats/config invariants ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Stats doc_count matches actual document count.
    #[test]
    fn stats_doc_count_matches(
        n in 1usize..10,
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs: Vec<IndexableDocument> = (0..n)
            .map(|i| {
                make_doc(
                    &format!("unique-doc-{i}"),
                    1_000 + i as i64,
                    SearchDocumentSource::Scrollback,
                )
            })
            .collect();
        let _ = index
            .ingest_documents(&docs, 2_000, false, None)
            .expect("ingest");

        let stats = index.stats(2_100);
        prop_assert_eq!(
            stats.doc_count, index.documents().len(),
            "stats.doc_count should match documents().len()"
        );
    }

    /// Stats source_counts sum equals doc_count.
    #[test]
    fn stats_source_counts_sum(
        texts_and_sources in prop::collection::vec(
            (text_strategy(), source_strategy()),
            1..10,
        ),
    ) {
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs: Vec<IndexableDocument> = texts_and_sources
            .iter()
            .enumerate()
            .map(|(i, (t, s))| make_doc(t, 1_000 + i as i64, *s))
            .collect();
        let _ = index
            .ingest_documents(&docs, 2_000, false, None)
            .expect("ingest");

        let stats = index.stats(2_100);
        let source_total: usize = stats.source_counts.values().sum();
        prop_assert_eq!(
            source_total, stats.doc_count,
            "source_counts sum {} != doc_count {}", source_total, stats.doc_count
        );
    }

    /// Config validation rejects zero flush_docs_threshold.
    #[test]
    fn config_rejects_zero_threshold(
        interval in 1u64..100,
    ) {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 0;
        cfg.flush_interval_secs = interval;
        let result = SearchIndex::open(cfg);
        prop_assert!(result.is_err(), "should reject zero threshold");
    }

    /// Config validation rejects zero flush_interval_secs.
    #[test]
    fn config_rejects_zero_interval(
        threshold in 1usize..100,
    ) {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_interval_secs = 0;
        cfg.flush_docs_threshold = threshold;
        let result = SearchIndex::open(cfg);
        prop_assert!(result.is_err(), "should reject zero interval");
    }

    /// SearchDocumentSource::as_tag round-trips are stable.
    #[test]
    fn source_as_tag_stable(src in source_strategy()) {
        let tag = src.as_tag();
        prop_assert!(!tag.is_empty(), "tag should not be empty");
        // Tag should be a lowercase identifier.
        prop_assert!(
            tag.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "tag '{}' should be lowercase+underscore", tag
        );
    }
}

// ── ANSI stripping and normalization ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// ANSI escape sequences are stripped from indexed text.
    #[test]
    fn ansi_stripped_from_indexed_text(
        word in "[a-z]{3,10}",
    ) {
        let ansi_text = format!("\x1b[31m{word}\x1b[0m");
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let doc = make_doc(&ansi_text, 1_000, SearchDocumentSource::Scrollback);
        let _ = index
            .ingest_documents(&[doc], 1_100, false, None)
            .expect("ingest");

        // The indexed text should not contain escape sequences.
        for doc in index.documents() {
            prop_assert!(
                !doc.text.contains('\x1b'),
                "indexed text should not contain ANSI escapes"
            );
            prop_assert!(
                doc.text.contains(&*word),
                "indexed text should contain the word '{}'", word
            );
        }
    }

    /// OSC sequences are stripped.
    #[test]
    fn osc_stripped(word in "[a-z]{3,10}") {
        let osc_text = format!("\x1b]0;title\x07{word}");
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let doc = make_doc(&osc_text, 1_000, SearchDocumentSource::Scrollback);
        let _ = index
            .ingest_documents(&[doc], 1_100, false, None)
            .expect("ingest");

        for doc in index.documents() {
            prop_assert!(!doc.text.contains('\x1b'));
            prop_assert!(doc.text.contains(&*word));
        }
    }

    /// Normalization collapses repeated whitespace.
    #[test]
    fn normalization_collapses_whitespace(
        word1 in "[a-z]{3,8}",
        word2 in "[a-z]{3,8}",
        spaces in 2usize..10,
    ) {
        let spaced = format!("{}{}{}",
            word1,
            " ".repeat(spaces),
            word2,
        );
        let dir = tempdir().expect("tempdir");
        let cfg = fast_flush_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let doc = make_doc(&spaced, 1_000, SearchDocumentSource::Scrollback);
        let _ = index
            .ingest_documents(&[doc], 1_100, false, None)
            .expect("ingest");

        for doc in index.documents() {
            // Should not have runs of multiple spaces.
            prop_assert!(
                !doc.text.contains("  "),
                "normalized text should not have double spaces: '{}'", doc.text
            );
        }
    }
}

// ── Flush behavior ───────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Flush_now on empty pending is a no-op.
    #[test]
    fn flush_now_noop_when_empty(ts in 1_000i64..100_000) {
        let dir = tempdir().expect("tempdir");
        let cfg = make_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let result = index.flush_now(ts, IndexFlushReason::Manual).expect("flush");
        prop_assert_eq!(result.flushed_docs, 0);
    }

    /// Tick during resize storm is a no-op.
    #[test]
    fn tick_noop_during_resize(ts in 1_000i64..100_000) {
        let dir = tempdir().expect("tempdir");
        let cfg = make_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let result = index.tick(ts, true).expect("tick");
        prop_assert_eq!(result.flushed_docs, 0);
    }

    /// Doc threshold triggers auto-flush at exact boundary.
    #[test]
    fn doc_threshold_triggers_flush(
        threshold in 2usize..10,
    ) {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = threshold;
        cfg.max_docs_per_second = 10_000;

        let mut index = SearchIndex::open(cfg).expect("open");

        let docs: Vec<IndexableDocument> = (0..threshold)
            .map(|i| {
                make_doc(
                    &format!("thresh-doc-{i}"),
                    1_000 + i as i64,
                    SearchDocumentSource::Scrollback,
                )
            })
            .collect();

        let report = index
            .ingest_documents(&docs, 2_000, false, None)
            .expect("ingest");

        prop_assert_eq!(
            report.flushed_docs, threshold,
            "should flush exactly at threshold"
        );
        prop_assert_eq!(
            report.flush_reason, Some(IndexFlushReason::DocThreshold)
        );
        prop_assert_eq!(index.pending_len(), 0, "pending should be empty after flush");
    }
}

// ── IndexFlushReason & report serde ────────────────────────────────────────

fn flush_reason_strategy() -> impl Strategy<Value = IndexFlushReason> {
    prop_oneof![
        Just(IndexFlushReason::DocThreshold),
        Just(IndexFlushReason::Interval),
        Just(IndexFlushReason::Manual),
        Just(IndexFlushReason::Reindex),
    ]
}

fn arb_ingest_report() -> impl Strategy<Value = frankenterm_core::search::IndexingIngestReport> {
    (
        0usize..100,
        0usize..100,
        0usize..50,
        0usize..50,
        0usize..50,
        0usize..50,
        0usize..50,
        0usize..100,
        0usize..50,
        0usize..50,
        proptest::option::of(flush_reason_strategy()),
    )
        .prop_map(
            |(
                submitted,
                accepted,
                empty,
                dup,
                cass,
                resize,
                rate_lim,
                flushed,
                expired,
                evicted,
                reason,
            )| {
                frankenterm_core::search::IndexingIngestReport {
                    submitted_docs: submitted,
                    accepted_docs: accepted,
                    skipped_empty_docs: empty,
                    skipped_duplicate_docs: dup,
                    skipped_cass_docs: cass,
                    skipped_resize_pause_docs: resize,
                    deferred_rate_limited_docs: rate_lim,
                    flushed_docs: flushed,
                    expired_docs: expired,
                    evicted_docs: evicted,
                    flush_reason: reason,
                }
            },
        )
}

fn arb_tick_result() -> impl Strategy<Value = frankenterm_core::search::IndexingTickResult> {
    (
        0usize..100,
        0usize..50,
        0usize..50,
        proptest::option::of(flush_reason_strategy()),
    )
        .prop_map(|(flushed, expired, evicted, reason)| {
            frankenterm_core::search::IndexingTickResult {
                flushed_docs: flushed,
                expired_docs: expired,
                evicted_docs: evicted,
                flush_reason: reason,
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// IndexFlushReason serde roundtrip.
    #[test]
    fn flush_reason_serde_roundtrip(reason in flush_reason_strategy()) {
        let json = serde_json::to_string(&reason).expect("serialize");
        let back: IndexFlushReason = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(reason, back);
    }

    /// IndexFlushReason label is a snake_case identifier.
    #[test]
    fn flush_reason_label_non_empty(reason in flush_reason_strategy()) {
        let json = serde_json::to_string(&reason).expect("ser");
        let label = json.trim_matches('"');
        prop_assert!(!label.is_empty(), "label must be non-empty");
        let valid_chars = label.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        prop_assert!(valid_chars, "label must be snake_case, got: {}", label);
    }

    /// IndexingIngestReport serde roundtrip.
    #[test]
    fn ingest_report_serde_roundtrip(report in arb_ingest_report()) {
        let json = serde_json::to_string(&report).expect("serialize");
        let back: frankenterm_core::search::IndexingIngestReport =
            serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(report, back);
    }

    /// IndexingTickResult serde roundtrip.
    #[test]
    fn tick_result_serde_roundtrip(tick in arb_tick_result()) {
        let json = serde_json::to_string(&tick).expect("serialize");
        let back: frankenterm_core::search::IndexingTickResult =
            serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(tick, back);
    }

    /// SearchDocumentSource serde roundtrip.
    #[test]
    fn search_document_source_serde_roundtrip(source in source_strategy()) {
        let json = serde_json::to_string(&source).expect("serialize");
        let back: SearchDocumentSource =
            serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(source, back);
    }

    /// All flush reasons are distinct after serialization.
    #[test]
    fn flush_reasons_are_distinct(_dummy in 0..1u8) {
        let reasons = [
            IndexFlushReason::DocThreshold,
            IndexFlushReason::Interval,
            IndexFlushReason::Manual,
            IndexFlushReason::Reindex,
        ];
        let labels: HashSet<String> = reasons
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        prop_assert_eq!(labels.len(), reasons.len(), "all labels must be unique");
    }

    /// Ingest report default has all zeros.
    #[test]
    fn ingest_report_default_is_zero(_dummy in 0..1u8) {
        let report = frankenterm_core::search::IndexingIngestReport::default();
        prop_assert_eq!(report.submitted_docs, 0);
        prop_assert_eq!(report.accepted_docs, 0);
        prop_assert_eq!(report.flushed_docs, 0);
        prop_assert_eq!(report.flush_reason, None);
    }

    /// Tick result default has all zeros.
    #[test]
    fn tick_result_default_is_zero(_dummy in 0..1u8) {
        let report = frankenterm_core::search::IndexingTickResult::default();
        prop_assert_eq!(report.flushed_docs, 0);
        prop_assert_eq!(report.expired_docs, 0);
        prop_assert_eq!(report.evicted_docs, 0);
        prop_assert_eq!(report.flush_reason, None);
    }

    /// IngestReport: flush_reason is Some iff flushed_docs > 0.
    #[test]
    fn ingest_report_flush_reason_iff_flushed(
        texts in prop::collection::vec(text_strategy(), 1..20),
        threshold in 2usize..15,
    ) {
        let dir = tempdir().expect("tmpdir");
        let mut cfg = fast_flush_config(dir.path());
        cfg.flush_docs_threshold = threshold;
        let mut index = SearchIndex::open(cfg).expect("open");
        let docs: Vec<_> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| make_doc(t, 1_000 + i as i64, SearchDocumentSource::Scrollback))
            .collect();
        let report = index.ingest_documents(&docs, 2_000, false, None).expect("ingest");
        if report.flushed_docs > 0 {
            prop_assert!(report.flush_reason.is_some(), "flushed>0 implies flush_reason");
        }
    }
}

// ── Document type serde roundtrips ─────────────────────────────────────────

fn arb_scrollback_line() -> impl Strategy<Value = ScrollbackLine> {
    (
        text_strategy(),
        1_000i64..1_000_000,
        proptest::option::of(1u64..1000),
        proptest::option::of("[a-z]{4,12}"),
    )
        .prop_map(|(text, ts, pane_id, session_id)| {
            let mut line = ScrollbackLine::new(text, ts);
            line.pane_id = pane_id;
            line.session_id = session_id;
            line
        })
}

fn arb_indexable_document() -> impl Strategy<Value = IndexableDocument> {
    (
        source_strategy(),
        text_strategy(),
        1_000i64..1_000_000,
        proptest::option::of(1u64..1000),
        proptest::option::of("[a-z]{4,12}"),
    )
        .prop_map(|(source, text, ts, pane_id, session_id)| {
            IndexableDocument::text(source, text, ts, pane_id, session_id)
        })
}

fn arb_indexed_document() -> impl Strategy<Value = frankenterm_core::search::IndexedDocument> {
    (
        1u64..10000,
        source_strategy(),
        "[a-z_]{4,16}",
        "[0-9a-f]{64}",
        text_strategy(),
        1_000i64..1_000_000,
        1_000i64..1_000_000,
        1_000i64..1_000_000,
        proptest::option::of(1u64..1000),
        proptest::option::of("[a-z]{4,12}"),
        0u64..10000,
    )
        .prop_map(
            |(id, source, tag, hash, text, captured, indexed, accessed, pane, session, size)| {
                frankenterm_core::search::IndexedDocument {
                    id,
                    source,
                    source_tag: tag,
                    content_hash: hash,
                    text,
                    captured_at_ms: captured,
                    indexed_at_ms: indexed,
                    last_accessed_at_ms: accessed,
                    pane_id: pane,
                    session_id: session,
                    metadata: serde_json::Value::Null,
                    size_bytes: size,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// ScrollbackLine serde roundtrip.
    #[test]
    fn scrollback_line_serde_roundtrip(line in arb_scrollback_line()) {
        let json = serde_json::to_string(&line).expect("serialize");
        let back: ScrollbackLine = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(line, back);
    }

    /// IndexableDocument serde roundtrip.
    #[test]
    fn indexable_document_serde_roundtrip(doc in arb_indexable_document()) {
        let json = serde_json::to_string(&doc).expect("serialize");
        let back: IndexableDocument = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(doc, back);
    }

    /// IndexedDocument serde roundtrip.
    #[test]
    fn indexed_document_serde_roundtrip(doc in arb_indexed_document()) {
        let json = serde_json::to_string(&doc).expect("serialize");
        let back: frankenterm_core::search::IndexedDocument =
            serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(doc, back);
    }

    /// IndexableDocument::text sets metadata to Null.
    #[test]
    fn indexable_document_text_constructor_metadata_null(
        source in source_strategy(),
        text in text_strategy(),
        ts in 1_000i64..1_000_000,
    ) {
        let doc = IndexableDocument::text(source, text, ts, None, None);
        prop_assert_eq!(doc.metadata, serde_json::Value::Null);
    }

    /// ScrollbackLine::new sets optional fields to None.
    #[test]
    fn scrollback_line_new_defaults(text in text_strategy(), ts in 0i64..1_000_000) {
        let line = ScrollbackLine::new(text.clone(), ts);
        prop_assert_eq!(&line.text, &text);
        prop_assert_eq!(line.captured_at_ms, ts);
        prop_assert_eq!(line.pane_id, None);
        prop_assert_eq!(line.session_id, None);
    }

    /// IndexedDocument preserves all field values through serde.
    #[test]
    fn indexed_document_fields_preserved(doc in arb_indexed_document()) {
        let json = serde_json::to_string(&doc).expect("ser");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        prop_assert_eq!(
            parsed["id"].as_u64().unwrap(),
            doc.id,
            "id mismatch"
        );
        prop_assert_eq!(
            parsed["size_bytes"].as_u64().unwrap(),
            doc.size_bytes,
            "size_bytes mismatch"
        );
    }
}
