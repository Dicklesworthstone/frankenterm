//! Semantic + hybrid search quality harness with reproducible metrics and
//! threshold enforcement.
//!
//! Bead: wa-oegrb.5.5
//!
//! This mirrors the lexical `tantivy_quality` harness but evaluates ranking
//! quality across three lanes:
//! - lexical baseline
//! - semantic baseline
//! - hybrid fusion (RRF)
//!
//! The harness is deterministic and CI-friendly: queries are pure ranked lists,
//! metrics are computed from stable relevance sets, and thresholds produce a
//! machine-readable pass/fail report.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::search::HybridSearchService;

/// Input query definition for quality evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticEvalQuery {
    /// Human-readable query/test name.
    pub name: String,
    /// Optional query description.
    #[serde(default)]
    pub description: String,
    /// Ranked lexical candidates: `(segment_id, score)`.
    pub lexical_ranked: Vec<(u64, f32)>,
    /// Ranked semantic candidates: `(segment_id, score)`.
    pub semantic_ranked: Vec<(u64, f32)>,
    /// Set of relevant ids for relevance metrics.
    pub relevant_ids: Vec<u64>,
    /// Cutoff for @k metrics.
    pub top_k: usize,
}

/// Per-lane ranking metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RankingMetrics {
    /// Precision at K.
    pub precision_at_k: f64,
    /// Recall at K.
    pub recall_at_k: f64,
    /// NDCG at K (binary relevance).
    pub ndcg_at_k: f64,
    /// Mean reciprocal rank.
    pub mrr: f64,
}

/// Ranking output for one retrieval lane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneEvaluation {
    /// Ranked ids after deterministic dedupe/truncation.
    pub ranked_ids: Vec<u64>,
    /// Computed relevance metrics.
    pub metrics: RankingMetrics,
}

/// Per-query comparison across lexical/semantic/hybrid lanes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryComparison {
    /// Query name.
    pub name: String,
    /// Query description.
    #[serde(default)]
    pub description: String,
    /// Metric cutoff.
    pub top_k: usize,
    /// Lexical baseline evaluation.
    pub lexical: LaneEvaluation,
    /// Semantic baseline evaluation.
    pub semantic: LaneEvaluation,
    /// Hybrid fusion evaluation.
    pub hybrid: LaneEvaluation,
    /// Hybrid - lexical NDCG delta.
    pub hybrid_vs_lexical_ndcg_delta: f64,
    /// Hybrid - semantic NDCG delta.
    pub hybrid_vs_semantic_ndcg_delta: f64,
    /// Hybrid - lexical precision@k delta.
    pub hybrid_vs_lexical_precision_delta: f64,
}

/// Regression thresholds for CI-style quality gating.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionThresholds {
    /// Minimum allowed hybrid NDCG delta versus lexical baseline.
    pub min_hybrid_ndcg_delta_vs_lexical: f64,
    /// Minimum allowed hybrid precision@k.
    pub min_hybrid_precision_at_k: f64,
    /// Minimum allowed hybrid recall@k.
    pub min_hybrid_recall_at_k: f64,
}

impl Default for RegressionThresholds {
    fn default() -> Self {
        Self {
            // Non-regression default: do not allow hybrid NDCG to regress.
            min_hybrid_ndcg_delta_vs_lexical: 0.0,
            // Floor guards: ensure at least minimal useful signal survives.
            min_hybrid_precision_at_k: 0.25,
            min_hybrid_recall_at_k: 0.25,
        }
    }
}

/// Threshold failure for one query/metric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThresholdViolation {
    /// Query name.
    pub query: String,
    /// Metric identifier.
    pub metric: String,
    /// Observed value.
    pub actual: f64,
    /// Required threshold.
    pub required: f64,
}

/// Aggregate quality summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualitySummary {
    /// Number of evaluated queries.
    pub total_queries: usize,
    /// Mean hybrid precision@k.
    pub mean_hybrid_precision_at_k: f64,
    /// Mean hybrid recall@k.
    pub mean_hybrid_recall_at_k: f64,
    /// Mean hybrid NDCG@k.
    pub mean_hybrid_ndcg_at_k: f64,
    /// Mean hybrid-vs-lexical NDCG delta.
    pub mean_hybrid_vs_lexical_ndcg_delta: f64,
}

/// Full quality report (machine-readable, CI-friendly).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticQualityReport {
    /// Per-query comparisons.
    pub queries: Vec<QueryComparison>,
    /// Summary metrics.
    pub summary: QualitySummary,
    /// Threshold violations.
    pub violations: Vec<ThresholdViolation>,
    /// Overall pass/fail gate.
    pub passed: bool,
}

/// Semantic/hybrid evaluation harness.
pub struct SemanticQualityHarness {
    queries: Vec<SemanticEvalQuery>,
    thresholds: RegressionThresholds,
    rrf_k: u32,
}

impl SemanticQualityHarness {
    /// Build a harness with default thresholds and default RRF k=60.
    pub fn new(queries: Vec<SemanticEvalQuery>) -> Self {
        Self {
            queries,
            thresholds: RegressionThresholds::default(),
            rrf_k: 60,
        }
    }

    /// Override regression thresholds.
    #[must_use]
    pub fn with_thresholds(mut self, thresholds: RegressionThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Override RRF `k` used by hybrid fusion lane.
    #[must_use]
    pub fn with_rrf_k(mut self, rrf_k: u32) -> Self {
        self.rrf_k = rrf_k;
        self
    }

    /// Run all queries and return a deterministic quality report.
    pub fn run(&self) -> SemanticQualityReport {
        let mut comparisons = Vec::with_capacity(self.queries.len());
        let mut violations = Vec::new();

        for query in &self.queries {
            let comparison = evaluate_query(query, self.rrf_k);
            collect_violations(&comparison, &self.thresholds, &mut violations);
            comparisons.push(comparison);
        }

        let summary = summarize(&comparisons);
        let passed = violations.is_empty();

        SemanticQualityReport {
            queries: comparisons,
            summary,
            violations,
            passed,
        }
    }
}

fn evaluate_query(query: &SemanticEvalQuery, rrf_k: u32) -> QueryComparison {
    let top_k = query.top_k.max(1);
    let relevant: HashSet<u64> = query.relevant_ids.iter().copied().collect();

    let lexical_ids = ranked_ids(&query.lexical_ranked, top_k);
    let semantic_ids = ranked_ids(&query.semantic_ranked, top_k);

    let hybrid_fused = HybridSearchService::new().with_rrf_k(rrf_k).fuse(
        &query.lexical_ranked,
        &query.semantic_ranked,
        top_k,
    );
    let hybrid_ids: Vec<u64> = hybrid_fused.iter().map(|hit| hit.id).collect();

    let lexical_metrics = compute_metrics(&lexical_ids, &relevant, top_k);
    let semantic_metrics = compute_metrics(&semantic_ids, &relevant, top_k);
    let hybrid_metrics = compute_metrics(&hybrid_ids, &relevant, top_k);

    QueryComparison {
        name: query.name.clone(),
        description: query.description.clone(),
        top_k,
        lexical: LaneEvaluation {
            ranked_ids: lexical_ids,
            metrics: lexical_metrics.clone(),
        },
        semantic: LaneEvaluation {
            ranked_ids: semantic_ids,
            metrics: semantic_metrics.clone(),
        },
        hybrid: LaneEvaluation {
            ranked_ids: hybrid_ids,
            metrics: hybrid_metrics.clone(),
        },
        hybrid_vs_lexical_ndcg_delta: hybrid_metrics.ndcg_at_k - lexical_metrics.ndcg_at_k,
        hybrid_vs_semantic_ndcg_delta: hybrid_metrics.ndcg_at_k - semantic_metrics.ndcg_at_k,
        hybrid_vs_lexical_precision_delta: hybrid_metrics.precision_at_k
            - lexical_metrics.precision_at_k,
    }
}

fn collect_violations(
    comparison: &QueryComparison,
    thresholds: &RegressionThresholds,
    out: &mut Vec<ThresholdViolation>,
) {
    if comparison.hybrid_vs_lexical_ndcg_delta < thresholds.min_hybrid_ndcg_delta_vs_lexical {
        out.push(ThresholdViolation {
            query: comparison.name.clone(),
            metric: "hybrid_vs_lexical_ndcg_delta".to_string(),
            actual: comparison.hybrid_vs_lexical_ndcg_delta,
            required: thresholds.min_hybrid_ndcg_delta_vs_lexical,
        });
    }

    if comparison.hybrid.metrics.precision_at_k < thresholds.min_hybrid_precision_at_k {
        out.push(ThresholdViolation {
            query: comparison.name.clone(),
            metric: "hybrid_precision_at_k".to_string(),
            actual: comparison.hybrid.metrics.precision_at_k,
            required: thresholds.min_hybrid_precision_at_k,
        });
    }

    if comparison.hybrid.metrics.recall_at_k < thresholds.min_hybrid_recall_at_k {
        out.push(ThresholdViolation {
            query: comparison.name.clone(),
            metric: "hybrid_recall_at_k".to_string(),
            actual: comparison.hybrid.metrics.recall_at_k,
            required: thresholds.min_hybrid_recall_at_k,
        });
    }
}

fn summarize(comparisons: &[QueryComparison]) -> QualitySummary {
    if comparisons.is_empty() {
        return QualitySummary {
            total_queries: 0,
            mean_hybrid_precision_at_k: 0.0,
            mean_hybrid_recall_at_k: 0.0,
            mean_hybrid_ndcg_at_k: 0.0,
            mean_hybrid_vs_lexical_ndcg_delta: 0.0,
        };
    }

    let count = comparisons.len() as f64;
    let mean_hybrid_precision_at_k = comparisons
        .iter()
        .map(|q| q.hybrid.metrics.precision_at_k)
        .sum::<f64>()
        / count;
    let mean_hybrid_recall_at_k = comparisons
        .iter()
        .map(|q| q.hybrid.metrics.recall_at_k)
        .sum::<f64>()
        / count;
    let mean_hybrid_ndcg_at_k = comparisons
        .iter()
        .map(|q| q.hybrid.metrics.ndcg_at_k)
        .sum::<f64>()
        / count;
    let mean_hybrid_vs_lexical_ndcg_delta = comparisons
        .iter()
        .map(|q| q.hybrid_vs_lexical_ndcg_delta)
        .sum::<f64>()
        / count;

    QualitySummary {
        total_queries: comparisons.len(),
        mean_hybrid_precision_at_k,
        mean_hybrid_recall_at_k,
        mean_hybrid_ndcg_at_k,
        mean_hybrid_vs_lexical_ndcg_delta,
    }
}

fn ranked_ids(ranked: &[(u64, f32)], top_k: usize) -> Vec<u64> {
    let mut out = Vec::with_capacity(top_k);
    let mut seen = HashSet::with_capacity(top_k);
    for (id, _) in ranked {
        if out.len() >= top_k {
            break;
        }
        if seen.insert(*id) {
            out.push(*id);
        }
    }
    out
}

fn compute_metrics(ranked_ids: &[u64], relevant: &HashSet<u64>, top_k: usize) -> RankingMetrics {
    if top_k == 0 {
        return RankingMetrics::default();
    }

    let eval_slice = &ranked_ids[..ranked_ids.len().min(top_k)];
    let hits = eval_slice.iter().filter(|id| relevant.contains(id)).count();

    let denom = eval_slice.len().max(1) as f64;
    let precision_at_k = hits as f64 / denom;

    let recall_at_k = if relevant.is_empty() {
        0.0
    } else {
        hits as f64 / relevant.len() as f64
    };

    let dcg = eval_slice
        .iter()
        .enumerate()
        .map(|(idx, id)| {
            let rel = if relevant.contains(id) { 1.0 } else { 0.0 };
            rel / ((idx + 2) as f64).log2()
        })
        .sum::<f64>();

    let ideal_hits = relevant.len().min(top_k);
    let idcg = (0..ideal_hits)
        .map(|idx| 1.0 / ((idx + 2) as f64).log2())
        .sum::<f64>();
    let ndcg_at_k = if idcg > 0.0 { dcg / idcg } else { 0.0 };

    let mrr = eval_slice
        .iter()
        .position(|id| relevant.contains(id))
        .map_or(0.0, |idx| 1.0 / (idx as f64 + 1.0));

    RankingMetrics {
        precision_at_k,
        recall_at_k,
        ndcg_at_k,
        mrr,
    }
}

/// Built-in semantic/hybrid evaluation corpus.
///
/// These fixtures encode representative operator/forensic retrieval cases and
/// are intentionally deterministic to keep CI signal stable.
pub fn default_semantic_eval_queries() -> Vec<SemanticEvalQuery> {
    vec![
        SemanticEvalQuery {
            name: "build_errors".to_string(),
            description: "Compiler/runtime error retrieval quality".to_string(),
            lexical_ranked: vec![(101, 0.92), (102, 0.88), (103, 0.80), (104, 0.77)],
            semantic_ranked: vec![(103, 0.95), (101, 0.93), (105, 0.90), (106, 0.60)],
            relevant_ids: vec![101, 103, 105],
            top_k: 3,
        },
        SemanticEvalQuery {
            name: "network_timeout".to_string(),
            description: "Connection timeout triage".to_string(),
            lexical_ranked: vec![(201, 0.90), (202, 0.83), (203, 0.81)],
            semantic_ranked: vec![(204, 0.96), (201, 0.91), (205, 0.82)],
            relevant_ids: vec![201, 204],
            top_k: 3,
        },
        SemanticEvalQuery {
            name: "auth_device_flow".to_string(),
            description: "Device-code auth remediation lookup".to_string(),
            lexical_ranked: vec![(301, 0.93), (302, 0.82), (303, 0.79)],
            semantic_ranked: vec![(304, 0.95), (301, 0.90), (305, 0.80)],
            relevant_ids: vec![301, 304],
            top_k: 3,
        },
        SemanticEvalQuery {
            name: "rate_limit_handling".to_string(),
            description: "Rate-limit fallback guidance retrieval".to_string(),
            lexical_ranked: vec![(401, 0.91), (402, 0.85), (403, 0.78)],
            semantic_ranked: vec![(404, 0.97), (401, 0.92), (405, 0.81)],
            relevant_ids: vec![401, 404],
            top_k: 3,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_is_deterministic_for_same_inputs() {
        let queries = default_semantic_eval_queries();
        let harness = SemanticQualityHarness::new(queries.clone());
        let report_a = harness.run();
        let report_b = SemanticQualityHarness::new(queries).run();

        let json_a = serde_json::to_string(&report_a).unwrap();
        let json_b = serde_json::to_string(&report_b).unwrap();
        assert_eq!(json_a, json_b);
    }

    #[test]
    fn thresholds_fail_when_set_too_strict() {
        let thresholds = RegressionThresholds {
            min_hybrid_ndcg_delta_vs_lexical: 0.5,
            min_hybrid_precision_at_k: 0.95,
            min_hybrid_recall_at_k: 0.95,
        };
        let report = SemanticQualityHarness::new(default_semantic_eval_queries())
            .with_thresholds(thresholds)
            .run();
        assert!(!report.passed);
        assert!(!report.violations.is_empty());
    }

    #[test]
    fn report_serialization_roundtrip() {
        let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();
        let json = serde_json::to_string(&report).unwrap();
        let parsed: SemanticQualityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.summary.total_queries, report.summary.total_queries);
    }

    // -----------------------------------------------------------------------
    // RankingMetrics defaults
    // -----------------------------------------------------------------------

    #[test]
    fn ranking_metrics_default_is_zeroed() {
        let m = RankingMetrics::default();
        assert!(m.precision_at_k.abs() < f64::EPSILON);
        assert!(m.recall_at_k.abs() < f64::EPSILON);
        assert!(m.ndcg_at_k.abs() < f64::EPSILON);
        assert!(m.mrr.abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // RegressionThresholds defaults
    // -----------------------------------------------------------------------

    #[test]
    fn regression_thresholds_default_values() {
        let t = RegressionThresholds::default();
        assert!(t.min_hybrid_ndcg_delta_vs_lexical.abs() < f64::EPSILON);
        assert!((t.min_hybrid_precision_at_k - 0.25).abs() < f64::EPSILON);
        assert!((t.min_hybrid_recall_at_k - 0.25).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // ranked_ids helper
    // -----------------------------------------------------------------------

    #[test]
    fn ranked_ids_truncates_to_top_k() {
        let ranked = vec![(1, 0.9), (2, 0.8), (3, 0.7), (4, 0.6)];
        let ids = ranked_ids(&ranked, 2);
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn ranked_ids_deduplicates() {
        let ranked = vec![(1, 0.9), (1, 0.85), (2, 0.8), (2, 0.7), (3, 0.6)];
        let ids = ranked_ids(&ranked, 3);
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn ranked_ids_empty_input() {
        let ids = ranked_ids(&[], 5);
        assert!(ids.is_empty());
    }

    #[test]
    fn ranked_ids_top_k_larger_than_input() {
        let ranked = vec![(1, 0.9), (2, 0.8)];
        let ids = ranked_ids(&ranked, 10);
        assert_eq!(ids, vec![1, 2]);
    }

    // -----------------------------------------------------------------------
    // compute_metrics
    // -----------------------------------------------------------------------

    #[test]
    fn compute_metrics_perfect_ranking() {
        let relevant: HashSet<u64> = [1, 2, 3].into_iter().collect();
        let ranked = vec![1, 2, 3];
        let m = compute_metrics(&ranked, &relevant, 3);
        assert!((m.precision_at_k - 1.0).abs() < f64::EPSILON);
        assert!((m.recall_at_k - 1.0).abs() < f64::EPSILON);
        assert!((m.ndcg_at_k - 1.0).abs() < f64::EPSILON);
        assert!((m.mrr - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_metrics_no_relevant_hits() {
        let relevant: HashSet<u64> = [10, 20].into_iter().collect();
        let ranked = vec![1, 2, 3];
        let m = compute_metrics(&ranked, &relevant, 3);
        assert!(m.precision_at_k.abs() < f64::EPSILON);
        assert!(m.recall_at_k.abs() < f64::EPSILON);
        assert!(m.ndcg_at_k.abs() < f64::EPSILON);
        assert!(m.mrr.abs() < f64::EPSILON);
    }

    #[test]
    fn compute_metrics_empty_relevant_set() {
        let relevant: HashSet<u64> = HashSet::new();
        let ranked = vec![1, 2, 3];
        let m = compute_metrics(&ranked, &relevant, 3);
        assert!(m.recall_at_k.abs() < f64::EPSILON);
    }

    #[test]
    fn compute_metrics_empty_ranked() {
        let relevant: HashSet<u64> = [1, 2].into_iter().collect();
        let m = compute_metrics(&[], &relevant, 3);
        assert!(m.precision_at_k.abs() < f64::EPSILON);
        assert!(m.recall_at_k.abs() < f64::EPSILON);
    }

    #[test]
    fn compute_metrics_top_k_zero_returns_default() {
        let relevant: HashSet<u64> = [1].into_iter().collect();
        let m = compute_metrics(&[1], &relevant, 0);
        assert!(m.precision_at_k.abs() < f64::EPSILON);
    }

    #[test]
    fn compute_metrics_mrr_first_hit_at_position_2() {
        let relevant: HashSet<u64> = [2].into_iter().collect();
        let ranked = vec![1, 2, 3]; // first relevant at idx 1 → MRR = 1/2
        let m = compute_metrics(&ranked, &relevant, 3);
        assert!((m.mrr - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_metrics_partial_recall() {
        let relevant: HashSet<u64> = [1, 2, 3, 4].into_iter().collect();
        let ranked = vec![1, 5, 3]; // 2 out of 4 relevant
        let m = compute_metrics(&ranked, &relevant, 3);
        assert!((m.recall_at_k - 0.5).abs() < f64::EPSILON);
        assert!((m.precision_at_k - 2.0 / 3.0).abs() < 1e-10);
    }

    // -----------------------------------------------------------------------
    // summarize
    // -----------------------------------------------------------------------

    #[test]
    fn summarize_empty_queries() {
        let s = summarize(&[]);
        assert_eq!(s.total_queries, 0);
        assert!(s.mean_hybrid_precision_at_k.abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // collect_violations
    // -----------------------------------------------------------------------

    #[test]
    fn collect_violations_none_when_thresholds_met() {
        let comparison = QueryComparison {
            name: "test".to_string(),
            description: String::new(),
            top_k: 3,
            lexical: LaneEvaluation {
                ranked_ids: vec![1],
                metrics: RankingMetrics::default(),
            },
            semantic: LaneEvaluation {
                ranked_ids: vec![1],
                metrics: RankingMetrics::default(),
            },
            hybrid: LaneEvaluation {
                ranked_ids: vec![1],
                metrics: RankingMetrics {
                    precision_at_k: 0.5,
                    recall_at_k: 0.5,
                    ndcg_at_k: 0.5,
                    mrr: 0.5,
                },
            },
            hybrid_vs_lexical_ndcg_delta: 0.5,
            hybrid_vs_semantic_ndcg_delta: 0.3,
            hybrid_vs_lexical_precision_delta: 0.2,
        };
        let thresholds = RegressionThresholds::default();
        let mut violations = Vec::new();
        collect_violations(&comparison, &thresholds, &mut violations);
        assert!(violations.is_empty());
    }

    #[test]
    fn collect_violations_detects_all_three_threshold_types() {
        let comparison = QueryComparison {
            name: "bad".to_string(),
            description: String::new(),
            top_k: 3,
            lexical: LaneEvaluation {
                ranked_ids: vec![],
                metrics: RankingMetrics::default(),
            },
            semantic: LaneEvaluation {
                ranked_ids: vec![],
                metrics: RankingMetrics::default(),
            },
            hybrid: LaneEvaluation {
                ranked_ids: vec![],
                metrics: RankingMetrics {
                    precision_at_k: 0.0,
                    recall_at_k: 0.0,
                    ndcg_at_k: 0.0,
                    mrr: 0.0,
                },
            },
            hybrid_vs_lexical_ndcg_delta: -1.0,
            hybrid_vs_semantic_ndcg_delta: -1.0,
            hybrid_vs_lexical_precision_delta: -1.0,
        };
        let thresholds = RegressionThresholds {
            min_hybrid_ndcg_delta_vs_lexical: 0.0,
            min_hybrid_precision_at_k: 0.25,
            min_hybrid_recall_at_k: 0.25,
        };
        let mut violations = Vec::new();
        collect_violations(&comparison, &thresholds, &mut violations);
        assert_eq!(violations.len(), 3);
        let metrics: Vec<&str> = violations.iter().map(|v| v.metric.as_str()).collect();
        assert!(metrics.contains(&"hybrid_vs_lexical_ndcg_delta"));
        assert!(metrics.contains(&"hybrid_precision_at_k"));
        assert!(metrics.contains(&"hybrid_recall_at_k"));
    }

    // -----------------------------------------------------------------------
    // Harness configuration
    // -----------------------------------------------------------------------

    #[test]
    fn harness_with_rrf_k_produces_valid_reports() {
        let queries = default_semantic_eval_queries();
        let report_k1 = SemanticQualityHarness::new(queries.clone())
            .with_rrf_k(1)
            .run();
        let report_k60 = SemanticQualityHarness::new(queries).with_rrf_k(60).run();
        // Both should pass with default thresholds regardless of k.
        assert!(report_k1.passed);
        assert!(report_k60.passed);
        assert_eq!(report_k1.summary.total_queries, 4);
        assert_eq!(report_k60.summary.total_queries, 4);
    }

    #[test]
    fn harness_empty_queries_passes() {
        let report = SemanticQualityHarness::new(vec![]).run();
        assert!(report.passed);
        assert_eq!(report.summary.total_queries, 0);
        assert!(report.violations.is_empty());
    }

    // -----------------------------------------------------------------------
    // Serde roundtrip for data types
    // -----------------------------------------------------------------------

    #[test]
    fn semantic_eval_query_serde_roundtrip() {
        let query = SemanticEvalQuery {
            name: "test".to_string(),
            description: "desc".to_string(),
            lexical_ranked: vec![(1, 0.9), (2, 0.8)],
            semantic_ranked: vec![(3, 0.95)],
            relevant_ids: vec![1, 3],
            top_k: 2,
        };
        let json = serde_json::to_string(&query).unwrap();
        let back: SemanticEvalQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "test");
        assert_eq!(back.top_k, 2);
        assert_eq!(back.relevant_ids, vec![1, 3]);
    }

    #[test]
    fn threshold_violation_serde_roundtrip() {
        let v = ThresholdViolation {
            query: "q1".to_string(),
            metric: "precision".to_string(),
            actual: 0.1,
            required: 0.5,
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: ThresholdViolation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.query, "q1");
        assert!((back.actual - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn default_eval_queries_are_nonempty() {
        let queries = default_semantic_eval_queries();
        assert!(!queries.is_empty());
        for q in &queries {
            assert!(!q.name.is_empty());
            assert!(q.top_k > 0);
            assert!(!q.relevant_ids.is_empty());
        }
    }
}
