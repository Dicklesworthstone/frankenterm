//! Property-based tests for search robot API types (ft-dr6zv.1.6).

use proptest::prelude::*;

use frankenterm_core::robot_types::{
    ExplainedSearchHit, PipelineWatermarkInfo, SearchExplainData, SearchHit,
    SearchPipelineControlResult, SearchPipelineStatusData, SearchPipelineTiming,
    SearchScoringBreakdown, SearchStreamPhase,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_scoring_breakdown() -> impl Strategy<Value = SearchScoringBreakdown> {
    (
        proptest::option::of(0.0f64..100.0),            // bm25
        proptest::collection::vec("[a-z]{2,10}", 0..5), // matching_terms
        proptest::option::of(0.0f64..1.0),              // semantic_similarity
        proptest::option::of(prop_oneof![
            Just("hash".to_string()),
            Just("model2vec".to_string()),
            Just("fastembed".to_string()),
        ]), // embedder_tier
        proptest::option::of(1usize..100),              // rrf_rank
        proptest::option::of(0.0f64..1.0),              // rrf_score
        proptest::option::of(0.0f64..1.0),              // reranker_score
        0.0f64..100.0,                                  // final_score
    )
        .prop_map(|(bm25, terms, sem, tier, rank, rrf, rerank, final_s)| {
            SearchScoringBreakdown {
                bm25_score: bm25,
                matching_terms: terms,
                semantic_similarity: sem,
                embedder_tier: tier,
                rrf_rank: rank,
                rrf_score: rrf,
                reranker_score: rerank,
                final_score: final_s,
            }
        })
}

fn arb_search_hit() -> impl Strategy<Value = SearchHit> {
    (
        1i64..10_000,            // segment_id
        1u64..100,               // pane_id
        1u64..1000,              // seq
        1_000_000i64..2_000_000, // captured_at
        0.0f64..100.0,           // score
    )
        .prop_map(|(seg, pane, seq, cap, score)| SearchHit {
            segment_id: seg,
            pane_id: pane,
            seq,
            captured_at: cap,
            score,
            snippet: None,
            content: None,
            semantic_score: None,
            fusion_rank: None,
        })
}

fn arb_pipeline_timing() -> impl Strategy<Value = SearchPipelineTiming> {
    (
        1u64..100_000,                      // total_us
        proptest::option::of(0u64..50_000), // lexical_us
        proptest::option::of(0u64..50_000), // semantic_us
        proptest::option::of(0u64..10_000), // fusion_us
        proptest::option::of(0u64..30_000), // rerank_us
    )
        .prop_map(|(total, lex, sem, fuse, rerank)| SearchPipelineTiming {
            total_us: total,
            lexical_us: lex,
            semantic_us: sem,
            fusion_us: fuse,
            rerank_us: rerank,
        })
}

fn arb_watermark_info() -> impl Strategy<Value = PipelineWatermarkInfo> {
    (
        1u64..1000,                         // pane_id
        0i64..2_000_000,                    // last_indexed_at_ms
        0u64..10_000,                       // total_docs_indexed
        proptest::option::of("[a-z]{4,8}"), // session_id
    )
        .prop_map(|(pane_id, ts, docs, sess)| PipelineWatermarkInfo {
            pane_id,
            last_indexed_at_ms: ts,
            total_docs_indexed: docs,
            session_id: sess,
        })
}

// ---------------------------------------------------------------------------
// SRA-1: SearchScoringBreakdown serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn sra_1_scoring_breakdown_serde(breakdown in arb_scoring_breakdown()) {
        let json = serde_json::to_string(&breakdown).unwrap();
        let parsed: SearchScoringBreakdown = serde_json::from_str(&json).unwrap();

        // Compare non-float fields exactly.
        prop_assert_eq!(&parsed.matching_terms, &breakdown.matching_terms);
        prop_assert_eq!(parsed.embedder_tier, breakdown.embedder_tier);
        prop_assert_eq!(parsed.rrf_rank, breakdown.rrf_rank);

        // Float fields with tolerance.
        if let (Some(a), Some(b)) = (parsed.bm25_score, breakdown.bm25_score) {
            prop_assert!((a - b).abs() < 1e-10, "bm25 drift");
        }
        if let (Some(a), Some(b)) = (parsed.semantic_similarity, breakdown.semantic_similarity) {
            prop_assert!((a - b).abs() < 1e-10, "semantic drift");
        }
        prop_assert!(
            (parsed.final_score - breakdown.final_score).abs() < 1e-10,
            "final_score drift"
        );
    }
}

// ---------------------------------------------------------------------------
// SRA-2: ExplainedSearchHit flattening preserves base hit
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn sra_2_explained_hit_flattens(
        hit in arb_search_hit(),
        scoring in arb_scoring_breakdown(),
    ) {
        let explained = ExplainedSearchHit {
            hit: hit.clone(),
            scoring: scoring.clone(),
        };
        let json = serde_json::to_string(&explained).unwrap();
        let parsed: ExplainedSearchHit = serde_json::from_str(&json).unwrap();

        // Base hit fields preserved.
        prop_assert_eq!(parsed.hit.segment_id, hit.segment_id);
        prop_assert_eq!(parsed.hit.pane_id, hit.pane_id);
        prop_assert_eq!(parsed.hit.seq, hit.seq);
        prop_assert_eq!(parsed.hit.captured_at, hit.captured_at);

        // Scoring fields preserved.
        prop_assert_eq!(&parsed.scoring.matching_terms, &scoring.matching_terms);
        prop_assert_eq!(parsed.scoring.rrf_rank, scoring.rrf_rank);
    }
}

// ---------------------------------------------------------------------------
// SRA-3: SearchStreamPhase tag discrimination
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn sra_3_stream_phase_discrimination(
        variant in 0u8..3,
        count in 0usize..1000,
        time_us in 0u64..100_000,
    ) {
        let phase = match variant {
            0 => SearchStreamPhase::Fast { result_count: count },
            1 => SearchStreamPhase::Quality { result_count: count },
            _ => SearchStreamPhase::Done {
                total_results: count,
                total_us: time_us,
            },
        };

        let json = serde_json::to_string(&phase).unwrap();
        let parsed: SearchStreamPhase = serde_json::from_str(&json).unwrap();

        // Verify the correct variant survived the roundtrip.
        match (&phase, &parsed) {
            (SearchStreamPhase::Fast { result_count: a }, SearchStreamPhase::Fast { result_count: b }) => {
                prop_assert_eq!(a, b);
            }
            (SearchStreamPhase::Quality { result_count: a }, SearchStreamPhase::Quality { result_count: b }) => {
                prop_assert_eq!(a, b);
            }
            (
                SearchStreamPhase::Done { total_results: a, total_us: ta },
                SearchStreamPhase::Done { total_results: b, total_us: tb },
            ) => {
                prop_assert_eq!(a, b);
                prop_assert_eq!(ta, tb);
            }
            _ => {
                prop_assert!(false, "variant mismatch in roundtrip");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SRA-4: SearchPipelineTiming serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn sra_4_pipeline_timing_serde(timing in arb_pipeline_timing()) {
        let json = serde_json::to_string(&timing).unwrap();
        let parsed: SearchPipelineTiming = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.total_us, timing.total_us);
        prop_assert_eq!(parsed.lexical_us, timing.lexical_us);
        prop_assert_eq!(parsed.semantic_us, timing.semantic_us);
        prop_assert_eq!(parsed.fusion_us, timing.fusion_us);
        prop_assert_eq!(parsed.rerank_us, timing.rerank_us);
    }
}

// ---------------------------------------------------------------------------
// SRA-5: PipelineWatermarkInfo serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn sra_5_watermark_info_serde(wm in arb_watermark_info()) {
        let json = serde_json::to_string(&wm).unwrap();
        let parsed: PipelineWatermarkInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.pane_id, wm.pane_id);
        prop_assert_eq!(parsed.last_indexed_at_ms, wm.last_indexed_at_ms);
        prop_assert_eq!(parsed.total_docs_indexed, wm.total_docs_indexed);
        prop_assert_eq!(parsed.session_id, wm.session_id);
    }
}

// ---------------------------------------------------------------------------
// SRA-6: SearchPipelineStatusData serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn sra_6_pipeline_status_serde(
        state in prop_oneof![Just("running"), Just("paused"), Just("stopped")],
        wm_count in 0usize..5,
        total_ticks in 0u64..10_000,
        total_docs in 0u64..100_000,
        total_lines in 0u64..1_000_000,
    ) {
        let watermarks: Vec<PipelineWatermarkInfo> = (0..wm_count)
            .map(|i| PipelineWatermarkInfo {
                pane_id: i as u64,
                last_indexed_at_ms: 1000 + i as i64 * 100,
                total_docs_indexed: 10 + i as u64,
                session_id: None,
            })
            .collect();

        let status = SearchPipelineStatusData {
            state: state.to_string(),
            watermarks,
            total_ticks,
            total_docs_indexed: total_docs,
            total_lines_consumed: total_lines,
            index_stats: None,
        };

        let json = serde_json::to_string(&status).unwrap();
        let parsed: SearchPipelineStatusData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.state, status.state);
        prop_assert_eq!(parsed.watermarks.len(), wm_count);
        prop_assert_eq!(parsed.total_ticks, total_ticks);
        prop_assert_eq!(parsed.total_docs_indexed, total_docs);
        prop_assert_eq!(parsed.total_lines_consumed, total_lines);
    }
}

// ---------------------------------------------------------------------------
// SRA-7: SearchPipelineControlResult serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn sra_7_control_result_serde(
        action in prop_oneof![Just("pause"), Just("resume"), Just("rebuild"), Just("stop")],
        success in any::<bool>(),
        state_after in prop_oneof![Just("running"), Just("paused"), Just("stopped")],
        message in proptest::option::of("[a-z ]{5,30}"),
    ) {
        let result = SearchPipelineControlResult {
            action: action.to_string(),
            success,
            state_after: state_after.to_string(),
            message,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: SearchPipelineControlResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.action, result.action);
        prop_assert_eq!(parsed.success, result.success);
        prop_assert_eq!(parsed.state_after, result.state_after);
        prop_assert_eq!(parsed.message, result.message);
    }
}

// ---------------------------------------------------------------------------
// SRA-8: SearchExplainData serde roundtrip (structure-only)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn sra_8_explain_data_serde(
        query in "[a-z ]{3,20}",
        mode in prop_oneof![Just("lexical"), Just("semantic"), Just("hybrid")],
        limit in 1usize..100,
        pane_filter in proptest::option::of(1u64..100),
        total_hits in 0usize..500,
    ) {
        let data = SearchExplainData {
            query: query.clone(),
            results: vec![],
            total_hits,
            limit,
            pane_filter,
            mode: mode.to_string(),
            timing: None,
            tier_metrics: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let parsed: SearchExplainData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.query, query);
        prop_assert_eq!(parsed.mode, mode);
        prop_assert_eq!(parsed.limit, limit);
        prop_assert_eq!(parsed.pane_filter, pane_filter);
        prop_assert_eq!(parsed.total_hits, total_hits);
    }
}
