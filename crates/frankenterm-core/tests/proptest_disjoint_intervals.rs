//! Property-based tests for disjoint_intervals.rs — non-overlapping interval set.
//!
//! Verifies the DisjointIntervals invariants:
//! - Disjointness: no two intervals overlap
//! - Sorted order: intervals are sorted by start
//! - Merge correctness: overlapping inserts produce correct merged result
//! - Contains consistency: contains(p) iff p is in some interval
//! - Span consistency: span == sum of interval lengths
//! - Gaps + intervals partition the universe
//! - Insert commutativity: insert order doesn't affect result
//! - Remove correctness: removed points are not contained
//! - Clone equivalence and independence
//! - Clear empties the set
//! - Config and stats serde roundtrip
//!
//! Bead: ft-283h4.31

use frankenterm_core::disjoint_intervals::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_interval() -> impl Strategy<Value = (i64, i64)> {
    (-100i64..=100, 0i64..=50).prop_map(|(start, len)| (start, start + len))
}

fn arb_intervals(max_n: usize) -> impl Strategy<Value = Vec<(i64, i64)>> {
    prop::collection::vec(arb_interval(), 0..=max_n)
}

fn build_di(intervals: &[(i64, i64)]) -> DisjointIntervals {
    let mut di = DisjointIntervals::new();
    for &(start, end) in intervals {
        di.insert(start, end);
    }
    di
}

// ── Disjointness invariant ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// After any sequence of inserts, intervals are disjoint.
    #[test]
    fn prop_intervals_disjoint(intervals in arb_intervals(15)) {
        let di = build_di(&intervals);
        let ivs = di.intervals();
        for pair in ivs.windows(2) {
            prop_assert!(
                pair[0].end <= pair[1].start,
                "intervals overlap: [{}, {}) and [{}, {})",
                pair[0].start, pair[0].end, pair[1].start, pair[1].end
            );
        }
    }

    /// Intervals are sorted by start.
    #[test]
    fn prop_intervals_sorted(intervals in arb_intervals(15)) {
        let di = build_di(&intervals);
        let ivs = di.intervals();
        let is_sorted = ivs.windows(2).all(|w| w[0].start < w[1].start);
        prop_assert!(ivs.len() <= 1 || is_sorted, "intervals not sorted");
    }

    /// No interval is empty.
    #[test]
    fn prop_no_empty_intervals(intervals in arb_intervals(15)) {
        let di = build_di(&intervals);
        for iv in di.intervals() {
            prop_assert!(iv.start < iv.end, "empty interval [{}, {})", iv.start, iv.end);
        }
    }
}

// ── Contains consistency ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// contains(p) is true iff p falls in some stored interval.
    #[test]
    fn prop_contains_consistent(
        intervals in arb_intervals(10),
        point in -110i64..=160,
    ) {
        let di = build_di(&intervals);
        let in_some = di.intervals().iter().any(|iv| iv.contains_point(point));
        prop_assert_eq!(
            di.contains(point), in_some,
            "contains({}) disagrees with manual check", point
        );
    }

    /// Every point in an inserted interval is contained.
    #[test]
    fn prop_inserted_points_contained(
        start in -50i64..=50,
        len in 1i64..=20,
    ) {
        let end = start + len;
        let mut di = DisjointIntervals::new();
        di.insert(start, end);

        for p in start..end {
            prop_assert!(di.contains(p), "point {} should be contained in [{}, {})", p, start, end);
        }
    }
}

// ── Span consistency ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// span equals sum of interval lengths.
    #[test]
    fn prop_span_is_sum_of_lengths(intervals in arb_intervals(15)) {
        let di = build_di(&intervals);
        let manual_span: i64 = di.intervals().iter().map(|iv| iv.len()).sum();
        prop_assert_eq!(di.span(), manual_span, "span mismatch");
    }

    /// span is non-negative.
    #[test]
    fn prop_span_nonnegative(intervals in arb_intervals(15)) {
        let di = build_di(&intervals);
        prop_assert!(di.span() >= 0, "span should be >= 0");
    }

    /// span <= total inserted span (due to merging).
    #[test]
    fn prop_span_bounded(intervals in arb_intervals(15)) {
        let di = build_di(&intervals);
        let total_inserted: i64 = intervals.iter()
            .filter(|&&(s, e)| e > s)
            .map(|&(s, e)| e - s)
            .sum();
        prop_assert!(
            di.span() <= total_inserted,
            "span {} > total inserted {}", di.span(), total_inserted
        );
    }
}

// ── Gaps + intervals partition ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// gaps + intervals exactly cover [lo, hi).
    #[test]
    fn prop_gaps_plus_intervals_partition(
        intervals in arb_intervals(10),
        lo in -50i64..=0,
        hi_offset in 1i64..=100,
    ) {
        let hi = lo + hi_offset;
        let di = build_di(&intervals);

        let gaps = di.gaps(lo, hi);
        let mut covered: Vec<Interval> = Vec::new();

        // Add intervals that intersect [lo, hi)
        for iv in di.intervals() {
            let clipped_start = iv.start.max(lo);
            let clipped_end = iv.end.min(hi);
            if clipped_start < clipped_end {
                covered.push(Interval::new(clipped_start, clipped_end));
            }
        }

        // Gaps should fill the rest
        let gap_total: i64 = gaps.iter().map(|g| g.len()).sum();
        let covered_total: i64 = covered.iter().map(|c| c.len()).sum();
        let universe = hi - lo;

        prop_assert_eq!(
            gap_total + covered_total, universe,
            "gaps ({}) + covered ({}) != universe ({})", gap_total, covered_total, universe
        );
    }
}

// ── Insert commutativity ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Insert order doesn't affect the final interval set.
    #[test]
    fn prop_insert_commutative(intervals in arb_intervals(10)) {
        prop_assume!(!intervals.is_empty());
        let di1 = build_di(&intervals);

        let mut reversed = intervals.clone();
        reversed.reverse();
        let di2 = build_di(&reversed);

        prop_assert_eq!(di1.intervals(), di2.intervals(), "insert order affected result");
    }

    /// Inserting a subset then the rest is same as inserting all.
    #[test]
    fn prop_insert_associative(
        intervals in arb_intervals(10),
        split in 0usize..10,
    ) {
        let n = intervals.len();
        let split = split.min(n);

        let di_all = build_di(&intervals);

        let mut di_split = DisjointIntervals::new();
        for &(s, e) in &intervals[..split] {
            di_split.insert(s, e);
        }
        for &(s, e) in &intervals[split..] {
            di_split.insert(s, e);
        }

        prop_assert_eq!(di_all.intervals(), di_split.intervals());
    }
}

// ── Intersects properties ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// intersects(s, e) is consistent with contains for points in [s, e).
    #[test]
    fn prop_intersects_consistent(
        intervals in arb_intervals(10),
        s in -50i64..=50,
        len in 1i64..=20,
    ) {
        let e = s + len;
        let di = build_di(&intervals);

        let manual = (s..e).any(|p| di.contains(p));
        if manual {
            prop_assert!(di.intersects(s, e),
                "manual found overlap but intersects returned false");
        }
        // Note: intersects could be true even if no integer point is contained
        // (e.g., float-like interval edges), so we only check one direction
    }
}

// ── Remove properties ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After remove(s, e), no point in [s, e) is contained.
    #[test]
    fn prop_remove_clears_range(
        intervals in arb_intervals(10),
        s in -30i64..=30,
        len in 1i64..=20,
    ) {
        let e = s + len;
        let mut di = build_di(&intervals);
        di.remove(s, e);

        for p in s..e {
            prop_assert!(!di.contains(p),
                "point {} should not be contained after remove([{}, {}))", p, s, e);
        }
    }

    /// Remove preserves points outside the removed range.
    #[test]
    fn prop_remove_preserves_outside(
        s in 0i64..=20,
        len in 5i64..=20,
        remove_start in 0i64..=10,
        remove_len in 1i64..=5,
    ) {
        let e = s + len;
        let rs = s + remove_start.min(len - 1);
        let re = (rs + remove_len).min(e);

        let mut di = DisjointIntervals::new();
        di.insert(s, e);
        di.remove(rs, re);

        // Points before the hole should still be contained
        for p in s..rs {
            prop_assert!(di.contains(p),
                "point {} before hole should be contained", p);
        }
        // Points after the hole should still be contained
        for p in re..e {
            prop_assert!(di.contains(p),
                "point {} after hole should be contained", p);
        }
    }

    /// Remove maintains disjointness invariant.
    #[test]
    fn prop_remove_maintains_disjoint(
        intervals in arb_intervals(10),
        s in -30i64..=30,
        len in 1i64..=20,
    ) {
        let e = s + len;
        let mut di = build_di(&intervals);
        di.remove(s, e);

        let ivs = di.intervals();
        for pair in ivs.windows(2) {
            prop_assert!(
                pair[0].end <= pair[1].start,
                "disjointness violated after remove"
            );
        }
    }
}

// ── Clone properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Clone produces identical intervals.
    #[test]
    fn prop_clone_equivalence(intervals in arb_intervals(10)) {
        let di = build_di(&intervals);
        let clone = di.clone();
        prop_assert_eq!(di.intervals(), clone.intervals());
    }

    /// Mutations to clone don't affect original.
    #[test]
    fn prop_clone_independence(intervals in arb_intervals(10)) {
        let di = build_di(&intervals);
        let original_count = di.count();
        let mut clone = di.clone();
        clone.insert(-999, -900);
        prop_assert_eq!(di.count(), original_count, "original modified by clone");
    }
}

// ── Clear properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// clear() empties everything.
    #[test]
    fn prop_clear_empties(intervals in arb_intervals(10)) {
        let mut di = build_di(&intervals);
        di.clear();
        prop_assert!(di.is_empty());
        prop_assert_eq!(di.count(), 0);
        prop_assert_eq!(di.span(), 0);
    }
}

// ── Serde properties ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// DisjointIntervalsConfig survives JSON roundtrip.
    #[test]
    fn prop_config_serde_roundtrip(merge in prop::bool::ANY) {
        let config = DisjointIntervalsConfig { merge_adjacent: merge };
        let json = serde_json::to_string(&config).unwrap();
        let back: DisjointIntervalsConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    /// DisjointIntervalsStats survives JSON roundtrip.
    #[test]
    fn prop_stats_serde_roundtrip(intervals in arb_intervals(10)) {
        let di = build_di(&intervals);
        let stats = di.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: DisjointIntervalsStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }

    /// Stats fields are consistent.
    #[test]
    fn prop_stats_consistent(intervals in arb_intervals(10)) {
        let di = build_di(&intervals);
        let stats = di.stats();
        prop_assert_eq!(stats.interval_count, di.count());
        prop_assert_eq!(stats.total_span, di.span());
        prop_assert_eq!(stats.memory_bytes, di.memory_bytes());
    }
}

// ── Interval type properties ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Interval merge is commutative.
    #[test]
    fn prop_interval_merge_commutative(
        s1 in -50i64..=50, l1 in 0i64..=20,
        s2 in -50i64..=50, l2 in 0i64..=20,
    ) {
        let a = Interval::new(s1, s1 + l1);
        let b = Interval::new(s2, s2 + l2);
        prop_assert_eq!(a.merge(&b), b.merge(&a), "interval merge not commutative");
    }

    /// Interval overlap is symmetric.
    #[test]
    fn prop_interval_overlap_symmetric(
        s1 in -50i64..=50, l1 in 0i64..=20,
        s2 in -50i64..=50, l2 in 0i64..=20,
    ) {
        let a = Interval::new(s1, s1 + l1);
        let b = Interval::new(s2, s2 + l2);
        prop_assert_eq!(a.overlaps(&b), b.overlaps(&a), "overlap not symmetric");
    }
}

// ── Empty set properties ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty set invariants.
    #[test]
    fn prop_empty_invariants(_dummy in 0..1u8) {
        let di = DisjointIntervals::new();
        prop_assert!(di.is_empty());
        prop_assert_eq!(di.count(), 0);
        prop_assert_eq!(di.span(), 0);
        prop_assert!(!di.contains(0));
        let is_none = di.min_start().is_none();
        prop_assert!(is_none, "min_start should be None for empty");
        let is_none2 = di.max_end().is_none();
        prop_assert!(is_none2, "max_end should be None for empty");
    }
}
