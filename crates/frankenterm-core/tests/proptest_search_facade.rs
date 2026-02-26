//! Property-based tests for SearchFacade (ft-dr6zv.1.3.C1).

use proptest::prelude::*;

use frankenterm_core::search::{
    FacadeConfig, FacadeRouting, HybridSearchService, SearchFacade, SearchMode,
    check_schema_preservation,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_routing() -> impl Strategy<Value = FacadeRouting> {
    prop_oneof![
        Just(FacadeRouting::Legacy),
        Just(FacadeRouting::Orchestrated),
        Just(FacadeRouting::Shadow),
    ]
}

#[allow(dead_code)]
fn arb_mode() -> impl Strategy<Value = SearchMode> {
    prop_oneof![
        Just(SearchMode::Lexical),
        Just(SearchMode::Semantic),
        Just(SearchMode::Hybrid),
    ]
}

fn arb_ranked_list(max_len: usize) -> impl Strategy<Value = Vec<(u64, f32)>> {
    proptest::collection::vec((1u64..1000, 0.0f32..100.0), 0..=max_len)
}

// ---------------------------------------------------------------------------
// FAC-1: Legacy facade produces identical results to direct HybridSearchService
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn fac_1_legacy_matches_direct(
        lex in arb_ranked_list(20),
        sem in arb_ranked_list(20),
        top_k in 1usize..50,
        rrf_k in 1u32..200,
    ) {
        let direct = HybridSearchService::new()
            .with_rrf_k(rrf_k)
            .fuse(&lex, &sem, top_k);

        let facade = SearchFacade::new()
            .with_rrf_k(rrf_k)
            .fuse(&lex, &sem, top_k);

        prop_assert_eq!(direct.len(), facade.len(), "result count must match");
        for (d, f) in direct.iter().zip(facade.iter()) {
            prop_assert_eq!(d.id, f.id, "IDs must match");
            prop_assert!(
                (d.score - f.score).abs() < 1e-5,
                "scores must match: {} vs {}",
                d.score,
                f.score
            );
        }
    }
}

// ---------------------------------------------------------------------------
// FAC-2: Orchestrated path never panics
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn fac_2_orchestrated_never_panics(
        lex in arb_ranked_list(20),
        sem in arb_ranked_list(20),
        top_k in 1usize..50,
    ) {
        let config = FacadeConfig {
            routing: FacadeRouting::Orchestrated,
            ..FacadeConfig::default()
        };
        let facade = SearchFacade::with_config(config);
        let _results = facade.fuse(&lex, &sem, top_k);
        // Just verify no panic.
    }
}

// ---------------------------------------------------------------------------
// FAC-3: Shadow mode always returns legacy results
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn fac_3_shadow_returns_legacy(
        lex in arb_ranked_list(20),
        sem in arb_ranked_list(20),
        top_k in 1usize..50,
    ) {
        let legacy_results = SearchFacade::new().fuse(&lex, &sem, top_k);

        let config = FacadeConfig {
            routing: FacadeRouting::Shadow,
            ..FacadeConfig::default()
        };
        let shadow_results = SearchFacade::with_config(config).fuse(&lex, &sem, top_k);

        prop_assert_eq!(legacy_results.len(), shadow_results.len());
        for (l, s) in legacy_results.iter().zip(shadow_results.iter()) {
            prop_assert_eq!(l.id, s.id);
            prop_assert!(
                (l.score - s.score).abs() < 1e-5,
                "shadow must return legacy scores"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// FAC-4: Result count respects top_k
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn fac_4_top_k_respected(
        routing in arb_routing(),
        lex in arb_ranked_list(20),
        sem in arb_ranked_list(20),
        top_k in 1usize..50,
    ) {
        let config = FacadeConfig {
            routing,
            ..FacadeConfig::default()
        };
        let results = SearchFacade::with_config(config).fuse(&lex, &sem, top_k);
        prop_assert!(
            results.len() <= top_k,
            "result count {} exceeds top_k {}",
            results.len(),
            top_k
        );
    }
}

// ---------------------------------------------------------------------------
// FAC-5: Results are sorted descending by score
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn fac_5_sorted_descending(
        routing in arb_routing(),
        lex in arb_ranked_list(20),
        sem in arb_ranked_list(20),
        top_k in 1usize..50,
    ) {
        let config = FacadeConfig {
            routing,
            ..FacadeConfig::default()
        };
        let results = SearchFacade::with_config(config).fuse(&lex, &sem, top_k);
        for window in results.windows(2) {
            prop_assert!(
                window[0].score >= window[1].score - 1e-8,
                "not sorted: {} > {}",
                window[1].score,
                window[0].score
            );
        }
    }
}

// ---------------------------------------------------------------------------
// FAC-6: Fusion is deterministic
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn fac_6_deterministic(
        routing in arb_routing(),
        lex in arb_ranked_list(10),
        sem in arb_ranked_list(10),
        top_k in 1usize..20,
    ) {
        let config = FacadeConfig {
            routing,
            ..FacadeConfig::default()
        };
        let r1 = SearchFacade::with_config(config.clone()).fuse(&lex, &sem, top_k);
        let r2 = SearchFacade::with_config(config).fuse(&lex, &sem, top_k);
        prop_assert_eq!(r1.len(), r2.len());
        for (a, b) in r1.iter().zip(r2.iter()) {
            prop_assert_eq!(a.id, b.id);
            prop_assert!((a.score - b.score).abs() < 1e-8);
        }
    }
}

// ---------------------------------------------------------------------------
// FAC-7: Routing serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn fac_7_routing_serde(routing in arb_routing()) {
        let json = serde_json::to_string(&routing).unwrap();
        let parsed: FacadeRouting = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(routing, parsed);
    }
}

// ---------------------------------------------------------------------------
// FAC-8: Config serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn fac_8_config_serde(
        routing in arb_routing(),
        score_thresh in 0.0f32..10.0,
        tau_thresh in -1.0f32..1.0,
    ) {
        let config = FacadeConfig {
            routing,
            orchestrator: Default::default(),
            shadow_score_threshold: score_thresh,
            shadow_tau_threshold: tau_thresh,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: FacadeConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.routing, parsed.routing);
        prop_assert!((config.shadow_score_threshold - parsed.shadow_score_threshold).abs() < 1e-6);
    }
}

// ---------------------------------------------------------------------------
// FAC-9: Empty inputs are always safe
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn fac_9_empty_inputs(routing in arb_routing()) {
        let config = FacadeConfig {
            routing,
            ..FacadeConfig::default()
        };
        let results = SearchFacade::with_config(config).fuse(&[], &[], 10);
        prop_assert!(results.is_empty());
    }
}

// ---------------------------------------------------------------------------
// FAC-10: Mode affects results
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn fac_10_lexical_mode_ignores_semantic(
        lex in arb_ranked_list(5),
        sem in arb_ranked_list(5),
        top_k in 1usize..20,
    ) {
        let facade = SearchFacade::new().with_mode(SearchMode::Lexical);
        let results = facade.fuse(&lex, &sem, top_k);
        // All results should have Some lexical_rank and None semantic_rank
        for r in &results {
            prop_assert!(r.lexical_rank.is_some());
            prop_assert!(r.semantic_rank.is_none());
        }
    }
}

// ---------------------------------------------------------------------------
// SGT-1: Schema gate is reflexive
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn sgt_1_reflexive(
        n_fields in 0usize..10,
    ) {
        let fields: Vec<_> = (0..n_fields)
            .map(|i| frankenterm_core::search::SchemaField {
                name: format!("field_{}", i),
                field_type: "String".to_string(),
                required: true,
                indexed: true,
            })
            .collect();
        let snap = frankenterm_core::search::SchemaSnapshot {
            fields,
            version: "test".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&snap, &snap);
        prop_assert!(result.safe, "self-comparison must be safe");
    }
}

// ---------------------------------------------------------------------------
// SGT-2: Adding fields is always safe
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn sgt_2_additions_safe(
        n_source in 0usize..5,
        n_extra in 1usize..5,
    ) {
        let source_fields: Vec<_> = (0..n_source)
            .map(|i| frankenterm_core::search::SchemaField {
                name: format!("field_{}", i),
                field_type: "String".to_string(),
                required: true,
                indexed: true,
            })
            .collect();
        let mut target_fields = source_fields.clone();
        for i in n_source..(n_source + n_extra) {
            target_fields.push(frankenterm_core::search::SchemaField {
                name: format!("field_{}", i),
                field_type: "String".to_string(),
                required: false,
                indexed: false,
            });
        }
        let source = frankenterm_core::search::SchemaSnapshot {
            fields: source_fields,
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = frankenterm_core::search::SchemaSnapshot {
            fields: target_fields,
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        prop_assert!(result.safe);
        prop_assert_eq!(result.added_fields.len(), n_extra);
    }
}
