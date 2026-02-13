//! Property-based tests for the `tantivy_reindex` module.
//!
//! Covers `BackfillRange` serde roundtrips and `includes()`/`past_end()`
//! correctness, plus `ReindexProgress` serde roundtrips.

use frankenterm_core::tantivy_reindex::{BackfillRange, ReindexProgress};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_backfill_range() -> impl Strategy<Value = BackfillRange> {
    prop_oneof![
        (0_u64..100_000, 0_u64..100_000).prop_map(|(a, b)| {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            BackfillRange::OrdinalRange { start, end }
        }),
        (0_u64..2_000_000_000_000, 0_u64..2_000_000_000_000).prop_map(|(a, b)| {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            BackfillRange::TimeRange {
                start_ms: start,
                end_ms: end,
            }
        }),
        Just(BackfillRange::All),
    ]
}

fn arb_reindex_progress() -> impl Strategy<Value = ReindexProgress> {
    (
        0_u64..100_000,
        0_u64..100_000,
        0_u64..10_000,
        0_u64..10_000,
        0_u64..1000,
        proptest::option::of(0_u64..100_000),
        any::<bool>(),
        0_u64..10_000,
    )
        .prop_map(
            |(
                events_read,
                events_indexed,
                events_skipped,
                events_filtered,
                batches_committed,
                current_ordinal,
                caught_up,
                docs_cleared,
            )| {
                ReindexProgress {
                    events_read,
                    events_indexed,
                    events_skipped,
                    events_filtered,
                    batches_committed,
                    current_ordinal,
                    caught_up,
                    docs_cleared,
                }
            },
        )
}

// =========================================================================
// BackfillRange — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_range_serde(range in arb_backfill_range()) {
        let json = serde_json::to_string(&range).unwrap();
        let back: BackfillRange = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, range);
    }

    #[test]
    fn prop_range_deterministic(range in arb_backfill_range()) {
        let j1 = serde_json::to_string(&range).unwrap();
        let j2 = serde_json::to_string(&range).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// BackfillRange::includes — correctness
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// All always includes any ordinal/timestamp.
    #[test]
    fn prop_all_includes_everything(ordinal in 0_u64..100_000, ts in 0_u64..2_000_000_000_000) {
        prop_assert!(BackfillRange::All.includes(ordinal, ts));
    }

    /// OrdinalRange includes events within [start, end].
    #[test]
    fn prop_ordinal_range_within(
        start in 0_u64..50_000,
        end in 50_000_u64..100_000,
        ordinal in 0_u64..100_000,
    ) {
        let range = BackfillRange::OrdinalRange { start, end };
        let expected = ordinal >= start && ordinal <= end;
        prop_assert_eq!(range.includes(ordinal, 0), expected);
    }

    /// TimeRange includes events within [start_ms, end_ms].
    #[test]
    fn prop_time_range_within(
        start_ms in 0_u64..500_000,
        end_ms in 500_000_u64..1_000_000,
        ts in 0_u64..1_000_000,
    ) {
        let range = BackfillRange::TimeRange { start_ms, end_ms };
        let expected = ts >= start_ms && ts <= end_ms;
        // ordinal is ignored for TimeRange
        prop_assert_eq!(range.includes(0, ts), expected);
    }

    /// OrdinalRange at exact boundaries includes start and end.
    #[test]
    fn prop_ordinal_boundaries(start in 0_u64..50_000, end in 50_000_u64..100_000) {
        let range = BackfillRange::OrdinalRange { start, end };
        prop_assert!(range.includes(start, 0));
        prop_assert!(range.includes(end, 0));
    }
}

// =========================================================================
// BackfillRange::past_end — correctness
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// All never signals past_end.
    #[test]
    fn prop_all_never_past_end(ordinal in 0_u64..u64::MAX) {
        prop_assert!(!BackfillRange::All.past_end(ordinal));
    }

    /// TimeRange never signals past_end (events may not be time-ordered).
    #[test]
    fn prop_time_range_never_past_end(
        start in 0_u64..500_000,
        end in 500_000_u64..1_000_000,
        ordinal in 0_u64..u64::MAX,
    ) {
        let range = BackfillRange::TimeRange { start_ms: start, end_ms: end };
        prop_assert!(!range.past_end(ordinal));
    }

    /// OrdinalRange signals past_end only when ordinal > end.
    #[test]
    fn prop_ordinal_past_end(
        start in 0_u64..50_000,
        end in 50_000_u64..100_000,
        ordinal in 0_u64..200_000,
    ) {
        let range = BackfillRange::OrdinalRange { start, end };
        prop_assert_eq!(range.past_end(ordinal), ordinal > end);
    }

    /// past_end implies !includes for OrdinalRange.
    #[test]
    fn prop_past_end_implies_not_includes(
        start in 0_u64..50_000,
        end in 50_000_u64..100_000,
        ordinal in 0_u64..200_000,
    ) {
        let range = BackfillRange::OrdinalRange { start, end };
        if range.past_end(ordinal) {
            prop_assert!(!range.includes(ordinal, 0));
        }
    }
}

// =========================================================================
// ReindexProgress — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_progress_serde(progress in arb_reindex_progress()) {
        let json = serde_json::to_string(&progress).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, progress);
    }

    #[test]
    fn prop_progress_deterministic(progress in arb_reindex_progress()) {
        let j1 = serde_json::to_string(&progress).unwrap();
        let j2 = serde_json::to_string(&progress).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn all_range_includes_any() {
    assert!(BackfillRange::All.includes(0, 0));
    assert!(BackfillRange::All.includes(u64::MAX, u64::MAX));
}

#[test]
fn ordinal_range_excludes_outside() {
    let range = BackfillRange::OrdinalRange { start: 10, end: 20 };
    assert!(!range.includes(5, 0));
    assert!(!range.includes(25, 0));
    assert!(range.includes(10, 0));
    assert!(range.includes(15, 0));
    assert!(range.includes(20, 0));
}

#[test]
fn past_end_ordinal() {
    let range = BackfillRange::OrdinalRange { start: 10, end: 20 };
    assert!(!range.past_end(15));
    assert!(!range.past_end(20));
    assert!(range.past_end(21));
}
