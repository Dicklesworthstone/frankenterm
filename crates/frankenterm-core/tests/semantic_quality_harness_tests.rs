//! Integration tests for semantic/hybrid quality harness (wa-oegrb.5.5).

use frankenterm_core::semantic_quality::{
    RegressionThresholds, SemanticQualityHarness, default_semantic_eval_queries,
};

#[test]
fn semantic_quality_default_corpus_passes_default_thresholds() {
    let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();

    assert!(
        report.passed,
        "default semantic quality harness should pass: {:#?}",
        report.violations
    );
    assert_eq!(report.summary.total_queries, report.queries.len());
    assert!(!report.queries.is_empty());
}

#[test]
fn semantic_quality_report_quantifies_lane_deltas() {
    let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();

    for query in &report.queries {
        assert!(
            query.hybrid_vs_lexical_ndcg_delta.is_finite(),
            "ndcg delta should be finite"
        );
        assert!(
            query.hybrid_vs_semantic_ndcg_delta.is_finite(),
            "ndcg delta vs semantic should be finite"
        );
        assert!(
            query.hybrid_vs_lexical_precision_delta.is_finite(),
            "precision delta should be finite"
        );

        assert!(
            query.hybrid.metrics.precision_at_k >= 0.0
                && query.hybrid.metrics.precision_at_k <= 1.0
        );
        assert!(query.hybrid.metrics.recall_at_k >= 0.0 && query.hybrid.metrics.recall_at_k <= 1.0);
        assert!(query.hybrid.metrics.ndcg_at_k >= 0.0 && query.hybrid.metrics.ndcg_at_k <= 1.0);
    }
}

#[test]
fn semantic_quality_thresholds_enforced_in_automation() {
    let strict_thresholds = RegressionThresholds {
        min_hybrid_ndcg_delta_vs_lexical: 0.40,
        min_hybrid_precision_at_k: 0.95,
        min_hybrid_recall_at_k: 0.95,
    };

    let report = SemanticQualityHarness::new(default_semantic_eval_queries())
        .with_thresholds(strict_thresholds)
        .run();

    assert!(!report.passed);
    assert!(!report.violations.is_empty());
}
