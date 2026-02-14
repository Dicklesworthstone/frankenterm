//! Property-based tests for the semantic_quality module.
//!
//! Verifies mathematical invariants of information-retrieval ranking metrics
//! (precision, recall, NDCG, MRR) and quality harness behavior across
//! randomized inputs.

use std::collections::HashSet;

use proptest::prelude::*;

use frankenterm_core::semantic_quality::{
    RegressionThresholds, SemanticEvalQuery, SemanticQualityHarness, SemanticQualityReport,
    ThresholdViolation, default_semantic_eval_queries,
};

// ── Strategies ────────────────────────────────────────────────────────

/// Generate a (segment_id, score) pair for ranked lists.
fn arb_ranked_entry() -> impl Strategy<Value = (u64, f32)> {
    (0_u64..200, 0.0_f32..1.0)
}

/// Generate a ranked list of (id, score) pairs.
fn arb_ranked_list(max_len: usize) -> impl Strategy<Value = Vec<(u64, f32)>> {
    prop::collection::vec(arb_ranked_entry(), 0..=max_len)
}

/// Generate a set of relevant IDs.
fn arb_relevant_ids() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0_u64..200, 0..=20)
}

/// Generate a complete SemanticEvalQuery with controlled parameters.
fn arb_eval_query() -> impl Strategy<Value = SemanticEvalQuery> {
    (
        "[a-z]{3,10}",       // name
        arb_ranked_list(20), // lexical_ranked
        arb_ranked_list(20), // semantic_ranked
        arb_relevant_ids(),  // relevant_ids
        1_usize..=20,        // top_k
    )
        .prop_map(
            |(name, lexical, semantic, relevant, top_k)| SemanticEvalQuery {
                name,
                description: String::new(),
                lexical_ranked: lexical,
                semantic_ranked: semantic,
                relevant_ids: relevant,
                top_k,
            },
        )
}

/// Generate a query where all ranked items are relevant (perfect ranking).
fn arb_perfect_query() -> impl Strategy<Value = SemanticEvalQuery> {
    (1_usize..=10).prop_map(|k| {
        let ids: Vec<u64> = (1..=k as u64).collect();
        let lexical: Vec<(u64, f32)> = ids.iter().map(|&id| (id, 0.9)).collect();
        let semantic: Vec<(u64, f32)> = ids.iter().map(|&id| (id, 0.95)).collect();
        SemanticEvalQuery {
            name: "perfect".to_string(),
            description: String::new(),
            lexical_ranked: lexical,
            semantic_ranked: semantic,
            relevant_ids: ids,
            top_k: k,
        }
    })
}

/// Generate a query where NO ranked items are relevant (zero-hit ranking).
fn arb_zero_hit_query() -> impl Strategy<Value = SemanticEvalQuery> {
    (1_usize..=10, 1_usize..=10).prop_map(|(k, n_ranked)| {
        // Ranked IDs: 1..=n_ranked, relevant IDs: 1000..1010 (no overlap)
        let lexical: Vec<(u64, f32)> = (1..=n_ranked as u64).map(|id| (id, 0.8)).collect();
        let semantic: Vec<(u64, f32)> = (1..=n_ranked as u64).map(|id| (id, 0.85)).collect();
        let relevant: Vec<u64> = (1000..1010).collect();
        SemanticEvalQuery {
            name: "zero_hit".to_string(),
            description: String::new(),
            lexical_ranked: lexical,
            semantic_ranked: semantic,
            relevant_ids: relevant,
            top_k: k,
        }
    })
}

/// Generate regression thresholds in [0, 1] range.
fn arb_thresholds() -> impl Strategy<Value = RegressionThresholds> {
    (
        -1.0_f64..1.0, // min_hybrid_ndcg_delta_vs_lexical
        0.0_f64..1.0,  // min_hybrid_precision_at_k
        0.0_f64..1.0,  // min_hybrid_recall_at_k
    )
        .prop_map(|(ndcg_delta, precision, recall)| RegressionThresholds {
            min_hybrid_ndcg_delta_vs_lexical: ndcg_delta,
            min_hybrid_precision_at_k: precision,
            min_hybrid_recall_at_k: recall,
        })
}

/// Generate a list of queries for the harness.
fn arb_query_list() -> impl Strategy<Value = Vec<SemanticEvalQuery>> {
    prop::collection::vec(arb_eval_query(), 1..=5)
}

// ── Metric bound invariants ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// All ranking metrics in the output are bounded in [0, 1].
    #[test]
    fn all_metrics_bounded_zero_to_one(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        for comparison in &report.queries {
            for lane in [&comparison.lexical, &comparison.semantic, &comparison.hybrid] {
                let m = &lane.metrics;
                prop_assert!(m.precision_at_k >= 0.0 && m.precision_at_k <= 1.0,
                    "precision_at_k out of bounds: {}", m.precision_at_k);
                prop_assert!(m.recall_at_k >= 0.0 && m.recall_at_k <= 1.0,
                    "recall_at_k out of bounds: {}", m.recall_at_k);
                prop_assert!(m.ndcg_at_k >= 0.0 && m.ndcg_at_k <= 1.0,
                    "ndcg_at_k out of bounds: {}", m.ndcg_at_k);
                prop_assert!(m.mrr >= 0.0 && m.mrr <= 1.0,
                    "mrr out of bounds: {}", m.mrr);
            }
        }
    }

    /// Summary metrics are bounded in [0, 1].
    #[test]
    fn summary_metrics_bounded(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        let s = &report.summary;
        prop_assert!(s.mean_hybrid_precision_at_k >= 0.0 && s.mean_hybrid_precision_at_k <= 1.0);
        prop_assert!(s.mean_hybrid_recall_at_k >= 0.0 && s.mean_hybrid_recall_at_k <= 1.0);
        prop_assert!(s.mean_hybrid_ndcg_at_k >= 0.0 && s.mean_hybrid_ndcg_at_k <= 1.0);
    }
}

// ── Perfect ranking invariants ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// When all ranked items are relevant and ranking is perfect,
    /// precision and recall should both be 1.0 for each lane.
    #[test]
    fn perfect_ranking_gets_full_precision_and_recall(query in arb_perfect_query()) {
        let report = SemanticQualityHarness::new(vec![query]).run();
        let comparison = &report.queries[0];

        // Lexical lane: items directly match relevant set
        let m = &comparison.lexical.metrics;
        prop_assert!(
            (m.precision_at_k - 1.0).abs() < 1e-10,
            "lexical precision should be 1.0, got {}", m.precision_at_k
        );
        prop_assert!(
            (m.recall_at_k - 1.0).abs() < 1e-10,
            "lexical recall should be 1.0, got {}", m.recall_at_k
        );

        // Semantic lane: items directly match relevant set
        let m = &comparison.semantic.metrics;
        prop_assert!(
            (m.precision_at_k - 1.0).abs() < 1e-10,
            "semantic precision should be 1.0, got {}", m.precision_at_k
        );
        prop_assert!(
            (m.recall_at_k - 1.0).abs() < 1e-10,
            "semantic recall should be 1.0, got {}", m.recall_at_k
        );
    }

    /// Perfect ranking yields NDCG = 1.0 and MRR = 1.0.
    #[test]
    fn perfect_ranking_gets_full_ndcg_and_mrr(query in arb_perfect_query()) {
        let report = SemanticQualityHarness::new(vec![query]).run();
        let comparison = &report.queries[0];

        for lane in [&comparison.lexical, &comparison.semantic] {
            let m = &lane.metrics;
            prop_assert!(
                (m.ndcg_at_k - 1.0).abs() < 1e-10,
                "expected NDCG=1.0 for perfect ranking, got {}", m.ndcg_at_k
            );
            prop_assert!(
                (m.mrr - 1.0).abs() < 1e-10,
                "expected MRR=1.0 for perfect ranking, got {}", m.mrr
            );
        }
    }
}

// ── Zero-hit ranking invariants ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// When no ranked items appear in the relevant set, precision = 0.
    #[test]
    fn zero_hit_ranking_has_zero_precision(query in arb_zero_hit_query()) {
        let report = SemanticQualityHarness::new(vec![query]).run();
        let comparison = &report.queries[0];

        for lane in [&comparison.lexical, &comparison.semantic, &comparison.hybrid] {
            let m = &lane.metrics;
            prop_assert!(
                m.precision_at_k.abs() < 1e-10,
                "precision should be 0 with no relevant hits, got {}", m.precision_at_k
            );
            prop_assert!(
                m.recall_at_k.abs() < 1e-10,
                "recall should be 0 with no relevant hits, got {}", m.recall_at_k
            );
            prop_assert!(
                m.ndcg_at_k.abs() < 1e-10,
                "NDCG should be 0 with no relevant hits, got {}", m.ndcg_at_k
            );
            prop_assert!(
                m.mrr.abs() < 1e-10,
                "MRR should be 0 with no relevant hits, got {}", m.mrr
            );
        }
    }
}

// ── Determinism ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Running the harness twice on the same input produces identical reports.
    #[test]
    fn harness_is_deterministic(queries in arb_query_list()) {
        let a = SemanticQualityHarness::new(queries.clone()).run();
        let b = SemanticQualityHarness::new(queries).run();

        let json_a = serde_json::to_string(&a).expect("serialize a");
        let json_b = serde_json::to_string(&b).expect("serialize b");
        prop_assert_eq!(json_a, json_b, "harness not deterministic");
    }
}

// ── Summary averaging invariants ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Summary total_queries matches the number of input queries.
    #[test]
    fn summary_total_queries_matches_input(queries in arb_query_list()) {
        let count = queries.len();
        let report = SemanticQualityHarness::new(queries).run();
        prop_assert_eq!(report.summary.total_queries, count);
        prop_assert_eq!(report.queries.len(), count);
    }

    /// Mean hybrid precision is the arithmetic mean of individual precisions.
    #[test]
    fn mean_hybrid_precision_is_average(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        if report.queries.is_empty() { return Ok(()); }

        let individual_sum: f64 = report.queries.iter()
            .map(|q| q.hybrid.metrics.precision_at_k)
            .sum();
        let expected_mean = individual_sum / report.queries.len() as f64;
        prop_assert!(
            (report.summary.mean_hybrid_precision_at_k - expected_mean).abs() < 1e-10,
            "mean precision {} != expected {}",
            report.summary.mean_hybrid_precision_at_k, expected_mean
        );
    }

    /// Mean hybrid recall is the arithmetic mean of individual recalls.
    #[test]
    fn mean_hybrid_recall_is_average(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        if report.queries.is_empty() { return Ok(()); }

        let individual_sum: f64 = report.queries.iter()
            .map(|q| q.hybrid.metrics.recall_at_k)
            .sum();
        let expected_mean = individual_sum / report.queries.len() as f64;
        prop_assert!(
            (report.summary.mean_hybrid_recall_at_k - expected_mean).abs() < 1e-10,
            "mean recall {} != expected {}",
            report.summary.mean_hybrid_recall_at_k, expected_mean
        );
    }

    /// Mean hybrid NDCG is the arithmetic mean of individual NDCGs.
    #[test]
    fn mean_hybrid_ndcg_is_average(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        if report.queries.is_empty() { return Ok(()); }

        let individual_sum: f64 = report.queries.iter()
            .map(|q| q.hybrid.metrics.ndcg_at_k)
            .sum();
        let expected_mean = individual_sum / report.queries.len() as f64;
        prop_assert!(
            (report.summary.mean_hybrid_ndcg_at_k - expected_mean).abs() < 1e-10,
            "mean NDCG {} != expected {}",
            report.summary.mean_hybrid_ndcg_at_k, expected_mean
        );
    }
}

// ── Delta consistency ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// hybrid_vs_lexical_ndcg_delta = hybrid.ndcg - lexical.ndcg exactly.
    #[test]
    fn ndcg_delta_is_consistent(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        for q in &report.queries {
            let expected = q.hybrid.metrics.ndcg_at_k - q.lexical.metrics.ndcg_at_k;
            prop_assert!(
                (q.hybrid_vs_lexical_ndcg_delta - expected).abs() < 1e-10,
                "hybrid_vs_lexical_ndcg_delta {} != expected {}",
                q.hybrid_vs_lexical_ndcg_delta, expected
            );
        }
    }

    /// hybrid_vs_semantic_ndcg_delta = hybrid.ndcg - semantic.ndcg exactly.
    #[test]
    fn semantic_ndcg_delta_is_consistent(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        for q in &report.queries {
            let expected = q.hybrid.metrics.ndcg_at_k - q.semantic.metrics.ndcg_at_k;
            prop_assert!(
                (q.hybrid_vs_semantic_ndcg_delta - expected).abs() < 1e-10,
                "hybrid_vs_semantic_ndcg_delta {} != expected {}",
                q.hybrid_vs_semantic_ndcg_delta, expected
            );
        }
    }

    /// hybrid_vs_lexical_precision_delta = hybrid.precision - lexical.precision exactly.
    #[test]
    fn precision_delta_is_consistent(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        for q in &report.queries {
            let expected = q.hybrid.metrics.precision_at_k - q.lexical.metrics.precision_at_k;
            prop_assert!(
                (q.hybrid_vs_lexical_precision_delta - expected).abs() < 1e-10,
                "hybrid_vs_lexical_precision_delta {} != expected {}",
                q.hybrid_vs_lexical_precision_delta, expected
            );
        }
    }
}

// ── Threshold violation invariants ────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// passed == violations.is_empty() always.
    #[test]
    fn passed_iff_no_violations(
        queries in arb_query_list(),
        thresholds in arb_thresholds(),
    ) {
        let report = SemanticQualityHarness::new(queries)
            .with_thresholds(thresholds)
            .run();
        prop_assert_eq!(
            report.passed,
            report.violations.is_empty(),
            "passed={} but violations.len()={}",
            report.passed, report.violations.len()
        );
    }

    /// With zero thresholds, the harness always passes.
    #[test]
    fn zero_thresholds_always_pass(queries in arb_query_list()) {
        let thresholds = RegressionThresholds {
            min_hybrid_ndcg_delta_vs_lexical: f64::NEG_INFINITY,
            min_hybrid_precision_at_k: 0.0,
            min_hybrid_recall_at_k: 0.0,
        };
        let report = SemanticQualityHarness::new(queries)
            .with_thresholds(thresholds)
            .run();
        prop_assert!(report.passed, "zero thresholds should always pass");
        prop_assert!(report.violations.is_empty());
    }

    /// Every violation references a valid query name from the input.
    #[test]
    fn violations_reference_valid_queries(
        queries in arb_query_list(),
        thresholds in arb_thresholds(),
    ) {
        let query_names: HashSet<String> = queries.iter().map(|q| q.name.clone()).collect();
        let report = SemanticQualityHarness::new(queries)
            .with_thresholds(thresholds)
            .run();
        for v in &report.violations {
            prop_assert!(
                query_names.contains(&v.query),
                "violation references unknown query: {}", v.query
            );
        }
    }

    /// Every violation has actual < required (that's why it's a violation).
    #[test]
    fn violation_actual_less_than_required(
        queries in arb_query_list(),
        thresholds in arb_thresholds(),
    ) {
        let report = SemanticQualityHarness::new(queries)
            .with_thresholds(thresholds)
            .run();
        for v in &report.violations {
            prop_assert!(
                v.actual < v.required,
                "violation {} for query {}: actual={} >= required={}",
                v.metric, v.query, v.actual, v.required
            );
        }
    }
}

// ── Ranked IDs invariants (via harness output) ────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Ranked IDs in each lane are deduplicated (no duplicates).
    #[test]
    fn ranked_ids_are_deduplicated(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        for q in &report.queries {
            for (lane_name, lane) in [
                ("lexical", &q.lexical),
                ("semantic", &q.semantic),
                ("hybrid", &q.hybrid),
            ] {
                let unique: HashSet<u64> = lane.ranked_ids.iter().copied().collect();
                prop_assert_eq!(
                    lane.ranked_ids.len(),
                    unique.len(),
                    "{} lane has duplicates: {:?}", lane_name, lane.ranked_ids
                );
            }
        }
    }

    /// Ranked IDs in each lane have at most top_k elements.
    #[test]
    fn ranked_ids_bounded_by_top_k(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        for q in &report.queries {
            for (lane_name, lane) in [
                ("lexical", &q.lexical),
                ("semantic", &q.semantic),
                ("hybrid", &q.hybrid),
            ] {
                prop_assert!(
                    lane.ranked_ids.len() <= q.top_k,
                    "{} lane has {} ids, top_k={}", lane_name, lane.ranked_ids.len(), q.top_k
                );
            }
        }
    }
}

// ── MRR invariants ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// MRR is either 0 or 1/(position+1) for some integer position >= 0.
    /// That means MRR ∈ {0, 1, 1/2, 1/3, 1/4, ...}.
    #[test]
    fn mrr_is_reciprocal_rank(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        for q in &report.queries {
            for (lane_name, lane) in [
                ("lexical", &q.lexical),
                ("semantic", &q.semantic),
                ("hybrid", &q.hybrid),
            ] {
                let mrr = lane.metrics.mrr;
                if mrr.abs() < 1e-10 {
                    continue; // MRR = 0 is valid (no hit)
                }
                // MRR should be 1/k for some k in 1..=top_k
                let recip = 1.0 / mrr;
                let k = recip.round() as u64;
                prop_assert!(
                    (recip - k as f64).abs() < 1e-8,
                    "{} lane MRR={} is not a reciprocal rank (1/{} != {})",
                    lane_name, mrr, k, mrr
                );
                prop_assert!(k >= 1, "{} lane MRR reciprocal k={} < 1", lane_name, k);
            }
        }
    }
}

// ── RRF k parameter invariants ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Different RRF k values produce valid (bounded) metrics.
    #[test]
    fn rrf_k_variation_produces_valid_metrics(
        queries in arb_query_list(),
        rrf_k in 1_u32..200,
    ) {
        let report = SemanticQualityHarness::new(queries)
            .with_rrf_k(rrf_k)
            .run();
        for q in &report.queries {
            let m = &q.hybrid.metrics;
            prop_assert!(m.precision_at_k >= 0.0 && m.precision_at_k <= 1.0);
            prop_assert!(m.recall_at_k >= 0.0 && m.recall_at_k <= 1.0);
            prop_assert!(m.ndcg_at_k >= 0.0 && m.ndcg_at_k <= 1.0);
            prop_assert!(m.mrr >= 0.0 && m.mrr <= 1.0);
        }
    }
}

// ── Serde roundtrip ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// SemanticEvalQuery survives JSON roundtrip.
    #[test]
    fn eval_query_serde_roundtrip(query in arb_eval_query()) {
        let json = serde_json::to_string(&query).expect("serialize query");
        let restored: SemanticEvalQuery = serde_json::from_str(&json).expect("deserialize query");
        prop_assert_eq!(query.name, restored.name);
        prop_assert_eq!(query.top_k, restored.top_k);
        prop_assert_eq!(query.relevant_ids, restored.relevant_ids);
        prop_assert_eq!(query.lexical_ranked.len(), restored.lexical_ranked.len());
        prop_assert_eq!(query.semantic_ranked.len(), restored.semantic_ranked.len());
    }

    /// SemanticQualityReport survives JSON roundtrip.
    #[test]
    fn report_serde_roundtrip(queries in arb_query_list()) {
        let report = SemanticQualityHarness::new(queries).run();
        let json = serde_json::to_string(&report).expect("serialize report");
        let restored: SemanticQualityReport = serde_json::from_str(&json).expect("deserialize report");
        prop_assert_eq!(report.passed, restored.passed);
        prop_assert_eq!(report.summary.total_queries, restored.summary.total_queries);
        prop_assert_eq!(report.violations.len(), restored.violations.len());
        prop_assert_eq!(report.queries.len(), restored.queries.len());
    }

    /// RegressionThresholds survives JSON roundtrip.
    #[test]
    fn thresholds_serde_roundtrip(thresholds in arb_thresholds()) {
        let json = serde_json::to_string(&thresholds).expect("serialize thresholds");
        let restored: RegressionThresholds = serde_json::from_str(&json).expect("deserialize thresholds");
        prop_assert!(
            (thresholds.min_hybrid_ndcg_delta_vs_lexical - restored.min_hybrid_ndcg_delta_vs_lexical).abs() < 1e-10
        );
        prop_assert!(
            (thresholds.min_hybrid_precision_at_k - restored.min_hybrid_precision_at_k).abs() < 1e-10
        );
        prop_assert!(
            (thresholds.min_hybrid_recall_at_k - restored.min_hybrid_recall_at_k).abs() < 1e-10
        );
    }

    /// ThresholdViolation survives JSON roundtrip.
    #[test]
    fn violation_serde_roundtrip(
        actual in -10.0_f64..10.0,
        required in -10.0_f64..10.0,
    ) {
        let v = ThresholdViolation {
            query: "test_query".to_string(),
            metric: "precision".to_string(),
            actual,
            required,
        };
        let json = serde_json::to_string(&v).expect("serialize violation");
        let restored: ThresholdViolation = serde_json::from_str(&json).expect("deserialize violation");
        prop_assert_eq!(v.query, restored.query);
        prop_assert_eq!(v.metric, restored.metric);
        prop_assert!((v.actual - restored.actual).abs() < 1e-10);
        prop_assert!((v.required - restored.required).abs() < 1e-10);
    }
}

// ── NDCG monotonicity: more relevant hits → higher or equal NDCG ──────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Adding a relevant item to the ranked list cannot decrease precision.
    /// This tests a weaker version: checking that the harness handles
    /// growing relevant sets without violating metric bounds.
    #[test]
    fn growing_relevant_set_keeps_valid_metrics(
        base_ids in prop::collection::vec(1_u64..50, 1..=5),
        extra_ids in prop::collection::vec(50_u64..100, 1..=5),
        top_k in 1_usize..=10,
    ) {
        let ranked: Vec<(u64, f32)> = (1..=10_u64).map(|id| (id, (id as f32).mul_add(-0.05, 0.9))).collect();

        let query_small = SemanticEvalQuery {
            name: "small".to_string(),
            description: String::new(),
            lexical_ranked: ranked.clone(),
            semantic_ranked: ranked.clone(),
            relevant_ids: base_ids.clone(),
            top_k,
        };

        let mut big_relevant = base_ids;
        big_relevant.extend(extra_ids);
        let query_big = SemanticEvalQuery {
            name: "big".to_string(),
            description: String::new(),
            lexical_ranked: ranked.clone(),
            semantic_ranked: ranked,
            relevant_ids: big_relevant,
            top_k,
        };

        let report_small = SemanticQualityHarness::new(vec![query_small]).run();
        let report_big = SemanticQualityHarness::new(vec![query_big]).run();

        // Both should produce valid bounded metrics
        for report in [&report_small, &report_big] {
            let m = &report.queries[0].hybrid.metrics;
            prop_assert!(m.precision_at_k >= 0.0 && m.precision_at_k <= 1.0);
            prop_assert!(m.recall_at_k >= 0.0 && m.recall_at_k <= 1.0);
            prop_assert!(m.ndcg_at_k >= 0.0 && m.ndcg_at_k <= 1.0);
        }
    }
}

// ── Default corpus invariants ─────────────────────────────────────────

#[test]
fn default_corpus_is_nonempty_and_valid() {
    let queries = default_semantic_eval_queries();
    assert!(!queries.is_empty());
    for q in &queries {
        assert!(!q.name.is_empty());
        assert!(q.top_k > 0);
        assert!(!q.relevant_ids.is_empty());
        assert!(!q.lexical_ranked.is_empty());
        assert!(!q.semantic_ranked.is_empty());
    }
}

#[test]
fn default_corpus_passes_default_thresholds() {
    let report = SemanticQualityHarness::new(default_semantic_eval_queries()).run();
    assert!(
        report.passed,
        "default corpus should pass default thresholds: {:?}",
        report.violations
    );
}
