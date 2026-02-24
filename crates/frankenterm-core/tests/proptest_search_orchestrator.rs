//! Property-based tests for SearchOrchestrator (ft-dr6zv.1.3.2).
//!
//! Verifies algebraic invariants of the orchestration layer:
//! backend selection, config serde, fusion determinism, comparison symmetry,
//! and ranking stability.

use frankenterm_core::search::orchestrator::{
    LegacySearchInput, OrchestrationBackend, OrchestratorConfig, SearchModeConfig,
    SearchOrchestrator,
};
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn arb_backend() -> impl Strategy<Value = OrchestrationBackend> {
    prop_oneof![
        Just(OrchestrationBackend::Legacy),
        Just(OrchestrationBackend::Bridge),
    ]
}

fn arb_search_mode() -> impl Strategy<Value = SearchModeConfig> {
    prop_oneof![
        Just(SearchModeConfig::Lexical),
        Just(SearchModeConfig::Semantic),
        Just(SearchModeConfig::Hybrid),
    ]
}

fn arb_config() -> impl Strategy<Value = OrchestratorConfig> {
    (
        arb_backend(),
        arb_search_mode(),
        1..=200_u32,          // rrf_k
        0.0..=1.0_f32,        // alpha
        0.1..=5.0_f32,        // lexical_weight
        0.1..=5.0_f32,        // semantic_weight
        any::<bool>(),         // fallback_to_legacy
    )
        .prop_map(|(backend, mode, rrf_k, alpha, lw, sw, fallback)| OrchestratorConfig {
            backend,
            mode,
            rrf_k,
            alpha,
            lexical_weight: lw,
            semantic_weight: sw,
            fallback_to_legacy: fallback,
        })
}

fn arb_ranked_list(max_len: usize) -> impl Strategy<Value = Vec<(u64, f32)>> {
    prop::collection::vec((1..=1000_u64, 0.001..=100.0_f32), 0..=max_len)
        .prop_map(|mut v| {
            // Deduplicate by ID (first wins)
            let mut seen = std::collections::HashSet::new();
            v.retain(|(id, _)| seen.insert(*id));
            // Sort by score descending
            v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            v
        })
}

fn arb_input() -> impl Strategy<Value = LegacySearchInput> {
    (arb_ranked_list(20), arb_ranked_list(20), 1..=50_usize).prop_map(
        |(lexical_ranked, semantic_ranked, top_k)| LegacySearchInput {
            lexical_ranked,
            semantic_ranked,
            top_k,
        },
    )
}

// ── Backend Properties ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // BE-1: Backend parse roundtrip
    #[test]
    fn backend_parse_roundtrip(backend in arb_backend()) {
        let s = backend.as_str();
        let parsed = OrchestrationBackend::parse(s);
        prop_assert_eq!(backend, parsed);
    }

    // BE-2: Backend serde roundtrip
    #[test]
    fn backend_serde_roundtrip(backend in arb_backend()) {
        let json = serde_json::to_string(&backend).unwrap();
        let back: OrchestrationBackend = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(backend, back);
    }

    // BE-3: Unknown strings parse to Legacy
    #[test]
    fn unknown_string_parses_to_legacy(s in "[a-zA-Z]{1,20}") {
        let parsed = OrchestrationBackend::parse(&s);
        // Any non-bridge string should be Legacy
        let is_bridge_alias = matches!(
            s.to_lowercase().as_str(),
            "bridge" | "frankensearch" | "two_tier" | "twotier"
        );
        if is_bridge_alias {
            prop_assert_eq!(parsed, OrchestrationBackend::Bridge);
        } else {
            prop_assert_eq!(parsed, OrchestrationBackend::Legacy);
        }
    }
}

// ── Config Properties ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // CFG-1: Config serde roundtrip
    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: OrchestratorConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.backend, back.backend);
        prop_assert_eq!(config.mode, back.mode);
        prop_assert_eq!(config.rrf_k, back.rrf_k);
        prop_assert!(config.fallback_to_legacy == back.fallback_to_legacy);
    }

    // CFG-2: SearchModeConfig roundtrip via SearchMode
    #[test]
    fn search_mode_config_roundtrip(mode in arb_search_mode()) {
        use frankenterm_core::search::SearchMode;
        let search_mode: SearchMode = mode.into();
        let back: SearchModeConfig = search_mode.into();
        prop_assert_eq!(mode, back);
    }
}

// ── Fusion Properties ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // FUS-1: Deterministic fusion — same input produces same output
    #[test]
    fn fusion_deterministic(
        config in arb_config(),
        input in arb_input(),
    ) {
        let orch = SearchOrchestrator::new(config);
        let r1 = orch.fuse_ranked(&input);
        let r2 = orch.fuse_ranked(&input);

        prop_assert_eq!(r1.results.len(), r2.results.len());
        for (a, b) in r1.results.iter().zip(r2.results.iter()) {
            prop_assert_eq!(a.id, b.id);
            prop_assert!(
                (a.score - b.score).abs() < 1e-10,
                "score mismatch for id={}: {} vs {}", a.id, a.score, b.score
            );
        }
    }

    // FUS-2: Result count ≤ top_k
    #[test]
    fn result_count_bounded_by_top_k(
        config in arb_config(),
        input in arb_input(),
    ) {
        let orch = SearchOrchestrator::new(config);
        let result = orch.fuse_ranked(&input);
        prop_assert!(result.results.len() <= input.top_k);
    }

    // FUS-3: Empty input produces empty results
    #[test]
    fn empty_input_empty_results(config in arb_config()) {
        let orch = SearchOrchestrator::new(config);
        let input = LegacySearchInput {
            lexical_ranked: vec![],
            semantic_ranked: vec![],
            top_k: 10,
        };
        let result = orch.fuse_ranked(&input);
        prop_assert!(result.results.is_empty());
    }

    // FUS-4: All result IDs come from input
    #[test]
    fn result_ids_from_input(
        config in arb_config(),
        input in arb_input(),
    ) {
        let orch = SearchOrchestrator::new(config);
        let result = orch.fuse_ranked(&input);

        let input_ids: std::collections::HashSet<u64> = input.lexical_ranked.iter()
            .chain(input.semantic_ranked.iter())
            .map(|(id, _)| *id)
            .collect();

        for r in &result.results {
            prop_assert!(
                input_ids.contains(&r.id),
                "result id {} not in input", r.id
            );
        }
    }

    // FUS-5: No duplicate IDs in results
    #[test]
    fn no_duplicate_result_ids(
        config in arb_config(),
        input in arb_input(),
    ) {
        let orch = SearchOrchestrator::new(config);
        let result = orch.fuse_ranked(&input);

        let mut seen = std::collections::HashSet::new();
        for r in &result.results {
            prop_assert!(seen.insert(r.id), "duplicate id: {}", r.id);
        }
    }

    // FUS-6: Results are sorted by score descending
    #[test]
    fn results_sorted_by_score(
        config in arb_config(),
        input in arb_input(),
    ) {
        let orch = SearchOrchestrator::new(config);
        let result = orch.fuse_ranked(&input);

        for window in result.results.windows(2) {
            prop_assert!(
                window[0].score >= window[1].score - 1e-10,
                "not sorted: {} >= {}", window[0].score, window[1].score
            );
        }
    }

    // FUS-7: Metrics report correct backend string
    #[test]
    fn metrics_backend_matches(
        config in arb_config(),
        input in arb_input(),
    ) {
        let expected_backend = config.backend.as_str();
        let orch = SearchOrchestrator::new(config);
        let result = orch.fuse_ranked(&input);
        prop_assert_eq!(result.metrics.backend.as_str(), expected_backend);
    }

    // FUS-8: Metrics candidate counts match input
    #[test]
    fn metrics_candidate_counts(
        config in arb_config(),
        input in arb_input(),
    ) {
        let orch = SearchOrchestrator::new(config);
        let result = orch.fuse_ranked(&input);
        prop_assert_eq!(result.metrics.lexical_candidates, input.lexical_ranked.len());
        prop_assert_eq!(result.metrics.semantic_candidates, input.semantic_ranked.len());
    }
}

// ── Comparison Properties ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // CMP-1: In B1, both backends produce identical rankings
    #[test]
    fn b1_backends_agree(input in arb_input()) {
        let orch = SearchOrchestrator::new(OrchestratorConfig::default());
        let comparison = orch.compare_backends(&input);
        prop_assert!(
            comparison.ranking_match,
            "B1 backends should produce identical rankings"
        );
        prop_assert!(
            comparison.max_score_diff < 1e-6,
            "B1 score diff: {}", comparison.max_score_diff
        );
    }

    // CMP-2: Comparison result counts match
    #[test]
    fn comparison_result_counts_match(input in arb_input()) {
        let orch = SearchOrchestrator::new(OrchestratorConfig::default());
        let comparison = orch.compare_backends(&input);
        prop_assert_eq!(
            comparison.legacy.results.len(),
            comparison.bridge.results.len()
        );
    }

    // CMP-3: Comparison metrics report correct backends
    #[test]
    fn comparison_metrics_backends(input in arb_input()) {
        let orch = SearchOrchestrator::new(OrchestratorConfig::default());
        let comparison = orch.compare_backends(&input);
        prop_assert_eq!(comparison.legacy.metrics.backend.as_str(), "legacy");
        prop_assert_eq!(comparison.bridge.metrics.backend.as_str(), "bridge");
    }
}

// ── Lexical-only / Semantic-only Properties ────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // LSO-1: Lexical-only mode ignores semantic list
    #[test]
    fn lexical_only_ignores_semantic(
        lexical in arb_ranked_list(10),
        semantic in arb_ranked_list(10),
        top_k in 1..=20_usize,
    ) {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            mode: SearchModeConfig::Lexical,
            ..Default::default()
        });
        let result = orch.fuse_ranked(&LegacySearchInput {
            lexical_ranked: lexical.clone(),
            semantic_ranked: semantic,
            top_k,
        });

        // All result IDs should come from lexical list
        let lex_ids: std::collections::HashSet<u64> = lexical.iter().map(|(id, _)| *id).collect();
        for r in &result.results {
            prop_assert!(lex_ids.contains(&r.id), "id {} not in lexical list", r.id);
        }
    }

    // LSO-2: Semantic-only mode ignores lexical list
    #[test]
    fn semantic_only_ignores_lexical(
        lexical in arb_ranked_list(10),
        semantic in arb_ranked_list(10),
        top_k in 1..=20_usize,
    ) {
        let orch = SearchOrchestrator::new(OrchestratorConfig {
            mode: SearchModeConfig::Semantic,
            ..Default::default()
        });
        let result = orch.fuse_ranked(&LegacySearchInput {
            lexical_ranked: lexical,
            semantic_ranked: semantic.clone(),
            top_k,
        });

        let sem_ids: std::collections::HashSet<u64> = semantic.iter().map(|(id, _)| *id).collect();
        for r in &result.results {
            prop_assert!(sem_ids.contains(&r.id), "id {} not in semantic list", r.id);
        }
    }
}
