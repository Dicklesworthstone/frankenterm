//! Property-based tests for the `restore_scrollback` module.
//!
//! Covers `InjectionConfig` serde roundtrips and defaults, `ScrollbackData`
//! construction and truncation invariants, and `InjectionReport` aggregation
//! methods.

use frankenterm_core::restore_scrollback::{
    InjectionConfig, InjectionReport, PaneInjectionStats, ScrollbackData,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_injection_config() -> impl Strategy<Value = InjectionConfig> {
    (
        1_usize..100_000,
        256_usize..65536,
        0_u64..100,
        1_usize..20,
    )
        .prop_map(
            |(max_lines, chunk_size, inter_chunk_delay_ms, concurrent_injections)| {
                InjectionConfig {
                    max_lines,
                    chunk_size,
                    inter_chunk_delay_ms,
                    concurrent_injections,
                }
            },
        )
}

fn arb_segments() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec("[A-Za-z0-9 ]{0,50}", 0..20)
}

fn arb_pane_stats() -> impl Strategy<Value = PaneInjectionStats> {
    (
        0_u64..1000,
        0_u64..1000,
        0_usize..500,
        0_usize..50000,
        0_usize..100,
    )
        .prop_map(
            |(old_pane_id, new_pane_id, lines_injected, bytes_written, chunks_sent)| {
                PaneInjectionStats {
                    old_pane_id,
                    new_pane_id,
                    lines_injected,
                    bytes_written,
                    chunks_sent,
                }
            },
        )
}

// =========================================================================
// InjectionConfig — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// InjectionConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_config_serde(config in arb_injection_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: InjectionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_lines, config.max_lines);
        prop_assert_eq!(back.chunk_size, config.chunk_size);
        prop_assert_eq!(back.inter_chunk_delay_ms, config.inter_chunk_delay_ms);
        prop_assert_eq!(back.concurrent_injections, config.concurrent_injections);
    }

    /// Default InjectionConfig has expected values.
    #[test]
    fn prop_config_defaults(_dummy in 0..1_u8) {
        let config = InjectionConfig::default();
        prop_assert_eq!(config.max_lines, 10_000);
        prop_assert_eq!(config.chunk_size, 4096);
        prop_assert_eq!(config.inter_chunk_delay_ms, 1);
        prop_assert_eq!(config.concurrent_injections, 5);
    }

    /// InjectionConfig deserializes from empty JSON with defaults.
    #[test]
    fn prop_config_from_empty_json(_dummy in 0..1_u8) {
        let back: InjectionConfig = serde_json::from_str("{}").unwrap();
        prop_assert_eq!(back.max_lines, 10_000);
        prop_assert_eq!(back.chunk_size, 4096);
    }

    /// InjectionConfig partial JSON fills missing with defaults.
    #[test]
    fn prop_config_partial_json(max_lines in 1_usize..50_000) {
        let json = format!("{{\"max_lines\":{}}}", max_lines);
        let back: InjectionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_lines, max_lines);
        // Missing fields get defaults
        prop_assert_eq!(back.chunk_size, 4096);
        prop_assert_eq!(back.inter_chunk_delay_ms, 1);
        prop_assert_eq!(back.concurrent_injections, 5);
    }

    /// InjectionConfig serde is deterministic.
    #[test]
    fn prop_config_deterministic(config in arb_injection_config()) {
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// ScrollbackData — construction and truncation
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// from_segments correctly counts total_bytes.
    #[test]
    fn prop_from_segments_byte_count(segments in arb_segments()) {
        let expected_bytes: usize = segments.iter().map(|s| s.len()).sum();
        let data = ScrollbackData::from_segments(segments.clone());
        prop_assert_eq!(data.total_bytes, expected_bytes);
        prop_assert_eq!(data.lines.len(), segments.len());
    }

    /// from_segments with empty input gives zero bytes.
    #[test]
    fn prop_from_segments_empty(_dummy in 0..1_u8) {
        let data = ScrollbackData::from_segments(vec![]);
        prop_assert_eq!(data.total_bytes, 0);
        prop_assert!(data.lines.is_empty());
    }

    /// truncate reduces line count to max when needed.
    #[test]
    fn prop_truncate_reduces(segments in proptest::collection::vec("[a-z]{5,10}", 5..20), max in 1_usize..4) {
        let mut data = ScrollbackData::from_segments(segments.clone());
        data.truncate(max);
        prop_assert!(data.lines.len() <= max);
    }

    /// truncate keeps most recent lines.
    #[test]
    fn prop_truncate_keeps_recent(segments in proptest::collection::vec("[a-z]{5,10}", 5..20), max in 1_usize..4) {
        let mut data = ScrollbackData::from_segments(segments.clone());
        data.truncate(max);
        // The retained lines should be the last `max` lines from original
        let expected: Vec<_> = segments.iter().rev().take(max).rev().cloned().collect();
        prop_assert_eq!(&data.lines, &expected);
    }

    /// truncate is idempotent.
    #[test]
    fn prop_truncate_idempotent(segments in arb_segments(), max in 1_usize..50) {
        let mut data1 = ScrollbackData::from_segments(segments.clone());
        let mut data2 = ScrollbackData::from_segments(segments);
        data1.truncate(max);
        data2.truncate(max);
        data2.truncate(max); // second truncation
        prop_assert_eq!(&data1.lines, &data2.lines);
        prop_assert_eq!(data1.total_bytes, data2.total_bytes);
    }

    /// truncate doesn't change data when max >= line count.
    #[test]
    fn prop_truncate_noop_when_within_limit(segments in arb_segments()) {
        let max = segments.len() + 10;
        let mut data = ScrollbackData::from_segments(segments.clone());
        let original_bytes = data.total_bytes;
        data.truncate(max);
        prop_assert_eq!(data.lines.len(), segments.len());
        prop_assert_eq!(data.total_bytes, original_bytes);
    }

    /// truncate updates total_bytes correctly.
    #[test]
    fn prop_truncate_updates_bytes(segments in proptest::collection::vec("[a-z]{5,10}", 5..20), max in 1_usize..4) {
        let mut data = ScrollbackData::from_segments(segments);
        data.truncate(max);
        let recalculated: usize = data.lines.iter().map(|s| s.len()).sum();
        prop_assert_eq!(data.total_bytes, recalculated);
    }
}

// =========================================================================
// InjectionReport — aggregation
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// success_count equals number of successes.
    #[test]
    fn prop_success_count(stats in proptest::collection::vec(arb_pane_stats(), 0..10)) {
        let report = InjectionReport {
            successes: stats.clone(),
            failures: vec![],
            skipped: vec![],
        };
        prop_assert_eq!(report.success_count(), stats.len());
    }

    /// failure_count equals number of failures.
    #[test]
    fn prop_failure_count(n in 0_usize..10) {
        let failures: Vec<(u64, String)> = (0..n).map(|i| (i as u64, "err".to_string())).collect();
        let report = InjectionReport {
            successes: vec![],
            failures,
            skipped: vec![],
        };
        prop_assert_eq!(report.failure_count(), n);
    }

    /// total_bytes sums bytes_written across successes.
    #[test]
    fn prop_total_bytes(stats in proptest::collection::vec(arb_pane_stats(), 0..10)) {
        let expected: usize = stats.iter().map(|s| s.bytes_written).sum();
        let report = InjectionReport {
            successes: stats,
            failures: vec![],
            skipped: vec![],
        };
        prop_assert_eq!(report.total_bytes(), expected);
    }

    /// Default report has all zero counts.
    #[test]
    fn prop_default_report(_dummy in 0..1_u8) {
        let report = InjectionReport::default();
        prop_assert_eq!(report.success_count(), 0);
        prop_assert_eq!(report.failure_count(), 0);
        prop_assert_eq!(report.total_bytes(), 0);
        prop_assert!(report.skipped.is_empty());
    }

    /// total_bytes is zero when no successes.
    #[test]
    fn prop_total_bytes_no_successes(n in 0_usize..5) {
        let failures: Vec<(u64, String)> = (0..n).map(|i| (i as u64, "err".to_string())).collect();
        let report = InjectionReport {
            successes: vec![],
            failures,
            skipped: vec![],
        };
        prop_assert_eq!(report.total_bytes(), 0);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn config_default_values() {
    let config = InjectionConfig::default();
    assert_eq!(config.max_lines, 10_000);
    assert_eq!(config.chunk_size, 4096);
    assert_eq!(config.inter_chunk_delay_ms, 1);
    assert_eq!(config.concurrent_injections, 5);
}

#[test]
fn scrollback_from_segments_basic() {
    let data = ScrollbackData::from_segments(vec![
        "line1".to_string(),
        "line2".to_string(),
        "line3".to_string(),
    ]);
    assert_eq!(data.lines.len(), 3);
    assert_eq!(data.total_bytes, 15); // 5 + 5 + 5
}

#[test]
fn scrollback_truncate_basic() {
    let mut data = ScrollbackData::from_segments(vec![
        "aa".to_string(),
        "bb".to_string(),
        "cc".to_string(),
        "dd".to_string(),
    ]);
    data.truncate(2);
    assert_eq!(data.lines, vec!["cc", "dd"]); // keeps most recent
    assert_eq!(data.total_bytes, 4);
}

#[test]
fn report_aggregation() {
    let report = InjectionReport {
        successes: vec![
            PaneInjectionStats {
                old_pane_id: 1,
                new_pane_id: 10,
                lines_injected: 100,
                bytes_written: 5000,
                chunks_sent: 2,
            },
            PaneInjectionStats {
                old_pane_id: 2,
                new_pane_id: 20,
                lines_injected: 50,
                bytes_written: 3000,
                chunks_sent: 1,
            },
        ],
        failures: vec![(3, "timeout".to_string())],
        skipped: vec![4, 5],
    };
    assert_eq!(report.success_count(), 2);
    assert_eq!(report.failure_count(), 1);
    assert_eq!(report.total_bytes(), 8000);
}
