use frankenterm_core::search_explain::{
    SearchExplainEvidence, SearchExplainReason, SearchExplainResult,
};
use proptest::prelude::*;

fn arb_evidence() -> impl Strategy<Value = SearchExplainEvidence> {
    ("[a-z_]{1,24}", ".{0,64}").prop_map(|(key, value)| SearchExplainEvidence { key, value })
}
fn arb_reason() -> impl Strategy<Value = SearchExplainReason> {
    (
        proptest::sample::select(vec![
            "fts_not_initialized",
            "pane_filtered_out",
            "capture_gap_detected",
            "retention_pruned",
            "stale_segments",
        ]),
        ".{1,80}",
        proptest::collection::vec(arb_evidence(), 0..6),
        proptest::collection::vec(".{1,60}", 0..6),
        0.0f64..=1.0f64,
    )
        .prop_map(|(code, summary, evidence, suggestions, confidence)| SearchExplainReason {
            code,
            summary,
            evidence,
            suggestions,
            confidence,
        })
}

fn arb_result() -> impl Strategy<Value = SearchExplainResult> {
    (
        ".{0,80}",
        proptest::option::of(0u64..256),
        0usize..64,
        0usize..64,
        0usize..64,
        0u64..10_000,
        proptest::collection::vec(arb_reason(), 0..8),
    )
        .prop_map(
            |(
                query,
                pane_filter,
                total_panes,
                observed_panes,
                ignored_panes,
                total_segments,
                reasons,
            )| SearchExplainResult {
                query,
                pane_filter,
                total_panes,
                observed_panes,
                ignored_panes,
                total_segments,
                reasons,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn search_explain_evidence_serializes_expected_fields(evidence in arb_evidence()) {
        let value = serde_json::to_value(&evidence).unwrap();
        let object = value.as_object().unwrap();

        prop_assert_eq!(object.get("key").and_then(|v| v.as_str()), Some(evidence.key.as_str()));
        prop_assert_eq!(object.get("value").and_then(|v| v.as_str()), Some(evidence.value.as_str()));
        prop_assert_eq!(object.len(), 2);
    }

    #[test]
    fn search_explain_reason_serializes_nested_evidence_and_suggestions(reason in arb_reason()) {
        let value = serde_json::to_value(&reason).unwrap();
        let object = value.as_object().unwrap();

        prop_assert_eq!(object.get("code").and_then(|v| v.as_str()), Some(reason.code));
        prop_assert_eq!(object.get("summary").and_then(|v| v.as_str()), Some(reason.summary.as_str()));
        prop_assert_eq!(object.get("confidence").and_then(|v| v.as_f64()), Some(reason.confidence));
        prop_assert_eq!(
            object.get("evidence").and_then(|v| v.as_array()).map(std::vec::Vec::len),
            Some(reason.evidence.len())
        );
        prop_assert_eq!(
            object.get("suggestions").and_then(|v| v.as_array()).map(std::vec::Vec::len),
            Some(reason.suggestions.len())
        );
    }

    #[test]
    fn search_explain_result_serializes_top_level_counts_and_reason_population(result in arb_result()) {
        let value = serde_json::to_value(&result).unwrap();
        let object = value.as_object().unwrap();

        prop_assert_eq!(object.get("query").and_then(|v| v.as_str()), Some(result.query.as_str()));
        prop_assert_eq!(object.get("pane_filter").and_then(|v| v.as_u64()), result.pane_filter);
        prop_assert_eq!(object.get("total_panes").and_then(|v| v.as_u64()), Some(result.total_panes as u64));
        prop_assert_eq!(object.get("observed_panes").and_then(|v| v.as_u64()), Some(result.observed_panes as u64));
        prop_assert_eq!(object.get("ignored_panes").and_then(|v| v.as_u64()), Some(result.ignored_panes as u64));
        prop_assert_eq!(object.get("total_segments").and_then(|v| v.as_u64()), Some(result.total_segments));
        prop_assert_eq!(
            object.get("reasons").and_then(|v| v.as_array()).map(std::vec::Vec::len),
            Some(result.reasons.len())
        );
    }

    #[test]
    fn search_explain_reason_serialization_is_deterministic(reason in arb_reason()) {
        let json_a = serde_json::to_string(&reason).unwrap();
        let json_b = serde_json::to_string(&reason).unwrap();

        prop_assert_eq!(json_a, json_b);
    }
}
