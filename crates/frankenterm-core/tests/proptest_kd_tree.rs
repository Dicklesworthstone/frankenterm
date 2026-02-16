//! Property-based tests for `kd_tree` module.
//!
//! Verifies correctness invariants:
//! - Nearest neighbor matches brute-force scan
//! - K-nearest returns k closest points
//! - Range query matches brute-force filtering
//! - Radius query matches brute-force distance check
//! - Build and insert produce consistent results
//! - Serde roundtrip

use frankenterm_core::kd_tree::{KdTree, Point, VecPoint};
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn point2d_strategy() -> impl Strategy<Value = VecPoint> {
    (-100.0f64..100.0, -100.0f64..100.0).prop_map(|(x, y)| VecPoint::new2d(x, y))
}

fn points_with_values(max_len: usize) -> impl Strategy<Value = Vec<(VecPoint, i32)>> {
    prop::collection::vec((point2d_strategy(), any::<i32>()), 1..max_len)
}

// ── Brute-force reference ────────────────────────────────────────────

fn brute_nearest(points: &[(VecPoint, i32)], query: &VecPoint) -> (usize, f64) {
    points
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (i, p.dist_sq(query)))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .unwrap()
}

fn brute_range(
    points: &[(VecPoint, i32)],
    min_bounds: &[f64],
    max_bounds: &[f64],
) -> Vec<i32> {
    points
        .iter()
        .filter(|(p, _)| {
            (0..p.dims()).all(|d| {
                let c = p.coord(d);
                c >= min_bounds[d] && c <= max_bounds[d]
            })
        })
        .map(|(_, v)| *v)
        .collect()
}

fn brute_radius(points: &[(VecPoint, i32)], query: &VecPoint, radius: f64) -> Vec<i32> {
    let r_sq = radius * radius;
    points
        .iter()
        .filter(|(p, _)| p.dist_sq(query) <= r_sq)
        .map(|(_, v)| *v)
        .collect()
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── Nearest matches brute force ──────────────────────────────

    #[test]
    fn nearest_matches_brute(
        items in points_with_values(30),
        query in point2d_strategy()
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let (_, _, tree_dist_sq) = tree.nearest(&query).unwrap();
        let (_, brute_dist_sq) = brute_nearest(&items, &query);

        prop_assert!(
            (tree_dist_sq - brute_dist_sq).abs() < 1e-6,
            "nearest mismatch: tree={}, brute={}",
            tree_dist_sq, brute_dist_sq
        );
    }

    // ── K-nearest returns correct count ──────────────────────────

    #[test]
    fn k_nearest_count(
        items in points_with_values(30),
        k in 1usize..10
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let results = tree.k_nearest(&VecPoint::new2d(0.0, 0.0), k);
        let expected = k.min(items.len());
        prop_assert_eq!(results.len(), expected);
    }

    // ── K-nearest distances are sorted ───────────────────────────

    #[test]
    fn k_nearest_sorted(
        items in points_with_values(30),
        query in point2d_strategy()
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let results = tree.k_nearest(&query, 5);

        for i in 1..results.len() {
            let (_, _, d_prev) = results[i - 1];
            let (_, _, d_curr) = results[i];
            prop_assert!(d_prev <= d_curr + 1e-10, "knn not sorted");
        }
    }

    // ── K-nearest includes the true nearest ──────────────────────

    #[test]
    fn k_nearest_includes_nearest(
        items in points_with_values(30),
        query in point2d_strategy()
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let (_, _, nearest_dist) = tree.nearest(&query).unwrap();
        let knn = tree.k_nearest(&query, 3);

        prop_assert!(!knn.is_empty());
        let (_, _, knn_closest) = knn[0];
        prop_assert!(
            (knn_closest - nearest_dist).abs() < 1e-6,
            "knn[0] doesn't match nearest"
        );
    }

    // ── Range query matches brute force ──────────────────────────

    #[test]
    fn range_query_matches(
        items in points_with_values(30),
        (cx, cy) in (-50.0f64..50.0, -50.0f64..50.0),
        (hw, hh) in (1.0f64..30.0, 1.0f64..30.0)
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let min_b = [cx - hw, cy - hh];
        let max_b = [cx + hw, cy + hh];

        let tree_results: Vec<i32> = {
            let mut r: Vec<i32> = tree.range_query(&min_b, &max_b)
                .iter()
                .map(|(_, v)| **v)
                .collect();
            r.sort();
            r
        };

        let mut expected = brute_range(&items, &min_b, &max_b);
        expected.sort();

        prop_assert_eq!(tree_results, expected);
    }

    // ── Radius query matches brute force ─────────────────────────

    #[test]
    fn radius_query_matches(
        items in points_with_values(30),
        query in point2d_strategy(),
        radius in 1.0f64..50.0
    ) {
        let tree = KdTree::build(items.clone(), 2);

        let mut tree_results: Vec<i32> = tree.radius_query(&query, radius)
            .iter()
            .map(|(_, v, _)| **v)
            .collect();
        tree_results.sort();

        let mut expected = brute_radius(&items, &query, radius);
        expected.sort();

        prop_assert_eq!(tree_results, expected);
    }

    // ── Length matches ────────────────────────────────────────────

    #[test]
    fn length_matches(items in points_with_values(50)) {
        let tree = KdTree::build(items.clone(), 2);
        prop_assert_eq!(tree.len(), items.len());
    }

    // ── Points returns all ───────────────────────────────────────

    #[test]
    fn points_returns_all(items in points_with_values(30)) {
        let tree = KdTree::build(items.clone(), 2);
        let points = tree.points();
        prop_assert_eq!(points.len(), items.len());
    }

    // ── Serde roundtrip ──────────────────────────────────────────

    #[test]
    fn serde_roundtrip(items in points_with_values(20)) {
        let tree = KdTree::build(items.clone(), 2);
        let json = serde_json::to_string(&tree).unwrap();
        let restored: KdTree<VecPoint, i32> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.len(), tree.len());
        prop_assert_eq!(restored.dims(), tree.dims());
    }

    // ── Insert produces same results as build ────────────────────

    #[test]
    fn insert_finds_nearest(
        items in points_with_values(20),
        query in point2d_strategy()
    ) {
        // Build by insertion
        let mut tree: KdTree<VecPoint, i32> = KdTree::new(2);
        for (p, v) in &items {
            tree.insert(p.clone(), *v);
        }

        let (_, _, insert_dist) = tree.nearest(&query).unwrap();
        let (_, brute_dist) = brute_nearest(&items, &query);

        prop_assert!(
            (insert_dist - brute_dist).abs() < 1e-6,
            "insert tree nearest mismatch"
        );
    }

    // ── Empty k-nearest ──────────────────────────────────────────

    #[test]
    fn empty_k_nearest(query in point2d_strategy()) {
        let tree: KdTree<VecPoint, i32> = KdTree::new(2);
        prop_assert!(tree.k_nearest(&query, 5).is_empty());
    }

    // ── Zero radius returns only coincident points ───────────────

    #[test]
    fn zero_radius_only_exact(items in points_with_values(20)) {
        let tree = KdTree::build(items.clone(), 2);
        if let Some((p, _, _)) = tree.nearest(&VecPoint::new2d(0.0, 0.0)) {
            let results = tree.radius_query(p, 0.0);
            // All results should be at distance 0 from the query point
            for (_, _, dist_sq) in &results {
                prop_assert!(*dist_sq < 1e-10, "non-zero distance in zero-radius query");
            }
        }
    }
}
