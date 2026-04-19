#![cfg(feature = "frankensearch")]

use frankenterm_core::search::{
    RerankBridgeMetrics, RerankExplanation, RerankerBridgeConfig, compute_bridge_metrics,
};
use proptest::prelude::*;

fn arb_rerank_bridge_metrics() -> impl Strategy<Value = RerankBridgeMetrics> {
    (
        0usize..256,
        0usize..256,
        "[a-z0-9_-]{1,24}",
        0usize..256,
        0usize..256,
        0usize..256,
        0usize..256,
        -1000.0f32..1000.0f32,
    )
        .prop_map(
            |(
                input_count,
                output_count,
                reranker_id,
                promoted_count,
                demoted_count,
                unchanged_count,
                max_rank_change,
                mean_score_delta,
            )| RerankBridgeMetrics {
                input_count,
                output_count,
                reranker_id,
                promoted_count,
                demoted_count,
                unchanged_count,
                max_rank_change,
                mean_score_delta,
            },
        )
}

fn arb_reranker_bridge_config() -> impl Strategy<Value = RerankerBridgeConfig> {
    (0usize..2048, 0usize..512, 1usize..4096).prop_map(
        |(top_k_rerank, min_candidates, max_length)| RerankerBridgeConfig {
            top_k_rerank,
            min_candidates,
            max_length,
        },
    )
}

fn arb_rerank_explanation() -> impl Strategy<Value = RerankExplanation> {
    (
        0u64..4096,
        0usize..128,
        0usize..128,
        -1000.0f32..1000.0f32,
        -1000.0f32..1000.0f32,
    )
        .prop_map(
            |(doc_id, original_rank, reranked_rank, original_score, rerank_score)| {
                let rank_delta = original_rank as i64 - reranked_rank as i64;
                let score_delta = rerank_score - original_score;
                RerankExplanation {
                    doc_id,
                    original_rank,
                    reranked_rank,
                    original_score,
                    rerank_score,
                    rank_delta,
                    score_delta,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn rerank_bridge_metrics_json_roundtrip(metrics in arb_rerank_bridge_metrics()) {
        let json = serde_json::to_string(&metrics).unwrap();
        let back: RerankBridgeMetrics = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(back.input_count, metrics.input_count);
        prop_assert_eq!(back.output_count, metrics.output_count);
        prop_assert_eq!(back.reranker_id, metrics.reranker_id);
        prop_assert_eq!(back.promoted_count, metrics.promoted_count);
        prop_assert_eq!(back.demoted_count, metrics.demoted_count);
        prop_assert_eq!(back.unchanged_count, metrics.unchanged_count);
        prop_assert_eq!(back.max_rank_change, metrics.max_rank_change);
        prop_assert_eq!(back.mean_score_delta.to_bits(), metrics.mean_score_delta.to_bits());
    }

    #[test]
    fn reranker_bridge_config_json_roundtrip(config in arb_reranker_bridge_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: RerankerBridgeConfig = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(back.top_k_rerank, config.top_k_rerank);
        prop_assert_eq!(back.min_candidates, config.min_candidates);
        prop_assert_eq!(back.max_length, config.max_length);
    }

    #[test]
    fn compute_bridge_metrics_matches_explanation_population(
        explanations in proptest::collection::vec(arb_rerank_explanation(), 0..64),
        reranker_id in "[a-z0-9_-]{1,24}",
        input_count in 0usize..128,
    ) {
        let metrics = compute_bridge_metrics(&explanations, &reranker_id, input_count);
        let promoted = explanations.iter().filter(|exp| exp.rank_delta > 0).count();
        let demoted = explanations.iter().filter(|exp| exp.rank_delta < 0).count();
        let unchanged = explanations.iter().filter(|exp| exp.rank_delta == 0).count();
        let max_rank_change = explanations
            .iter()
            .map(|exp| exp.rank_delta.unsigned_abs() as usize)
            .max()
            .unwrap_or(0);
        let mean_score_delta = if explanations.is_empty() {
            0.0
        } else {
            explanations.iter().map(|exp| exp.score_delta).sum::<f32>() / explanations.len() as f32
        };

        prop_assert_eq!(metrics.input_count, input_count);
        prop_assert_eq!(metrics.output_count, explanations.len());
        prop_assert_eq!(metrics.reranker_id, reranker_id);
        prop_assert_eq!(metrics.promoted_count, promoted);
        prop_assert_eq!(metrics.demoted_count, demoted);
        prop_assert_eq!(metrics.unchanged_count, unchanged);
        prop_assert_eq!(metrics.max_rank_change, max_rank_change);
        prop_assert_eq!(metrics.mean_score_delta.to_bits(), mean_score_delta.to_bits());
    }
}

#[test]
fn reranker_bridge_config_empty_json_uses_defaults() {
    let back: RerankerBridgeConfig = serde_json::from_str("{}").unwrap();

    assert_eq!(back.top_k_rerank, 100);
    assert_eq!(back.min_candidates, 5);
    assert_eq!(back.max_length, 512);
}
