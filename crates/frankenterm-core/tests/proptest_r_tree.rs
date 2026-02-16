//! Property-based tests for `r_tree` module.
//!
//! Verifies correctness invariants:
//! - Point query returns all containing rectangles
//! - Range query returns all overlapping rectangles
//! - No false negatives (brute-force comparison)
//! - Nearest neighbor correctness
//! - Entry count consistency
//! - Serde roundtrip

use frankenterm_core::r_tree::{RTree, Rect};
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn rect_strategy() -> impl Strategy<Value = Rect> {
    (
        -100.0f64..100.0,
        -100.0f64..100.0,
        1.0f64..50.0,
        1.0f64..50.0,
    )
        .prop_map(|(x, y, w, h)| Rect::new(x, y, x + w, y + h))
}

fn point_strategy() -> impl Strategy<Value = (f64, f64)> {
    (-150.0f64..150.0, -150.0f64..150.0)
}

fn rects_with_values(max_len: usize) -> impl Strategy<Value = Vec<(Rect, i32)>> {
    prop::collection::vec((rect_strategy(), any::<i32>()), 1..max_len)
}

// ── Brute-force reference ──────────────────────────────────────────────

fn brute_point_query(entries: &[(Rect, i32)], x: f64, y: f64) -> Vec<i32> {
    entries
        .iter()
        .filter(|(r, _)| r.contains_point(x, y))
        .map(|(_, v)| *v)
        .collect()
}

fn brute_range_query(entries: &[(Rect, i32)], query: &Rect) -> Vec<i32> {
    entries
        .iter()
        .filter(|(r, _)| r.overlaps(query))
        .map(|(_, v)| *v)
        .collect()
}

fn brute_nearest(entries: &[(Rect, i32)], x: f64, y: f64) -> Option<(i32, f64)> {
    entries
        .iter()
        .map(|(r, v)| (*v, r.min_distance(x, y)))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── Point query matches brute force ──────────────────────────

    #[test]
    fn point_query_matches(
        entries in rects_with_values(30),
        (px, py) in point_strategy()
    ) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        let mut tree_results: Vec<i32> = tree.query_point(px, py)
            .iter()
            .map(|(_, v)| **v)
            .collect();
        let mut expected = brute_point_query(&entries, px, py);

        tree_results.sort();
        expected.sort();
        prop_assert_eq!(tree_results, expected);
    }

    // ── Range query matches brute force ──────────────────────────

    #[test]
    fn range_query_matches(
        entries in rects_with_values(30),
        query_rect in rect_strategy()
    ) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        let mut tree_results: Vec<i32> = tree.query(&query_rect)
            .iter()
            .map(|(_, v)| **v)
            .collect();
        let mut expected = brute_range_query(&entries, &query_rect);

        tree_results.sort();
        expected.sort();
        prop_assert_eq!(tree_results, expected);
    }

    // ── Nearest matches brute force ──────────────────────────────

    #[test]
    fn nearest_matches(
        entries in rects_with_values(30),
        (px, py) in point_strategy()
    ) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        let tree_nearest = tree.nearest(px, py);
        let brute = brute_nearest(&entries, px, py);

        match (tree_nearest, brute) {
            (Some((_, _, tree_dist)), Some((_, brute_dist))) => {
                prop_assert!(
                    (tree_dist - brute_dist).abs() < 1e-6,
                    "nearest distance mismatch: tree={}, brute={}",
                    tree_dist, brute_dist
                );
            }
            (None, None) => {}
            _ => prop_assert!(false, "nearest presence mismatch"),
        }
    }

    // ── Length matches insertion count ────────────────────────────

    #[test]
    fn length_matches(entries in rects_with_values(50)) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        prop_assert_eq!(tree.len(), entries.len());
    }

    // ── Entries returns all ──────────────────────────────────────

    #[test]
    fn entries_returns_all(entries in rects_with_values(30)) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        let tree_entries = tree.entries();
        prop_assert_eq!(tree_entries.len(), entries.len());

        let mut tree_vals: Vec<i32> = tree_entries.iter().map(|(_, v)| **v).collect();
        let mut expected: Vec<i32> = entries.iter().map(|(_, v)| *v).collect();
        tree_vals.sort();
        expected.sort();
        prop_assert_eq!(tree_vals, expected);
    }

    // ── Query full space returns all ─────────────────────────────

    #[test]
    fn query_full_space(entries in rects_with_values(30)) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        let huge_query = Rect::new(-1e6, -1e6, 1e6, 1e6);
        let results = tree.query(&huge_query);
        prop_assert_eq!(results.len(), entries.len());
    }

    // ── Serde roundtrip ──────────────────────────────────────────

    #[test]
    fn serde_roundtrip(entries in rects_with_values(30)) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        let json = serde_json::to_string(&tree).unwrap();
        let restored: RTree<i32> = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.len(), tree.len());
    }

    // ── Empty tree queries ───────────────────────────────────────

    #[test]
    fn empty_tree_queries((px, py) in point_strategy()) {
        let tree: RTree<i32> = RTree::new();
        prop_assert!(tree.query_point(px, py).is_empty());
        prop_assert!(tree.nearest(px, py).is_none());
        prop_assert!(tree.is_empty());
    }

    // ── Rect geometry ────────────────────────────────────────────

    #[test]
    fn rect_union_contains_both(a in rect_strategy(), b in rect_strategy()) {
        let u = a.union(&b);
        // Union should contain all corners of both rectangles
        prop_assert!(u.contains_point(a.x_min, a.y_min));
        prop_assert!(u.contains_point(a.x_max, a.y_max));
        prop_assert!(u.contains_point(b.x_min, b.y_min));
        prop_assert!(u.contains_point(b.x_max, b.y_max));
    }

    #[test]
    fn rect_area_non_negative(r in rect_strategy()) {
        prop_assert!(r.area() >= 0.0);
    }

    #[test]
    fn rect_self_overlap(r in rect_strategy()) {
        prop_assert!(r.overlaps(&r));
    }

    #[test]
    fn rect_min_distance_inside(r in rect_strategy()) {
        let cx = (r.x_min + r.x_max) / 2.0;
        let cy = (r.y_min + r.y_max) / 2.0;
        prop_assert!((r.min_distance(cx, cy) - 0.0).abs() < 1e-10);
    }
}
