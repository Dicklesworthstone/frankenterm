//! Property tests for `SearchPrefetchAdvisor::evaluate_candidate`
//! (`crates/frankenterm-core/src/search_prefetch_advisor.rs`).
//!
//! The module had 4 inline scenario tests but no property coverage of the
//! admission decision cascade. These assert invariants that hold regardless of
//! the configured thresholds (integer/enum decisions — no float dependence).

use frankenterm_core::fleet_memory_controller::{
    FleetMemoryTier, FleetMemoryTierBudgetRecord, FleetMemoryTierBudgetSnapshot,
};
use frankenterm_core::search_prefetch_advisor::{
    SearchPrefetchAdvisor, SearchPrefetchCandidate, SearchPrefetchContext,
    SearchPrefetchDecisionKind, SearchPrefetchKind,
};
use frankenterm_core::storage::{
    SemanticBudgetConfig, SemanticBudgetMetrics, SemanticBudgetSnapshot,
};
use proptest::prelude::*;

const NOW_MS: i64 = 1_000;

fn tier_budget(search_budget: u64, search_actual: u64) -> FleetMemoryTierBudgetSnapshot {
    FleetMemoryTierBudgetSnapshot::from_tiers([
        FleetMemoryTierBudgetRecord::new(FleetMemoryTier::HotResident, 8_000, 4_000),
        FleetMemoryTierBudgetRecord::new(
            FleetMemoryTier::SearchIndexCache,
            search_budget,
            search_actual,
        ),
    ])
}

fn semantic_snapshot(backoff_until_ms: Option<i64>) -> SemanticBudgetSnapshot {
    SemanticBudgetSnapshot {
        config: SemanticBudgetConfig::default(),
        metrics: SemanticBudgetMetrics::default(),
        ewma_semantic_latency_ms: 0.0,
        backoff_until_ms,
        cache_entries: 0,
    }
}

fn candidate(bytes: u64, obs: u32, lat_without: u64, lat_with: u64) -> SearchPrefetchCandidate {
    SearchPrefetchCandidate::repeated_query(
        "q:fingerprint",
        SearchPrefetchKind::SemanticIndex,
        bytes,
        obs,
        lat_without,
        lat_with,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Every evaluate_candidate call counts exactly one considered candidate,
    /// copies the candidate's byte estimate into the decision, and reports the
    /// context's pressure tier.
    #[test]
    fn decision_basic_invariants(
        bytes in 0u64..3_000,
        obs in 0u32..8,
        lat_without in 0u64..300,
        lat_with in 0u64..300,
        search_budget in 0u64..4_000,
        search_actual in 0u64..4_000,
        backoff in prop_oneof![Just(None), (0i64..5_000).prop_map(Some)],
    ) {
        let tiers = tier_budget(search_budget, search_actual);
        let semantic = semantic_snapshot(backoff);
        let mut advisor = SearchPrefetchAdvisor::default();
        let cand = candidate(bytes, obs, lat_without, lat_with);
        let ctx = SearchPrefetchContext {
            now_ms: NOW_MS,
            semantic_budget: Some(&semantic),
            tier_budget: &tiers,
        };

        let decision = advisor.evaluate_candidate(cand, ctx);

        prop_assert_eq!(decision.estimated_bytes, bytes);
        prop_assert_eq!(decision.pressure_tier, tiers.pressure_tier());
        prop_assert_eq!(advisor.telemetry().considered_candidates, 1);
    }

    /// A zero-byte candidate is always SkipZeroBytes — the highest-precedence
    /// guard — regardless of pressure, budget, or signal.
    #[test]
    fn zero_bytes_always_skipped(
        obs in 0u32..8,
        search_budget in 0u64..4_000,
        search_actual in 0u64..4_000,
        backoff in prop_oneof![Just(None), (0i64..5_000).prop_map(Some)],
    ) {
        let tiers = tier_budget(search_budget, search_actual);
        let semantic = semantic_snapshot(backoff);
        let mut advisor = SearchPrefetchAdvisor::default();
        let ctx = SearchPrefetchContext {
            now_ms: NOW_MS,
            semantic_budget: Some(&semantic),
            tier_budget: &tiers,
        };
        let decision = advisor.evaluate_candidate(candidate(0, obs, 300, 0), ctx);
        prop_assert_eq!(decision.kind, SearchPrefetchDecisionKind::SkipZeroBytes);
    }

    /// Admission implies the per-candidate guards passed: positive byte estimate
    /// within the per-candidate cap.
    #[test]
    fn admit_implies_guards_passed(
        bytes in 1u64..3_000,
        obs in 0u32..8,
        lat_without in 0u64..400,
        lat_with in 0u64..400,
        search_budget in 0u64..6_000,
        search_actual in 0u64..6_000,
    ) {
        let tiers = tier_budget(search_budget, search_actual);
        let mut advisor = SearchPrefetchAdvisor::default();
        let cfg = advisor.config();
        let ctx = SearchPrefetchContext {
            now_ms: NOW_MS,
            semantic_budget: None,
            tier_budget: &tiers,
        };
        let decision = advisor.evaluate_candidate(candidate(bytes, obs, lat_without, lat_with), ctx);
        if decision.kind == SearchPrefetchDecisionKind::Admit {
            prop_assert!(bytes > 0);
            prop_assert!(bytes <= cfg.max_prefetch_bytes_per_candidate);
            prop_assert!(
                decision.estimated_latency_saved_ms >= cfg.min_estimated_latency_saved_ms
            );
        }
    }

    /// The decision is deterministic: two fresh advisors with identical inputs
    /// produce the same decision kind.
    #[test]
    fn decision_is_deterministic(
        bytes in 0u64..3_000,
        obs in 0u32..8,
        lat_without in 0u64..400,
        lat_with in 0u64..400,
        search_budget in 0u64..6_000,
        search_actual in 0u64..6_000,
        backoff in prop_oneof![Just(None), (0i64..5_000).prop_map(Some)],
    ) {
        let tiers = tier_budget(search_budget, search_actual);
        let semantic = semantic_snapshot(backoff);
        let make_ctx = || SearchPrefetchContext {
            now_ms: NOW_MS,
            semantic_budget: Some(&semantic),
            tier_budget: &tiers,
        };
        let mut a = SearchPrefetchAdvisor::default();
        let mut b = SearchPrefetchAdvisor::default();
        let da = a.evaluate_candidate(candidate(bytes, obs, lat_without, lat_with), make_ctx());
        let db = b.evaluate_candidate(candidate(bytes, obs, lat_without, lat_with), make_ctx());
        prop_assert_eq!(da.kind, db.kind);
        prop_assert_eq!(da.estimated_bytes, db.estimated_bytes);
    }
}
