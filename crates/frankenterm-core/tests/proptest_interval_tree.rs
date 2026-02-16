//! Property-based tests for `interval_tree` module.
//!
//! Verifies correctness invariants of the augmented interval tree using proptest:
//! - Overlap query completeness and soundness
//! - Stabbing query consistency with overlap queries
//! - BST ordering and AVL balance invariants
//! - Max-high augmentation correctness
//! - Serde roundtrip preservation
//! - Insertion order independence for queries
//! - Removal correctness
//! - Iterator ordering

use frankenterm_core::interval_tree::{Interval, IntervalTree};
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn interval_strategy() -> impl Strategy<Value = Interval<i32>> {
    (0..1000i32, 1..100i32).prop_map(|(low, width)| Interval::new(low, low + width))
}

fn intervals_strategy(max_len: usize) -> impl Strategy<Value = Vec<(Interval<i32>, u32)>> {
    prop::collection::vec((interval_strategy(), 0..1000u32), 0..max_len)
}

fn tree_strategy(max_len: usize) -> impl Strategy<Value = IntervalTree<i32, u32>> {
    intervals_strategy(max_len).prop_map(|intervals| {
        let mut tree = IntervalTree::new();
        for (iv, val) in intervals {
            tree.insert(iv, val);
        }
        tree
    })
}

fn point_strategy() -> impl Strategy<Value = i32> {
    0..1100i32
}

// ── Brute-force reference implementation ───────────────────────────────

fn brute_force_overlap(intervals: &[(Interval<i32>, u32)], query: &Interval<i32>) -> Vec<usize> {
    intervals
        .iter()
        .enumerate()
        .filter(|(_, (iv, _))| iv.overlaps(query))
        .map(|(i, _)| i)
        .collect()
}

fn brute_force_point(intervals: &[(Interval<i32>, u32)], point: &i32) -> Vec<usize> {
    intervals
        .iter()
        .enumerate()
        .filter(|(_, (iv, _))| iv.contains_point(point))
        .map(|(i, _)| i)
        .collect()
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // ── Overlap query correctness ──────────────────────────────────

    #[test]
    fn overlap_query_complete(
        intervals in intervals_strategy(50),
        query in interval_strategy()
    ) {
        // Every interval that overlaps the query must appear in results
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        let results = tree.query_overlap(&query);
        let expected = brute_force_overlap(&intervals, &query);

        prop_assert_eq!(
            results.len(), expected.len(),
            "overlap query returned {} results, expected {} for query {:?}",
            results.len(), expected.len(), query
        );
    }

    #[test]
    fn overlap_query_sound(
        intervals in intervals_strategy(50),
        query in interval_strategy()
    ) {
        // Every result from query must actually overlap the query
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        let results = tree.query_overlap(&query);
        for (iv, _) in &results {
            prop_assert!(
                iv.overlaps(&query),
                "result interval {:?} does not overlap query {:?}", iv, query
            );
        }
    }

    // ── Stabbing query correctness ─────────────────────────────────

    #[test]
    fn stabbing_query_complete(
        intervals in intervals_strategy(50),
        point in point_strategy()
    ) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        let results = tree.query_point(&point);
        let expected = brute_force_point(&intervals, &point);

        prop_assert_eq!(
            results.len(), expected.len(),
            "stabbing query at {} returned {} results, expected {}",
            point, results.len(), expected.len()
        );
    }

    #[test]
    fn stabbing_query_sound(
        intervals in intervals_strategy(50),
        point in point_strategy()
    ) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        let results = tree.query_point(&point);
        for (iv, _) in &results {
            prop_assert!(
                iv.contains_point(&point),
                "result interval {:?} does not contain point {}", iv, point
            );
        }
    }

    // ── Stabbing consistent with overlap ───────────────────────────

    #[test]
    fn stabbing_consistent_with_overlap(
        intervals in intervals_strategy(30),
        point in point_strategy()
    ) {
        // query_point(p) should match query_overlap([p, p+1))
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        let stab_results = tree.query_point(&point);
        let overlap_results = tree.query_overlap(&Interval::new(point, point + 1));

        prop_assert_eq!(
            stab_results.len(), overlap_results.len(),
            "stabbing and overlap queries disagree at point {}",
            point
        );
    }

    // ── AVL balance invariant ──────────────────────────────────────

    #[test]
    fn avl_balance_maintained(intervals in intervals_strategy(100)) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        // Height should be O(log n): for AVL, h <= 1.44 * log2(n+2)
        let n = tree.len();
        if n > 0 {
            let max_height = (1.45 * ((n + 2) as f64).log2()) as i32 + 2;
            let h = IntervalTree::height(&tree);
            prop_assert!(
                h <= max_height,
                "tree height {} exceeds AVL bound {} for {} elements", h, max_height, n
            );
        }
    }

    // ── Size tracking ──────────────────────────────────────────────

    #[test]
    fn len_matches_inserts(intervals in intervals_strategy(100)) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }
        prop_assert_eq!(tree.len(), intervals.len());
    }

    // ── Max-high augmentation ──────────────────────────────────────

    #[test]
    fn max_high_is_global_max(intervals in intervals_strategy(50)) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        if let Some(tree_max) = tree.max_high() {
            let brute_max = intervals.iter().map(|(iv, _)| &iv.high).max().unwrap();
            prop_assert_eq!(tree_max, brute_max);
        } else {
            prop_assert!(intervals.is_empty());
        }
    }

    // ── Min-low is leftmost ────────────────────────────────────────

    #[test]
    fn min_low_is_global_min(intervals in intervals_strategy(50)) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        if let Some(tree_min) = tree.min_low() {
            let brute_min = intervals.iter().map(|(iv, _)| &iv.low).min().unwrap();
            prop_assert_eq!(tree_min, brute_min);
        } else {
            prop_assert!(intervals.is_empty());
        }
    }

    // ── Iterator ordering ──────────────────────────────────────────

    #[test]
    fn iterator_sorted_by_low(intervals in intervals_strategy(50)) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        let lows: Vec<i32> = tree.iter().map(|(iv, _)| iv.low).collect();
        for w in lows.windows(2) {
            prop_assert!(
                w[0] <= w[1],
                "iterator not sorted: {} > {}", w[0], w[1]
            );
        }
    }

    #[test]
    fn iterator_count_matches_len(intervals in intervals_strategy(50)) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        let count = tree.iter().count();
        prop_assert_eq!(count, tree.len());
    }

    // ── Insertion order independence ────────────────────────────────

    #[test]
    fn query_results_independent_of_insertion_order(
        intervals in intervals_strategy(30),
        query in interval_strategy()
    ) {
        // Build tree in forward order
        let mut tree_forward = IntervalTree::new();
        for (iv, val) in &intervals {
            tree_forward.insert(iv.clone(), *val);
        }

        // Build tree in reverse order
        let mut tree_reverse = IntervalTree::new();
        for (iv, val) in intervals.iter().rev() {
            tree_reverse.insert(iv.clone(), *val);
        }

        let results_fwd = tree_forward.query_overlap(&query);
        let results_rev = tree_reverse.query_overlap(&query);

        prop_assert_eq!(
            results_fwd.len(), results_rev.len(),
            "different insertion orders give different result counts for query {:?}",
            query
        );
    }

    // ── Serde roundtrip ────────────────────────────────────────────

    #[test]
    fn serde_roundtrip_preserves_data(intervals in intervals_strategy(30)) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        let json = serde_json::to_string(&tree).unwrap();
        let restored: IntervalTree<i32, u32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), tree.len());

        // Verify all intervals present
        for (iv, _) in &intervals {
            let orig = tree.query_overlap(iv);
            let rest = restored.query_overlap(iv);
            prop_assert_eq!(
                orig.len(), rest.len(),
                "serde roundtrip changed overlap results for {:?}", iv
            );
        }
    }

    #[test]
    fn serde_roundtrip_preserves_max_high(intervals in intervals_strategy(30)) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        let json = serde_json::to_string(&tree).unwrap();
        let restored: IntervalTree<i32, u32> = serde_json::from_str(&json).unwrap();

        let orig_max = tree.max_high().cloned();
        let rest_max = restored.max_high().cloned();
        prop_assert_eq!(orig_max, rest_max);
    }

    // ── Removal correctness ────────────────────────────────────────

    #[test]
    fn remove_reduces_len(intervals in intervals_strategy(20)) {
        if intervals.is_empty() {
            return Ok(());
        }

        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        let before = tree.len();
        let target = &intervals[0].0;

        // Count how many copies of this interval exist
        let copies = intervals.iter().filter(|(iv, _)| iv.low == target.low && iv.high == target.high).count();

        let removed = tree.remove(target);
        prop_assert_eq!(removed.len(), copies);
        prop_assert_eq!(tree.len(), before - copies);
    }

    #[test]
    fn remove_preserves_other_intervals(intervals in intervals_strategy(20)) {
        if intervals.len() < 2 {
            return Ok(());
        }

        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        // Remove the first interval
        let target = &intervals[0].0;
        tree.remove(target);

        // Check that non-matching intervals are still findable
        for (iv, _) in intervals.iter().skip(1) {
            if iv.low != target.low || iv.high != target.high {
                let results = tree.query_overlap(iv);
                let has_exact = results.iter().any(|(r, _)| r.low == iv.low && r.high == iv.high);
                prop_assert!(
                    has_exact,
                    "interval {:?} lost after removing {:?}", iv, target
                );
            }
        }
    }

    // ── Empty interval queries ─────────────────────────────────────

    #[test]
    fn empty_interval_query_returns_nothing(intervals in intervals_strategy(30)) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        // [x, x) is empty, should match nothing
        let point = 500i32;
        let results = tree.query_overlap(&Interval::new(point, point));
        prop_assert!(results.is_empty());
    }

    // ── Overlap symmetry ───────────────────────────────────────────

    #[test]
    fn interval_overlap_is_symmetric(
        a in interval_strategy(),
        b in interval_strategy()
    ) {
        prop_assert_eq!(
            a.overlaps(&b), b.overlaps(&a),
            "overlap not symmetric: {:?} vs {:?}", a, b
        );
    }

    // ── Contains-point consistency ─────────────────────────────────

    #[test]
    fn contains_point_matches_overlap_unit(
        iv in interval_strategy(),
        point in point_strategy()
    ) {
        // contains_point(p) ↔ overlaps([p, p+1))
        let unit = Interval::new(point, point + 1);
        prop_assert_eq!(
            iv.contains_point(&point), iv.overlaps(&unit),
            "contains_point and overlap disagree for {:?} at {}", iv, point
        );
    }

    // ── Monotone max-high on insert ────────────────────────────────

    #[test]
    fn max_high_monotone_on_insert(intervals in intervals_strategy(50)) {
        let mut tree = IntervalTree::new();
        let mut prev_max: Option<i32> = None;

        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
            let current_max = tree.max_high().cloned().unwrap();

            if let Some(pm) = prev_max {
                prop_assert!(
                    current_max >= pm,
                    "max_high decreased from {} to {} after insert", pm, current_max
                );
            }
            prev_max = Some(current_max);
        }
    }

    // ── Intervals sorted len consistent ────────────────────────────

    #[test]
    fn intervals_sorted_len_consistent(intervals in intervals_strategy(50)) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        let sorted = tree.intervals_sorted();
        prop_assert_eq!(sorted.len(), tree.len());
    }

    // ── FromIterator consistency ───────────────────────────────────

    #[test]
    fn from_iter_same_as_manual_insert(
        intervals in intervals_strategy(30),
        query in interval_strategy()
    ) {
        let tree_manual: IntervalTree<i32, u32> = {
            let mut t = IntervalTree::new();
            for (iv, val) in &intervals {
                t.insert(iv.clone(), *val);
            }
            t
        };

        let tree_collected: IntervalTree<i32, u32> =
            intervals.iter().cloned().collect();

        let results_manual = tree_manual.query_overlap(&query);
        let results_collected = tree_collected.query_overlap(&query);

        prop_assert_eq!(results_manual.len(), results_collected.len());
    }

    // ── Non-empty tree has min/max ─────────────────────────────────

    #[test]
    fn nonempty_tree_has_min_max(intervals in intervals_strategy(50)) {
        let mut tree = IntervalTree::new();
        for (iv, val) in &intervals {
            tree.insert(iv.clone(), *val);
        }

        if tree.is_empty() {
            prop_assert!(tree.min_low().is_none());
            prop_assert!(tree.max_high().is_none());
        } else {
            prop_assert!(tree.min_low().is_some());
            prop_assert!(tree.max_high().is_some());
        }
    }

    // ── Default is empty ───────────────────────────────────────────

    #[test]
    fn default_tree_is_empty(_x in 0..1i32) {
        let tree: IntervalTree<i32, ()> = IntervalTree::default();
        prop_assert!(tree.is_empty());
        prop_assert_eq!(tree.len(), 0);
    }

    // ── Large tree performance sanity ──────────────────────────────

    #[test]
    fn large_tree_query_returns(
        n in 50..200usize,
        query in interval_strategy()
    ) {
        let mut tree = IntervalTree::new();
        for i in 0..(n as i32) {
            tree.insert(Interval::new(i * 2, i * 2 + 5), i as u32);
        }

        // Just verify it doesn't panic and returns reasonable count
        let results = tree.query_overlap(&query);
        prop_assert!(results.len() <= n);
    }
}
