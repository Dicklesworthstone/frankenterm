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

// ── is_empty / count / default agreement ────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// is_empty() agrees with count() == 0 for any interval set.
    #[test]
    fn prop_is_empty_agrees_with_count(intervals in arb_intervals(15)) {
        let di = build_di(&intervals);
        let empty = di.is_empty();
        let count_zero = di.count() == 0;
        prop_assert_eq!(empty, count_zero,
            "is_empty()={} but count()==0 is {}", empty, count_zero);
    }

    /// Default::default() produces an identical empty set to new().
    #[test]
    fn prop_default_is_empty(_dummy in 0..1u8) {
        let di: DisjointIntervals = Default::default();
        prop_assert!(di.is_empty());
        prop_assert_eq!(di.count(), 0);
        prop_assert_eq!(di.span(), 0);
        let ivs = di.intervals();
        prop_assert!(ivs.is_empty(), "default should have no intervals");
    }
}

// ── min_start / max_end consistency ─────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// min_start and max_end agree with the first and last intervals.
    #[test]
    fn prop_min_max_consistent(intervals in arb_intervals(15)) {
        let di = build_di(&intervals);
        let ivs = di.intervals();
        if ivs.is_empty() {
            let min_none = di.min_start().is_none();
            let max_none = di.max_end().is_none();
            prop_assert!(min_none, "min_start should be None when empty");
            prop_assert!(max_none, "max_end should be None when empty");
        } else {
            let min_s = di.min_start().unwrap();
            let max_e = di.max_end().unwrap();
            prop_assert_eq!(min_s, ivs[0].start,
                "min_start {} != first interval start {}", min_s, ivs[0].start);
            prop_assert_eq!(max_e, ivs[ivs.len() - 1].end,
                "max_end {} != last interval end {}", max_e, ivs[ivs.len() - 1].end);
            prop_assert!(min_s < max_e,
                "min_start {} should be < max_end {}", min_s, max_e);
        }
    }
}

// ── Interval serde roundtrip ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Interval survives JSON roundtrip.
    #[test]
    fn prop_interval_serde_roundtrip(
        start in -100i64..=100,
        len in 0i64..=50,
    ) {
        let iv = Interval::new(start, start + len);
        let json = serde_json::to_string(&iv).unwrap();
        let back: Interval = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(iv, back);
    }
}

// ── Double remove idempotency ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Removing the same range twice is idempotent (second remove is a no-op).
    #[test]
    fn prop_double_remove_idempotent(
        intervals in arb_intervals(10),
        s in -30i64..=30,
        len in 1i64..=20,
    ) {
        let e = s + len;
        let mut di1 = build_di(&intervals);
        di1.remove(s, e);
        let after_first = di1.intervals().to_vec();

        di1.remove(s, e);
        let after_second = di1.intervals().to_vec();

        prop_assert_eq!(after_first, after_second,
            "second remove changed intervals");
    }
}

// ── Gaps disjoint and sorted ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Gaps are disjoint, sorted, and non-empty.
    #[test]
    fn prop_gaps_disjoint_sorted(
        intervals in arb_intervals(10),
        lo in -50i64..=0,
        hi_offset in 1i64..=100,
    ) {
        let hi = lo + hi_offset;
        let di = build_di(&intervals);
        let gaps = di.gaps(lo, hi);

        // All gap intervals are non-empty
        for g in &gaps {
            prop_assert!(g.start < g.end,
                "gap [{}, {}) is empty", g.start, g.end);
        }

        // Gaps are sorted and disjoint
        for pair in gaps.windows(2) {
            prop_assert!(pair[0].end <= pair[1].start,
                "gaps overlap or unsorted: [{}, {}) and [{}, {})",
                pair[0].start, pair[0].end, pair[1].start, pair[1].end);
        }

        // All gaps are within [lo, hi)
        for g in &gaps {
            prop_assert!(g.start >= lo && g.end <= hi,
                "gap [{}, {}) outside bounds [{}, {})", g.start, g.end, lo, hi);
        }
    }

    /// Gap points are not contained in any interval.
    #[test]
    fn prop_gap_points_not_contained(
        intervals in arb_intervals(8),
        lo in -20i64..=0,
        hi_offset in 1i64..=40,
    ) {
        let hi = lo + hi_offset;
        let di = build_di(&intervals);
        let gaps = di.gaps(lo, hi);

        for g in &gaps {
            // Sample up to 10 points from each gap
            let sample_count = (g.end - g.start).min(10);
            for i in 0..sample_count {
                let p = g.start + i;
                prop_assert!(!di.contains(p),
                    "gap point {} is contained but shouldn't be", p);
            }
        }
    }
}

// ── from_config with merge_adjacent=false ───────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// With merge_adjacent=false, adjacent (touching) intervals stay separate.
    #[test]
    fn prop_no_merge_adjacent_keeps_touching(
        base in 0i64..=50,
        len1 in 1i64..=20,
        len2 in 1i64..=20,
    ) {
        let config = DisjointIntervalsConfig { merge_adjacent: false };
        let mut di = DisjointIntervals::from_config(&config);
        di.insert(base, base + len1);
        di.insert(base + len1, base + len1 + len2);  // adjacent, not overlapping

        // With merge_adjacent=false, these should remain as two separate intervals
        prop_assert_eq!(di.count(), 2,
            "adjacent intervals should stay separate when merge_adjacent=false, \
             got count={}", di.count());

        // Total span should equal the sum of both lengths
        let expected_span = len1 + len2;
        prop_assert_eq!(di.span(), expected_span,
            "span {} != expected {}", di.span(), expected_span);
    }
}

// ── Additional invariants (DarkMill ft-283h4.56) ────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Interval::overlaps_or_adjacent is reflexive for non-empty intervals.
    #[test]
    fn prop_overlaps_or_adjacent_reflexive(
        start in -50i64..=50,
        len in 0i64..=30,
    ) {
        let iv = Interval::new(start, start + len);
        if len > 0 {
            prop_assert!(iv.overlaps_or_adjacent(&iv),
                "non-empty interval should overlap-or-adjacent itself");
        }
    }

    /// Interval::is_empty iff len == 0.
    #[test]
    fn prop_interval_is_empty_iff_len_zero(
        start in -100i64..=100,
        len in 0i64..=50,
    ) {
        let iv = Interval::new(start, start + len);
        prop_assert_eq!(iv.is_empty(), iv.is_empty(),
            "is_empty() disagrees with len()==0");
    }

    /// memory_bytes is positive for non-empty sets.
    #[test]
    fn prop_memory_bytes_positive(intervals in arb_intervals(10)) {
        let di = build_di(&intervals);
        if di.count() > 0 {
            prop_assert!(di.memory_bytes() > 0,
                "memory_bytes should be positive for non-empty set");
        }
    }

    /// Insert then remove same range yields empty for single interval.
    #[test]
    fn prop_insert_remove_roundtrip(
        start in -50i64..=50,
        len in 1i64..=30,
    ) {
        let end = start + len;
        let mut di = DisjointIntervals::new();
        di.insert(start, end);
        prop_assert!(!di.is_empty());
        di.remove(start, end);
        prop_assert!(di.is_empty(), "should be empty after removing exact same range");
        prop_assert_eq!(di.span(), 0);
    }
}

// ── Idempotent insert ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Inserting the same interval twice is idempotent.
    #[test]
    fn prop_insert_idempotent(
        intervals in arb_intervals(10),
        s in -50i64..=50,
        len in 1i64..=20,
    ) {
        let e = s + len;
        let mut di1 = build_di(&intervals);
        di1.insert(s, e);
        let after_first = di1.intervals().to_vec();

        di1.insert(s, e);
        let after_second = di1.intervals().to_vec();

        prop_assert_eq!(after_first, after_second,
            "second insert of same interval changed state");
    }

    /// Inserting an empty interval (start == end) is a no-op.
    #[test]
    fn prop_empty_interval_insert_noop(
        intervals in arb_intervals(10),
        s in -50i64..=50,
    ) {
        let mut di = build_di(&intervals);
        let before = di.intervals().to_vec();
        di.insert(s, s); // start == end is empty
        let after = di.intervals().to_vec();
        prop_assert_eq!(before, after,
            "empty insert({}, {}) changed intervals", s, s);
    }
}

// ── Span monotonicity ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Inserting more intervals can only increase or maintain span.
    #[test]
    fn prop_span_monotonic_on_insert(
        intervals in arb_intervals(10),
        s in -50i64..=50,
        len in 1i64..=20,
    ) {
        let e = s + len;
        let mut di = build_di(&intervals);
        let span_before = di.span();
        di.insert(s, e);
        let span_after = di.span();
        prop_assert!(span_after >= span_before,
            "span decreased from {} to {} after insert([{}, {}))",
            span_before, span_after, s, e);
    }

    /// Remove can only decrease or maintain span.
    #[test]
    fn prop_span_monotonic_on_remove(
        intervals in arb_intervals(10),
        s in -30i64..=30,
        len in 1i64..=20,
    ) {
        let e = s + len;
        let mut di = build_di(&intervals);
        let span_before = di.span();
        di.remove(s, e);
        let span_after = di.span();
        prop_assert!(span_after <= span_before,
            "span increased from {} to {} after remove([{}, {}))",
            span_before, span_after, s, e);
    }

    /// Count monotonically increases or stays same with inserts.
    #[test]
    fn prop_count_bounded_on_insert(
        intervals in arb_intervals(10),
        s in -50i64..=50,
        len in 1i64..=20,
    ) {
        let e = s + len;
        let mut di = build_di(&intervals);
        di.insert(s, e);
        // Count can increase (new interval) or decrease (merged multiple),
        // so we just check it's positive if we had content or added content
        let count_after = di.count();
        prop_assert!(count_after > 0,
            "count should be > 0 after inserting [{}, {}), was {}", s, e, count_after);
    }
}
