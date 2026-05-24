//! Property tests for the semantic-search freshness classifier and proof-case
//! builder (`crates/frankenterm-core/src/search/mod.rs`).
//!
//! These functions are pure and deterministic — they decide whether
//! semantic-search evidence can truthfully claim freshness — so they are
//! exercised here with property tests that mirror the documented contract.
//! ft-kt3yw follow-up (cc2/storage+search): proof-lane-independent coverage.

use frankenterm_core::search::{
    SemanticSearchFreshnessStatus, SemanticSearchProofCase, SemanticSearchProofCaseInput,
    classify_semantic_search_freshness,
};
use proptest::prelude::*;

fn arb_indexed() -> impl Strategy<Value = Option<bool>> {
    prop_oneof![Just(None), Just(Some(true)), Just(Some(false))]
}

fn arb_captured() -> impl Strategy<Value = Option<i64>> {
    prop_oneof![Just(None), any::<i64>().prop_map(Some)]
}

proptest! {
    /// The classifier must never panic for any combination of inputs,
    /// including i64 extremes that could overflow naive subtraction.
    #[test]
    fn classify_never_panics(
        indexed in arb_indexed(),
        captured in arb_captured(),
        observed in any::<i64>(),
        window in any::<i64>(),
    ) {
        let _ = classify_semantic_search_freshness(indexed, captured, observed, window);
    }

    /// `indexed == Some(false)` (the latest segment is missing from the index)
    /// is the highest-precedence signal and must always classify as Stale,
    /// regardless of timestamps or window.
    #[test]
    fn not_indexed_is_always_stale(
        captured in arb_captured(),
        observed in any::<i64>(),
        window in any::<i64>(),
    ) {
        let status = classify_semantic_search_freshness(Some(false), captured, observed, window);
        prop_assert_eq!(status, SemanticSearchFreshnessStatus::Stale);
    }

    /// When the segment is not known-unindexed and no capture timestamp is
    /// available, freshness is Unknown.
    #[test]
    fn missing_timestamp_is_unknown(
        indexed in prop_oneof![Just(None), Just(Some(true))],
        observed in any::<i64>(),
        window in any::<i64>(),
    ) {
        let status = classify_semantic_search_freshness(indexed, None, observed, window);
        prop_assert_eq!(status, SemanticSearchFreshnessStatus::Unknown);
    }

    /// A capture timestamp ahead of the observation timestamp is FutureDated
    /// (clock skew), provided the segment is not known-unindexed.
    #[test]
    fn future_capture_is_future_dated(
        indexed in prop_oneof![Just(None), Just(Some(true))],
        observed in any::<i64>(),
        skew in 1i64..=1_000_000_000,
        window in any::<i64>(),
    ) {
        let captured = observed.saturating_add(skew);
        // Only meaningful when the saturating add actually moved ahead.
        prop_assume!(captured > observed);
        let status = classify_semantic_search_freshness(indexed, Some(captured), observed, window);
        prop_assert_eq!(status, SemanticSearchFreshnessStatus::FutureDated);
    }

    /// Within the (non-negative) freshness window the result is Fresh; beyond
    /// it the result is Stale. Mirrors the implementation's saturating math and
    /// negative-window clamp.
    #[test]
    fn in_window_fresh_else_stale(
        indexed in prop_oneof![Just(None), Just(Some(true))],
        captured in any::<i64>(),
        observed in any::<i64>(),
        window in any::<i64>(),
    ) {
        // Restrict to the non-future-dated regime this property describes.
        prop_assume!(captured <= observed);
        let status = classify_semantic_search_freshness(indexed, Some(captured), observed, window);
        let age = observed.saturating_sub(captured);
        let expected = if age <= window.max(0) {
            SemanticSearchFreshnessStatus::Fresh
        } else {
            SemanticSearchFreshnessStatus::Stale
        };
        prop_assert_eq!(status, expected);
    }

    /// A negative freshness window is treated identically to a zero window.
    #[test]
    fn negative_window_equals_zero_window(
        indexed in arb_indexed(),
        captured in arb_captured(),
        observed in any::<i64>(),
        neg_window in i64::MIN..0,
    ) {
        let with_negative =
            classify_semantic_search_freshness(indexed, captured, observed, neg_window);
        let with_zero = classify_semantic_search_freshness(indexed, captured, observed, 0);
        prop_assert_eq!(with_negative, with_zero);
    }
}

fn arb_input(
    latest_id: Option<i64>,
    embedded: Vec<i64>,
    captured: Option<i64>,
    observed: i64,
    window: i64,
) -> SemanticSearchProofCaseInput {
    SemanticSearchProofCaseInput {
        case_id: "case".to_string(),
        requested_mode: "hybrid".to_string(),
        effective_mode: "semantic".to_string(),
        embedder_id: "embedder-1".to_string(),
        embedder_tier: "fastembed".to_string(),
        embedder_dimension: 4,
        latest_segment_id: latest_id,
        latest_segment_captured_at_ms: captured,
        embedded_segment_ids: embedded,
        result_segment_ids: vec![1, 2],
        observed_at_ms: observed,
        freshness_window_ms: window,
        fallback_reason: None,
        semantic_budget_state: "active".to_string(),
        semantic_cache_hit: false,
        semantic_rows_scanned: 3,
    }
}

proptest! {
    /// `stale_index` is true exactly when there is a latest segment that is
    /// absent from the embedded set, and the computed freshness status matches
    /// the standalone classifier fed the same derived "indexed" signal.
    #[test]
    fn from_input_index_and_freshness_consistent(
        latest_id in prop_oneof![Just(None), (0i64..50).prop_map(Some)],
        embedded in prop::collection::vec(0i64..50, 0..8),
        captured in arb_captured(),
        observed in any::<i64>(),
        window in any::<i64>(),
    ) {
        let input = arb_input(latest_id, embedded.clone(), captured, observed, window);
        let case = SemanticSearchProofCase::from_input(input);

        let derived_indexed = latest_id.map(|id| embedded.contains(&id));
        let expected_status =
            classify_semantic_search_freshness(derived_indexed, captured, observed, window);

        prop_assert_eq!(case.freshness_status, expected_status);
        prop_assert_eq!(case.stale_index, derived_indexed == Some(false));
    }

    /// `freshness_age_ms` is populated exactly when a capture timestamp exists
    /// and the observation is at or after it, with a saturating age.
    #[test]
    fn from_input_age_is_some_only_when_not_future(
        latest_id in prop_oneof![Just(None), (0i64..50).prop_map(Some)],
        embedded in prop::collection::vec(0i64..50, 0..8),
        captured in arb_captured(),
        observed in any::<i64>(),
        window in any::<i64>(),
    ) {
        let input = arb_input(latest_id, embedded, captured, observed, window);
        let case = SemanticSearchProofCase::from_input(input);

        match captured {
            Some(c) if observed >= c => {
                prop_assert_eq!(case.freshness_age_ms, Some(observed.saturating_sub(c)));
            }
            _ => prop_assert_eq!(case.freshness_age_ms, None),
        }
    }

    /// The built proof case round-trips losslessly through JSON.
    #[test]
    fn from_input_serde_roundtrip(
        latest_id in prop_oneof![Just(None), (0i64..50).prop_map(Some)],
        embedded in prop::collection::vec(0i64..50, 0..8),
        captured in arb_captured(),
        observed in any::<i64>(),
        window in any::<i64>(),
    ) {
        let input = arb_input(latest_id, embedded, captured, observed, window);
        let case = SemanticSearchProofCase::from_input(input);
        let json = serde_json::to_string(&case).expect("serialize proof case");
        let back: SemanticSearchProofCase =
            serde_json::from_str(&json).expect("deserialize proof case");
        prop_assert_eq!(case, back);
    }
}
