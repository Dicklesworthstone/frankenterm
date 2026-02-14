//! Property-based tests for the `tantivy_reindex` module.
//!
//! Covers `BackfillRange` serde roundtrips, `includes()`/`past_end()`
//! correctness and complementarity, plus `ReindexProgress` serde roundtrips
//! and structural invariants.

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

    /// JSON for All variant contains the tag.
    #[test]
    fn prop_all_variant_tag(_dummy in 0..1_u8) {
        let json = serde_json::to_string(&BackfillRange::All).unwrap();
        prop_assert!(json.contains("All"), "All variant should serialize with tag: {}", json);
    }

    /// OrdinalRange JSON contains start and end fields.
    #[test]
    fn prop_ordinal_range_has_fields(
        start in 0_u64..50_000,
        end in 50_000_u64..100_000,
    ) {
        let range = BackfillRange::OrdinalRange { start, end };
        let json = serde_json::to_string(&range).unwrap();
        prop_assert!(json.contains("start"), "should contain 'start': {}", json);
        prop_assert!(json.contains("end"), "should contain 'end': {}", json);
    }

    /// TimeRange JSON contains start_ms and end_ms fields.
    #[test]
    fn prop_time_range_has_fields(
        start in 0_u64..500_000,
        end in 500_000_u64..1_000_000,
    ) {
        let range = BackfillRange::TimeRange { start_ms: start, end_ms: end };
        let json = serde_json::to_string(&range).unwrap();
        prop_assert!(json.contains("start_ms"), "should contain 'start_ms': {}", json);
        prop_assert!(json.contains("end_ms"), "should contain 'end_ms': {}", json);
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

    /// TimeRange at exact boundaries includes start_ms and end_ms.
    #[test]
    fn prop_time_range_boundaries(
        start in 0_u64..500_000,
        end in 500_000_u64..1_000_000,
    ) {
        let range = BackfillRange::TimeRange { start_ms: start, end_ms: end };
        prop_assert!(range.includes(0, start), "should include start_ms boundary");
        prop_assert!(range.includes(0, end), "should include end_ms boundary");
    }

    /// OrdinalRange with start == end (single point range) includes only that point.
    #[test]
    fn prop_ordinal_single_point(
        point in 0_u64..100_000,
        other in 0_u64..100_000,
    ) {
        let range = BackfillRange::OrdinalRange { start: point, end: point };
        prop_assert_eq!(range.includes(other, 0), other == point,
            "single-point range should only include the point itself");
    }

    /// TimeRange with start == end (single point) includes only that timestamp.
    #[test]
    fn prop_time_range_single_point(
        point in 0_u64..1_000_000,
        other in 0_u64..1_000_000,
    ) {
        let range = BackfillRange::TimeRange { start_ms: point, end_ms: point };
        prop_assert_eq!(range.includes(0, other), other == point,
            "single-point time range should only include that timestamp");
    }

    /// OrdinalRange excludes ordinals below start.
    #[test]
    fn prop_ordinal_excludes_below(
        start in 1_u64..50_000,
        end in 50_000_u64..100_000,
    ) {
        let range = BackfillRange::OrdinalRange { start, end };
        prop_assert!(!range.includes(start - 1, 0),
            "ordinal {} should be excluded (start={})", start - 1, start);
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

    /// For OrdinalRange: ordinal == end → includes true AND past_end false.
    #[test]
    fn prop_ordinal_at_end_boundary(
        start in 0_u64..50_000,
        end in 50_000_u64..100_000,
    ) {
        let range = BackfillRange::OrdinalRange { start, end };
        prop_assert!(range.includes(end, 0),
            "ordinal at end should be included");
        prop_assert!(!range.past_end(end),
            "ordinal at end should not be past_end");
    }

    /// For OrdinalRange: ordinal == end + 1 → includes false AND past_end true.
    #[test]
    fn prop_ordinal_just_past_end(
        start in 0_u64..50_000,
        end in 50_000_u64..99_999,
    ) {
        let range = BackfillRange::OrdinalRange { start, end };
        prop_assert!(!range.includes(end + 1, 0),
            "ordinal just past end should not be included");
        prop_assert!(range.past_end(end + 1),
            "ordinal just past end should be past_end");
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

    /// ReindexProgress JSON contains all required fields.
    #[test]
    fn prop_progress_has_all_fields(progress in arb_reindex_progress()) {
        let json = serde_json::to_string(&progress).unwrap();
        prop_assert!(json.contains("\"events_read\""), "missing events_read");
        prop_assert!(json.contains("\"events_indexed\""), "missing events_indexed");
        prop_assert!(json.contains("\"events_skipped\""), "missing events_skipped");
        prop_assert!(json.contains("\"events_filtered\""), "missing events_filtered");
        prop_assert!(json.contains("\"batches_committed\""), "missing batches_committed");
        prop_assert!(json.contains("\"caught_up\""), "missing caught_up");
        prop_assert!(json.contains("\"docs_cleared\""), "missing docs_cleared");
    }

    /// ReindexProgress with all zeros roundtrips correctly.
    #[test]
    fn prop_progress_zero_values(_dummy in 0..1_u8) {
        let progress = ReindexProgress {
            events_read: 0,
            events_indexed: 0,
            events_skipped: 0,
            events_filtered: 0,
            batches_committed: 0,
            current_ordinal: None,
            caught_up: false,
            docs_cleared: 0,
        };
        let json = serde_json::to_string(&progress).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, progress);
    }

    /// ReindexProgress with max u64 values roundtrips.
    #[test]
    fn prop_progress_max_values(_dummy in 0..1_u8) {
        let progress = ReindexProgress {
            events_read: u64::MAX,
            events_indexed: u64::MAX,
            events_skipped: u64::MAX,
            events_filtered: u64::MAX,
            batches_committed: u64::MAX,
            current_ordinal: Some(u64::MAX),
            caught_up: true,
            docs_cleared: u64::MAX,
        };
        let json = serde_json::to_string(&progress).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.events_read, u64::MAX);
        prop_assert_eq!(back.current_ordinal, Some(u64::MAX));
        prop_assert!(back.caught_up);
    }

    /// ReindexProgress with None current_ordinal excludes it or includes null.
    #[test]
    fn prop_progress_none_ordinal_roundtrips(_dummy in 0..1_u8) {
        let progress = ReindexProgress {
            events_read: 42,
            events_indexed: 40,
            events_skipped: 2,
            events_filtered: 0,
            batches_committed: 1,
            current_ordinal: None,
            caught_up: false,
            docs_cleared: 0,
        };
        let json = serde_json::to_string(&progress).unwrap();
        let back: ReindexProgress = serde_json::from_str(&json).unwrap();
        prop_assert!(back.current_ordinal.is_none());
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

#[test]
fn time_range_excludes_outside() {
    let range = BackfillRange::TimeRange {
        start_ms: 1000,
        end_ms: 2000,
    };
    assert!(!range.includes(0, 999));
    assert!(range.includes(0, 1000));
    assert!(range.includes(0, 1500));
    assert!(range.includes(0, 2000));
    assert!(!range.includes(0, 2001));
}
