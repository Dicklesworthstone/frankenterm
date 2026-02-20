//! Property-based tests for `kd_tree` module.
//!
//! Verifies correctness invariants:
//! - Nearest neighbor matches brute-force scan
//! - K-nearest returns k closest points
//! - Range query matches brute-force filtering
//! - Radius query matches brute-force distance check
//! - Build and insert produce consistent results
//! - Serde roundtrip
//! - Dimensional consistency, distance non-negativity
//! - Clone equivalence, single-point edge cases

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

fn brute_range(points: &[(VecPoint, i32)], min_bounds: &[f64], max_bounds: &[f64]) -> Vec<i32> {
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

fn brute_knn_distances(points: &[(VecPoint, i32)], query: &VecPoint, k: usize) -> Vec<f64> {
    let mut dists: Vec<f64> = points.iter().map(|(p, _)| p.dist_sq(query)).collect();
    dists.sort_by(|a, b| a.partial_cmp(b).unwrap());
    dists.truncate(k);
    dists
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

    // ══════════════════════════════════════════════════════════════
    // NEW TESTS (13 additional)
    // ══════════════════════════════════════════════════════════════

    // ── build() and sequential insert() produce same range query ─

    #[test]
    fn build_vs_insert_range_query(
        items in points_with_values(25),
        (cx, cy) in (-50.0f64..50.0, -50.0f64..50.0),
        (hw, hh) in (1.0f64..30.0, 1.0f64..30.0)
    ) {
        let built = KdTree::build(items.clone(), 2);

        let mut inserted: KdTree<VecPoint, i32> = KdTree::new(2);
        for (p, v) in &items {
            inserted.insert(p.clone(), *v);
        }

        let min_b = [cx - hw, cy - hh];
        let max_b = [cx + hw, cy + hh];

        let mut built_vals: Vec<i32> = built.range_query(&min_b, &max_b)
            .iter()
            .map(|(_, v)| **v)
            .collect();
        built_vals.sort();

        let mut insert_vals: Vec<i32> = inserted.range_query(&min_b, &max_b)
            .iter()
            .map(|(_, v)| **v)
            .collect();
        insert_vals.sort();

        prop_assert_eq!(built_vals, insert_vals);
    }

    // ── dims() matches construction parameter ────────────────────

    #[test]
    fn dims_preserved(dims in 1usize..8) {
        let tree: KdTree<VecPoint, i32> = KdTree::new(dims);
        prop_assert_eq!(tree.dims(), dims, "dims mismatch: expected {}", dims);
    }

    // ── Nearest distance is always non-negative ──────────────────

    #[test]
    fn nearest_distance_non_negative(
        items in points_with_values(30),
        query in point2d_strategy()
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let (_, _, dist_sq) = tree.nearest(&query).unwrap();
        prop_assert!(
            dist_sq >= 0.0,
            "nearest dist_sq is negative: {}",
            dist_sq
        );
    }

    // ── k_nearest with k >= len returns all points ───────────────

    #[test]
    fn k_nearest_all_returns_all(
        items in points_with_values(20),
        query in point2d_strategy()
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let k = items.len() + 10; // deliberately larger than tree size
        let results = tree.k_nearest(&query, k);
        prop_assert_eq!(
            results.len(), items.len(),
            "expected {} results but got {}",
            items.len(), results.len()
        );
    }

    // ── Range query with huge bounds returns all points ───────────

    #[test]
    fn range_query_full_space(items in points_with_values(30)) {
        let tree = KdTree::build(items.clone(), 2);
        let min_b = [-1e18, -1e18];
        let max_b = [1e18, 1e18];
        let results = tree.range_query(&min_b, &max_b);
        prop_assert_eq!(
            results.len(), items.len(),
            "full-space range should return all {} points but got {}",
            items.len(), results.len()
        );
    }

    // ── Range query with tiny bounds far away returns empty ───────

    #[test]
    fn range_query_empty(items in points_with_values(20)) {
        // All points are in [-100, 100], so bounds at 1e6 should find nothing
        let tree = KdTree::build(items.clone(), 2);
        let min_b = [1e6, 1e6];
        let max_b = [1e6 + 0.001, 1e6 + 0.001];
        let results = tree.range_query(&min_b, &max_b);
        prop_assert!(
            results.is_empty(),
            "expected empty range query but got {} results",
            results.len()
        );
    }

    // ── All radius query results have dist_sq <= radius^2 ────────

    #[test]
    fn radius_results_within_radius(
        items in points_with_values(30),
        query in point2d_strategy(),
        radius in 0.1f64..50.0
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let results = tree.radius_query(&query, radius);
        let r_sq = radius * radius;
        for (_, _, dist_sq) in &results {
            prop_assert!(
                *dist_sq <= r_sq + 1e-10,
                "radius result dist_sq {} exceeds radius^2 {}",
                dist_sq, r_sq
            );
        }
    }

    // ── KNN distances match brute-force sorted distances ─────────

    #[test]
    fn k_nearest_distances_match_brute(
        items in points_with_values(30),
        query in point2d_strategy(),
        k in 1usize..10
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let results = tree.k_nearest(&query, k);
        let tree_dists: Vec<f64> = results.iter().map(|(_, _, d)| *d).collect();
        let brute_dists = brute_knn_distances(&items, &query, k);

        prop_assert_eq!(
            tree_dists.len(), brute_dists.len(),
            "knn length mismatch: tree={}, brute={}",
            tree_dists.len(), brute_dists.len()
        );
        for i in 0..tree_dists.len() {
            prop_assert!(
                (tree_dists[i] - brute_dists[i]).abs() < 1e-6,
                "knn distance mismatch at index {}: tree={}, brute={}",
                i, tree_dists[i], brute_dists[i]
            );
        }
    }

    // ── Nearest result point exists in tree.points() ─────────────

    #[test]
    fn nearest_is_in_points(
        items in points_with_values(20),
        query in point2d_strategy()
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let (nearest_pt, _, _) = tree.nearest(&query).unwrap();
        let all_points = tree.points();
        let found = all_points.iter().any(|(p, _)| {
            p.dist_sq(nearest_pt) < 1e-10
        });
        prop_assert!(found, "nearest point not found in tree.points()");
    }

    // ── Serde roundtrip preserves nearest query results ──────────

    #[test]
    fn serde_preserves_nearest(
        items in points_with_values(20),
        query in point2d_strategy()
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let json = serde_json::to_string(&tree).unwrap();
        let restored: KdTree<VecPoint, i32> = serde_json::from_str(&json).unwrap();

        let (_, _, orig_dist) = tree.nearest(&query).unwrap();
        let (_, _, rest_dist) = restored.nearest(&query).unwrap();

        prop_assert!(
            (orig_dist - rest_dist).abs() < 1e-10,
            "serde roundtrip changed nearest: orig={}, restored={}",
            orig_dist, rest_dist
        );
    }

    // ── Each insert increments len by 1 ──────────────────────────

    #[test]
    fn insert_increments_length(items in points_with_values(30)) {
        let mut tree: KdTree<VecPoint, i32> = KdTree::new(2);
        for (i, (p, v)) in items.iter().enumerate() {
            prop_assert_eq!(
                tree.len(), i,
                "before insert {}: expected len={}, got={}",
                i, i, tree.len()
            );
            tree.insert(p.clone(), *v);
            prop_assert_eq!(
                tree.len(), i + 1,
                "after insert {}: expected len={}, got={}",
                i, i + 1, tree.len()
            );
        }
    }

    // ── Single-point tree: nearest and k_nearest return it ───────

    #[test]
    fn single_point_tree(
        point in point2d_strategy(),
        val in any::<i32>(),
        query in point2d_strategy()
    ) {
        let tree = KdTree::build(vec![(point.clone(), val)], 2);

        // nearest returns the single point
        let (p, v, dist_sq) = tree.nearest(&query).unwrap();
        prop_assert_eq!(*v, val);
        let expected_dist = point.dist_sq(&query);
        prop_assert!(
            (dist_sq - expected_dist).abs() < 1e-10,
            "single point nearest dist mismatch: got={}, expected={}",
            dist_sq, expected_dist
        );
        prop_assert!(
            p.dist_sq(&point) < 1e-10,
            "single point nearest returned wrong point"
        );

        // k_nearest returns exactly 1 result
        let knn = tree.k_nearest(&query, 5);
        prop_assert_eq!(knn.len(), 1, "single point k_nearest should return 1");

        prop_assert_eq!(tree.len(), 1);
        let is_not_empty = !tree.is_empty();
        prop_assert!(is_not_empty, "single point tree should not be empty");
    }

    // ── Cloned tree produces same nearest and range query results ─

    #[test]
    fn clone_equivalence(
        items in points_with_values(25),
        query in point2d_strategy(),
        (cx, cy) in (-50.0f64..50.0, -50.0f64..50.0),
        (hw, hh) in (1.0f64..30.0, 1.0f64..30.0)
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let cloned = tree.clone();

        // Nearest should match
        let (_, _, orig_dist) = tree.nearest(&query).unwrap();
        let (_, _, clone_dist) = cloned.nearest(&query).unwrap();
        prop_assert!(
            (orig_dist - clone_dist).abs() < 1e-10,
            "clone nearest mismatch: orig={}, clone={}",
            orig_dist, clone_dist
        );

        // Range query should match
        let min_b = [cx - hw, cy - hh];
        let max_b = [cx + hw, cy + hh];

        let mut orig_vals: Vec<i32> = tree.range_query(&min_b, &max_b)
            .iter()
            .map(|(_, v)| **v)
            .collect();
        orig_vals.sort();

        let mut clone_vals: Vec<i32> = cloned.range_query(&min_b, &max_b)
            .iter()
            .map(|(_, v)| **v)
            .collect();
        clone_vals.sort();

        prop_assert_eq!(orig_vals, clone_vals);

        // Structural properties should match
        prop_assert_eq!(tree.len(), cloned.len());
        prop_assert_eq!(tree.dims(), cloned.dims());
    }

    // ══════════════════════════════════════════════════════════════
    // NEW TESTS (6 additional — properties 26–31)
    // ══════════════════════════════════════════════════════════════

    // ── is_empty agrees with len ─────────────────────────────────

    #[test]
    fn is_empty_agrees_with_len(items in points_with_values(30)) {
        let tree = KdTree::build(items.clone(), 2);
        let len = tree.len();
        let empty = tree.is_empty();
        prop_assert_eq!(empty, len == 0, "is_empty={} but len={}", empty, len);

        // Also verify a fresh empty tree
        let empty_tree: KdTree<VecPoint, i32> = KdTree::new(2);
        let e_len = empty_tree.len();
        let e_empty = empty_tree.is_empty();
        prop_assert!(e_empty, "new tree should be empty");
        prop_assert_eq!(e_len, 0, "new tree len should be 0");
    }

    // ── Clone independence: mutating clone does not affect original ─

    #[test]
    fn clone_independence_insert(
        items in points_with_values(20),
        extra in point2d_strategy(),
        extra_val in any::<i32>()
    ) {
        let tree = KdTree::build(items.clone(), 2);
        let orig_len = tree.len();
        let mut cloned = tree.clone();
        cloned.insert(extra, extra_val);

        // Original unchanged
        prop_assert_eq!(tree.len(), orig_len, "original len changed after clone mutation");
        // Clone grew
        prop_assert_eq!(cloned.len(), orig_len + 1, "cloned len should be original+1");
    }

    // ── Debug format is non-empty and contains "KdTree" ──────────

    #[test]
    fn debug_format_nonempty(items in points_with_values(10)) {
        let tree = KdTree::build(items.clone(), 2);
        let debug_str = format!("{:?}", tree);
        let has_kdtree = debug_str.contains("KdTree");
        prop_assert!(has_kdtree, "Debug output should contain 'KdTree'");
        let is_nonempty = !debug_str.is_empty();
        prop_assert!(is_nonempty, "Debug output should not be empty");
    }

    // ── Display format matches "KdTree(N points, 2D)" pattern ────

    #[test]
    fn display_format_pattern(items in points_with_values(30)) {
        let tree = KdTree::build(items.clone(), 2);
        let display_str = format!("{}", tree);
        let expected = format!("KdTree({} points, 2D)", items.len());
        prop_assert_eq!(display_str, expected);
    }

    // ── dist_sq is symmetric: d(a,b) == d(b,a) ──────────────────

    #[test]
    fn dist_sq_symmetric(
        a in point2d_strategy(),
        b in point2d_strategy()
    ) {
        let ab = a.dist_sq(&b);
        let ba = b.dist_sq(&a);
        prop_assert!(
            (ab - ba).abs() < 1e-10,
            "dist_sq not symmetric: d(a,b)={}, d(b,a)={}",
            ab, ba
        );
    }

    // ── dist_sq triangle inequality: sqrt(d(a,c)) <= sqrt(d(a,b)) + sqrt(d(b,c)) ─

    #[test]
    fn dist_sq_triangle_inequality(
        a in point2d_strategy(),
        b in point2d_strategy(),
        c in point2d_strategy()
    ) {
        let d_ab = a.dist_sq(&b).sqrt();
        let d_bc = b.dist_sq(&c).sqrt();
        let d_ac = a.dist_sq(&c).sqrt();
        prop_assert!(
            d_ac <= d_ab + d_bc + 1e-10,
            "triangle inequality violated: d(a,c)={} > d(a,b)+d(b,c)={}",
            d_ac, d_ab + d_bc
        );
    }
}
