//! Property tests for `StorageRangeFilter`
//! (`crates/frankenterm-core/src/storage_range_filter.rs`).
//!
//! The filter answers "could a recorded timestamp range intersect this query?"
//! to let SQLite skip non-matching scans. It is an *exact* interval backend
//! (`false_positive_rate_bound() == 0.0`), so `could_have_match` must have
//! neither false negatives (the safety property — never skip a real match) nor
//! false positives. It had inline unit tests but no dedicated property suite.

use frankenterm_core::storage_range_filter::StorageRangeFilter;
use proptest::prelude::*;

fn norm(a: i64, b: i64) -> (i64, i64) {
    if a <= b { (a, b) } else { (b, a) }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Exactness: `could_have_match(low, high)` is true iff some recorded range
    /// overlaps `[low, high]`. Merging touching/overlapping intervals preserves
    /// integer-point coverage, so the brute-force check over the raw recorded
    /// ranges is the ground truth.
    #[test]
    fn membership_is_exact(
        recorded in prop::collection::vec((any::<i64>(), any::<i64>()), 0..12),
        q_lo in any::<i64>(),
        q_hi in any::<i64>(),
    ) {
        let mut filter = StorageRangeFilter::new("events");
        for &(a, b) in &recorded {
            filter.record_range(a, b);
        }

        let (low, high) = norm(q_lo, q_hi);
        let expected = recorded.iter().any(|&(a, b)| {
            let (rs, re) = norm(a, b);
            rs <= high && re >= low
        });

        prop_assert_eq!(
            filter.could_have_match(q_lo, q_hi),
            expected,
            "could_have_match disagrees with brute-force overlap for query [{}, {}]",
            low,
            high
        );
    }

    /// Every recorded range is self-matchable (an explicit no-false-negative
    /// check on the exact bounds that were inserted).
    #[test]
    fn recorded_range_always_matches_itself(
        recorded in prop::collection::vec((any::<i64>(), any::<i64>()), 1..12),
    ) {
        let mut filter = StorageRangeFilter::new("events");
        for &(a, b) in &recorded {
            filter.record_range(a, b);
        }
        for &(a, b) in &recorded {
            prop_assert!(
                filter.could_have_match(a, b),
                "recorded range [{}, {}] not matchable",
                a, b
            );
        }
    }

    /// Merged ranges are well-formed, strictly ascending, and non-touching: any
    /// two consecutive intervals have a gap of at least 2 (else they would have
    /// merged). Uses i128 to compare without overflow at i64 extremes.
    #[test]
    fn ranges_are_disjoint_and_sorted(
        recorded in prop::collection::vec((any::<i64>(), any::<i64>()), 0..16),
    ) {
        let mut filter = StorageRangeFilter::new("events");
        for &(a, b) in &recorded {
            filter.record_range(a, b);
        }
        let ranges: Vec<(i64, i64)> = filter.ranges().collect();
        for &(s, e) in &ranges {
            prop_assert!(s <= e, "ill-formed range [{}, {}]", s, e);
        }
        for window in ranges.windows(2) {
            let (_, e1) = window[0];
            let (s2, _) = window[1];
            prop_assert!(
                i128::from(s2) > i128::from(e1) + 1,
                "consecutive ranges touch/overlap: {:?} then {:?}",
                window[0],
                window[1]
            );
        }
    }

    /// Recording the same set of ranges twice is idempotent (merge is a fixed
    /// point): the merged representation is unchanged by replaying the inserts.
    #[test]
    fn record_is_idempotent(
        recorded in prop::collection::vec((any::<i64>(), any::<i64>()), 0..12),
    ) {
        let mut once = StorageRangeFilter::new("events");
        for &(a, b) in &recorded {
            once.record_range(a, b);
        }
        let mut twice = StorageRangeFilter::new("events");
        for &(a, b) in &recorded {
            twice.record_range(a, b);
        }
        for &(a, b) in &recorded {
            twice.record_range(a, b);
        }
        let once_ranges: Vec<(i64, i64)> = once.ranges().collect();
        let twice_ranges: Vec<(i64, i64)> = twice.ranges().collect();
        prop_assert_eq!(once_ranges, twice_ranges);
    }

    /// A snapshot reflects the live merged ranges.
    #[test]
    fn snapshot_matches_ranges(
        recorded in prop::collection::vec((any::<i64>(), any::<i64>()), 0..12),
    ) {
        let mut filter = StorageRangeFilter::new("events");
        for &(a, b) in &recorded {
            filter.record_range(a, b);
        }
        let live: Vec<(i64, i64)> = filter.ranges().collect();
        let snap = filter.snapshot();
        prop_assert_eq!(snap.ranges, live);
        prop_assert!((snap.false_positive_rate_bound - 0.0).abs() < f64::EPSILON);
    }
}
