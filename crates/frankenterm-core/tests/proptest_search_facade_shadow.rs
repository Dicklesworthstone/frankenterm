//! Conformance property for the search facade's shadow comparison
//! (`crates/frankenterm-core/src/search/facade.rs`).
//!
//! In `FacadeRouting::Shadow` the facade runs the legacy fusion path
//! (`HybridSearchService::fuse`) and the orchestrated path
//! (`SearchOrchestrator::fuse_ranked`) against the same inputs and compares
//! them. `build_orchestrator` mirrors the legacy service's mode/rrf_k/alpha/
//! weights, and both ultimately delegate to `HybridSearchService::fuse` through
//! the single Hybrid fusion path (`rrf_fuse_with_frankensearch`, local fallback
//! when the `frankensearch` feature is off). The two paths therefore produce
//! identical results: an exact ranking match and zero score divergence for any
//! input — the property a cutover gate relies on.
//!
//! We assert the backend equivalence directly (`ranking_match` + zero
//! `max_score_diff`) rather than `passed`, which additionally depends on the
//! configured tau threshold and the degenerate `kendall_tau == 0.0` for
//! fewer-than-two results (`run_regression_suite` lowers the threshold to -2.0
//! for exactly that reason).

use frankenterm_core::search::{FacadeConfig, FacadeRouting, SearchFacade};
use proptest::prelude::*;

fn arb_ranked_list(max_len: usize) -> impl Strategy<Value = Vec<(u64, f32)>> {
    proptest::collection::hash_set(1u64..=80, 0..=max_len).prop_flat_map(|ids| {
        let ids_vec: Vec<u64> = ids.into_iter().collect();
        let n = ids_vec.len();
        proptest::collection::vec(0.0f32..100.0, n..=n)
            .prop_map(move |scores| ids_vec.iter().copied().zip(scores).collect())
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Legacy and orchestrated fusion agree under Shadow routing: identical
    /// candidate ranking and zero score divergence for any inputs.
    #[test]
    fn shadow_legacy_and_orchestrated_agree(
        lexical in arb_ranked_list(12),
        semantic in arb_ranked_list(12),
        top_k in 1usize..=24,
    ) {
        let config = FacadeConfig {
            routing: FacadeRouting::Shadow,
            ..Default::default()
        };
        let facade = SearchFacade::with_config(config);
        let result = facade.fuse_with_metrics(&lexical, &semantic, top_k);

        let cmp = result
            .shadow_comparison
            .expect("Shadow routing must produce a shadow comparison");
        prop_assert!(cmp.ranking_match, "legacy and orchestrated rankings diverged");
        prop_assert!(
            cmp.max_score_diff.abs() < 1e-6,
            "legacy/orchestrated score divergence: {}",
            cmp.max_score_diff
        );
        // The legacy path's surfaced results never exceed the requested top_k.
        prop_assert!(result.results.len() <= top_k);
    }
}
