//! Property-based tests for the `restore_scrollback` module.
//!
//! Covers `ScrollbackData` construction and truncation invariants,
//! fail-closed `InjectionReport` defaults, and suppression-guard semantics.

use frankenterm_core::restore_scrollback::{InjectionGuard, InjectionReport, ScrollbackData};
use proptest::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// =========================================================================
// Strategies
// =========================================================================

fn arb_segments() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec("[A-Za-z0-9 ]{0,50}", 0..20)
}

// =========================================================================
// ScrollbackData — construction and truncation
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// from_terminal_lines correctly counts total_bytes.
    #[test]
    fn prop_from_terminal_lines_byte_count(segments in arb_segments()) {
        let expected_bytes: usize = segments.iter().map(|s| s.len()).sum();
        let data = ScrollbackData::from_terminal_lines(segments.clone());
        prop_assert_eq!(data.total_bytes(), expected_bytes);
        prop_assert_eq!(data.lines.len(), segments.len());
    }

    /// from_terminal_lines with empty input gives zero bytes.
    #[test]
    fn prop_from_terminal_lines_empty(_dummy in 0..1_u8) {
        let data = ScrollbackData::from_terminal_lines(vec![]);
        prop_assert_eq!(data.total_bytes(), 0);
        prop_assert!(data.lines.is_empty());
    }

    /// truncate reduces line count to max when needed.
    #[test]
    fn prop_truncate_reduces(segments in proptest::collection::vec("[a-z]{5,10}", 5..20), max in 1_usize..4) {
        let mut data = ScrollbackData::from_terminal_lines(segments.clone());
        data.truncate(max);
        prop_assert!(data.lines.len() <= max);
    }

    /// truncate keeps most recent lines.
    #[test]
    fn prop_truncate_keeps_recent(segments in proptest::collection::vec("[a-z]{5,10}", 5..20), max in 1_usize..4) {
        let mut data = ScrollbackData::from_terminal_lines(segments.clone());
        data.truncate(max);
        // The retained lines should be the last `max` lines from original
        let expected: Vec<_> = segments.iter().rev().take(max).rev().cloned().collect();
        prop_assert_eq!(&data.lines, &expected);
    }

    /// truncate is idempotent.
    #[test]
    fn prop_truncate_idempotent(segments in arb_segments(), max in 1_usize..50) {
        let mut data1 = ScrollbackData::from_terminal_lines(segments.clone());
        let mut data2 = ScrollbackData::from_terminal_lines(segments);
        data1.truncate(max);
        data2.truncate(max);
        data2.truncate(max); // second truncation
        prop_assert_eq!(&data1.lines, &data2.lines);
        prop_assert_eq!(data1.total_bytes(), data2.total_bytes());
    }

    /// truncate doesn't change data when max >= line count.
    #[test]
    fn prop_truncate_noop_when_within_limit(segments in arb_segments()) {
        let max = segments.len() + 10;
        let mut data = ScrollbackData::from_terminal_lines(segments.clone());
        let original_bytes = data.total_bytes();
        data.truncate(max);
        prop_assert_eq!(data.lines.len(), segments.len());
        prop_assert_eq!(data.total_bytes(), original_bytes);
    }

    /// truncate updates total_bytes correctly.
    #[test]
    fn prop_truncate_updates_bytes(segments in proptest::collection::vec("[a-z]{5,10}", 5..20), max in 1_usize..4) {
        let mut data = ScrollbackData::from_terminal_lines(segments);
        data.truncate(max);
        let recalculated: usize = data.lines.iter().map(|s| s.len()).sum();
        prop_assert_eq!(data.total_bytes(), recalculated);
    }
}

// =========================================================================
// InjectionReport — fail-closed aggregate surface
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// The unavailable output channel has a finite zeroed default report.
    #[test]
    fn prop_default_report(_dummy in 0..1_u8) {
        let report = InjectionReport::default();
        prop_assert_eq!(report.success_count(), 0);
        prop_assert_eq!(report.failure_count(), 0);
        prop_assert_eq!(report.total_bytes(), 0);
        prop_assert_eq!(report.skipped_count(), 0);
        prop_assert!(report.skipped_sample().is_empty());
        let debug = format!("{report:?}");
        prop_assert!(debug.len() < 256);
    }
}

// =========================================================================
// InjectionGuard — suppression semantics
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Guard suppresses exactly the pane IDs it was given.
    #[test]
    fn prop_guard_suppresses_given_panes(
        pane_ids in proptest::collection::vec(0_u64..1000, 1..10),
    ) {
        let suppressed = Arc::new(Mutex::new(HashMap::new()));
        let _guard = InjectionGuard::new(suppressed.clone(), pane_ids.clone()).unwrap();

        for &id in &pane_ids {
            prop_assert!(InjectionGuard::is_suppressed(&suppressed, id),
                "pane {} should be suppressed", id);
        }
    }

    /// Guard does not suppress pane IDs not in its list.
    #[test]
    fn prop_guard_does_not_suppress_others(
        guarded in proptest::collection::vec(0_u64..500, 1..5),
        other in 501_u64..1000,
    ) {
        let suppressed = Arc::new(Mutex::new(HashMap::new()));
        let _guard = InjectionGuard::new(suppressed.clone(), guarded).unwrap();

        prop_assert!(!InjectionGuard::is_suppressed(&suppressed, other),
            "pane {} should NOT be suppressed", other);
    }

    /// Dropping the guard removes suppression for its panes.
    #[test]
    fn prop_guard_drop_clears(
        pane_ids in proptest::collection::vec(0_u64..1000, 1..10),
    ) {
        let suppressed = Arc::new(Mutex::new(HashMap::new()));

        {
            let _guard = InjectionGuard::new(suppressed.clone(), pane_ids.clone()).unwrap();
            // suppression is active
            for &id in &pane_ids {
                prop_assert!(InjectionGuard::is_suppressed(&suppressed, id));
            }
        }
        // guard dropped — suppression should be cleared
        for &id in &pane_ids {
            prop_assert!(!InjectionGuard::is_suppressed(&suppressed, id),
                "pane {} should no longer be suppressed after drop", id);
        }
    }

    /// Multiple guards with disjoint panes both suppress independently.
    #[test]
    fn prop_guard_multiple_disjoint(
        panes_a in proptest::collection::vec(0_u64..500, 1..5),
        panes_b in proptest::collection::vec(500_u64..1000, 1..5),
    ) {
        let suppressed = Arc::new(Mutex::new(HashMap::new()));
        let _guard_a = InjectionGuard::new(suppressed.clone(), panes_a.clone()).unwrap();
        let _guard_b = InjectionGuard::new(suppressed.clone(), panes_b.clone()).unwrap();

        for &id in &panes_a {
            prop_assert!(InjectionGuard::is_suppressed(&suppressed, id));
        }
        for &id in &panes_b {
            prop_assert!(InjectionGuard::is_suppressed(&suppressed, id));
        }
    }

    /// Dropping one guard doesn't affect the other's suppression.
    #[test]
    fn prop_guard_partial_drop(
        panes_a in proptest::collection::vec(0_u64..500, 1..5),
        panes_b in proptest::collection::vec(500_u64..1000, 1..5),
    ) {
        let suppressed = Arc::new(Mutex::new(HashMap::new()));
        let _guard_b = InjectionGuard::new(suppressed.clone(), panes_b.clone()).unwrap();

        {
            let _guard_a = InjectionGuard::new(suppressed.clone(), panes_a.clone()).unwrap();
        }
        // guard_a dropped, guard_b still active
        for &id in &panes_b {
            prop_assert!(InjectionGuard::is_suppressed(&suppressed, id),
                "pane {} from guard_b should still be suppressed", id);
        }
    }

    /// Empty guard suppresses nothing and cleans up nothing.
    #[test]
    fn prop_guard_empty(_dummy in 0..1_u8) {
        let suppressed = Arc::new(Mutex::new(HashMap::new()));
        let _guard = InjectionGuard::new(suppressed.clone(), vec![]).unwrap();
        prop_assert!(suppressed.lock().unwrap().is_empty());
    }
}

// =========================================================================
// ScrollbackData — additional edge cases
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Single-line scrollback preserves the content and byte count.
    #[test]
    fn prop_single_line(content in "[A-Za-z0-9]{1,200}") {
        let data = ScrollbackData::from_terminal_lines(vec![content.clone()]);
        prop_assert_eq!(data.lines.len(), 1);
        prop_assert_eq!(data.total_bytes(), content.len());
        prop_assert_eq!(&data.lines[0], &content);
    }

    /// Truncating to zero leaves empty data.
    #[test]
    fn prop_truncate_to_zero(segments in proptest::collection::vec("[a-z]{1,10}", 1..20)) {
        let mut data = ScrollbackData::from_terminal_lines(segments);
        data.truncate(0);
        prop_assert!(data.lines.is_empty());
        prop_assert_eq!(data.total_bytes(), 0);
    }

    /// Truncating to 1 keeps only the last line.
    #[test]
    fn prop_truncate_to_one(segments in proptest::collection::vec("[a-z]{1,10}", 2..20)) {
        let last = segments.last().unwrap().clone();
        let mut data = ScrollbackData::from_terminal_lines(segments);
        data.truncate(1);
        prop_assert_eq!(data.lines.len(), 1);
        prop_assert_eq!(&data.lines[0], &last);
        prop_assert_eq!(data.total_bytes(), last.len());
    }

    /// total_bytes is always the sum of line lengths.
    #[test]
    fn prop_total_bytes_invariant(segments in arb_segments()) {
        let data = ScrollbackData::from_terminal_lines(segments.clone());
        let sum: usize = segments.iter().map(|s| s.len()).sum();
        prop_assert_eq!(data.total_bytes(), sum);
    }

    /// Truncation never increases total_bytes.
    #[test]
    fn prop_truncate_never_increases_bytes(
        segments in proptest::collection::vec("[a-z]{1,10}", 1..20),
        max in 0_usize..30,
    ) {
        let original = ScrollbackData::from_terminal_lines(segments.clone());
        let original_bytes = original.total_bytes();

        let mut data = ScrollbackData::from_terminal_lines(segments);
        data.truncate(max);
        prop_assert!(data.total_bytes() <= original_bytes,
            "truncated bytes {} should not exceed original {}", data.total_bytes(), original_bytes);
    }

    /// Truncation never increases line count.
    #[test]
    fn prop_truncate_never_increases_lines(
        segments in proptest::collection::vec("[a-z]{1,10}", 0..20),
        max in 0_usize..30,
    ) {
        let original_len = segments.len();
        let mut data = ScrollbackData::from_terminal_lines(segments);
        data.truncate(max);
        prop_assert!(data.lines.len() <= original_len);
        prop_assert!(data.lines.len() <= max);
    }

    /// from_terminal_lines preserves order of lines.
    #[test]
    fn prop_from_terminal_lines_preserves_order(segments in arb_segments()) {
        let data = ScrollbackData::from_terminal_lines(segments.clone());
        prop_assert_eq!(&data.lines, &segments);
    }

    /// Clone produces identical data.
    #[test]
    fn prop_clone_identical(segments in arb_segments()) {
        let data = ScrollbackData::from_terminal_lines(segments);
        let cloned = data.clone();
        prop_assert_eq!(&data.lines, &cloned.lines);
        prop_assert_eq!(data.total_bytes(), cloned.total_bytes());
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn scrollback_from_terminal_lines_basic() {
    let data = ScrollbackData::from_terminal_lines(vec![
        "line1".to_string(),
        "line2".to_string(),
        "line3".to_string(),
    ]);
    assert_eq!(data.lines.len(), 3);
    assert_eq!(data.total_bytes(), 15); // 5 + 5 + 5
}

#[test]
fn scrollback_truncate_basic() {
    let mut data = ScrollbackData::from_terminal_lines(vec![
        "aa".to_string(),
        "bb".to_string(),
        "cc".to_string(),
        "dd".to_string(),
    ]);
    data.truncate(2);
    assert_eq!(data.lines, vec!["cc", "dd"]); // keeps most recent
    assert_eq!(data.total_bytes(), 4);
}
