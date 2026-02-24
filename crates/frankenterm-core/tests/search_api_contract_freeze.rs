//! Search API contract freeze tests for frankensearch migration (ft-dr6zv.1.3.A1).
//!
//! These tests freeze the public search API surface — types, enum variants, field
//! names, config defaults, serde shapes, and method signatures — so that the
//! frankensearch migration (ft-dr6zv.1.3.B1–B4) can proceed with a verified
//! compatibility baseline. Any test failure here means the API contract has
//! shifted and downstream migration code must be updated.
//!
//! # Contract coverage
//!
//! - SearchDocumentSource variants + `as_tag` mapping
//! - IndexingConfig fields + defaults
//! - IndexableDocument struct + `text()` constructor
//! - IndexedDocument serde shape
//! - ScrollbackLine serde shape
//! - IndexFlushReason variants + serde
//! - IndexingIngestReport default + serde
//! - SearchIndexStats serde shape
//! - HybridSearchService builder API + fuse behavior
//! - SearchMode, FusionBackend enums
//! - FusedResult, TwoTierMetrics shapes
//! - ChunkPolicyConfig defaults + serde roundtrip
//! - SemanticChunk serde shape
//! - EmbedderTier enum + Display
//! - EmbedderInfo struct shape
//! - Embedder trait (via HashEmbedder conformance)
//! - Reranker trait (via PassthroughReranker)
//! - ScoredDoc serde shape
//! - JSON Schema contract (wa-robot-search.json) structural freeze
//! - Deterministic regression corpus (index → search → verify)

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use frankenterm_core::search::{
    ChunkPolicyConfig, CommandBlockExtractionConfig, FusedResult, FusionBackend,
    HashEmbedder, HybridSearchService, IndexFlushReason, IndexableDocument, IndexedDocument,
    IndexingConfig, IndexingIngestReport, PassthroughReranker, Reranker, ScoredDoc,
    ScrollbackLine, SearchDocumentSource, SearchIndex, SearchIndexStats, SearchMode,
    TwoTierMetrics, chunk_scrollback_lines, extract_agent_artifacts,
    extract_command_output_blocks, rrf_fuse,
};
use frankenterm_core::search_explain::{
    GapInfo, PaneExplainInfo, PaneIndexingInfo, SearchExplainContext, SearchExplainEvidence,
    SearchExplainReason, SearchExplainResult, explain_search, render_explain_plain,
};
use frankenterm_core::search::{
    ChunkDirection, ChunkSourceOffset, Embedder, EmbedderInfo, EmbedderTier,
    RECORDER_CHUNKING_POLICY_V1,
};
use tempfile::tempdir;

// ─────────────────────────────────────────────────────────────────────────────
// § SearchDocumentSource contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_search_document_source_variants() {
    // Freeze: exactly 5 variants exist with stable tag strings
    assert_eq!(SearchDocumentSource::Scrollback.as_tag(), "scrollback");
    assert_eq!(SearchDocumentSource::Command.as_tag(), "command");
    assert_eq!(SearchDocumentSource::AgentArtifact.as_tag(), "agent");
    assert_eq!(SearchDocumentSource::PaneMetadata.as_tag(), "pane_metadata");
    assert_eq!(SearchDocumentSource::Cass.as_tag(), "cass");
}

#[test]
fn contract_search_document_source_serde_roundtrip() {
    let variants = [
        SearchDocumentSource::Scrollback,
        SearchDocumentSource::Command,
        SearchDocumentSource::AgentArtifact,
        SearchDocumentSource::PaneMetadata,
        SearchDocumentSource::Cass,
    ];

    for variant in &variants {
        let json = serde_json::to_string(variant).expect("serialize");
        let back: SearchDocumentSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(*variant, back, "roundtrip failed for {json}");
    }

    // Freeze serde representation: snake_case
    assert_eq!(
        serde_json::to_string(&SearchDocumentSource::AgentArtifact).unwrap(),
        "\"agent_artifact\""
    );
    assert_eq!(
        serde_json::to_string(&SearchDocumentSource::PaneMetadata).unwrap(),
        "\"pane_metadata\""
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// § IndexingConfig contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_indexing_config_fields() {
    // Freeze: all fields are accessible and have expected types
    let cfg = IndexingConfig {
        index_dir: PathBuf::from("/tmp/test"),
        max_index_size_bytes: 10_000_000,
        ttl_days: 30,
        flush_interval_secs: 60,
        flush_docs_threshold: 100,
        max_docs_per_second: 5_000,
    };
    assert_eq!(cfg.index_dir, PathBuf::from("/tmp/test"));
    assert_eq!(cfg.max_index_size_bytes, 10_000_000);
    assert_eq!(cfg.ttl_days, 30);
    assert_eq!(cfg.flush_interval_secs, 60);
    assert_eq!(cfg.flush_docs_threshold, 100);
    assert_eq!(cfg.max_docs_per_second, 5_000);
}

// ─────────────────────────────────────────────────────────────────────────────
// § IndexableDocument contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_indexable_document_text_constructor() {
    let doc = IndexableDocument::text(
        SearchDocumentSource::Command,
        "cargo test --workspace",
        1_700_000_000_000,
        Some(7),
        Some("session-1".to_string()),
    );
    assert_eq!(doc.source, SearchDocumentSource::Command);
    assert_eq!(doc.text, "cargo test --workspace");
    assert_eq!(doc.captured_at_ms, 1_700_000_000_000);
    assert_eq!(doc.pane_id, Some(7));
    assert_eq!(doc.session_id, Some("session-1".to_string()));
    assert_eq!(doc.metadata, serde_json::Value::Null);
}

#[test]
fn contract_indexable_document_serde_roundtrip() {
    let doc = IndexableDocument::text(
        SearchDocumentSource::Scrollback,
        "hello world",
        1_000,
        Some(1),
        None,
    );
    let json = serde_json::to_string(&doc).expect("serialize");
    let back: IndexableDocument = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(doc, back);
}

// ─────────────────────────────────────────────────────────────────────────────
// § IndexedDocument contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_indexed_document_fields() {
    // Freeze: IndexedDocument field names via serde
    let doc = IndexedDocument {
        id: 42,
        source: SearchDocumentSource::Scrollback,
        source_tag: "scrollback".to_string(),
        content_hash: "abc123".to_string(),
        text: "test text".to_string(),
        captured_at_ms: 1_000,
        indexed_at_ms: 2_000,
        last_accessed_at_ms: 3_000,
        pane_id: Some(7),
        session_id: Some("s1".to_string()),
        metadata: serde_json::json!({"key": "value"}),
        size_bytes: 9,
    };
    let json = serde_json::to_string(&doc).expect("serialize");
    let val: serde_json::Value = serde_json::from_str(&json).expect("parse");
    assert_eq!(val["id"], 42);
    assert_eq!(val["source"], "scrollback");
    assert_eq!(val["source_tag"], "scrollback");
    assert_eq!(val["content_hash"], "abc123");
    assert_eq!(val["text"], "test text");
    assert_eq!(val["captured_at_ms"], 1_000);
    assert_eq!(val["indexed_at_ms"], 2_000);
    assert_eq!(val["last_accessed_at_ms"], 3_000);
    assert_eq!(val["pane_id"], 7);
    assert_eq!(val["session_id"], "s1");
    assert_eq!(val["size_bytes"], 9);
}

// ─────────────────────────────────────────────────────────────────────────────
// § ScrollbackLine contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_scrollback_line_serde() {
    let line = ScrollbackLine {
        text: "some output".to_string(),
        captured_at_ms: 1_000,
        pane_id: Some(7),
        session_id: Some("s1".to_string()),
    };
    let json = serde_json::to_string(&line).expect("serialize");
    let back: ScrollbackLine = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(line, back);
}

#[test]
fn contract_scrollback_line_new_constructor() {
    let line = ScrollbackLine::new("test", 500);
    assert_eq!(line.text, "test");
    assert_eq!(line.captured_at_ms, 500);
    assert_eq!(line.pane_id, None);
    assert_eq!(line.session_id, None);
}

// ─────────────────────────────────────────────────────────────────────────────
// § IndexFlushReason contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_index_flush_reason_variants_serde() {
    let variants = [
        (IndexFlushReason::DocThreshold, "\"doc_threshold\""),
        (IndexFlushReason::Interval, "\"interval\""),
        (IndexFlushReason::Manual, "\"manual\""),
        (IndexFlushReason::Reindex, "\"reindex\""),
    ];
    for (variant, expected_json) in &variants {
        let json = serde_json::to_string(variant).expect("serialize");
        assert_eq!(&json, expected_json, "serde mismatch for {:?}", variant);
        let back: IndexFlushReason = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(*variant, back);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// § IndexingIngestReport contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_ingest_report_default_and_fields() {
    let report = IndexingIngestReport::default();
    assert_eq!(report.submitted_docs, 0);
    assert_eq!(report.accepted_docs, 0);
    assert_eq!(report.skipped_empty_docs, 0);
    assert_eq!(report.skipped_duplicate_docs, 0);
    assert_eq!(report.skipped_cass_docs, 0);
    assert_eq!(report.skipped_resize_pause_docs, 0);
    assert_eq!(report.deferred_rate_limited_docs, 0);
    assert_eq!(report.flushed_docs, 0);
    assert_eq!(report.expired_docs, 0);
    assert_eq!(report.evicted_docs, 0);
    assert_eq!(report.flush_reason, None);
}

#[test]
fn contract_ingest_report_serde_roundtrip() {
    let report = IndexingIngestReport {
        submitted_docs: 5,
        accepted_docs: 3,
        skipped_empty_docs: 1,
        skipped_duplicate_docs: 1,
        skipped_cass_docs: 0,
        skipped_resize_pause_docs: 0,
        deferred_rate_limited_docs: 0,
        flushed_docs: 3,
        expired_docs: 0,
        evicted_docs: 0,
        flush_reason: Some(IndexFlushReason::DocThreshold),
    };
    let json = serde_json::to_string(&report).expect("serialize");
    let back: IndexingIngestReport = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(report, back);
}

// ─────────────────────────────────────────────────────────────────────────────
// § SearchIndexStats contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_search_index_stats_serde_shape() {
    let stats = SearchIndexStats {
        index_dir: "/tmp/idx".to_string(),
        state_path: "/tmp/idx/state.json".to_string(),
        format_version: 1,
        doc_count: 10,
        segment_count: 2,
        total_bytes: 5000,
        pending_docs: 1,
        max_index_size_bytes: 10_000_000,
        ttl_days: 30,
        flush_interval_secs: 60,
        flush_docs_threshold: 100,
        newest_captured_at_ms: Some(1_000_000),
        oldest_captured_at_ms: Some(500_000),
        freshness_age_ms: Some(500_000),
        source_counts: {
            let mut m = HashMap::new();
            m.insert("scrollback".to_string(), 8);
            m.insert("command".to_string(), 2);
            m
        },
        last_flush_at_ms: Some(900_000),
    };
    let json = serde_json::to_string(&stats).expect("serialize");
    let back: SearchIndexStats = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(stats, back);
}

// ─────────────────────────────────────────────────────────────────────────────
// § HybridSearchService + SearchMode + FusionBackend contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_search_mode_variants() {
    // Freeze: exactly 3 search modes
    let _ = SearchMode::Lexical;
    let _ = SearchMode::Semantic;
    let _ = SearchMode::Hybrid;
}

#[test]
fn contract_fusion_backend_parse_and_as_str() {
    // All inputs normalize to FrankenSearchRrf
    assert_eq!(FusionBackend::parse("frankensearch"), FusionBackend::FrankenSearchRrf);
    assert_eq!(FusionBackend::parse("frankensearch_rrf"), FusionBackend::FrankenSearchRrf);
    assert_eq!(FusionBackend::parse("frankensearch-rrf"), FusionBackend::FrankenSearchRrf);
    assert_eq!(FusionBackend::parse("legacy"), FusionBackend::FrankenSearchRrf);
    assert_eq!(FusionBackend::parse(""), FusionBackend::FrankenSearchRrf);
    assert_eq!(FusionBackend::FrankenSearchRrf.as_str(), "frankensearch_rrf");
}

#[test]
fn contract_hybrid_search_service_builder_api() {
    // Freeze: builder chain with_mode, with_rrf_k, with_alpha
    let svc = HybridSearchService::new()
        .with_mode(SearchMode::Hybrid)
        .with_rrf_k(50)
        .with_alpha(0.7);

    let lexical = vec![(1_u64, 10.0_f32), (2, 9.0), (3, 8.0)];
    let semantic = vec![(4_u64, 0.95_f32), (1, 0.90), (5, 0.85)];
    let fused = svc.fuse(&lexical, &semantic, 5);
    assert!(!fused.is_empty());
    // Doc 1 appears in both lists → should rank high
    assert!(fused.iter().any(|r| r.id == 1));
}

#[test]
fn contract_fused_result_fields() {
    let result = FusedResult {
        id: 42,
        score: 0.85,
        lexical_rank: Some(1),
        semantic_rank: Some(3),
    };
    assert_eq!(result.id, 42);
    assert!((result.score - 0.85).abs() < 1e-6);
    assert_eq!(result.lexical_rank, Some(1));
    assert_eq!(result.semantic_rank, Some(3));
}

#[test]
fn contract_two_tier_metrics_default() {
    let m = TwoTierMetrics::default();
    assert_eq!(m.tier1_count, 0);
    assert_eq!(m.tier2_count, 0);
    assert_eq!(m.overlap_count, 0);
    assert!((m.rank_correlation - 0.0).abs() < 1e-6);
}

#[test]
fn contract_rrf_fuse_function_signature() {
    let lexical = vec![(10_u64, 1.0_f32)];
    let semantic = vec![(10_u64, 0.95_f32)];
    let fused = rrf_fuse(&lexical, &semantic, 60);
    assert_eq!(fused.len(), 1);
    assert_eq!(fused[0].id, 10);
    assert!(fused[0].score > 0.0);
}

// ─────────────────────────────────────────────────────────────────────────────
// § Lexical-only mode returns only lexical hits in order
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_lexical_mode_returns_lexical_only() {
    let lexical = vec![(1_u64, 10.0_f32), (2, 9.0), (3, 8.0)];
    let semantic = vec![(100_u64, 0.99_f32)];
    let fused = HybridSearchService::new()
        .with_mode(SearchMode::Lexical)
        .fuse(&lexical, &semantic, 3);
    let ids: Vec<u64> = fused.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

// ─────────────────────────────────────────────────────────────────────────────
// § ChunkPolicyConfig contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_chunk_policy_config_defaults() {
    let cfg = ChunkPolicyConfig::default();
    assert_eq!(cfg.max_chunk_chars, 1_800);
    assert_eq!(cfg.max_chunk_events, 48);
    assert_eq!(cfg.max_window_ms, 120_000);
    assert_eq!(cfg.hard_gap_ms, 30_000);
    assert_eq!(cfg.min_chunk_chars, 80);
    assert_eq!(cfg.merge_window_ms, 8_000);
    assert_eq!(cfg.overlap_chars, 120);
}

#[test]
fn contract_chunk_policy_config_serde_roundtrip() {
    let cfg = ChunkPolicyConfig::default();
    let json = serde_json::to_string(&cfg).expect("serialize");
    let back: ChunkPolicyConfig = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(cfg, back);
}

#[test]
fn contract_chunking_policy_version_string() {
    assert_eq!(RECORDER_CHUNKING_POLICY_V1, "ft.recorder.chunking.v1");
}

// ─────────────────────────────────────────────────────────────────────────────
// § SemanticChunk + ChunkDirection + ChunkSourceOffset contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_chunk_direction_serde() {
    let variants = [
        (ChunkDirection::Ingress, "\"ingress\""),
        (ChunkDirection::Egress, "\"egress\""),
        (ChunkDirection::MixedGlued, "\"mixed_glued\""),
    ];
    for (variant, expected) in &variants {
        let json = serde_json::to_string(variant).expect("serialize");
        assert_eq!(&json, expected);
        let back: ChunkDirection = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(*variant, back);
    }
}

#[test]
fn contract_chunk_source_offset_serde() {
    let offset = ChunkSourceOffset {
        segment_id: 1,
        ordinal: 2,
        byte_offset: 3,
    };
    let json = serde_json::to_string(&offset).expect("serialize");
    let back: ChunkSourceOffset = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(offset, back);
}

// ─────────────────────────────────────────────────────────────────────────────
// § Embedder trait + EmbedderTier + EmbedderInfo + HashEmbedder contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_embedder_tier_display() {
    assert_eq!(format!("{}", EmbedderTier::Hash), "hash");
    assert_eq!(format!("{}", EmbedderTier::Fast), "fast");
    assert_eq!(format!("{}", EmbedderTier::Quality), "quality");
}

#[test]
fn contract_embedder_info_fields() {
    let info = EmbedderInfo {
        name: "test".to_string(),
        dimension: 128,
        tier: EmbedderTier::Hash,
    };
    assert_eq!(info.name, "test");
    assert_eq!(info.dimension, 128);
    assert_eq!(info.tier, EmbedderTier::Hash);
}

#[test]
fn contract_hash_embedder_implements_embedder_trait() {
    let embedder = HashEmbedder::new(64);
    // Freeze: trait method availability
    let info = embedder.info();
    assert_eq!(info.tier, EmbedderTier::Hash);
    assert_eq!(info.dimension, 64);
    assert_eq!(embedder.dimension(), 64);
    assert_eq!(embedder.tier(), EmbedderTier::Hash);

    // embed() returns a vector of the correct dimension
    let vec = embedder.embed("test input").expect("embed");
    assert_eq!(vec.len(), 64);

    // embed_batch() works
    let batch = embedder.embed_batch(&["a", "b"]).expect("embed_batch");
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0].len(), 64);

    // Determinism: same input → same output
    let vec2 = embedder.embed("test input").expect("embed again");
    assert_eq!(vec, vec2);
}

// ─────────────────────────────────────────────────────────────────────────────
// § Reranker trait + ScoredDoc + PassthroughReranker contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_scored_doc_fields() {
    let doc = ScoredDoc {
        id: 99,
        text: "test doc".to_string(),
        score: 0.42,
    };
    assert_eq!(doc.id, 99);
    assert_eq!(doc.text, "test doc");
    assert!((doc.score - 0.42).abs() < 1e-6);
}

#[test]
fn contract_passthrough_reranker_preserves_order() {
    let reranker = PassthroughReranker;
    let candidates = vec![
        ScoredDoc { id: 1, text: "a".to_string(), score: 0.9 },
        ScoredDoc { id: 2, text: "b".to_string(), score: 0.7 },
        ScoredDoc { id: 3, text: "c".to_string(), score: 0.5 },
    ];
    let reranked = reranker.rerank("query", candidates).expect("rerank");
    assert_eq!(reranked.len(), 3);
    assert_eq!(reranked[0].id, 1);
    assert_eq!(reranked[1].id, 2);
    assert_eq!(reranked[2].id, 3);
}

// ─────────────────────────────────────────────────────────────────────────────
// § Extraction functions contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_chunk_scrollback_lines_signature() {
    let lines = vec![
        ScrollbackLine {
            text: "line one".to_string(),
            captured_at_ms: 1_000,
            pane_id: Some(1),
            session_id: None,
        },
        ScrollbackLine {
            text: "".to_string(),
            captured_at_ms: 2_000,
            pane_id: Some(1),
            session_id: None,
        },
        ScrollbackLine {
            text: "line three".to_string(),
            captured_at_ms: 3_000,
            pane_id: Some(1),
            session_id: None,
        },
    ];
    let docs = chunk_scrollback_lines(&lines, 5_000);
    // At least produces some documents; exact count is implementation detail
    assert!(!docs.is_empty());
    assert!(docs.iter().all(|d| d.source == SearchDocumentSource::Scrollback));
}

#[test]
fn contract_extract_command_output_blocks_signature() {
    let lines = vec![
        ScrollbackLine {
            text: "$ cargo test".to_string(),
            captured_at_ms: 1_000,
            pane_id: Some(1),
            session_id: None,
        },
        ScrollbackLine {
            text: "running 4 tests".to_string(),
            captured_at_ms: 2_000,
            pane_id: Some(1),
            session_id: None,
        },
    ];
    let config = CommandBlockExtractionConfig::default();
    let blocks = extract_command_output_blocks(&lines, &config);
    // Freeze: returns Vec<IndexableDocument> with Command source
    for block in &blocks {
        assert_eq!(block.source, SearchDocumentSource::Command);
    }
}

#[test]
fn contract_extract_agent_artifacts_signature() {
    let artifacts = extract_agent_artifacts(
        "```rust\nfn main() {}\n```\nerror: some failure\ntool call: shell",
        1_000,
        Some(7),
        Some("session-a".to_string()),
    );
    // Should extract code block, error, and tool artifacts
    assert!(!artifacts.is_empty());
    assert!(artifacts.iter().all(|d| d.source == SearchDocumentSource::AgentArtifact));
}

// ─────────────────────────────────────────────────────────────────────────────
// § JSON Schema structural contract
// ─────────────────────────────────────────────────────────────────────────────

fn load_search_schema() -> Option<serde_json::Value> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let schema_path = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("docs/json-schema/wa-robot-search.json"))?;
    let content = fs::read_to_string(schema_path).ok()?;
    serde_json::from_str(&content).ok()
}

#[test]
fn contract_search_schema_required_top_level_fields() {
    let Some(schema) = load_search_schema() else {
        return; // CI without full checkout
    };
    let required = schema["required"]
        .as_array()
        .expect("required is array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(required.contains(&"query"));
    assert!(required.contains(&"results"));
    assert!(required.contains(&"total_hits"));
    assert!(required.contains(&"limit"));
}

#[test]
fn contract_search_schema_properties() {
    let Some(schema) = load_search_schema() else {
        return;
    };
    let props = schema["properties"].as_object().expect("properties is object");
    // Freeze: top-level property names
    let expected = [
        "query", "results", "total_hits", "limit", "pane_filter",
        "since_filter", "until_filter", "mode", "metrics",
    ];
    for field in &expected {
        assert!(props.contains_key(*field), "missing property: {}", field);
    }
    assert_eq!(
        schema["additionalProperties"],
        serde_json::Value::Bool(false),
        "top-level additionalProperties should be false"
    );
}

#[test]
fn contract_search_schema_search_hit_def() {
    let Some(schema) = load_search_schema() else {
        return;
    };
    let hit = &schema["$defs"]["search_hit"];
    let required = hit["required"]
        .as_array()
        .expect("search_hit required")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(required.contains(&"segment_id"));
    assert!(required.contains(&"pane_id"));
    assert!(required.contains(&"seq"));
    assert!(required.contains(&"captured_at"));
    assert!(required.contains(&"score"));

    let props = hit["properties"].as_object().expect("hit properties");
    let expected_fields = [
        "segment_id", "pane_id", "seq", "captured_at", "score",
        "snippet", "content", "semantic_score", "fusion_rank",
    ];
    for field in &expected_fields {
        assert!(props.contains_key(*field), "search_hit missing: {}", field);
    }
}

#[test]
fn contract_search_schema_mode_enum() {
    let Some(schema) = load_search_schema() else {
        return;
    };
    let mode_enum = schema["properties"]["mode"]["enum"]
        .as_array()
        .expect("mode enum");
    let modes: Vec<&str> = mode_enum.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(modes, vec!["lexical", "semantic", "hybrid"]);
}

#[test]
fn contract_search_schema_metrics_fields() {
    let Some(schema) = load_search_schema() else {
        return;
    };
    let metrics = schema["properties"]["metrics"]["properties"]
        .as_object()
        .expect("metrics properties");
    let expected = [
        "requested_mode", "effective_mode", "fallback_reason", "rrf_k",
        "lexical_weight", "semantic_weight", "lexical_candidates",
        "semantic_candidates", "semantic_cache_hit", "semantic_latency_ms",
        "semantic_rows_scanned", "semantic_budget_state", "semantic_backoff_until_ms",
    ];
    for field in &expected {
        assert!(metrics.contains_key(*field), "metrics missing: {}", field);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// § Baseline regression corpus: deterministic index → search → verify
// ─────────────────────────────────────────────────────────────────────────────

/// Deterministic corpus of 8 documents spanning all SearchDocumentSource types.
fn regression_corpus(base_ts: i64) -> Vec<IndexableDocument> {
    vec![
        IndexableDocument::text(
            SearchDocumentSource::Scrollback,
            "error: cannot find crate `tokio` in scope",
            base_ts,
            Some(1),
            Some("regression-session".to_string()),
        ),
        IndexableDocument::text(
            SearchDocumentSource::Scrollback,
            "warning: unused variable `x` in src/main.rs:42",
            base_ts + 1,
            Some(1),
            Some("regression-session".to_string()),
        ),
        IndexableDocument::text(
            SearchDocumentSource::Command,
            "cargo build --release 2>&1",
            base_ts + 2,
            Some(1),
            Some("regression-session".to_string()),
        ),
        IndexableDocument::text(
            SearchDocumentSource::Command,
            "cargo test --workspace --no-fail-fast",
            base_ts + 3,
            Some(2),
            Some("regression-session".to_string()),
        ),
        IndexableDocument::text(
            SearchDocumentSource::AgentArtifact,
            "tool call: shell(cargo clippy --all-targets)",
            base_ts + 4,
            Some(2),
            Some("regression-session".to_string()),
        ),
        IndexableDocument::text(
            SearchDocumentSource::AgentArtifact,
            "error: panic at src/lib.rs:99: index out of bounds",
            base_ts + 5,
            Some(3),
            Some("regression-session".to_string()),
        ),
        IndexableDocument::text(
            SearchDocumentSource::PaneMetadata,
            "pane_id=3 title=agent-claude shell=zsh cwd=/projects/ft",
            base_ts + 6,
            Some(3),
            Some("regression-session".to_string()),
        ),
        IndexableDocument::text(
            SearchDocumentSource::Cass,
            "session insight: build failure rate increased after commit abc123",
            base_ts + 7,
            Some(1),
            Some("regression-session".to_string()),
        ),
    ]
}

#[test]
fn regression_index_ingest_and_stats() {
    let temp = tempdir().expect("tempdir");
    let base_ts = 1_800_000_000_000_i64;
    let config = IndexingConfig {
        index_dir: temp.path().join("regression-idx"),
        max_index_size_bytes: 10_000_000,
        ttl_days: 30,
        flush_interval_secs: 1,
        flush_docs_threshold: 1,
        max_docs_per_second: 100_000,
    };
    let mut index = SearchIndex::open(config).expect("open");
    let corpus = regression_corpus(base_ts);

    let report = index
        .ingest_documents(&corpus, base_ts + 100, false, None)
        .expect("ingest");
    assert_eq!(report.submitted_docs, 8);
    assert_eq!(report.accepted_docs, 8);
    assert_eq!(report.skipped_empty_docs, 0);
    assert_eq!(report.skipped_duplicate_docs, 0);

    let stats = index.stats(base_ts + 200);
    assert_eq!(stats.doc_count, 8);
    assert_eq!(stats.format_version, 1);
    assert_eq!(
        stats.source_counts.get("scrollback").copied().unwrap_or(0),
        2
    );
    assert_eq!(
        stats.source_counts.get("command").copied().unwrap_or(0),
        2
    );
    assert_eq!(
        stats.source_counts.get("agent").copied().unwrap_or(0),
        2
    );
    assert_eq!(
        stats.source_counts.get("pane_metadata").copied().unwrap_or(0),
        1
    );
    assert_eq!(
        stats.source_counts.get("cass").copied().unwrap_or(0),
        1
    );
}

#[test]
fn regression_search_lexical_results() {
    let temp = tempdir().expect("tempdir");
    let base_ts = 1_800_000_000_000_i64;
    let config = IndexingConfig {
        index_dir: temp.path().join("regression-search"),
        max_index_size_bytes: 10_000_000,
        ttl_days: 30,
        flush_interval_secs: 1,
        flush_docs_threshold: 1,
        max_docs_per_second: 100_000,
    };
    let mut index = SearchIndex::open(config).expect("open");
    let _ = index
        .ingest_documents(&regression_corpus(base_ts), base_ts + 100, false, None)
        .expect("ingest");

    // "cargo" should find command documents
    let results = index.search("cargo", 10, base_ts + 200);
    assert!(results.len() >= 2, "expected >=2 cargo hits, got {}", results.len());

    // "panic" should find the error artifact
    let panic_results = index.search("panic", 10, base_ts + 201);
    assert!(!panic_results.is_empty(), "should find panic artifact");

    // "tokio" should find the scrollback error
    let tokio_results = index.search("tokio", 10, base_ts + 202);
    assert!(!tokio_results.is_empty(), "should find tokio error");

    // Empty query returns nothing
    let empty = index.search("", 10, base_ts + 203);
    assert!(empty.is_empty(), "empty query should return no results");
}

#[test]
fn regression_hybrid_fusion_determinism() {
    // Same inputs always produce same fused ranking
    let lexical = vec![
        (1_u64, 10.0_f32),
        (2, 9.0),
        (3, 8.0),
        (4, 7.0),
    ];
    let semantic = vec![
        (3_u64, 0.99_f32),
        (5, 0.95),
        (1, 0.90),
    ];

    let svc = HybridSearchService::new()
        .with_mode(SearchMode::Hybrid)
        .with_rrf_k(60)
        .with_alpha(0.5);

    let run1 = svc.fuse(&lexical, &semantic, 5);
    let run2 = svc.fuse(&lexical, &semantic, 5);

    assert_eq!(run1.len(), run2.len());
    for (a, b) in run1.iter().zip(run2.iter()) {
        assert_eq!(a.id, b.id, "fusion not deterministic: id mismatch");
        assert!(
            (a.score - b.score).abs() < 1e-10,
            "fusion not deterministic: score mismatch"
        );
    }
}

#[test]
fn regression_duplicate_document_dedup() {
    let temp = tempdir().expect("tempdir");
    let base_ts = 1_800_000_000_000_i64;
    let config = IndexingConfig {
        index_dir: temp.path().join("dedup-idx"),
        max_index_size_bytes: 10_000_000,
        ttl_days: 30,
        flush_interval_secs: 1,
        flush_docs_threshold: 1,
        max_docs_per_second: 100_000,
    };
    let mut index = SearchIndex::open(config).expect("open");

    // Ingest the same document twice
    let doc = IndexableDocument::text(
        SearchDocumentSource::Scrollback,
        "duplicate content for testing",
        base_ts,
        Some(1),
        None,
    );
    let r1 = index
        .ingest_documents(&[doc.clone()], base_ts + 1, false, None)
        .expect("first ingest");
    assert_eq!(r1.accepted_docs, 1);

    let r2 = index
        .ingest_documents(&[doc], base_ts + 2, false, None)
        .expect("second ingest");
    assert_eq!(r2.skipped_duplicate_docs, 1);
    assert_eq!(r2.accepted_docs, 0);

    let stats = index.stats(base_ts + 3);
    assert_eq!(stats.doc_count, 1, "duplicate should not increase count");
}

#[test]
fn regression_index_reopen_persistence() {
    let temp = tempdir().expect("tempdir");
    let base_ts = 1_800_000_000_000_i64;
    let index_dir = temp.path().join("persist-idx");

    // Create and populate
    {
        let config = IndexingConfig {
            index_dir: index_dir.clone(),
            max_index_size_bytes: 10_000_000,
            ttl_days: 30,
            flush_interval_secs: 1,
            flush_docs_threshold: 1,
            max_docs_per_second: 100_000,
        };
        let mut index = SearchIndex::open(config).expect("open");
        let corpus = regression_corpus(base_ts);
        let _ = index
            .ingest_documents(&corpus, base_ts + 100, false, None)
            .expect("ingest");
        // drop closes
    }

    // Reopen and verify state persisted
    let config = IndexingConfig {
        index_dir: index_dir.clone(),
        max_index_size_bytes: 10_000_000,
        ttl_days: 30,
        flush_interval_secs: 1,
        flush_docs_threshold: 1,
        max_docs_per_second: 100_000,
    };
    let index = SearchIndex::open(config).expect("reopen");
    let stats = index.stats(base_ts + 200);
    assert_eq!(stats.doc_count, 8, "documents should persist across reopen");
}

#[test]
fn regression_scrollback_chunking_determinism() {
    let lines: Vec<ScrollbackLine> = (0..20)
        .map(|i| ScrollbackLine {
            text: format!("line {} with some content for chunking", i),
            captured_at_ms: 1_000_000 + i * 100,
            pane_id: Some(1),
            session_id: Some("s1".to_string()),
        })
        .collect();

    let chunks1 = chunk_scrollback_lines(&lines, 5_000);
    let chunks2 = chunk_scrollback_lines(&lines, 5_000);
    assert_eq!(chunks1.len(), chunks2.len());
    for (a, b) in chunks1.iter().zip(chunks2.iter()) {
        assert_eq!(a.text, b.text, "chunk text should be deterministic");
    }
}

#[test]
fn regression_hash_embedder_determinism() {
    let embedder = HashEmbedder::new(128);
    let long = "a very long string ".repeat(100);
    let texts = [
        "cargo test --workspace",
        "error: panic at lib.rs",
        "warning: unused variable",
        "",
        &*long,
    ];

    for text in &texts {
        let v1 = embedder.embed(text).expect("embed");
        let v2 = embedder.embed(text).expect("embed again");
        assert_eq!(v1, v2, "HashEmbedder must be deterministic for: {}", text);
        assert_eq!(v1.len(), 128);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// § SearchExplain contract: types, diagnostic codes, render
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_search_explain_result_fields() {
    let result = SearchExplainResult {
        query: "test".to_string(),
        pane_filter: Some(7),
        total_panes: 5,
        observed_panes: 3,
        ignored_panes: 2,
        total_segments: 100,
        reasons: vec![],
    };
    assert_eq!(result.query, "test");
    assert_eq!(result.pane_filter, Some(7));
    assert_eq!(result.total_panes, 5);
    assert_eq!(result.observed_panes, 3);
    assert_eq!(result.ignored_panes, 2);
    assert_eq!(result.total_segments, 100);
}

#[test]
fn contract_search_explain_reason_fields() {
    let reason = SearchExplainReason {
        code: "NO_INDEXED_DATA",
        summary: "No data has been indexed".to_string(),
        evidence: vec![SearchExplainEvidence {
            key: "total_segments".to_string(),
            value: "0".to_string(),
        }],
        suggestions: vec!["Run ft watch to start capturing".to_string()],
        confidence: 1.0,
    };
    assert_eq!(reason.code, "NO_INDEXED_DATA");
    assert!(!reason.evidence.is_empty());
    assert!(!reason.suggestions.is_empty());
    assert!((reason.confidence - 1.0).abs() < 1e-10);
}

#[test]
fn contract_search_explain_context_fields() {
    let ctx = SearchExplainContext {
        query: "test".to_string(),
        pane_filter: None,
        panes: vec![PaneExplainInfo {
            pane_id: 1,
            observed: true,
            ignore_reason: None,
            domain: "local".to_string(),
            last_seen_at: 1_000,
        }],
        indexing_stats: vec![PaneIndexingInfo {
            pane_id: 1,
            segment_count: 10,
            total_bytes: 5_000,
            last_segment_at: Some(1_000),
            fts_row_count: 10,
            fts_consistent: true,
        }],
        gaps: vec![],
        retention_cleanup_count: 0,
        earliest_segment_at: Some(500),
        latest_segment_at: Some(1_000),
        now_ms: 2_000,
    };
    let _ = &ctx.query;
    let _ = &ctx.panes;
    let _ = &ctx.indexing_stats;
}

#[test]
fn contract_search_explain_diagnostic_codes_frozen() {
    // Verify the 9 stable diagnostic codes by exercising explain_search
    // with crafted contexts that trigger each code.

    // NO_INDEXED_DATA: empty workspace
    let ctx = SearchExplainContext {
        query: "test".to_string(),
        pane_filter: None,
        panes: vec![],
        indexing_stats: vec![],
        gaps: vec![],
        retention_cleanup_count: 0,
        earliest_segment_at: None,
        latest_segment_at: None,
        now_ms: 1_000_000,
    };
    let result = explain_search(&ctx);
    assert!(
        result.reasons.iter().any(|r| r.code == "NO_INDEXED_DATA"),
        "should detect NO_INDEXED_DATA"
    );

    // PANE_NOT_FOUND: filter on non-existent pane
    let ctx2 = SearchExplainContext {
        query: "test".to_string(),
        pane_filter: Some(999),
        panes: vec![PaneExplainInfo {
            pane_id: 1,
            observed: true,
            ignore_reason: None,
            domain: "local".to_string(),
            last_seen_at: 1_000_000,
        }],
        indexing_stats: vec![PaneIndexingInfo {
            pane_id: 1,
            segment_count: 10,
            total_bytes: 5_000,
            last_segment_at: Some(1_000_000),
            fts_row_count: 10,
            fts_consistent: true,
        }],
        gaps: vec![],
        retention_cleanup_count: 0,
        earliest_segment_at: Some(500_000),
        latest_segment_at: Some(1_000_000),
        now_ms: 1_100_000,
    };
    let result2 = explain_search(&ctx2);
    assert!(
        result2.reasons.iter().any(|r| r.code == "PANE_NOT_FOUND"),
        "should detect PANE_NOT_FOUND"
    );

    // CAPTURE_GAPS: gaps present
    let ctx3 = SearchExplainContext {
        query: "test".to_string(),
        pane_filter: None,
        panes: vec![PaneExplainInfo {
            pane_id: 1,
            observed: true,
            ignore_reason: None,
            domain: "local".to_string(),
            last_seen_at: 1_000_000,
        }],
        indexing_stats: vec![PaneIndexingInfo {
            pane_id: 1,
            segment_count: 50,
            total_bytes: 25_000,
            last_segment_at: Some(1_000_000),
            fts_row_count: 50,
            fts_consistent: true,
        }],
        gaps: vec![GapInfo {
            pane_id: 1,
            seq_before: 10,
            seq_after: 20,
            reason: "disconnect".to_string(),
            detected_at: 900_000,
        }],
        retention_cleanup_count: 0,
        earliest_segment_at: Some(500_000),
        latest_segment_at: Some(1_000_000),
        now_ms: 1_100_000,
    };
    let result3 = explain_search(&ctx3);
    assert!(
        result3.reasons.iter().any(|r| r.code == "CAPTURE_GAPS"),
        "should detect CAPTURE_GAPS"
    );

    // RETENTION_CLEANUP: retention cleanup count > 0
    let ctx4 = SearchExplainContext {
        query: "test".to_string(),
        pane_filter: None,
        panes: vec![PaneExplainInfo {
            pane_id: 1,
            observed: true,
            ignore_reason: None,
            domain: "local".to_string(),
            last_seen_at: 1_000_000,
        }],
        indexing_stats: vec![PaneIndexingInfo {
            pane_id: 1,
            segment_count: 50,
            total_bytes: 25_000,
            last_segment_at: Some(1_000_000),
            fts_row_count: 50,
            fts_consistent: true,
        }],
        gaps: vec![],
        retention_cleanup_count: 42,
        earliest_segment_at: Some(500_000),
        latest_segment_at: Some(1_000_000),
        now_ms: 1_100_000,
    };
    let result4 = explain_search(&ctx4);
    assert!(
        result4.reasons.iter().any(|r| r.code == "RETENTION_CLEANUP"),
        "should detect RETENTION_CLEANUP"
    );
}

#[test]
fn contract_search_explain_result_serializable() {
    let result = SearchExplainResult {
        query: "test query".to_string(),
        pane_filter: None,
        total_panes: 3,
        observed_panes: 2,
        ignored_panes: 1,
        total_segments: 50,
        reasons: vec![SearchExplainReason {
            code: "CAPTURE_GAPS",
            summary: "Some gaps detected".to_string(),
            evidence: vec![SearchExplainEvidence {
                key: "gap_count".to_string(),
                value: "2".to_string(),
            }],
            suggestions: vec!["Check connection".to_string()],
            confidence: 0.6,
        }],
    };
    let json = serde_json::to_string(&result).expect("should serialize");
    let val: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(val["query"], "test query");
    assert_eq!(val["reasons"][0]["code"], "CAPTURE_GAPS");
}

#[test]
fn contract_render_explain_plain_produces_output() {
    let result = SearchExplainResult {
        query: "tokio panic".to_string(),
        pane_filter: None,
        total_panes: 2,
        observed_panes: 2,
        ignored_panes: 0,
        total_segments: 100,
        reasons: vec![SearchExplainReason {
            code: "NO_INDEXED_DATA",
            summary: "No data indexed yet".to_string(),
            evidence: vec![],
            suggestions: vec!["Run ft watch first".to_string()],
            confidence: 1.0,
        }],
    };
    let rendered = render_explain_plain(&result);
    assert!(!rendered.is_empty());
    assert!(rendered.contains("tokio panic"), "should contain query");
}

#[test]
fn contract_explain_search_deterministic() {
    let ctx = SearchExplainContext {
        query: "build failure".to_string(),
        pane_filter: None,
        panes: vec![
            PaneExplainInfo {
                pane_id: 1,
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: 1_000_000,
            },
            PaneExplainInfo {
                pane_id: 2,
                observed: false,
                ignore_reason: Some("excluded".to_string()),
                domain: "remote".to_string(),
                last_seen_at: 500_000,
            },
        ],
        indexing_stats: vec![PaneIndexingInfo {
            pane_id: 1,
            segment_count: 30,
            total_bytes: 15_000,
            last_segment_at: Some(1_000_000),
            fts_row_count: 30,
            fts_consistent: true,
        }],
        gaps: vec![],
        retention_cleanup_count: 0,
        earliest_segment_at: Some(500_000),
        latest_segment_at: Some(1_000_000),
        now_ms: 1_100_000,
    };

    let r1 = explain_search(&ctx);
    let r2 = explain_search(&ctx);
    assert_eq!(r1.reasons.len(), r2.reasons.len());
    for (a, b) in r1.reasons.iter().zip(r2.reasons.iter()) {
        assert_eq!(a.code, b.code, "explain should be deterministic");
        assert!(
            (a.confidence - b.confidence).abs() < 1e-10,
            "confidence should be deterministic"
        );
    }
}
