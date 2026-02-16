//! Property-based tests for `r_tree` module.
//!
//! Verifies correctness invariants:
//! - Point query returns all containing rectangles
//! - Range query returns all overlapping rectangles
//! - No false negatives (brute-force comparison)
//! - Nearest neighbor correctness
//! - Entry count consistency
//! - Serde roundtrip
//! - Incremental length tracking
//! - Geometry properties: union, enlargement, overlap symmetry
//! - Query-subset-of-entries invariant
//! - Clone equivalence

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

    // ══════════════════════════════════════════════════════════════
    // NEW TESTS (13 additional properties)
    // ══════════════════════════════════════════════════════════════

    // ── 1. insert_increments_len ─────────────────────────────────

    #[test]
    fn insert_increments_len(entries in rects_with_values(50)) {
        let mut tree = RTree::new();
        for (i, &(rect, val)) in entries.iter().enumerate() {
            prop_assert_eq!(tree.len(), i, "len before insert {} should be {}", i, i);
            tree.insert(rect, val);
            prop_assert_eq!(
                tree.len(), i + 1,
                "len after insert {} should be {}", i, i + 1
            );
        }
    }

    // ── 2. serde_roundtrip_queries_preserved ─────────────────────

    #[test]
    fn serde_roundtrip_queries_preserved(
        entries in rects_with_values(30),
        points in prop::collection::vec(point_strategy(), 1..10)
    ) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        let json = serde_json::to_string(&tree).unwrap();
        let restored: RTree<i32> = serde_json::from_str(&json).unwrap();

        for &(px, py) in &points {
            let mut orig: Vec<i32> = tree.query_point(px, py)
                .iter()
                .map(|(_, v)| **v)
                .collect();
            let mut rest: Vec<i32> = restored.query_point(px, py)
                .iter()
                .map(|(_, v)| **v)
                .collect();

            orig.sort();
            rest.sort();
            prop_assert_eq!(
                orig, rest,
                "point query mismatch at ({}, {}) after serde roundtrip",
                px, py
            );
        }
    }

    // ── 3. point_inside_rect_found ───────────────────────────────

    #[test]
    fn point_inside_rect_found(
        rect in rect_strategy(),
        val in any::<i32>(),
        t_x in 0.0f64..1.0,
        t_y in 0.0f64..1.0
    ) {
        // Interpolate a point guaranteed to be inside the rect
        let px = rect.x_min + t_x * (rect.x_max - rect.x_min);
        let py = rect.y_min + t_y * (rect.y_max - rect.y_min);

        let mut tree = RTree::new();
        tree.insert(rect, val);

        let results: Vec<i32> = tree.query_point(px, py)
            .iter()
            .map(|(_, v)| **v)
            .collect();

        prop_assert!(
            results.contains(&val),
            "point ({}, {}) inside rect {:?} not found in query results",
            px, py, rect
        );
    }

    // ── 4. disjoint_rects_query ──────────────────────────────────

    #[test]
    fn disjoint_rects_query(
        x1 in -100.0f64..0.0,
        y1 in -100.0f64..0.0,
        w1 in 1.0f64..40.0,
        h1 in 1.0f64..40.0,
        x2 in 200.0f64..300.0,
        y2 in 200.0f64..300.0,
        w2 in 1.0f64..40.0,
        h2 in 1.0f64..40.0,
    ) {
        let r1 = Rect::new(x1, y1, x1 + w1, y1 + h1);
        let r2 = Rect::new(x2, y2, x2 + w2, y2 + h2);

        // r1 is fully in negative quadrant (max at ~40), r2 starts at 200+
        // They can never overlap.
        let mut tree = RTree::new();
        tree.insert(r1, 1);
        tree.insert(r2, 2);

        let results_r1: Vec<i32> = tree.query(&r1)
            .iter()
            .map(|(_, v)| **v)
            .collect();
        let results_r2: Vec<i32> = tree.query(&r2)
            .iter()
            .map(|(_, v)| **v)
            .collect();

        prop_assert!(
            !results_r1.contains(&2),
            "query for r1 should not return r2's value"
        );
        prop_assert!(
            !results_r2.contains(&1),
            "query for r2 should not return r1's value"
        );
    }

    // ── 5. nearest_distance_non_negative ─────────────────────────

    #[test]
    fn nearest_distance_non_negative(
        entries in rects_with_values(30),
        (px, py) in point_strategy()
    ) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        if let Some((_, _, dist)) = tree.nearest(px, py) {
            prop_assert!(
                dist >= 0.0,
                "nearest distance should be non-negative, got {}",
                dist
            );
        }
    }

    // ── 6. nearest_to_centroid_is_zero ───────────────────────────

    #[test]
    fn nearest_to_centroid_is_zero(
        rect in rect_strategy(),
        val in any::<i32>()
    ) {
        let cx = (rect.x_min + rect.x_max) / 2.0;
        let cy = (rect.y_min + rect.y_max) / 2.0;

        let mut tree = RTree::new();
        tree.insert(rect, val);

        let (_, _, dist) = tree.nearest(cx, cy).unwrap();
        prop_assert!(
            dist.abs() < 1e-10,
            "nearest distance to centroid should be ~0, got {}",
            dist
        );
    }

    // ── 7. rect_union_area_geq_parts ─────────────────────────────

    #[test]
    fn rect_union_area_geq_parts(a in rect_strategy(), b in rect_strategy()) {
        let union_area = a.union(&b).area();
        let max_part = a.area().max(b.area());
        prop_assert!(
            union_area >= max_part - 1e-10,
            "union area {} should be >= max(area_a, area_b) = {}",
            union_area, max_part
        );
    }

    // ── 8. rect_enlargement_non_negative ─────────────────────────

    #[test]
    fn rect_enlargement_non_negative(a in rect_strategy(), b in rect_strategy()) {
        let enlargement = a.enlargement(&b);
        prop_assert!(
            enlargement >= -1e-10,
            "enlargement should be non-negative, got {}",
            enlargement
        );
    }

    // ── 9. rect_overlap_symmetry ─────────────────────────────────

    #[test]
    fn rect_overlap_symmetry(a in rect_strategy(), b in rect_strategy()) {
        let ab = a.overlaps(&b);
        let ba = b.overlaps(&a);
        prop_assert_eq!(
            ab, ba,
            "a.overlaps(b)={} but b.overlaps(a)={}", ab, ba
        );
    }

    // ── 10. rect_contains_point_consistency ──────────────────────

    #[test]
    fn rect_contains_point_consistency(
        entries in rects_with_values(30),
        (px, py) in point_strategy()
    ) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        let tree_results: Vec<i32> = tree.query_point(px, py)
            .iter()
            .map(|(_, v)| **v)
            .collect();

        // For every entry whose rect contains the point, it must appear in results
        for &(rect, val) in &entries {
            if rect.contains_point(px, py) {
                prop_assert!(
                    tree_results.contains(&val),
                    "rect {:?} contains ({}, {}) but value {} not in query results",
                    rect, px, py, val
                );
            }
        }
    }

    // ── 11. query_subset_of_entries ──────────────────────────────

    #[test]
    fn query_subset_of_entries(
        entries in rects_with_values(30),
        query_rect in rect_strategy()
    ) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        let all_vals: Vec<i32> = tree.entries()
            .iter()
            .map(|(_, v)| **v)
            .collect();

        let query_vals: Vec<i32> = tree.query(&query_rect)
            .iter()
            .map(|(_, v)| **v)
            .collect();

        for qv in &query_vals {
            prop_assert!(
                all_vals.contains(qv),
                "query result value {} is not in entries()",
                qv
            );
        }
        prop_assert!(
            query_vals.len() <= all_vals.len(),
            "query returned {} results but tree only has {} entries",
            query_vals.len(), all_vals.len()
        );
    }

    // ── 12. nearest_is_closest ──────────────────────────────────

    #[test]
    fn nearest_is_closest(
        entries in rects_with_values(30),
        (px, py) in point_strategy()
    ) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        if let Some((_, _, nearest_dist)) = tree.nearest(px, py) {
            // Every other entry's distance must be >= nearest_dist
            for (rect, _) in tree.entries() {
                let d = rect.min_distance(px, py);
                prop_assert!(
                    d >= nearest_dist - 1e-6,
                    "entry at {:?} has distance {} < nearest distance {}",
                    rect, d, nearest_dist
                );
            }
        }
    }

    // ── 13. clone_equivalence ────────────────────────────────────

    #[test]
    fn clone_equivalence(
        entries in rects_with_values(30),
        points in prop::collection::vec(point_strategy(), 1..10)
    ) {
        let mut tree = RTree::new();
        for &(rect, val) in &entries {
            tree.insert(rect, val);
        }

        let cloned = tree.clone();

        prop_assert_eq!(tree.len(), cloned.len());
        prop_assert_eq!(tree.is_empty(), cloned.is_empty());

        for &(px, py) in &points {
            let mut orig: Vec<i32> = tree.query_point(px, py)
                .iter()
                .map(|(_, v)| **v)
                .collect();
            let mut cl: Vec<i32> = cloned.query_point(px, py)
                .iter()
                .map(|(_, v)| **v)
                .collect();

            orig.sort();
            cl.sort();
            prop_assert_eq!(
                orig, cl,
                "cloned tree point query mismatch at ({}, {})",
                px, py
            );
        }

        // Also check nearest for each point
        for &(px, py) in &points {
            let orig_near = tree.nearest(px, py);
            let cl_near = cloned.nearest(px, py);

            match (orig_near, cl_near) {
                (Some((_, _, d1)), Some((_, _, d2))) => {
                    prop_assert!(
                        (d1 - d2).abs() < 1e-10,
                        "cloned nearest distance mismatch: {} vs {}",
                        d1, d2
                    );
                }
                (None, None) => {}
                _ => prop_assert!(false, "cloned nearest presence mismatch"),
            }
        }
    }
}
