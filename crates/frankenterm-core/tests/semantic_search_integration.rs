//! End-to-end integration tests for the semantic search pipeline.
//!
//! Tests the full flow: embed → index → search → verify results.
//! Uses the HashEmbedder (always available, no ML deps required).

use frankenterm_core::search::{
    Embedder, EmbedderTier, FtviIndex, FusedResult,
    HashEmbedder, HybridSearchService, blend_two_tier,
    write_ftvi_vec,
};

/// Helper: embed texts and build an FTVI index.
fn embed_and_index(embedder: &HashEmbedder, segments: &[(u64, &str)]) -> (Vec<(u64, Vec<f32>)>, FtviIndex) {
    let dim = embedder.dimension() as u16;
    let embeddings: Vec<(u64, Vec<f32>)> = segments
        .iter()
        .map(|(id, text)| (*id, embedder.embed(text).unwrap()))
        .collect();

    let records: Vec<(u64, &[f32])> = embeddings
        .iter()
        .map(|(id, v)| (*id, v.as_slice()))
        .collect();

    let buf = write_ftvi_vec(dim, &records).unwrap();
    let index = FtviIndex::from_bytes(&buf).unwrap();
    (embeddings, index)
}

// =============================================================================
// HashEmbedder → FTVI → Search Pipeline
// =============================================================================

#[test]
fn end_to_end_hash_embed_index_search() {
    let embedder = HashEmbedder::new(64);

    let segments = vec![
        (1u64, "error: compilation failed with 3 errors"),
        (2, "warning: unused variable `x` in main.rs"),
        (3, "test result: ok. 42 passed; 0 failed"),
        (4, "error: cannot find module `database`"),
        (5, "build succeeded in 2.3 seconds"),
    ];

    let (_, index) = embed_and_index(&embedder, &segments);
    assert_eq!(index.len(), 5);

    // Search for "error compilation"
    let query_vec = embedder.embed("error compilation").unwrap();
    let results = index.search(&query_vec, 3);

    let top_ids: Vec<u64> = results.iter().map(|&(id, _)| id).collect();
    assert!(
        top_ids.contains(&1) || top_ids.contains(&4),
        "error-related segments should appear in top results: got {:?}",
        top_ids
    );
}

#[test]
fn hash_embedder_consistent_across_embeds() {
    let embedder = HashEmbedder::new(32);

    let v1 = embedder.embed("hello world").unwrap();
    let v2 = embedder.embed("hello world").unwrap();

    assert_eq!(v1, v2, "same input should produce same embedding");
}

#[test]
fn hash_embedder_different_inputs_differ() {
    let embedder = HashEmbedder::new(32);

    let v1 = embedder.embed("error compilation").unwrap();
    let v2 = embedder.embed("test passed successfully").unwrap();

    assert_ne!(v1, v2, "different inputs should produce different embeddings");
}

#[test]
fn hash_embedder_produces_unit_vectors() {
    let embedder = HashEmbedder::new(64);

    let v = embedder.embed("some terminal output content").unwrap();

    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 0.01,
        "embedding should be L2-normalized, got norm={norm}"
    );
}

#[test]
fn hash_embedder_batch_matches_single() {
    let embedder = HashEmbedder::new(48);

    let texts = vec!["first segment", "second segment", "third segment"];

    let batch_results = embedder.embed_batch(&texts).unwrap();
    assert_eq!(batch_results.len(), 3);

    for (i, text) in texts.iter().enumerate() {
        let single = embedder.embed(text).unwrap();
        assert_eq!(
            batch_results[i], single,
            "batch[{i}] should match single embed"
        );
    }
}

// =============================================================================
// Embedder Trait Interface Tests
// =============================================================================

#[test]
fn hash_embedder_info() {
    let embedder = HashEmbedder::new(128);
    let info = embedder.info();

    assert_eq!(info.name, "fnv1a-hash-128");
    assert_eq!(info.dimension, 128);
    assert_eq!(info.tier, EmbedderTier::Hash);
}

#[test]
fn hash_embedder_dimension() {
    let embedder = HashEmbedder::new(256);
    assert_eq!(embedder.dimension(), 256);
}

#[test]
fn hash_embedder_tier() {
    let embedder = HashEmbedder::new(64);
    assert_eq!(embedder.tier(), EmbedderTier::Hash);
}

// =============================================================================
// Full Pipeline: Embed → Index → Hybrid Search
// =============================================================================

#[test]
fn full_pipeline_with_hybrid_search() {
    let embedder = HashEmbedder::new(32);

    // Simulate BM25 results (lexical)
    let bm25_results = vec![
        (1u64, 10.5f32),
        (2, 8.2),
        (3, 5.0),
    ];

    let segments = vec![
        (1u64, "npm install failed with ENOENT"),
        (2, "package install error EPERM"),
        (3, "building dependencies from lock file"),
        (4, "yarn add package-name completed"),
        (5, "dependency resolution failed"),
    ];

    let (_, index) = embed_and_index(&embedder, &segments);

    let query_vec = embedder.embed("install failed").unwrap();
    let semantic_results: Vec<(u64, f32)> = index.search(&query_vec, 5);

    let service = HybridSearchService::new().with_rrf_k(60);
    let fused = service.fuse(&bm25_results, &semantic_results, 10);

    assert!(!fused.is_empty(), "fused results should not be empty");

    let top_3_ids: Vec<u64> = fused.iter().take(3).map(|r| r.id).collect();
    assert!(
        top_3_ids.contains(&1),
        "ID 1 should be in top 3 since it matches both lexical and semantic: got {:?}",
        top_3_ids
    );
}

#[test]
fn two_tier_pipeline() {
    let fast_embedder = HashEmbedder::new(32);
    let quality_embedder = HashEmbedder::new(64);

    let texts = vec![
        (1u64, "fatal error: segmentation fault"),
        (2, "warning: memory leak detected"),
        (3, "info: process started successfully"),
        (4, "error: stack overflow in thread main"),
        (5, "debug: allocating 1024 bytes"),
    ];

    let (_, fast_index) = embed_and_index(&fast_embedder, &texts);
    let (_, qual_index) = embed_and_index(&quality_embedder, &texts);

    let fast_query = fast_embedder.embed("error crash").unwrap();
    let fast_results = fast_index.search(&fast_query, 5);

    let qual_query = quality_embedder.embed("error crash").unwrap();
    let qual_results = qual_index.search(&qual_query, 5);

    // Convert to FusedResult for blend_two_tier
    let fast_fused: Vec<FusedResult> = fast_results
        .iter()
        .enumerate()
        .map(|(rank, &(id, score))| FusedResult {
            id,
            score,
            lexical_rank: Some(rank),
            semantic_rank: None,
        })
        .collect();

    let qual_fused: Vec<FusedResult> = qual_results
        .iter()
        .enumerate()
        .map(|(rank, &(id, score))| FusedResult {
            id,
            score,
            lexical_rank: None,
            semantic_rank: Some(rank),
        })
        .collect();

    let (blended, metrics) = blend_two_tier(&fast_fused, &qual_fused, 5, 0.7);

    assert!(!blended.is_empty());
    assert!(metrics.tier1_count > 0 || metrics.tier2_count > 0);
}

// =============================================================================
// Error Handling
// =============================================================================

#[test]
fn hash_embedder_empty_input() {
    let embedder = HashEmbedder::new(32);
    let v = embedder.embed("").unwrap();
    assert_eq!(v.len(), 32);
}

#[test]
fn hash_embedder_unicode_input() {
    let embedder = HashEmbedder::new(32);

    let v1 = embedder.embed("日本語テスト").unwrap();
    let v2 = embedder.embed("中文测试").unwrap();

    assert_eq!(v1.len(), 32);
    assert_eq!(v2.len(), 32);
    assert_ne!(v1, v2);
}

#[test]
fn hash_embedder_very_long_input() {
    let embedder = HashEmbedder::new(64);

    let long_text = "x".repeat(100_000);
    let v = embedder.embed(&long_text).unwrap();

    assert_eq!(v.len(), 64);
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 0.01);
}

// =============================================================================
// Search Quality Regression Tests
// =============================================================================

#[test]
fn similar_content_scores_higher_than_dissimilar() {
    let embedder = HashEmbedder::new(64);

    let query = embedder.embed("compilation error in main.rs").unwrap();
    let similar = embedder.embed("compilation failed in main.rs line 42").unwrap();
    let dissimilar = embedder.embed("network timeout connecting to database").unwrap();

    let sim_score: f32 = query.iter().zip(similar.iter()).map(|(a, b)| a * b).sum();
    let dis_score: f32 = query.iter().zip(dissimilar.iter()).map(|(a, b)| a * b).sum();

    assert!(
        sim_score > dis_score,
        "similar content should score higher: sim={sim_score} > dis={dis_score}"
    );
}
