//! Property-based tests for storage-level hybrid search result carriers.
//!
//! Covers [`HybridSearchBundle`] and [`HybridSearchResult`] — the two types
//! returned by `StorageHandle::hybrid_search_with_results` that had no
//! proptest coverage elsewhere in the tree (verified via grep across
//! `crates/frankenterm-core/tests/`).

use frankenterm_core::storage::{
    HybridSearchBundle, HybridSearchResult, SearchResult, Segment,
};
use proptest::prelude::*;

fn arb_segment() -> impl Strategy<Value = Segment> {
    (
        any::<i64>(),
        any::<u64>(),
        any::<u64>(),
        ".*",
        any::<usize>(),
        prop::option::of("[0-9a-f]{8,64}"),
        any::<i64>(),
    )
        .prop_map(
            |(id, pane_id, seq, content, content_len, content_hash, captured_at)| Segment {
                id,
                pane_id,
                seq,
                content,
                content_len,
                content_hash,
                captured_at,
            },
        )
}

fn arb_search_result() -> impl Strategy<Value = SearchResult> {
    (
        arb_segment(),
        prop::option::of(".{0,64}"),
        prop::option::of(".{0,64}"),
        any::<f64>().prop_filter("finite", |v| v.is_finite()),
    )
        .prop_map(|(segment, snippet, highlight, score)| SearchResult {
            segment,
            snippet,
            highlight,
            score,
        })
}

fn arb_hybrid_result() -> impl Strategy<Value = HybridSearchResult> {
    (
        arb_search_result(),
        prop::option::of(any::<f64>().prop_filter("finite", |v| v.is_finite())),
        prop::option::of(0usize..10_000),
        prop::option::of(0usize..10_000),
        prop::option::of(any::<f64>().prop_filter("finite", |v| v.is_finite())),
        prop::option::of(any::<f64>().prop_filter("finite", |v| v.is_finite())),
        0usize..10_000,
        any::<f64>().prop_filter("finite", |v| v.is_finite()),
    )
        .prop_map(
            |(
                result,
                semantic_score,
                lexical_rank,
                semantic_rank,
                lexical_contribution,
                semantic_contribution,
                fusion_rank,
                fusion_score,
            )| HybridSearchResult {
                result,
                semantic_score,
                lexical_rank,
                semantic_rank,
                lexical_contribution,
                semantic_contribution,
                fusion_rank,
                fusion_score,
            },
        )
}

fn arb_hybrid_bundle() -> impl Strategy<Value = HybridSearchBundle> {
    (
        (
            "(lexical|semantic|hybrid|auto)",
            "(lexical|semantic|hybrid|auto)",
            prop::option::of("[a-z_]{3,32}"),
            1u32..256,
            (0f32..1f32).prop_filter("finite", |v| v.is_finite()),
            (0f32..1f32).prop_filter("finite", |v| v.is_finite()),
            "[a-z_]{0,32}",
        ),
        (
            0usize..10_000,
            0usize..10_000,
            any::<bool>(),
            0u64..10_000_000,
            0usize..1_000_000,
            "(active|cache_hit|backoff|disabled|)",
            prop::option::of(0i64..9_999_999_999_999),
            prop::collection::vec(arb_hybrid_result(), 0..8),
        ),
    )
        .prop_map(
            |(
                (
                    mode,
                    requested_mode,
                    fallback_reason,
                    rrf_k,
                    lexical_weight,
                    semantic_weight,
                    fusion_backend,
                ),
                (
                    lexical_candidates,
                    semantic_candidates,
                    semantic_cache_hit,
                    semantic_latency_ms,
                    semantic_rows_scanned,
                    semantic_budget_state,
                    semantic_backoff_until_ms,
                    results,
                ),
            )| HybridSearchBundle {
                mode,
                requested_mode,
                fallback_reason,
                rrf_k,
                lexical_weight,
                semantic_weight,
                fusion_backend,
                lexical_candidates,
                semantic_candidates,
                semantic_cache_hit,
                semantic_latency_ms,
                semantic_rows_scanned,
                semantic_budget_state,
                semantic_backoff_until_ms,
                results,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// HybridSearchResult survives a JSON roundtrip with value-level equality
    /// on every non-float field (floats checked bitwise only when finite).
    #[test]
    fn hybrid_search_result_json_roundtrip(hit in arb_hybrid_result()) {
        let json = serde_json::to_string(&hit).unwrap();
        let back: HybridSearchResult = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(hit.result.segment.id, back.result.segment.id);
        prop_assert_eq!(hit.result.segment.pane_id, back.result.segment.pane_id);
        prop_assert_eq!(hit.result.segment.seq, back.result.segment.seq);
        prop_assert_eq!(&hit.result.segment.content, &back.result.segment.content);
        prop_assert_eq!(hit.result.segment.content_len, back.result.segment.content_len);
        prop_assert_eq!(&hit.result.segment.content_hash, &back.result.segment.content_hash);
        prop_assert_eq!(hit.result.segment.captured_at, back.result.segment.captured_at);
        prop_assert_eq!(&hit.result.snippet, &back.result.snippet);
        prop_assert_eq!(&hit.result.highlight, &back.result.highlight);
        prop_assert_eq!(hit.result.score.to_bits(), back.result.score.to_bits());
        prop_assert_eq!(hit.lexical_rank, back.lexical_rank);
        prop_assert_eq!(hit.semantic_rank, back.semantic_rank);
        prop_assert_eq!(hit.fusion_rank, back.fusion_rank);
        prop_assert_eq!(hit.fusion_score.to_bits(), back.fusion_score.to_bits());
        match (hit.semantic_score, back.semantic_score) {
            (None, None) => {}
            (Some(a), Some(b)) => prop_assert_eq!(a.to_bits(), b.to_bits()),
            _ => prop_assert!(false, "semantic_score option shape changed"),
        }
        match (hit.lexical_contribution, back.lexical_contribution) {
            (None, None) => {}
            (Some(a), Some(b)) => prop_assert_eq!(a.to_bits(), b.to_bits()),
            _ => prop_assert!(false, "lexical_contribution option shape changed"),
        }
        match (hit.semantic_contribution, back.semantic_contribution) {
            (None, None) => {}
            (Some(a), Some(b)) => prop_assert_eq!(a.to_bits(), b.to_bits()),
            _ => prop_assert!(false, "semantic_contribution option shape changed"),
        }
    }

    /// HybridSearchBundle roundtrips through JSON preserving every
    /// scalar field and result-count invariants.
    #[test]
    fn hybrid_search_bundle_json_roundtrip(bundle in arb_hybrid_bundle()) {
        let json = serde_json::to_string(&bundle).unwrap();
        let back: HybridSearchBundle = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(&bundle.mode, &back.mode);
        prop_assert_eq!(&bundle.requested_mode, &back.requested_mode);
        prop_assert_eq!(&bundle.fallback_reason, &back.fallback_reason);
        prop_assert_eq!(bundle.rrf_k, back.rrf_k);
        prop_assert_eq!(bundle.lexical_weight.to_bits(), back.lexical_weight.to_bits());
        prop_assert_eq!(bundle.semantic_weight.to_bits(), back.semantic_weight.to_bits());
        prop_assert_eq!(&bundle.fusion_backend, &back.fusion_backend);
        prop_assert_eq!(bundle.lexical_candidates, back.lexical_candidates);
        prop_assert_eq!(bundle.semantic_candidates, back.semantic_candidates);
        prop_assert_eq!(bundle.semantic_cache_hit, back.semantic_cache_hit);
        prop_assert_eq!(bundle.semantic_latency_ms, back.semantic_latency_ms);
        prop_assert_eq!(bundle.semantic_rows_scanned, back.semantic_rows_scanned);
        prop_assert_eq!(&bundle.semantic_budget_state, &back.semantic_budget_state);
        prop_assert_eq!(bundle.semantic_backoff_until_ms, back.semantic_backoff_until_ms);
        prop_assert_eq!(bundle.results.len(), back.results.len());
    }

    /// Field-default invariants — optional fields use serde defaults.
    ///
    /// The bundle declares `#[serde(default)]` on several optional fields;
    /// omitting them in JSON must parse to their zero/empty value rather
    /// than error.
    #[test]
    fn hybrid_search_bundle_accepts_minimal_payload(
        mode in "(lexical|semantic|hybrid|auto)",
        requested_mode in "(lexical|semantic|hybrid|auto)",
        rrf_k in 1u32..256,
        lexical_weight in 0f32..1f32,
        semantic_weight in 0f32..1f32,
        lexical_candidates in 0usize..10_000,
        semantic_candidates in 0usize..10_000,
    ) {
        let payload = serde_json::json!({
            "mode": mode,
            "requested_mode": requested_mode,
            "rrf_k": rrf_k,
            "lexical_weight": lexical_weight,
            "semantic_weight": semantic_weight,
            "lexical_candidates": lexical_candidates,
            "semantic_candidates": semantic_candidates,
            "results": [],
        });
        let bundle: HybridSearchBundle = serde_json::from_value(payload).unwrap();

        prop_assert_eq!(bundle.fallback_reason, None);
        prop_assert_eq!(&bundle.fusion_backend, "");
        prop_assert!(!bundle.semantic_cache_hit);
        prop_assert_eq!(bundle.semantic_latency_ms, 0);
        prop_assert_eq!(bundle.semantic_rows_scanned, 0);
        prop_assert_eq!(&bundle.semantic_budget_state, "");
        prop_assert_eq!(bundle.semantic_backoff_until_ms, None);
        prop_assert!(bundle.results.is_empty());
    }

    /// HybridSearchResult accepts a minimal payload: only `result`,
    /// `fusion_rank`, `fusion_score` are required; all rank/score extras
    /// are `#[serde(default)]`.
    #[test]
    fn hybrid_search_result_accepts_minimal_payload(
        segment_id in any::<i64>(),
        pane_id in any::<u64>(),
        seq in any::<u64>(),
        fusion_rank in 0usize..10_000,
        fusion_score in -1e6f64..1e6f64,
    ) {
        let payload = serde_json::json!({
            "result": {
                "segment": {
                    "id": segment_id,
                    "pane_id": pane_id,
                    "seq": seq,
                    "content": "",
                    "content_len": 0,
                    "content_hash": null,
                    "captured_at": 0,
                },
                "snippet": null,
                "highlight": null,
                "score": 0.0,
            },
            "fusion_rank": fusion_rank,
            "fusion_score": fusion_score,
        });

        let hit: HybridSearchResult = serde_json::from_value(payload).unwrap();
        prop_assert_eq!(hit.semantic_score, None);
        prop_assert_eq!(hit.lexical_rank, None);
        prop_assert_eq!(hit.semantic_rank, None);
        prop_assert_eq!(hit.lexical_contribution, None);
        prop_assert_eq!(hit.semantic_contribution, None);
        prop_assert_eq!(hit.fusion_rank, fusion_rank);
        prop_assert_eq!(hit.fusion_score.to_bits(), fusion_score.to_bits());
    }

    /// A bundle constructed with `results.len() == N` must report the
    /// same length after a JSON roundtrip regardless of other fields.
    #[test]
    fn hybrid_search_bundle_results_len_is_preserved(
        results in prop::collection::vec(arb_hybrid_result(), 0..16),
    ) {
        let bundle = HybridSearchBundle {
            mode: "hybrid".to_string(),
            requested_mode: "hybrid".to_string(),
            fallback_reason: None,
            rrf_k: 60,
            lexical_weight: 0.5,
            semantic_weight: 0.5,
            fusion_backend: "rrf".to_string(),
            lexical_candidates: results.len(),
            semantic_candidates: results.len(),
            semantic_cache_hit: false,
            semantic_latency_ms: 0,
            semantic_rows_scanned: 0,
            semantic_budget_state: "active".to_string(),
            semantic_backoff_until_ms: None,
            results: results.clone(),
        };

        let json = serde_json::to_string(&bundle).unwrap();
        let back: HybridSearchBundle = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.results.len(), results.len());
        for (original, roundtripped) in results.iter().zip(back.results.iter()) {
            prop_assert_eq!(original.fusion_rank, roundtripped.fusion_rank);
            prop_assert_eq!(
                original.fusion_score.to_bits(),
                roundtripped.fusion_score.to_bits()
            );
        }
    }
}
