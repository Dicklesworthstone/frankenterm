//! Search integration tests for indexing + extraction + hybrid fusion surfaces.

use frankenterm_core::search::{
    CommandBlockExtractionConfig, HybridSearchService, IndexFlushReason, IndexableDocument,
    IndexingConfig, ScrollbackLine, SearchDocumentSource, SearchIndex, SearchMode,
    chunk_scrollback_lines, extract_agent_artifacts, extract_command_output_blocks,
};
use tempfile::tempdir;

fn config_with_budget(index_dir: std::path::PathBuf, max_index_size_bytes: u64) -> IndexingConfig {
    IndexingConfig {
        index_dir,
        max_index_size_bytes,
        ttl_days: 30,
        flush_interval_secs: 1,
        flush_docs_threshold: 1,
        max_docs_per_second: 10_000,
    }
}

fn line(text: &str, captured_at_ms: i64) -> ScrollbackLine {
    ScrollbackLine {
        text: text.to_string(),
        captured_at_ms,
        pane_id: Some(7),
        session_id: Some("session-a".to_string()),
    }
}

#[test]
fn test_bridge_round_trip() {
    let temp = tempdir().expect("tempdir");
    let mut index = SearchIndex::open(config_with_budget(temp.path().join("index"), 1_000_000))
        .expect("open index");

    let now = 1_700_000_000_000_i64;
    let docs = vec![
        IndexableDocument::text(
            SearchDocumentSource::Scrollback,
            "build log: panic happened in worker",
            now,
            Some(7),
            Some("session-a".to_string()),
        ),
        IndexableDocument::text(
            SearchDocumentSource::Command,
            "cargo test --workspace",
            now + 1,
            Some(7),
            Some("session-a".to_string()),
        ),
        IndexableDocument::text(
            SearchDocumentSource::AgentArtifact,
            "error: panic: index out of bounds",
            now + 2,
            Some(7),
            Some("session-a".to_string()),
        ),
    ];

    let ingest = index
        .ingest_documents(&docs, now + 3, false, None)
        .expect("ingest documents");
    assert_eq!(ingest.accepted_docs, 3);
    assert!(ingest.flushed_docs >= 1);

    let lexical_docs = index.search("panic", 10, now + 4);
    assert!(!lexical_docs.is_empty());

    let lexical_ranked: Vec<(u64, f32)> = lexical_docs
        .iter()
        .enumerate()
        .map(|(rank, doc)| (doc.id, 1.0 / (rank as f32 + 1.0)))
        .collect();
    let semantic_ranked = vec![(lexical_docs[0].id, 0.99), (99_999, 0.5)];

    let fused = HybridSearchService::new()
        .with_mode(SearchMode::Hybrid)
        .fuse(&lexical_ranked, &semantic_ranked, 5);
    assert!(!fused.is_empty());
    assert_eq!(fused[0].id, lexical_docs[0].id);

    let stats = index.stats(now + 5);
    assert_eq!(stats.doc_count, 3);
    assert_eq!(
        stats
            .source_counts
            .get(SearchDocumentSource::Scrollback.as_tag()),
        Some(&1)
    );
    assert_eq!(
        stats
            .source_counts
            .get(SearchDocumentSource::Command.as_tag()),
        Some(&1)
    );
    assert_eq!(
        stats
            .source_counts
            .get(SearchDocumentSource::AgentArtifact.as_tag()),
        Some(&1)
    );
}

#[test]
fn test_progressive_delivery_ordering() {
    let lexical = vec![(101, 10.0), (102, 9.0), (103, 8.0)];
    let semantic = vec![(201, 0.95), (101, 0.90), (202, 0.85)];

    let initial = HybridSearchService::new()
        .with_mode(SearchMode::Lexical)
        .fuse(&lexical, &semantic, 2);
    assert_eq!(
        initial.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![101, 102]
    );

    let refined = HybridSearchService::new()
        .with_mode(SearchMode::Hybrid)
        .fuse(&lexical, &semantic, 4);
    assert!(!refined.is_empty());
    assert_eq!(refined[0].id, 101);
    assert!(refined.iter().any(|r| r.id == 201));
}

#[test]
fn test_index_size_limit() {
    let max_bytes = 1_024_u64;
    let temp = tempdir().expect("tempdir");
    let mut index = SearchIndex::open(config_with_budget(
        temp.path().join("bounded-index"),
        max_bytes,
    ))
    .expect("open bounded index");
    let now = 1_800_000_000_000_i64;

    for i in 0..24 {
        let text = format!("doc-{i} {}", "x".repeat(220));
        let doc = IndexableDocument::text(
            SearchDocumentSource::Scrollback,
            text,
            now + i,
            Some(9),
            None,
        );
        let report = index
            .ingest_documents(&[doc], now + i, false, None)
            .expect("ingest one doc");
        assert_eq!(report.accepted_docs, 1);
    }

    let _ = index
        .flush_now(now + 30, IndexFlushReason::Manual)
        .expect("flush now");

    let stats = index.stats(now + 31);
    assert!(stats.total_bytes <= max_bytes);
    assert!(stats.doc_count < 24);
    assert!(index.search("doc-0", 5, now + 32).is_empty());
}

#[test]
fn test_scrollback_indexing() {
    let base_ts = 1_900_000_000_000_i64;
    let lines = vec![
        line("cargo test --workspace", base_ts + 10),
        line("running 4 tests", base_ts + 20),
        line("", base_ts + 30),
        line("error: panic at src/lib.rs:42", base_ts + 40),
        line("tool call: shell", base_ts + 50),
    ];

    let scroll_docs = chunk_scrollback_lines(&lines, 5_000);
    assert_eq!(scroll_docs.len(), 2);
    assert!(
        scroll_docs
            .iter()
            .all(|doc| doc.source == SearchDocumentSource::Scrollback)
    );

    let command_docs =
        extract_command_output_blocks(&lines, &CommandBlockExtractionConfig::default());
    assert_eq!(command_docs.len(), 1);
    assert_eq!(command_docs[0].source, SearchDocumentSource::Command);

    let artifacts = extract_agent_artifacts(
        "```rust\npanic!(\"boom\");\n```\nerror: panic in parser\ntool result: ok",
        base_ts + 60,
        Some(7),
        Some("session-a".to_string()),
    );
    assert!(
        artifacts
            .iter()
            .any(|doc| doc.metadata["artifact_kind"] == "code_block")
    );
    assert!(
        artifacts
            .iter()
            .any(|doc| doc.metadata["artifact_kind"] == "error")
    );
    assert!(
        artifacts
            .iter()
            .any(|doc| doc.metadata["artifact_kind"] == "tool")
    );

    let temp = tempdir().expect("tempdir");
    let mut index = SearchIndex::open(config_with_budget(temp.path().join("index"), 2_000_000))
        .expect("open index");
    let now = base_ts + 100;

    let mut all_docs = Vec::new();
    all_docs.extend(scroll_docs);
    all_docs.extend(command_docs);
    all_docs.extend(artifacts);

    let report = index
        .ingest_documents(&all_docs, now, false, None)
        .expect("ingest extracted docs");
    assert!(report.accepted_docs >= 4);

    let hits = index.search("cargo", 10, now + 1);
    assert!(!hits.is_empty());
}
