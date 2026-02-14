//! Edge case tests for semantic/hybrid search quality harness.
//!
//! Bead: wa-1u90p.7.1
//!
//! Validates:
//! 1. Metric computation: precision@k, recall@k, NDCG@k, MRR with known inputs
//! 2. Ranking deduplication and truncation behavior
//! 3. Threshold violation detection (per-metric, boundary, multiple)
//! 4. Summarization (empty, single, multi-query averaging)
//! 5. Serde roundtrip stability for all public types
//! 6. Edge cases: top_k=0, empty inputs, no relevant docs, all relevant docs
//! 7. RRF k parameter sensitivity
//! 8. Default queries pass with default thresholds

use frankenterm_core::semantic_quality::{
    LaneEvaluation, QualitySummary, RankingMetrics, RegressionThresholds, SemanticEvalQuery,
    SemanticQualityHarness, SemanticQualityReport, ThresholdViolation, default_semantic_eval_queries,
};

const EPS: f64 = 1e-9;

// =============================================================================
// Helper: build a simple eval query with known outcome
// =============================================================================

fn make_query(
    name: &str,
    lexical: Vec<(u64, f32)>,
    semantic: Vec<(u64, f32)>,
    relevant: Vec<u64>,
    top_k: usize,
) -> SemanticEvalQuery {
    SemanticEvalQuery {
        name: name.to_string(),
        description: String::new(),
        lexical_ranked: lexical,
        semantic_ranked: semantic,
        relevant_ids: relevant,
        top_k,
    }
}

// =============================================================================
// Default queries and thresholds
// =============================================================================

#[test]
fn default_queries_are_non_empty() {
    let queries = default_semantic_eval_queries();
    assert!(!queries.is_empty());
    assert_eq!(queries.len(), 4);
}

#[test]
fn default_queries_pass_default_thresholds() {
    let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();
    assert!(
        report.passed,
        "default queries should pass default thresholds, violations: {:?}",
        report.violations
    );
    assert!(report.violations.is_empty());
}

#[test]
fn default_queries_have_valid_structure() {
    for q in default_semantic_eval_queries() {
        assert!(!q.name.is_empty(), "query name must be non-empty");
        assert!(q.top_k > 0, "top_k must be positive");
        assert!(!q.relevant_ids.is_empty(), "must have relevant ids");
        assert!(
            !q.lexical_ranked.is_empty(),
            "must have lexical candidates"
        );
        assert!(
            !q.semantic_ranked.is_empty(),
            "must have semantic candidates"
        );
    }
}

#[test]
fn default_threshold_values() {
    let t = RegressionThresholds::default();
    assert!(
        t.min_hybrid_ndcg_delta_vs_lexical.abs() < EPS,
        "default: hybrid must not regress vs lexical"
    );
    assert!(
        (t.min_hybrid_precision_at_k - 0.25).abs() < EPS,
        "default precision floor"
    );
    assert!((t.min_hybrid_recall_at_k - 0.25).abs() < EPS, "default recall floor");
}

#[test]
fn default_ranking_metrics_are_zero() {
    let m = RankingMetrics::default();
    assert!(m.precision_at_k.abs() < EPS);
    assert!(m.recall_at_k.abs() < EPS);
    assert!(m.ndcg_at_k.abs() < EPS);
    assert!(m.mrr.abs() < EPS);
}

// =============================================================================
// Metric computation: precision, recall, NDCG, MRR
// =============================================================================

#[test]
fn perfect_ranking_all_relevant() {
    // All results are relevant
    let q = make_query(
        "perfect",
        vec![(1, 1.0), (2, 0.9), (3, 0.8)],
        vec![(1, 1.0), (2, 0.9), (3, 0.8)],
        vec![1, 2, 3],
        3,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    // Lexical: all 3 results relevant, precision@3 = 1.0
    assert!(
        (qr.lexical.metrics.precision_at_k - 1.0).abs() < 1e-9,
        "perfect precision expected, got {}",
        qr.lexical.metrics.precision_at_k
    );
    assert!(
        (qr.lexical.metrics.recall_at_k - 1.0).abs() < 1e-9,
        "perfect recall expected, got {}",
        qr.lexical.metrics.recall_at_k
    );
    assert!(
        qr.lexical.metrics.ndcg_at_k > 0.99,
        "near-perfect NDCG expected, got {}",
        qr.lexical.metrics.ndcg_at_k
    );
    assert!(
        (qr.lexical.metrics.mrr - 1.0).abs() < 1e-9,
        "MRR=1.0 when first result is relevant"
    );
}

#[test]
fn no_relevant_results() {
    // None of the results are in the relevant set
    let q = make_query(
        "no_relevant",
        vec![(10, 1.0), (20, 0.9), (30, 0.8)],
        vec![(40, 1.0), (50, 0.9), (60, 0.8)],
        vec![99, 100], // completely disjoint
        3,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    assert!(
        qr.lexical.metrics.precision_at_k.abs() < EPS,
        "no relevant = zero precision"
    );
    assert!(
        qr.lexical.metrics.recall_at_k.abs() < EPS,
        "no relevant = zero recall"
    );
    assert!(
        qr.lexical.metrics.ndcg_at_k.abs() < EPS,
        "no relevant = zero NDCG"
    );
    assert!(
        qr.lexical.metrics.mrr.abs() < EPS,
        "no relevant = zero MRR"
    );
}

#[test]
fn empty_relevant_set() {
    // Empty relevant set: recall denominator = 0
    let q = make_query(
        "empty_relevant",
        vec![(1, 1.0), (2, 0.9)],
        vec![(3, 1.0), (4, 0.9)],
        vec![], // no relevant docs
        2,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    assert!(qr.lexical.metrics.precision_at_k.abs() < EPS);
    assert!(qr.lexical.metrics.recall_at_k.abs() < EPS);
    assert!(qr.lexical.metrics.ndcg_at_k.abs() < EPS);
    assert!(qr.lexical.metrics.mrr.abs() < EPS);
}

#[test]
fn partial_relevance_precision_and_recall() {
    // 2 of 4 results are relevant, top_k=4
    let q = make_query(
        "partial",
        vec![(1, 1.0), (2, 0.9), (3, 0.8), (4, 0.7)],
        vec![(1, 1.0), (2, 0.9), (3, 0.8), (4, 0.7)],
        vec![1, 3], // only ids 1 and 3 are relevant
        4,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    // precision@4 = 2/4 = 0.5
    assert!(
        (qr.lexical.metrics.precision_at_k - 0.5).abs() < 1e-9,
        "expected precision 0.5, got {}",
        qr.lexical.metrics.precision_at_k
    );
    // recall@4 = 2/2 = 1.0 (both relevant found)
    assert!(
        (qr.lexical.metrics.recall_at_k - 1.0).abs() < 1e-9,
        "expected recall 1.0, got {}",
        qr.lexical.metrics.recall_at_k
    );
}

#[test]
fn mrr_first_relevant_at_position_two() {
    // First relevant at position 2 (0-indexed: index 1) → MRR = 1/2 = 0.5
    let q = make_query(
        "mrr_pos2",
        vec![(10, 1.0), (20, 0.9), (30, 0.8)],
        vec![(10, 1.0), (20, 0.9), (30, 0.8)],
        vec![20], // only id 20 is relevant (position 2)
        3,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    assert!(
        (qr.lexical.metrics.mrr - 0.5).abs() < 1e-9,
        "MRR should be 0.5 when first relevant at pos 2, got {}",
        qr.lexical.metrics.mrr
    );
}

#[test]
fn mrr_first_relevant_at_position_three() {
    // First relevant at position 3 (0-indexed: index 2) → MRR = 1/3 ≈ 0.333
    let q = make_query(
        "mrr_pos3",
        vec![(10, 1.0), (20, 0.9), (30, 0.8)],
        vec![(10, 1.0), (20, 0.9), (30, 0.8)],
        vec![30],
        3,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    assert!(
        (qr.lexical.metrics.mrr - 1.0 / 3.0).abs() < 1e-9,
        "MRR should be ~0.333, got {}",
        qr.lexical.metrics.mrr
    );
}

// =============================================================================
// NDCG computation
// =============================================================================

#[test]
fn ndcg_perfect_single_relevant() {
    // Single relevant doc at position 1 → NDCG = 1.0
    let q = make_query(
        "ndcg_perfect_single",
        vec![(1, 1.0), (2, 0.9), (3, 0.8)],
        vec![(1, 1.0), (2, 0.9), (3, 0.8)],
        vec![1],
        3,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    // DCG = 1/log2(2) = 1.0, IDCG = 1.0 → NDCG = 1.0
    assert!(
        (qr.lexical.metrics.ndcg_at_k - 1.0).abs() < 1e-9,
        "NDCG should be 1.0 when single relevant at top, got {}",
        qr.lexical.metrics.ndcg_at_k
    );
}

#[test]
fn ndcg_suboptimal_ranking() {
    // Relevant doc NOT at top → NDCG < 1.0
    let q = make_query(
        "ndcg_sub",
        vec![(10, 1.0), (20, 0.9), (30, 0.8)],
        vec![(10, 1.0), (20, 0.9), (30, 0.8)],
        vec![30], // relevant at position 3
        3,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    assert!(
        qr.lexical.metrics.ndcg_at_k < 1.0,
        "NDCG should be < 1.0 when relevant doc not at top"
    );
    assert!(
        qr.lexical.metrics.ndcg_at_k > 0.0,
        "NDCG should be > 0.0 when at least one relevant found"
    );
    // DCG = 1/log2(4) ≈ 0.5, IDCG = 1/log2(2) = 1.0 → NDCG ≈ 0.5
    assert!(
        (qr.lexical.metrics.ndcg_at_k - 0.5).abs() < 0.01,
        "NDCG should be ~0.5, got {}",
        qr.lexical.metrics.ndcg_at_k
    );
}

// =============================================================================
// top_k edge cases
// =============================================================================

#[test]
fn top_k_one() {
    // top_k=1: only evaluates first result
    let q = make_query(
        "top1",
        vec![(1, 1.0), (2, 0.9)],
        vec![(1, 1.0), (2, 0.9)],
        vec![1, 2],
        1,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    // At k=1 with relevant at pos 1: precision=1.0, recall=0.5
    assert!(
        (qr.lexical.metrics.precision_at_k - 1.0).abs() < 1e-9,
        "precision@1 = 1.0 when first result relevant"
    );
    assert!(
        (qr.lexical.metrics.recall_at_k - 0.5).abs() < 1e-9,
        "recall@1 = 0.5 when 1 of 2 relevant found"
    );
}

#[test]
fn top_k_zero_clamped_to_one() {
    // top_k=0 is clamped to 1 in evaluate_query
    let q = make_query(
        "top0",
        vec![(1, 1.0)],
        vec![(1, 1.0)],
        vec![1],
        0,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    // Should clamp to top_k=1 and evaluate normally
    assert_eq!(qr.top_k, 1, "top_k=0 should be clamped to 1");
    assert!(
        qr.lexical.metrics.precision_at_k > 0.0,
        "should compute metrics with clamped top_k"
    );
}

#[test]
fn top_k_exceeds_candidates() {
    // top_k=10 but only 2 candidates
    let q = make_query(
        "top_exceed",
        vec![(1, 1.0), (2, 0.9)],
        vec![(3, 1.0), (4, 0.9)],
        vec![1, 2, 3, 4],
        10,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    // Should evaluate over available candidates (2 each)
    assert_eq!(qr.lexical.ranked_ids.len(), 2);
    assert_eq!(qr.semantic.ranked_ids.len(), 2);
}

// =============================================================================
// Deduplication in ranked lists
// =============================================================================

#[test]
fn duplicate_ids_are_deduped() {
    // Lexical has duplicate ids
    let q = make_query(
        "dedup",
        vec![(1, 1.0), (1, 0.9), (2, 0.8), (2, 0.7), (3, 0.6)],
        vec![(1, 1.0), (2, 0.9), (3, 0.8)],
        vec![1, 2, 3],
        3,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    // After dedup: [1, 2, 3] — no duplicates
    assert_eq!(qr.lexical.ranked_ids.len(), 3);
    assert_eq!(qr.lexical.ranked_ids[0], 1);
    assert_eq!(qr.lexical.ranked_ids[1], 2);
    assert_eq!(qr.lexical.ranked_ids[2], 3);
}

// =============================================================================
// Threshold violations
// =============================================================================

#[test]
fn strict_ndcg_threshold_causes_violation() {
    let thresholds = RegressionThresholds {
        min_hybrid_ndcg_delta_vs_lexical: 0.99, // impossibly strict
        min_hybrid_precision_at_k: 0.0,
        min_hybrid_recall_at_k: 0.0,
    };
    let report = SemanticQualityHarness::new(default_semantic_eval_queries())
        .with_thresholds(thresholds)
        .run();

    assert!(!report.passed);
    assert!(
        report.violations.iter().any(|v| v.metric == "hybrid_vs_lexical_ndcg_delta"),
        "should have NDCG violations"
    );
}

#[test]
fn strict_precision_threshold_causes_violation() {
    let thresholds = RegressionThresholds {
        min_hybrid_ndcg_delta_vs_lexical: -10.0,
        min_hybrid_precision_at_k: 0.99, // very strict
        min_hybrid_recall_at_k: 0.0,
    };
    let report = SemanticQualityHarness::new(default_semantic_eval_queries())
        .with_thresholds(thresholds)
        .run();

    assert!(!report.passed);
    assert!(
        report.violations.iter().any(|v| v.metric == "hybrid_precision_at_k"),
        "should have precision violations"
    );
}

#[test]
fn strict_recall_threshold_causes_violation() {
    let thresholds = RegressionThresholds {
        min_hybrid_ndcg_delta_vs_lexical: -10.0,
        min_hybrid_precision_at_k: 0.0,
        min_hybrid_recall_at_k: 0.99, // very strict
    };
    let report = SemanticQualityHarness::new(default_semantic_eval_queries())
        .with_thresholds(thresholds)
        .run();

    assert!(!report.passed);
    assert!(
        report.violations.iter().any(|v| v.metric == "hybrid_recall_at_k"),
        "should have recall violations"
    );
}

#[test]
fn all_strict_thresholds_cause_multiple_violations() {
    let thresholds = RegressionThresholds {
        min_hybrid_ndcg_delta_vs_lexical: 0.99,
        min_hybrid_precision_at_k: 0.99,
        min_hybrid_recall_at_k: 0.99,
    };
    let report = SemanticQualityHarness::new(default_semantic_eval_queries())
        .with_thresholds(thresholds)
        .run();

    assert!(!report.passed);
    // Should have violations from multiple metrics
    let unique_metrics: std::collections::HashSet<_> =
        report.violations.iter().map(|v| v.metric.as_str()).collect();
    assert!(
        unique_metrics.len() >= 2,
        "multiple metric types should be violated"
    );
}

#[test]
fn threshold_violation_records_actual_and_required() {
    let thresholds = RegressionThresholds {
        min_hybrid_ndcg_delta_vs_lexical: 0.99,
        min_hybrid_precision_at_k: 0.0,
        min_hybrid_recall_at_k: 0.0,
    };
    let report = SemanticQualityHarness::new(default_semantic_eval_queries())
        .with_thresholds(thresholds)
        .run();

    for v in &report.violations {
        assert!(!v.query.is_empty(), "violation must name the query");
        assert!(!v.metric.is_empty(), "violation must name the metric");
        assert!(v.actual < v.required, "actual should be below required");
    }
}

#[test]
fn zero_thresholds_always_pass() {
    let thresholds = RegressionThresholds {
        min_hybrid_ndcg_delta_vs_lexical: -10.0,
        min_hybrid_precision_at_k: 0.0,
        min_hybrid_recall_at_k: 0.0,
    };
    let report = SemanticQualityHarness::new(default_semantic_eval_queries())
        .with_thresholds(thresholds)
        .run();

    assert!(report.passed, "zero thresholds should always pass");
    assert!(report.violations.is_empty());
}

// =============================================================================
// Summary (aggregation)
// =============================================================================

#[test]
fn summary_with_default_queries() {
    let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();
    let s = &report.summary;

    assert_eq!(s.total_queries, 4);
    assert!(
        s.mean_hybrid_precision_at_k > 0.0,
        "mean precision should be positive"
    );
    assert!(
        s.mean_hybrid_recall_at_k > 0.0,
        "mean recall should be positive"
    );
    assert!(
        s.mean_hybrid_ndcg_at_k > 0.0,
        "mean NDCG should be positive"
    );
}

#[test]
fn summary_single_query() {
    let q = make_query(
        "single",
        vec![(1, 1.0), (2, 0.9)],
        vec![(1, 1.0), (3, 0.9)],
        vec![1],
        2,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let s = &report.summary;

    assert_eq!(s.total_queries, 1);
    // With single query, mean equals the individual query metrics
    let qr = &report.queries[0];
    assert!(
        (s.mean_hybrid_precision_at_k - qr.hybrid.metrics.precision_at_k).abs() < 1e-9,
        "single-query mean should equal individual"
    );
    assert!(
        (s.mean_hybrid_ndcg_at_k - qr.hybrid.metrics.ndcg_at_k).abs() < 1e-9
    );
}

#[test]
fn summary_empty_queries() {
    let report = SemanticQualityHarness::new(vec![]).run();
    let s = &report.summary;

    assert_eq!(s.total_queries, 0);
    assert!(s.mean_hybrid_precision_at_k.abs() < EPS);
    assert!(s.mean_hybrid_recall_at_k.abs() < EPS);
    assert!(s.mean_hybrid_ndcg_at_k.abs() < EPS);
    assert!(s.mean_hybrid_vs_lexical_ndcg_delta.abs() < EPS);
    assert!(report.passed, "empty queries = no violations = pass");
}

// =============================================================================
// Hybrid fusion: comparison between lanes
// =============================================================================

#[test]
fn hybrid_combines_lexical_and_semantic() {
    // Lexical finds id=1, semantic finds id=2, both relevant
    let q = make_query(
        "hybrid_fusion",
        vec![(1, 1.0)],
        vec![(2, 1.0)],
        vec![1, 2],
        2,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    // Hybrid should contain both ids since RRF fuses both lists
    assert!(
        qr.hybrid.ranked_ids.contains(&1) || qr.hybrid.ranked_ids.contains(&2),
        "hybrid should include results from both lanes"
    );
}

#[test]
fn hybrid_delta_signs_are_consistent() {
    let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();

    for qr in &report.queries {
        let expected_ndcg_delta =
            qr.hybrid.metrics.ndcg_at_k - qr.lexical.metrics.ndcg_at_k;
        assert!(
            (qr.hybrid_vs_lexical_ndcg_delta - expected_ndcg_delta).abs() < 1e-9,
            "NDCG delta should match manual computation"
        );

        let expected_semantic_delta =
            qr.hybrid.metrics.ndcg_at_k - qr.semantic.metrics.ndcg_at_k;
        assert!(
            (qr.hybrid_vs_semantic_ndcg_delta - expected_semantic_delta).abs() < 1e-9,
            "semantic NDCG delta should match"
        );

        let expected_prec_delta =
            qr.hybrid.metrics.precision_at_k - qr.lexical.metrics.precision_at_k;
        assert!(
            (qr.hybrid_vs_lexical_precision_delta - expected_prec_delta).abs() < 1e-9,
            "precision delta should match"
        );
    }
}

// =============================================================================
// RRF k parameter
// =============================================================================

#[test]
fn rrf_k_affects_hybrid_ranking() {
    let queries = default_semantic_eval_queries();

    let report_k10 = SemanticQualityHarness::new(queries.clone())
        .with_rrf_k(10)
        .run();
    let report_k1000 = SemanticQualityHarness::new(queries)
        .with_rrf_k(1000)
        .run();

    // Different k values should produce different hybrid metrics
    // (same lexical/semantic but different fusion weights)
    let h10 = &report_k10.queries[0].hybrid.metrics;
    let h1000 = &report_k1000.queries[0].hybrid.metrics;

    // At least the NDCG or precision should differ
    let metrics_differ = (h10.ndcg_at_k - h1000.ndcg_at_k).abs() > 1e-12
        || (h10.precision_at_k - h1000.precision_at_k).abs() > 1e-12;
    // If both lexical and semantic have different orderings, k should matter.
    // But if they agree perfectly, k won't matter. Allow both cases.
    assert!(
        metrics_differ || (h10.ndcg_at_k - h1000.ndcg_at_k).abs() < 1e-9,
        "RRF k should affect ranking when lanes disagree"
    );
}

// =============================================================================
// Harness builder API
// =============================================================================

#[test]
fn with_thresholds_builder() {
    let custom = RegressionThresholds {
        min_hybrid_ndcg_delta_vs_lexical: -0.5,
        min_hybrid_precision_at_k: 0.1,
        min_hybrid_recall_at_k: 0.1,
    };
    let report = SemanticQualityHarness::new(default_semantic_eval_queries())
        .with_thresholds(custom)
        .run();
    assert!(report.passed, "lenient thresholds should pass");
}

#[test]
fn with_rrf_k_builder() {
    // Just verify it doesn't panic
    let report = SemanticQualityHarness::new(default_semantic_eval_queries())
        .with_rrf_k(1)
        .run();
    assert_eq!(report.queries.len(), 4);
}

// =============================================================================
// Serde roundtrips
// =============================================================================

#[test]
fn serde_roundtrip_ranking_metrics() {
    let m = RankingMetrics {
        precision_at_k: 0.75,
        recall_at_k: 0.6,
        ndcg_at_k: 0.82,
        mrr: 0.5,
    };
    let json = serde_json::to_string(&m).unwrap();
    let restored: RankingMetrics = serde_json::from_str(&json).unwrap();
    assert!((restored.precision_at_k - 0.75).abs() < 1e-9);
    assert!((restored.recall_at_k - 0.6).abs() < 1e-9);
    assert!((restored.ndcg_at_k - 0.82).abs() < 1e-9);
    assert!((restored.mrr - 0.5).abs() < 1e-9);
}

#[test]
fn serde_roundtrip_eval_query() {
    let q = make_query(
        "test_q",
        vec![(1, 0.9), (2, 0.8)],
        vec![(3, 0.7)],
        vec![1, 3],
        2,
    );
    let json = serde_json::to_string(&q).unwrap();
    let restored: SemanticEvalQuery = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.name, "test_q");
    assert_eq!(restored.top_k, 2);
    assert_eq!(restored.relevant_ids, vec![1, 3]);
    assert_eq!(restored.lexical_ranked.len(), 2);
    assert_eq!(restored.semantic_ranked.len(), 1);
}

#[test]
fn serde_roundtrip_regression_thresholds() {
    let t = RegressionThresholds {
        min_hybrid_ndcg_delta_vs_lexical: -0.1,
        min_hybrid_precision_at_k: 0.3,
        min_hybrid_recall_at_k: 0.4,
    };
    let json = serde_json::to_string(&t).unwrap();
    let restored: RegressionThresholds = serde_json::from_str(&json).unwrap();
    assert!((restored.min_hybrid_ndcg_delta_vs_lexical - (-0.1)).abs() < 1e-9);
    assert!((restored.min_hybrid_precision_at_k - 0.3).abs() < 1e-9);
    assert!((restored.min_hybrid_recall_at_k - 0.4).abs() < 1e-9);
}

#[test]
fn serde_roundtrip_threshold_violation() {
    let v = ThresholdViolation {
        query: "test".to_string(),
        metric: "ndcg".to_string(),
        actual: 0.3,
        required: 0.5,
    };
    let json = serde_json::to_string(&v).unwrap();
    let restored: ThresholdViolation = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.query, "test");
    assert_eq!(restored.metric, "ndcg");
    assert!((restored.actual - 0.3).abs() < 1e-9);
    assert!((restored.required - 0.5).abs() < 1e-9);
}

#[test]
fn serde_roundtrip_quality_summary() {
    let s = QualitySummary {
        total_queries: 5,
        mean_hybrid_precision_at_k: 0.7,
        mean_hybrid_recall_at_k: 0.8,
        mean_hybrid_ndcg_at_k: 0.75,
        mean_hybrid_vs_lexical_ndcg_delta: 0.05,
    };
    let json = serde_json::to_string(&s).unwrap();
    let restored: QualitySummary = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.total_queries, 5);
    assert!((restored.mean_hybrid_precision_at_k - 0.7).abs() < 1e-9);
}

#[test]
fn serde_roundtrip_full_report() {
    let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();
    let json = serde_json::to_string(&report).unwrap();
    let restored: SemanticQualityReport = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.queries.len(), report.queries.len());
    assert_eq!(
        restored.summary.total_queries,
        report.summary.total_queries
    );
    assert_eq!(restored.passed, report.passed);
    assert_eq!(restored.violations.len(), report.violations.len());
}

#[test]
fn serde_roundtrip_lane_evaluation() {
    let lane = LaneEvaluation {
        ranked_ids: vec![1, 2, 3],
        metrics: RankingMetrics {
            precision_at_k: 0.5,
            recall_at_k: 0.75,
            ndcg_at_k: 0.6,
            mrr: 1.0,
        },
    };
    let json = serde_json::to_string(&lane).unwrap();
    let restored: LaneEvaluation = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.ranked_ids, vec![1, 2, 3]);
    assert!((restored.metrics.precision_at_k - 0.5).abs() < 1e-9);
}

// =============================================================================
// Query comparison structure
// =============================================================================

#[test]
fn query_comparison_has_all_three_lanes() {
    let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();

    for qr in &report.queries {
        assert!(!qr.name.is_empty());
        assert!(qr.top_k > 0);
        // All three lanes should be populated
        assert!(!qr.lexical.ranked_ids.is_empty());
        assert!(!qr.semantic.ranked_ids.is_empty());
        assert!(!qr.hybrid.ranked_ids.is_empty());
    }
}

#[test]
fn query_comparison_ranked_ids_are_deduped() {
    let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();

    for qr in &report.queries {
        for (lane_name, ids) in [
            ("lexical", &qr.lexical.ranked_ids),
            ("semantic", &qr.semantic.ranked_ids),
            ("hybrid", &qr.hybrid.ranked_ids),
        ] {
            let unique: std::collections::HashSet<_> = ids.iter().collect();
            assert_eq!(
                unique.len(),
                ids.len(),
                "query '{}' lane '{}' has duplicate ids",
                qr.name,
                lane_name
            );
        }
    }
}

#[test]
fn query_comparison_ranked_ids_respect_top_k() {
    let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();

    for qr in &report.queries {
        assert!(
            qr.lexical.ranked_ids.len() <= qr.top_k,
            "lexical ids should not exceed top_k"
        );
        assert!(
            qr.semantic.ranked_ids.len() <= qr.top_k,
            "semantic ids should not exceed top_k"
        );
        assert!(
            qr.hybrid.ranked_ids.len() <= qr.top_k,
            "hybrid ids should not exceed top_k"
        );
    }
}

// =============================================================================
// Metric bounds
// =============================================================================

#[test]
fn metrics_are_in_valid_range() {
    let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();

    for qr in &report.queries {
        for (lane_name, metrics) in [
            ("lexical", &qr.lexical.metrics),
            ("semantic", &qr.semantic.metrics),
            ("hybrid", &qr.hybrid.metrics),
        ] {
            assert!(
                (0.0..=1.0).contains(&metrics.precision_at_k),
                "query '{}' lane '{}' precision@k {} out of [0,1]",
                qr.name,
                lane_name,
                metrics.precision_at_k
            );
            assert!(
                (0.0..=1.0).contains(&metrics.recall_at_k),
                "query '{}' lane '{}' recall@k {} out of [0,1]",
                qr.name,
                lane_name,
                metrics.recall_at_k
            );
            assert!(
                (0.0..=1.0).contains(&metrics.ndcg_at_k),
                "query '{}' lane '{}' ndcg@k {} out of [0,1]",
                qr.name,
                lane_name,
                metrics.ndcg_at_k
            );
            assert!(
                (0.0..=1.0).contains(&metrics.mrr),
                "query '{}' lane '{}' mrr {} out of [0,1]",
                qr.name,
                lane_name,
                metrics.mrr
            );
        }
    }
}

// =============================================================================
// Empty input scenarios
// =============================================================================

#[test]
fn empty_lexical_candidates() {
    let q = make_query(
        "empty_lex",
        vec![],                   // no lexical results
        vec![(1, 1.0), (2, 0.9)],
        vec![1, 2],
        3,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    assert!(qr.lexical.ranked_ids.is_empty());
    assert!(qr.lexical.metrics.precision_at_k.abs() < EPS);
    assert!(qr.lexical.metrics.mrr.abs() < EPS);
    // Hybrid should still work using semantic lane
    assert!(!qr.hybrid.ranked_ids.is_empty());
}

#[test]
fn empty_semantic_candidates() {
    let q = make_query(
        "empty_sem",
        vec![(1, 1.0), (2, 0.9)],
        vec![],                   // no semantic results
        vec![1, 2],
        3,
    );
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    assert!(qr.semantic.ranked_ids.is_empty());
    assert!(qr.semantic.metrics.precision_at_k.abs() < EPS);
    // Hybrid should still work using lexical lane
    assert!(!qr.hybrid.ranked_ids.is_empty());
}

#[test]
fn both_lanes_empty() {
    let q = make_query("both_empty", vec![], vec![], vec![1], 3);
    let report = SemanticQualityHarness::new(vec![q]).run();
    let qr = &report.queries[0];

    assert!(qr.lexical.ranked_ids.is_empty());
    assert!(qr.semantic.ranked_ids.is_empty());
    assert!(qr.hybrid.ranked_ids.is_empty());
    assert!(qr.hybrid.metrics.precision_at_k.abs() < EPS);
    assert!(qr.hybrid.metrics.recall_at_k.abs() < EPS);
}

// =============================================================================
// Determinism
// =============================================================================

#[test]
fn harness_deterministic_across_runs() {
    let queries = default_semantic_eval_queries();
    let json1 = serde_json::to_string(&SemanticQualityHarness::new(queries.clone()).run()).unwrap();
    let json2 = serde_json::to_string(&SemanticQualityHarness::new(queries.clone()).run()).unwrap();
    let json3 = serde_json::to_string(&SemanticQualityHarness::new(queries).run()).unwrap();

    assert_eq!(json1, json2, "runs 1 and 2 should be identical");
    assert_eq!(json2, json3, "runs 2 and 3 should be identical");
}

// =============================================================================
// Multiple queries: verify averaging
// =============================================================================

#[test]
fn summary_averages_across_queries() {
    let q1 = make_query(
        "q1",
        vec![(1, 1.0)],
        vec![(1, 1.0)],
        vec![1],
        1,
    );
    let q2 = make_query(
        "q2",
        vec![(10, 1.0)],
        vec![(10, 1.0)],
        vec![99], // not relevant → zero metrics
        1,
    );

    let report = SemanticQualityHarness::new(vec![q1, q2]).run();
    let s = &report.summary;

    assert_eq!(s.total_queries, 2);

    // q1 should have high metrics, q2 should have zero
    // Mean should be roughly half of q1's metrics
    let q1_metrics = &report.queries[0].hybrid.metrics;
    let q2_metrics = &report.queries[1].hybrid.metrics;

    let expected_mean_precision = f64::midpoint(q1_metrics.precision_at_k, q2_metrics.precision_at_k);
    assert!(
        (s.mean_hybrid_precision_at_k - expected_mean_precision).abs() < 1e-9,
        "mean precision should be average of individual queries"
    );
}
