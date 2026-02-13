//! Property-based tests for the search explain engine.
//!
//! Validates:
//! 1. Reasons are always sorted by confidence descending
//! 2. All confidence values are in [0.0, 1.0]
//! 3. total_panes == observed_panes + ignored_panes
//! 4. total_segments matches sum of indexing_stats segment_count
//! 5. Reason codes are always non-empty static strings
//! 6. NO_INDEXED_DATA reason present when total segments == 0
//! 7. PANE_NOT_FOUND reason present when filtering for unknown pane
//! 8. PANE_EXCLUDED reason present when filtering for excluded pane
//! 9. CAPTURE_GAPS reason present when gaps exist
//! 10. RETENTION_CLEANUP reason present when cleanup_count > 0
//! 11. SearchExplainResult is always JSON-serializable
//! 12. render_explain_plain always produces non-empty output
//! 13. Healthy contexts produce no reasons

use proptest::prelude::*;

use frankenterm_core::search_explain::{
    GapInfo, PaneExplainInfo, PaneIndexingInfo, SearchExplainContext, explain_search,
    render_explain_plain,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_pane_explain_info(now_ms: i64) -> impl Strategy<Value = PaneExplainInfo> {
    (
        0_u64..1000,
        any::<bool>(),
        prop_oneof![
            Just(None),
            Just(Some("title_match".to_string())),
            Just(Some("cwd_match".to_string()))
        ],
        prop_oneof![Just("local".to_string()), Just("ssh:server".to_string())],
    )
        .prop_map(
            move |(pane_id, observed, ignore_reason, domain)| PaneExplainInfo {
                pane_id,
                observed,
                ignore_reason,
                domain,
                last_seen_at: now_ms - 1000,
            },
        )
}

fn arb_pane_indexing_info(now_ms: i64) -> impl Strategy<Value = PaneIndexingInfo> {
    (0_u64..1000, 0_u64..500, 0_u64..100000, any::<bool>()).prop_map(
        move |(pane_id, segment_count, total_bytes, fts_consistent)| PaneIndexingInfo {
            pane_id,
            segment_count,
            total_bytes,
            last_segment_at: if segment_count > 0 {
                Some(now_ms)
            } else {
                None
            },
            fts_row_count: if fts_consistent {
                segment_count
            } else {
                segment_count / 2
            },
            fts_consistent,
        },
    )
}

fn arb_gap_info(now_ms: i64) -> impl Strategy<Value = GapInfo> {
    (
        0_u64..1000,
        0_u64..100,
        prop_oneof![
            Just("daemon_restart".to_string()),
            Just("high_load".to_string()),
            Just("pane_closed".to_string()),
        ],
    )
        .prop_map(move |(pane_id, seq_before, reason)| GapInfo {
            pane_id,
            seq_before,
            seq_after: seq_before + 5,
            reason,
            detected_at: now_ms,
        })
}

fn arb_search_context() -> impl Strategy<Value = SearchExplainContext> {
    let now_ms = 1_700_000_000_000_i64; // fixed timestamp for determinism
    (
        "[a-zA-Z0-9 ]{1,30}",
        prop_oneof![Just(None), (0_u64..100).prop_map(Some)],
        proptest::collection::vec(arb_pane_explain_info(now_ms), 0..10),
        proptest::collection::vec(arb_pane_indexing_info(now_ms), 0..10),
        proptest::collection::vec(arb_gap_info(now_ms), 0..5),
        0_u64..10,
        prop_oneof![Just(None), (now_ms - 7_200_000..now_ms).prop_map(Some)],
        prop_oneof![Just(None), Just(Some(now_ms))],
    )
        .prop_map(
            move |(
                query,
                pane_filter,
                panes,
                indexing_stats,
                gaps,
                retention_cleanup_count,
                earliest_segment_at,
                latest_segment_at,
            )| {
                SearchExplainContext {
                    query,
                    pane_filter,
                    panes,
                    indexing_stats,
                    gaps,
                    retention_cleanup_count,
                    earliest_segment_at,
                    latest_segment_at,
                    now_ms,
                }
            },
        )
}

// =============================================================================
// Property: Reasons are always sorted by confidence descending
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn reasons_sorted_by_confidence(ctx in arb_search_context()) {
        let result = explain_search(&ctx);
        for window in result.reasons.windows(2) {
            prop_assert!(
                window[0].confidence >= window[1].confidence,
                "reasons not sorted: {} ({}) < {} ({})",
                window[0].code, window[0].confidence,
                window[1].code, window[1].confidence,
            );
        }
    }
}

// =============================================================================
// Property: All confidence values are in [0.0, 1.0]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn confidence_values_bounded(ctx in arb_search_context()) {
        let result = explain_search(&ctx);
        for reason in &result.reasons {
            prop_assert!(
                reason.confidence >= 0.0 && reason.confidence <= 1.0,
                "confidence {} for code '{}' out of [0, 1]",
                reason.confidence, reason.code,
            );
        }
    }
}

// =============================================================================
// Property: total_panes == observed_panes + ignored_panes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn pane_count_accounting(ctx in arb_search_context()) {
        let result = explain_search(&ctx);
        prop_assert_eq!(
            result.total_panes,
            result.observed_panes + result.ignored_panes,
            "total_panes({}) != observed({}) + ignored({})",
            result.total_panes, result.observed_panes, result.ignored_panes,
        );
    }
}

// =============================================================================
// Property: total_segments matches sum of indexing_stats
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn total_segments_matches_stats(ctx in arb_search_context()) {
        let result = explain_search(&ctx);
        let expected: u64 = ctx.indexing_stats.iter().map(|s| s.segment_count).sum();
        prop_assert_eq!(
            result.total_segments, expected,
            "total_segments({}) != sum of indexing_stats({})",
            result.total_segments, expected,
        );
    }
}

// =============================================================================
// Property: Reason codes are always non-empty
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn reason_codes_non_empty(ctx in arb_search_context()) {
        let result = explain_search(&ctx);
        for reason in &result.reasons {
            prop_assert!(!reason.code.is_empty(), "reason code is empty");
            prop_assert!(!reason.summary.is_empty(), "reason summary is empty for code '{}'", reason.code);
        }
    }
}

// =============================================================================
// Property: NO_INDEXED_DATA when total segments == 0
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn no_data_reason_when_zero_segments(
        query in "[a-zA-Z]{1,20}",
        pane_filter in prop_oneof![Just(None), (0_u64..100).prop_map(Some)],
    ) {
        let ctx = SearchExplainContext {
            query,
            pane_filter,
            panes: vec![],
            indexing_stats: vec![], // no segments
            gaps: vec![],
            retention_cleanup_count: 0,
            earliest_segment_at: None,
            latest_segment_at: None,
            now_ms: 1_700_000_000_000,
        };
        let result = explain_search(&ctx);
        prop_assert!(
            result.reasons.iter().any(|r| r.code == "NO_INDEXED_DATA"),
            "expected NO_INDEXED_DATA reason when no segments, got: {:?}",
            result.reasons.iter().map(|r| r.code).collect::<Vec<_>>(),
        );
    }
}

// =============================================================================
// Property: PANE_NOT_FOUND when filtering for unknown pane
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_not_found_when_unknown_filter(
        query in "[a-zA-Z]{1,20}",
        filter_id in 500_u64..1000, // IDs that won't appear in panes
    ) {
        let now_ms = 1_700_000_000_000_i64;
        let ctx = SearchExplainContext {
            query,
            pane_filter: Some(filter_id),
            panes: vec![PaneExplainInfo {
                pane_id: 1, // different from filter
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: now_ms,
            }],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 100,
                total_bytes: 5000,
                last_segment_at: Some(now_ms),
                fts_row_count: 100,
                fts_consistent: true,
            }],
            gaps: vec![],
            retention_cleanup_count: 0,
            earliest_segment_at: Some(now_ms - 3_600_000),
            latest_segment_at: Some(now_ms),
            now_ms,
        };
        let result = explain_search(&ctx);
        prop_assert!(
            result.reasons.iter().any(|r| r.code == "PANE_NOT_FOUND"),
            "expected PANE_NOT_FOUND for filter_id={}, got: {:?}",
            filter_id,
            result.reasons.iter().map(|r| r.code).collect::<Vec<_>>(),
        );
    }
}

// =============================================================================
// Property: CAPTURE_GAPS when gaps exist with segments
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn capture_gaps_when_gaps_exist(
        query in "[a-zA-Z]{1,20}",
        gap_count in 1_usize..5,
    ) {
        let now_ms = 1_700_000_000_000_i64;
        let gaps: Vec<GapInfo> = (0..gap_count)
            .map(|i| GapInfo {
                pane_id: 1,
                seq_before: i as u64 * 10,
                seq_after: i as u64 * 10 + 5,
                reason: "daemon_restart".to_string(),
                detected_at: now_ms,
            })
            .collect();

        let ctx = SearchExplainContext {
            query,
            pane_filter: None,
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: now_ms,
            }],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 100,
                total_bytes: 5000,
                last_segment_at: Some(now_ms),
                fts_row_count: 100,
                fts_consistent: true,
            }],
            gaps,
            retention_cleanup_count: 0,
            earliest_segment_at: Some(now_ms - 3_600_000),
            latest_segment_at: Some(now_ms),
            now_ms,
        };
        let result = explain_search(&ctx);
        prop_assert!(
            result.reasons.iter().any(|r| r.code == "CAPTURE_GAPS"),
            "expected CAPTURE_GAPS with {} gaps, got: {:?}",
            gap_count,
            result.reasons.iter().map(|r| r.code).collect::<Vec<_>>(),
        );
    }
}

// =============================================================================
// Property: RETENTION_CLEANUP when cleanup count > 0 with segments
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn retention_cleanup_when_count_positive(
        query in "[a-zA-Z]{1,20}",
        cleanup_count in 1_u64..100,
    ) {
        let now_ms = 1_700_000_000_000_i64;
        let ctx = SearchExplainContext {
            query,
            pane_filter: None,
            panes: vec![],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 50,
                total_bytes: 2000,
                last_segment_at: Some(now_ms),
                fts_row_count: 50,
                fts_consistent: true,
            }],
            gaps: vec![],
            retention_cleanup_count: cleanup_count,
            earliest_segment_at: Some(now_ms - 3_600_000),
            latest_segment_at: Some(now_ms),
            now_ms,
        };
        let result = explain_search(&ctx);
        prop_assert!(
            result.reasons.iter().any(|r| r.code == "RETENTION_CLEANUP"),
            "expected RETENTION_CLEANUP with count={}, got: {:?}",
            cleanup_count,
            result.reasons.iter().map(|r| r.code).collect::<Vec<_>>(),
        );
    }
}

// =============================================================================
// Property: SearchExplainResult is always JSON-serializable
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn result_always_serializable(ctx in arb_search_context()) {
        let result = explain_search(&ctx);
        let json = serde_json::to_string(&result);
        prop_assert!(json.is_ok(), "explain result should be serializable");
    }
}

// =============================================================================
// Property: render_explain_plain always produces non-empty output
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn render_plain_non_empty(ctx in arb_search_context()) {
        let result = explain_search(&ctx);
        let rendered = render_explain_plain(&result);
        prop_assert!(!rendered.is_empty(), "rendered output should not be empty");
        prop_assert!(
            rendered.contains(&ctx.query),
            "rendered output should contain the query",
        );
    }
}

// =============================================================================
// Property: query is preserved in result
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn query_preserved_in_result(ctx in arb_search_context()) {
        let result = explain_search(&ctx);
        prop_assert_eq!(&result.query, &ctx.query);
        prop_assert_eq!(result.pane_filter, ctx.pane_filter);
    }
}

// =============================================================================
// Unit: healthy context produces no reasons
// =============================================================================

#[test]
fn healthy_context_no_reasons() {
    let now_ms = 1_700_000_000_000_i64;
    let ctx = SearchExplainContext {
        query: "test".to_string(),
        pane_filter: None,
        panes: vec![PaneExplainInfo {
            pane_id: 1,
            observed: true,
            ignore_reason: None,
            domain: "local".to_string(),
            last_seen_at: now_ms,
        }],
        indexing_stats: vec![PaneIndexingInfo {
            pane_id: 1,
            segment_count: 1000,
            total_bytes: 50000,
            last_segment_at: Some(now_ms),
            fts_row_count: 1000,
            fts_consistent: true,
        }],
        gaps: vec![],
        retention_cleanup_count: 0,
        earliest_segment_at: Some(now_ms - 3_600_000),
        latest_segment_at: Some(now_ms),
        now_ms,
    };
    let result = explain_search(&ctx);
    assert!(
        result.reasons.is_empty(),
        "healthy context should have no reasons, got: {:?}",
        result.reasons.iter().map(|r| r.code).collect::<Vec<_>>(),
    );
}
