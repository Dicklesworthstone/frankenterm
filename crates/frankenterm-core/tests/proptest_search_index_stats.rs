//! Property-based tests for search index health snapshot carriers.

use std::collections::HashMap;

use frankenterm_core::search::SearchIndexStats;
use proptest::prelude::*;

fn arb_search_index_stats() -> impl Strategy<Value = SearchIndexStats> {
    (
        (
            "[A-Za-z0-9_./-]{3,64}",
            "[A-Za-z0-9_./-]{3,64}",
            1u32..16,
            0usize..100_000,
            0usize..10_000,
            0u64..10_000_000_000,
            0usize..10_000,
            1u64..10_000_000_000,
        ),
        (
            1u64..365,
            1u64..3600,
            1usize..10_000,
            prop_oneof![
                Just((None::<i64>, None::<i64>)),
                (0i64..9_999_999_999_999, 0i64..9_999_999_999_999)
                    .prop_map(|(a, b)| {
                        let (oldest, newest) = if a <= b { (a, b) } else { (b, a) };
                        (Some(oldest), Some(newest))
                    }),
            ],
            prop::option::of(0i64..9_999_999_999_999),
            prop::collection::hash_map("[a-z_]{3,20}", 0usize..10_000, 0..8),
            prop::option::of(0i64..9_999_999_999_999),
        ),
    )
        .prop_map(
            |(
                (
                    index_dir,
                    state_path,
                    format_version,
                    doc_count,
                    segment_count,
                    total_bytes,
                    pending_docs,
                    max_index_size_bytes,
                ),
                (
                    ttl_days,
                    flush_interval_secs,
                    flush_docs_threshold,
                    captured_window,
                    freshness_age_ms,
                    source_counts,
                    last_flush_at_ms,
                ),
            )| SearchIndexStats {
                oldest_captured_at_ms: captured_window.0,
                newest_captured_at_ms: captured_window.1,
                index_dir,
                state_path,
                format_version,
                doc_count,
                segment_count,
                total_bytes,
                pending_docs,
                max_index_size_bytes,
                ttl_days,
                flush_interval_secs,
                flush_docs_threshold,
                freshness_age_ms,
                source_counts,
                last_flush_at_ms,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn search_index_stats_roundtrip(stats in arb_search_index_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let back: SearchIndexStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }

    #[test]
    fn search_index_stats_time_fields_are_coherent(stats in arb_search_index_stats()) {
        if let (Some(oldest), Some(newest)) = (stats.oldest_captured_at_ms, stats.newest_captured_at_ms) {
            prop_assert!(oldest <= newest);
        }
        if let Some(age) = stats.freshness_age_ms {
            prop_assert!(age >= 0);
        }
        prop_assert!(stats.max_index_size_bytes > 0);
        prop_assert!(stats.ttl_days > 0);
        prop_assert!(stats.flush_interval_secs > 0);
        prop_assert!(stats.flush_docs_threshold > 0);
    }

    #[test]
    fn search_index_stats_source_counts_survive_json_shape(
        source_counts in prop::collection::hash_map("[a-z_]{3,20}", 0usize..10_000, 0..8)
    ) {
        let stats = SearchIndexStats {
            index_dir: "/tmp/index".to_string(),
            state_path: "/tmp/index/state.json".to_string(),
            format_version: 1,
            doc_count: 0,
            segment_count: 0,
            total_bytes: 0,
            pending_docs: 0,
            max_index_size_bytes: 1024,
            ttl_days: 30,
            flush_interval_secs: 5,
            flush_docs_threshold: 50,
            newest_captured_at_ms: None,
            oldest_captured_at_ms: None,
            freshness_age_ms: None,
            source_counts: source_counts.clone(),
            last_flush_at_ms: None,
        };

        let json = serde_json::to_value(&stats).unwrap();
        let parsed_counts: HashMap<String, usize> =
            serde_json::from_value(json.get("source_counts").cloned().unwrap()).unwrap();
        prop_assert_eq!(parsed_counts, source_counts);
    }
}
