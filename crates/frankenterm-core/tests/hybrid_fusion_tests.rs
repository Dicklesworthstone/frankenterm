//! Integration tests for RRF fusion, two-tier blending, and kendall tau.
//!
//! Tests cross-module interactions between hybrid_search components and
//! verifies that the fusion pipeline produces correct rankings.

use frankenterm_core::search::{
    blend_two_tier, kendall_tau, rrf_fuse, FusedResult, HybridSearchService, SearchMode,
    TwoTierMetrics,
};

// =============================================================================
// RRF Fusion Integration Tests
// =============================================================================

#[test]
fn rrf_fusion_preserves_all_unique_ids() {
    let lexical = vec![(1u64, 0.9f32), (2, 0.8), (3, 0.7)];
    let semantic = vec![(3u64, 0.95f32), (4, 0.85), (5, 0.75)];

    let fused = rrf_fuse(&lexical, &semantic, 60);

    let ids: Vec<u64> = fused.iter().map(|r| r.id).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
    assert!(ids.contains(&4));
    assert!(ids.contains(&5));
    assert_eq!(ids.len(), 5);
}

#[test]
fn rrf_fusion_ranks_overlap_higher() {
    let lexical = vec![(1u64, 0.5f32), (3, 0.4)];
    let semantic = vec![(3u64, 0.9f32), (2, 0.3)];

    let fused = rrf_fuse(&lexical, &semantic, 60);

    assert_eq!(fused[0].id, 3, "overlapping item should rank first");
    assert!(fused[0].lexical_rank.is_some());
    assert!(fused[0].semantic_rank.is_some());
}

#[test]
fn rrf_k_parameter_affects_score_distribution() {
    let lexical = vec![(1u64, 0.9f32), (2, 0.5)];
    let semantic = vec![(1u64, 0.8f32), (3, 0.6)];

    let fused_k10 = rrf_fuse(&lexical, &semantic, 10);
    let fused_k100 = rrf_fuse(&lexical, &semantic, 100);

    // Both should have same top item
    assert_eq!(fused_k10[0].id, fused_k100[0].id);
}

#[test]
fn rrf_empty_inputs() {
    let empty: Vec<(u64, f32)> = vec![];
    let non_empty = vec![(1u64, 0.5f32)];

    let fused = rrf_fuse(&empty, &empty, 60);
    assert!(fused.is_empty());

    let fused = rrf_fuse(&non_empty, &empty, 60);
    assert_eq!(fused.len(), 1);
    assert_eq!(fused[0].id, 1);

    let fused = rrf_fuse(&empty, &non_empty, 60);
    assert_eq!(fused.len(), 1);
}

#[test]
fn rrf_score_monotonically_decreases() {
    let lexical: Vec<(u64, f32)> = (0..20).map(|i| (i as u64, 1.0 - i as f32 * 0.05)).collect();
    let semantic: Vec<(u64, f32)> =
        (10..30).map(|i| (i as u64, 1.0 - (i - 10) as f32 * 0.05)).collect();

    let fused = rrf_fuse(&lexical, &semantic, 60);

    for window in fused.windows(2) {
        assert!(
            window[0].score >= window[1].score,
            "scores should be non-increasing: {} >= {}",
            window[0].score,
            window[1].score
        );
    }
}

// =============================================================================
// Two-Tier Blending Tests
// =============================================================================

fn make_fused(items: &[(u64, f32)]) -> Vec<FusedResult> {
    items
        .iter()
        .enumerate()
        .map(|(rank, &(id, score))| FusedResult {
            id,
            score,
            lexical_rank: Some(rank),
            semantic_rank: None,
        })
        .collect()
}

#[test]
fn blend_two_tier_combines_scores() {
    let tier1 = make_fused(&[(1, 0.9), (2, 0.7), (3, 0.5)]);
    let tier2 = make_fused(&[(2, 0.95), (1, 0.8), (4, 0.6)]);

    let (blended, metrics) = blend_two_tier(&tier1, &tier2, 10, 0.7);

    let ids: Vec<u64> = blended.iter().map(|r| r.id).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
    assert!(ids.contains(&4));

    assert!(metrics.overlap_count > 0);
}

#[test]
fn blend_top_k_limits_output() {
    let tier1: Vec<FusedResult> = (0..100)
        .map(|i| FusedResult {
            id: i,
            score: 1.0 - i as f32 * 0.01,
            lexical_rank: Some(i as usize),
            semantic_rank: None,
        })
        .collect();
    let tier2: Vec<FusedResult> = vec![];

    let (blended, _) = blend_two_tier(&tier1, &tier2, 5, 0.7);
    assert!(blended.len() <= 5);
}

#[test]
fn blend_metrics_track_counts() {
    let tier1 = make_fused(&[(1, 0.9), (2, 0.8), (3, 0.7)]);
    let tier2 = make_fused(&[(2, 0.95), (3, 0.85), (4, 0.75)]);

    let (_, metrics) = blend_two_tier(&tier1, &tier2, 10, 0.7);

    // tier1 has 3 unique, tier2 fills with items not in tier1 (only id=4)
    assert!(metrics.tier1_count > 0);
    assert_eq!(metrics.overlap_count, 2); // IDs 2 and 3 overlap
}

// =============================================================================
// Kendall Tau Rank Correlation Tests
// =============================================================================

#[test]
fn kendall_tau_perfect_agreement() {
    let a = vec![1u64, 2, 3, 4, 5];
    let b = vec![1u64, 2, 3, 4, 5];

    let tau = kendall_tau(&a, &b);
    assert!(
        (tau - 1.0).abs() < 1e-6,
        "identical rankings should give tau=1.0, got {tau}"
    );
}

#[test]
fn kendall_tau_perfect_disagreement() {
    let a = vec![1u64, 2, 3, 4, 5];
    let b = vec![5u64, 4, 3, 2, 1];

    let tau = kendall_tau(&a, &b);
    assert!(
        (tau - (-1.0)).abs() < 1e-6,
        "reversed rankings should give tau=-1.0, got {tau}"
    );
}

#[test]
fn kendall_tau_partial_overlap() {
    let a = vec![1u64, 2, 3];
    let b = vec![2u64, 3, 4];

    let tau = kendall_tau(&a, &b);
    assert!(
        (tau - 1.0).abs() < 1e-6,
        "overlapping items in same order, got {tau}"
    );
}

#[test]
fn kendall_tau_no_overlap() {
    let a = vec![1u64, 2, 3];
    let b = vec![4u64, 5, 6];

    let tau = kendall_tau(&a, &b);
    assert!(tau.abs() < 1e-6, "no overlap should give 0, got {tau}");
}

#[test]
fn kendall_tau_single_overlap() {
    let a = vec![1u64, 2];
    let b = vec![2u64, 3];

    let tau = kendall_tau(&a, &b);
    assert!(tau.abs() < 1e-6, "single overlap has no pairs, got {tau}");
}

// =============================================================================
// HybridSearchService Integration Tests
// =============================================================================

#[test]
fn hybrid_service_fuse_hybrid_mode() {
    let service = HybridSearchService::new();

    let lexical = vec![(1u64, 0.9f32)];
    let semantic = vec![(2u64, 0.8f32)];

    let results = service.fuse(&lexical, &semantic, 10);
    assert!(!results.is_empty());
}

#[test]
fn hybrid_service_lexical_only() {
    let service = HybridSearchService::new().with_mode(SearchMode::Lexical);

    let lexical = vec![(1u64, 0.9f32), (2, 0.8)];
    let semantic = vec![(3u64, 0.95f32)];

    let results = service.fuse(&lexical, &semantic, 10);

    let ids: Vec<u64> = results.iter().map(|r| r.id).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(!ids.contains(&3));
}

#[test]
fn hybrid_service_semantic_only() {
    let service = HybridSearchService::new().with_mode(SearchMode::Semantic);

    let lexical = vec![(1u64, 0.9f32)];
    let semantic = vec![(2u64, 0.95f32), (3, 0.85)];

    let results = service.fuse(&lexical, &semantic, 10);

    let ids: Vec<u64> = results.iter().map(|r| r.id).collect();
    assert!(!ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
}

#[test]
fn hybrid_service_with_custom_rrf_k() {
    let service = HybridSearchService::new().with_rrf_k(10);

    let lexical = vec![(1u64, 0.9f32), (2, 0.5)];
    let semantic = vec![(1u64, 0.8f32), (3, 0.6)];

    let results = service.fuse(&lexical, &semantic, 10);
    assert!(!results.is_empty());
    assert_eq!(results[0].id, 1);
}

// =============================================================================
// FusedResult Property Tests
// =============================================================================

#[test]
fn fused_result_has_both_ranks_for_overlap() {
    let lexical = vec![(10u64, 0.9f32), (20, 0.8)];
    let semantic = vec![(20u64, 0.95f32), (30, 0.7)];

    let fused = rrf_fuse(&lexical, &semantic, 60);

    let r20 = fused.iter().find(|r| r.id == 20).unwrap();
    assert!(r20.lexical_rank.is_some(), "should have lexical rank");
    assert!(r20.semantic_rank.is_some(), "should have semantic rank");

    let r10 = fused.iter().find(|r| r.id == 10).unwrap();
    assert!(r10.lexical_rank.is_some());
    assert!(r10.semantic_rank.is_none());

    let r30 = fused.iter().find(|r| r.id == 30).unwrap();
    assert!(r30.lexical_rank.is_none());
    assert!(r30.semantic_rank.is_some());
}

#[test]
fn fused_result_scores_are_positive() {
    let lexical = vec![(1u64, 0.5f32), (2, 0.3)];
    let semantic = vec![(2u64, 0.7f32), (3, 0.4)];

    let fused = rrf_fuse(&lexical, &semantic, 60);

    for r in &fused {
        assert!(r.score > 0.0, "RRF scores should be positive, got {}", r.score);
    }
}

// =============================================================================
// End-to-End Pipeline Tests
// =============================================================================

#[test]
fn full_hybrid_pipeline() {
    let bm25_results = vec![
        (100u64, 12.5f32),
        (200, 8.3),
        (300, 5.1),
        (400, 3.2),
    ];

    let semantic_results = vec![
        (200u64, 0.92f32),
        (500, 0.88),
        (100, 0.85),
        (600, 0.72),
    ];

    let service = HybridSearchService::new().with_rrf_k(60);
    let results = service.fuse(&bm25_results, &semantic_results, 10);

    let top_ids: Vec<u64> = results.iter().take(2).map(|r| r.id).collect();
    assert!(
        top_ids.contains(&100) || top_ids.contains(&200),
        "overlap items should be in top results: got {:?}",
        top_ids
    );

    assert_eq!(results.len(), 6);

    for window in results.windows(2) {
        assert!(window[0].score >= window[1].score);
    }
}

#[test]
fn two_tier_blending_end_to_end() {
    // Simulate fast tier RRF results
    let fast_fused = rrf_fuse(
        &[(100, 0.9), (200, 0.8), (300, 0.7), (400, 0.6)],
        &[(200, 0.95), (100, 0.88), (500, 0.82), (300, 0.60)],
        60,
    );

    // Simulate quality tier RRF results
    let quality_fused = rrf_fuse(
        &[(200, 0.95), (100, 0.85), (300, 0.5)],
        &[(200, 0.98), (500, 0.9), (100, 0.82)],
        60,
    );

    let (blended, metrics) = blend_two_tier(&fast_fused, &quality_fused, 5, 0.7);

    assert!(!blended.is_empty());
    assert!(blended.len() <= 5);
    assert!(metrics.overlap_count > 0);
}
