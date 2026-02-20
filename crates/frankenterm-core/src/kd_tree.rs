//! KD-tree — k-dimensional binary space partitioning for spatial queries.
//!
//! A KD-tree recursively partitions k-dimensional space along alternating
//! axes, enabling efficient nearest neighbor, range, and k-nearest
//! neighbor queries.
//!
//! # Complexity
//!
//! - **O(n log n)**: build from points
//! - **O(log n)** average: nearest neighbor, point query
//! - **O(n^(1-1/k) + m)**: range query returning m matches
//!
//! # Design
//!
//! Arena-allocated balanced tree built by median-of-coordinates partitioning.
//! Supports arbitrary dimensionality via `Point` trait. Includes both
//! exact nearest neighbor with branch pruning and k-nearest neighbor
//! queries using a bounded max-heap.
//!
//! # Use in FrankenTerm
//!
//! Multi-dimensional similarity search on pane feature vectors (output rate,
//! entropy, process type, size), nearest-neighbor classification for
//! anomaly detection, and spatial clustering of terminal metrics.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fmt;

// ── Point trait ───────────────────────────────────────────────────────

/// A point in k-dimensional space.
pub trait Point: Clone {
    /// Returns the number of dimensions.
    fn dims(&self) -> usize;

    /// Returns the coordinate along the given dimension.
    fn coord(&self, dim: usize) -> f64;

    /// Squared Euclidean distance to another point.
    fn dist_sq(&self, other: &Self) -> f64 {
        let d = self.dims();
        (0..d)
            .map(|i| {
                let diff = self.coord(i) - other.coord(i);
                diff * diff
            })
            .sum()
    }
}

/// A fixed-size point represented as a Vec of coordinates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VecPoint {
    pub coords: Vec<f64>,
}

impl VecPoint {
    /// Creates a new point from coordinates.
    pub fn new(coords: Vec<f64>) -> Self {
        Self { coords }
    }

    /// Creates a 2D point.
    pub fn new2d(x: f64, y: f64) -> Self {
        Self { coords: vec![x, y] }
    }

    /// Creates a 3D point.
    pub fn new3d(x: f64, y: f64, z: f64) -> Self {
        Self {
            coords: vec![x, y, z],
        }
    }
}

impl Point for VecPoint {
    fn dims(&self) -> usize {
        self.coords.len()
    }

    fn coord(&self, dim: usize) -> f64 {
        self.coords[dim]
    }
}

// ── KD-tree node ──────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct KdNode<P, V> {
    point: P,
    value: V,
    split_dim: usize,
    left: Option<usize>,
    right: Option<usize>,
}

// ── KdTree ────────────────────────────────────────────────────────────

/// A k-dimensional tree for spatial queries.
///
/// Built from a set of points, supports nearest neighbor, k-nearest,
/// and range queries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KdTree<P, V> {
    nodes: Vec<KdNode<P, V>>,
    root: Option<usize>,
    dims: usize,
}

impl<P: Point, V: Clone> KdTree<P, V> {
    /// Creates an empty KD-tree with the given dimensionality.
    pub fn new(dims: usize) -> Self {
        Self {
            nodes: Vec::new(),
            root: None,
            dims,
        }
    }

    /// Builds a KD-tree from a collection of (point, value) pairs.
    pub fn build(items: Vec<(P, V)>, dims: usize) -> Self {
        if items.is_empty() {
            return Self::new(dims);
        }

        let mut tree = Self {
            nodes: Vec::with_capacity(items.len()),
            root: None,
            dims,
        };

        let mut indexed: Vec<(usize, P, V)> = items
            .into_iter()
            .enumerate()
            .map(|(i, (p, v))| (i, p, v))
            .collect();

        tree.root = Some(tree.build_recursive(&mut indexed, 0));
        tree
    }

    fn build_recursive(&mut self, items: &mut [(usize, P, V)], depth: usize) -> usize {
        let n = items.len();
        if n == 1 {
            let (_, point, value) = items[0].clone();
            let idx = self.nodes.len();
            self.nodes.push(KdNode {
                point,
                value,
                split_dim: depth % self.dims,
                left: None,
                right: None,
            });
            return idx;
        }

        let split_dim = depth % self.dims;

        // Sort by split dimension and pick median
        items.sort_by(|a, b| {
            a.1.coord(split_dim)
                .partial_cmp(&b.1.coord(split_dim))
                .unwrap_or(Ordering::Equal)
        });

        let mid = n / 2;
        let (_, median_point, median_value) = items[mid].clone();

        let idx = self.nodes.len();
        self.nodes.push(KdNode {
            point: median_point,
            value: median_value,
            split_dim,
            left: None,
            right: None,
        });

        if mid > 0 {
            let left = self.build_recursive(&mut items[..mid], depth + 1);
            self.nodes[idx].left = Some(left);
        }

        if mid + 1 < n {
            let right = self.build_recursive(&mut items[mid + 1..], depth + 1);
            self.nodes[idx].right = Some(right);
        }

        idx
    }

    /// Inserts a point-value pair into the tree.
    pub fn insert(&mut self, point: P, value: V) {
        let new_idx = self.nodes.len();
        let split_dim = if let Some(root) = self.root {
            self.insert_at(root, &point, 0)
        } else {
            0
        };

        self.nodes.push(KdNode {
            point,
            value,
            split_dim,
            left: None,
            right: None,
        });

        if self.root.is_none() {
            self.root = Some(new_idx);
            return;
        }

        // Walk tree to find insertion point
        let mut current = self.root.unwrap();
        loop {
            let dim = self.nodes[current].split_dim;
            let go_left =
                self.nodes[new_idx].point.coord(dim) < self.nodes[current].point.coord(dim);

            if go_left {
                match self.nodes[current].left {
                    None => {
                        self.nodes[current].left = Some(new_idx);
                        break;
                    }
                    Some(l) => current = l,
                }
            } else {
                match self.nodes[current].right {
                    None => {
                        self.nodes[current].right = Some(new_idx);
                        break;
                    }
                    Some(r) => current = r,
                }
            }
        }
    }

    /// Returns the split dimension for a point if inserted at the given node.
    fn insert_at(&self, _node: usize, _point: &P, _depth: usize) -> usize {
        // Just compute the depth where this point would land
        let mut current = self.root;
        let mut depth = 0;
        while let Some(idx) = current {
            let dim = self.nodes[idx].split_dim;
            depth += 1;
            if _point.coord(dim) < self.nodes[idx].point.coord(dim) {
                current = self.nodes[idx].left;
            } else {
                current = self.nodes[idx].right;
            }
        }
        depth % self.dims
    }

    /// Finds the nearest neighbor to the query point.
    ///
    /// Returns `Some((point, value, distance_squared))` or None if empty.
    pub fn nearest(&self, query: &P) -> Option<(&P, &V, f64)> {
        let root = self.root?;
        let mut best_dist_sq = f64::INFINITY;
        let mut best_idx = root;

        self.nearest_recursive(root, query, &mut best_dist_sq, &mut best_idx);

        Some((
            &self.nodes[best_idx].point,
            &self.nodes[best_idx].value,
            best_dist_sq,
        ))
    }

    fn nearest_recursive(
        &self,
        node: usize,
        query: &P,
        best_dist_sq: &mut f64,
        best_idx: &mut usize,
    ) {
        let n = &self.nodes[node];
        let dist_sq = n.point.dist_sq(query);
        if dist_sq < *best_dist_sq {
            *best_dist_sq = dist_sq;
            *best_idx = node;
        }

        let dim = n.split_dim;
        let diff = query.coord(dim) - n.point.coord(dim);
        let diff_sq = diff * diff;

        // Visit the closer subtree first
        let (first, second) = if diff < 0.0 {
            (n.left, n.right)
        } else {
            (n.right, n.left)
        };

        if let Some(first_idx) = first {
            self.nearest_recursive(first_idx, query, best_dist_sq, best_idx);
        }

        // Only visit the other subtree if the splitting plane is closer
        // than the current best
        if diff_sq < *best_dist_sq {
            if let Some(second_idx) = second {
                self.nearest_recursive(second_idx, query, best_dist_sq, best_idx);
            }
        }
    }

    /// Finds the k nearest neighbors to the query point.
    ///
    /// Returns results sorted by distance (closest first).
    pub fn k_nearest(&self, query: &P, k: usize) -> Vec<(&P, &V, f64)> {
        if k == 0 || self.root.is_none() {
            return Vec::new();
        }

        let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::new();
        self.knn_recursive(self.root.unwrap(), query, k, &mut heap);

        let results: Vec<(&P, &V, f64)> = heap
            .into_sorted_vec()
            .into_iter()
            .map(|e| {
                (
                    &self.nodes[e.idx].point,
                    &self.nodes[e.idx].value,
                    e.dist_sq,
                )
            })
            .collect();
        // into_sorted_vec returns ascending order = closest first
        results
    }

    fn knn_recursive(&self, node: usize, query: &P, k: usize, heap: &mut BinaryHeap<HeapEntry>) {
        let n = &self.nodes[node];
        let dist_sq = n.point.dist_sq(query);

        if heap.len() < k {
            heap.push(HeapEntry { dist_sq, idx: node });
        } else if let Some(worst) = heap.peek() {
            if dist_sq < worst.dist_sq {
                heap.pop();
                heap.push(HeapEntry { dist_sq, idx: node });
            }
        }

        let dim = n.split_dim;
        let diff = query.coord(dim) - n.point.coord(dim);
        let diff_sq = diff * diff;

        let (first, second) = if diff < 0.0 {
            (n.left, n.right)
        } else {
            (n.right, n.left)
        };

        if let Some(first_idx) = first {
            self.knn_recursive(first_idx, query, k, heap);
        }

        let threshold = if heap.len() < k {
            f64::INFINITY
        } else {
            heap.peek().map(|e| e.dist_sq).unwrap_or(f64::INFINITY)
        };

        if diff_sq < threshold {
            if let Some(second_idx) = second {
                self.knn_recursive(second_idx, query, k, heap);
            }
        }
    }

    /// Range query: finds all points within the given bounding box.
    ///
    /// `min_bounds` and `max_bounds` define an axis-aligned bounding box.
    pub fn range_query(&self, min_bounds: &[f64], max_bounds: &[f64]) -> Vec<(&P, &V)> {
        let mut results = Vec::new();
        if let Some(root) = self.root {
            self.range_recursive(root, min_bounds, max_bounds, &mut results);
        }
        results
    }

    fn range_recursive<'a>(
        &'a self,
        node: usize,
        min_bounds: &[f64],
        max_bounds: &[f64],
        results: &mut Vec<(&'a P, &'a V)>,
    ) {
        let n = &self.nodes[node];

        // Check if this point is within bounds
        let in_range = (0..self.dims).all(|d| {
            let c = n.point.coord(d);
            c >= min_bounds[d] && c <= max_bounds[d]
        });

        if in_range {
            results.push((&n.point, &n.value));
        }

        let dim = n.split_dim;
        let split_val = n.point.coord(dim);

        // Visit left if min_bounds could intersect
        if min_bounds[dim] <= split_val {
            if let Some(left) = n.left {
                self.range_recursive(left, min_bounds, max_bounds, results);
            }
        }

        // Visit right if max_bounds could intersect
        if max_bounds[dim] >= split_val {
            if let Some(right) = n.right {
                self.range_recursive(right, min_bounds, max_bounds, results);
            }
        }
    }

    /// Finds all points within the given radius of the query point.
    pub fn radius_query(&self, query: &P, radius: f64) -> Vec<(&P, &V, f64)> {
        let radius_sq = radius * radius;
        let mut results = Vec::new();
        if let Some(root) = self.root {
            self.radius_recursive(root, query, radius_sq, &mut results);
        }
        results
    }

    fn radius_recursive<'a>(
        &'a self,
        node: usize,
        query: &P,
        radius_sq: f64,
        results: &mut Vec<(&'a P, &'a V, f64)>,
    ) {
        let n = &self.nodes[node];
        let dist_sq = n.point.dist_sq(query);

        if dist_sq <= radius_sq {
            results.push((&n.point, &n.value, dist_sq));
        }

        let dim = n.split_dim;
        let diff = query.coord(dim) - n.point.coord(dim);
        let diff_sq = diff * diff;

        let (first, second) = if diff < 0.0 {
            (n.left, n.right)
        } else {
            (n.right, n.left)
        };

        if let Some(first_idx) = first {
            self.radius_recursive(first_idx, query, radius_sq, results);
        }

        if diff_sq <= radius_sq {
            if let Some(second_idx) = second {
                self.radius_recursive(second_idx, query, radius_sq, results);
            }
        }
    }

    /// Returns the number of points in the tree.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns true if the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Returns the dimensionality.
    pub fn dims(&self) -> usize {
        self.dims
    }

    /// Returns all points in the tree.
    pub fn points(&self) -> Vec<(&P, &V)> {
        self.nodes.iter().map(|n| (&n.point, &n.value)).collect()
    }
}

// ── HeapEntry for KNN ─────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct HeapEntry {
    dist_sq: f64,
    idx: usize,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.dist_sq == other.dist_sq
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist_sq
            .partial_cmp(&other.dist_sq)
            .unwrap_or(Ordering::Equal)
    }
}

impl<P: Point + fmt::Debug, V: Clone> fmt::Display for KdTree<P, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KdTree({} points, {}D)", self.nodes.len(), self.dims)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn pt(x: f64, y: f64) -> VecPoint {
        VecPoint::new2d(x, y)
    }

    #[test]
    fn empty_tree() {
        let tree: KdTree<VecPoint, i32> = KdTree::new(2);
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
        assert_eq!(tree.dims(), 2);
        assert!(tree.nearest(&pt(0.0, 0.0)).is_none());
    }

    #[test]
    fn build_basic() {
        let items = vec![
            (pt(2.0, 3.0), 1),
            (pt(5.0, 4.0), 2),
            (pt(9.0, 6.0), 3),
            (pt(4.0, 7.0), 4),
            (pt(8.0, 1.0), 5),
            (pt(7.0, 2.0), 6),
        ];
        let tree = KdTree::build(items, 2);
        assert_eq!(tree.len(), 6);
        assert!(!tree.is_empty());
    }

    #[test]
    fn nearest_basic() {
        let items = vec![
            (pt(0.0, 0.0), "origin"),
            (pt(1.0, 0.0), "right"),
            (pt(0.0, 1.0), "up"),
            (pt(10.0, 10.0), "far"),
        ];
        let tree = KdTree::build(items, 2);

        let (p, v, dist) = tree.nearest(&pt(0.1, 0.1)).unwrap();
        assert_eq!(*v, "origin");
        assert!(dist < 0.1);
        assert_eq!(p.coords, vec![0.0, 0.0]);
    }

    #[test]
    fn nearest_exact_match() {
        let items = vec![(pt(1.0, 2.0), 1), (pt(3.0, 4.0), 2), (pt(5.0, 6.0), 3)];
        let tree = KdTree::build(items, 2);

        let (_, v, dist) = tree.nearest(&pt(3.0, 4.0)).unwrap();
        assert_eq!(*v, 2);
        assert!(dist < 1e-10);
    }

    #[test]
    fn k_nearest_basic() {
        let items = vec![
            (pt(0.0, 0.0), 1),
            (pt(1.0, 0.0), 2),
            (pt(2.0, 0.0), 3),
            (pt(3.0, 0.0), 4),
            (pt(100.0, 100.0), 5),
        ];
        let tree = KdTree::build(items, 2);

        let results = tree.k_nearest(&pt(0.5, 0.0), 3);
        assert_eq!(results.len(), 3);

        let values: Vec<&i32> = results.iter().map(|(_, v, _)| *v).collect();
        // Closest 3 should be points 1, 2, 3 (within distance 2.5)
        assert!(values.contains(&&1));
        assert!(values.contains(&&2));
        assert!(values.contains(&&3));
    }

    #[test]
    fn k_nearest_more_than_available() {
        let items = vec![(pt(0.0, 0.0), 1), (pt(1.0, 1.0), 2)];
        let tree = KdTree::build(items, 2);
        let results = tree.k_nearest(&pt(0.0, 0.0), 10);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn range_query_basic() {
        let items = vec![
            (pt(1.0, 1.0), 1),
            (pt(2.0, 2.0), 2),
            (pt(3.0, 3.0), 3),
            (pt(10.0, 10.0), 4),
        ];
        let tree = KdTree::build(items, 2);

        let results = tree.range_query(&[0.0, 0.0], &[5.0, 5.0]);
        assert_eq!(results.len(), 3);

        let values: Vec<&i32> = results.iter().map(|(_, v)| *v).collect();
        assert!(values.contains(&&1));
        assert!(values.contains(&&2));
        assert!(values.contains(&&3));
    }

    #[test]
    fn range_query_empty() {
        let items = vec![(pt(1.0, 1.0), 1), (pt(2.0, 2.0), 2)];
        let tree = KdTree::build(items, 2);
        let results = tree.range_query(&[10.0, 10.0], &[20.0, 20.0]);
        assert!(results.is_empty());
    }

    #[test]
    fn radius_query_basic() {
        let items = vec![
            (pt(0.0, 0.0), 1),
            (pt(1.0, 0.0), 2),
            (pt(0.0, 1.0), 3),
            (pt(100.0, 100.0), 4),
        ];
        let tree = KdTree::build(items, 2);

        let results = tree.radius_query(&pt(0.0, 0.0), 1.5);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn insert_single() {
        let mut tree: KdTree<VecPoint, i32> = KdTree::new(2);
        tree.insert(pt(1.0, 2.0), 42);
        assert_eq!(tree.len(), 1);
        let (_, v, _) = tree.nearest(&pt(1.0, 2.0)).unwrap();
        assert_eq!(*v, 42);
    }

    #[test]
    fn insert_multiple() {
        let mut tree: KdTree<VecPoint, i32> = KdTree::new(2);
        tree.insert(pt(5.0, 5.0), 1);
        tree.insert(pt(1.0, 1.0), 2);
        tree.insert(pt(9.0, 9.0), 3);

        assert_eq!(tree.len(), 3);
        let (_, v, _) = tree.nearest(&pt(1.0, 1.0)).unwrap();
        assert_eq!(*v, 2);
    }

    #[test]
    fn three_dimensions() {
        let items = vec![
            (VecPoint::new3d(0.0, 0.0, 0.0), "origin"),
            (VecPoint::new3d(1.0, 0.0, 0.0), "x"),
            (VecPoint::new3d(0.0, 1.0, 0.0), "y"),
            (VecPoint::new3d(0.0, 0.0, 1.0), "z"),
        ];
        let tree = KdTree::build(items, 3);
        assert_eq!(tree.dims(), 3);

        let (_, v, _) = tree.nearest(&VecPoint::new3d(0.1, 0.0, 0.0)).unwrap();
        assert_eq!(*v, "origin");
    }

    #[test]
    fn points_returns_all() {
        let items = vec![(pt(1.0, 2.0), 1), (pt(3.0, 4.0), 2), (pt(5.0, 6.0), 3)];
        let tree = KdTree::build(items, 2);
        assert_eq!(tree.points().len(), 3);
    }

    #[test]
    fn display_format() {
        let items = vec![(pt(1.0, 2.0), 1), (pt(3.0, 4.0), 2)];
        let tree = KdTree::build(items, 2);
        assert_eq!(format!("{}", tree), "KdTree(2 points, 2D)");
    }

    #[test]
    fn duplicate_points() {
        let items = vec![(pt(1.0, 1.0), 1), (pt(1.0, 1.0), 2), (pt(1.0, 1.0), 3)];
        let tree = KdTree::build(items, 2);
        assert_eq!(tree.len(), 3);

        let (_, _, dist) = tree.nearest(&pt(1.0, 1.0)).unwrap();
        assert!(dist < 1e-10);
    }

    #[test]
    fn serde_roundtrip() {
        let items = vec![(pt(1.0, 2.0), 10), (pt(3.0, 4.0), 20), (pt(5.0, 6.0), 30)];
        let tree = KdTree::build(items, 2);
        let json = serde_json::to_string(&tree).unwrap();
        let restored: KdTree<VecPoint, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), tree.len());
        assert_eq!(restored.dims(), tree.dims());
    }

    // ── Expanded test coverage ──────────────────────────────────────

    #[test]
    fn build_empty_produces_empty_tree() {
        let tree: KdTree<VecPoint, i32> = KdTree::build(vec![], 2);
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
        assert!(tree.nearest(&pt(0.0, 0.0)).is_none());
    }

    #[test]
    fn build_single_point() {
        let tree = KdTree::build(vec![(pt(3.0, 4.0), 42)], 2);
        assert_eq!(tree.len(), 1);
        assert!(!tree.is_empty());

        let (p, v, dist) = tree.nearest(&pt(3.0, 4.0)).unwrap();
        assert_eq!(*v, 42);
        assert!(dist < 1e-10);
        assert_eq!(p.coords, vec![3.0, 4.0]);
    }

    #[test]
    fn k_nearest_zero_returns_empty() {
        let tree = KdTree::build(vec![(pt(0.0, 0.0), 1)], 2);
        let results = tree.k_nearest(&pt(0.0, 0.0), 0);
        assert!(results.is_empty());
    }

    #[test]
    fn k_nearest_on_empty_tree() {
        let tree: KdTree<VecPoint, i32> = KdTree::new(2);
        let results = tree.k_nearest(&pt(0.0, 0.0), 5);
        assert!(results.is_empty());
    }

    #[test]
    fn range_query_on_empty_tree() {
        let tree: KdTree<VecPoint, i32> = KdTree::new(2);
        let results = tree.range_query(&[0.0, 0.0], &[10.0, 10.0]);
        assert!(results.is_empty());
    }

    #[test]
    fn radius_query_on_empty_tree() {
        let tree: KdTree<VecPoint, i32> = KdTree::new(2);
        let results = tree.radius_query(&pt(0.0, 0.0), 10.0);
        assert!(results.is_empty());
    }

    #[test]
    fn radius_query_zero_radius_exact_match() {
        let items = vec![(pt(0.0, 0.0), 1), (pt(1.0, 0.0), 2), (pt(0.0, 1.0), 3)];
        let tree = KdTree::build(items, 2);
        let results = tree.radius_query(&pt(0.0, 0.0), 0.0);
        assert_eq!(results.len(), 1);
        assert_eq!(*results[0].1, 1);
    }

    #[test]
    fn range_query_exact_boundary_inclusion() {
        // Points exactly on the boundary should be included (<=)
        let items = vec![(pt(0.0, 0.0), 1), (pt(5.0, 5.0), 2), (pt(10.0, 10.0), 3)];
        let tree = KdTree::build(items, 2);
        let results = tree.range_query(&[0.0, 0.0], &[5.0, 5.0]);
        assert_eq!(results.len(), 2);
        let values: Vec<&i32> = results.iter().map(|(_, v)| *v).collect();
        assert!(values.contains(&&1));
        assert!(values.contains(&&2));
    }

    #[test]
    fn k_nearest_sorted_by_distance() {
        let items = vec![
            (pt(0.0, 0.0), 1),
            (pt(3.0, 0.0), 2),
            (pt(1.0, 0.0), 3),
            (pt(2.0, 0.0), 4),
        ];
        let tree = KdTree::build(items, 2);
        let results = tree.k_nearest(&pt(0.0, 0.0), 4);
        assert_eq!(results.len(), 4);

        // Verify sorted by distance (ascending)
        for i in 1..results.len() {
            assert!(
                results[i].2 >= results[i - 1].2,
                "k_nearest not sorted: {} >= {}",
                results[i].2,
                results[i - 1].2,
            );
        }

        // First should be origin (dist=0)
        assert!(*results[0].1 == 1);
    }

    #[test]
    fn insert_updates_nearest() {
        let mut tree: KdTree<VecPoint, &str> = KdTree::new(2);
        tree.insert(pt(10.0, 10.0), "far");
        tree.insert(pt(0.1, 0.1), "close");

        let (_, v, _) = tree.nearest(&pt(0.0, 0.0)).unwrap();
        assert_eq!(*v, "close");

        // Insert an even closer point
        tree.insert(pt(0.01, 0.01), "closest");
        let (_, v, _) = tree.nearest(&pt(0.0, 0.0)).unwrap();
        assert_eq!(*v, "closest");
    }

    #[test]
    fn collinear_points() {
        // All points on x-axis
        let items = vec![
            (pt(1.0, 0.0), 1),
            (pt(3.0, 0.0), 3),
            (pt(5.0, 0.0), 5),
            (pt(7.0, 0.0), 7),
            (pt(9.0, 0.0), 9),
        ];
        let tree = KdTree::build(items, 2);
        assert_eq!(tree.len(), 5);

        let (_, v, _) = tree.nearest(&pt(4.0, 0.0)).unwrap();
        assert!(*v == 3 || *v == 5); // either neighbor is acceptable

        let results = tree.k_nearest(&pt(5.0, 0.0), 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn negative_coordinates() {
        let items = vec![
            (pt(-5.0, -5.0), 1),
            (pt(-1.0, -1.0), 2),
            (pt(0.0, 0.0), 3),
            (pt(1.0, 1.0), 4),
        ];
        let tree = KdTree::build(items, 2);

        let (_, v, _) = tree.nearest(&pt(-4.0, -4.0)).unwrap();
        assert_eq!(*v, 1);

        let results = tree.range_query(&[-6.0, -6.0], &[-0.5, -0.5]);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn serde_roundtrip_preserves_queries() {
        let items = vec![
            (pt(0.0, 0.0), "a"),
            (pt(1.0, 0.0), "b"),
            (pt(0.0, 1.0), "c"),
            (pt(5.0, 5.0), "d"),
        ];
        let tree = KdTree::build(items, 2);

        let json = serde_json::to_string(&tree).unwrap();
        let restored: KdTree<VecPoint, &str> = serde_json::from_str(&json).unwrap();

        // Same nearest result
        let (_, v_orig, _) = tree.nearest(&pt(0.1, 0.1)).unwrap();
        let (_, v_rest, _) = restored.nearest(&pt(0.1, 0.1)).unwrap();
        assert_eq!(v_orig, v_rest);

        // Same range result count
        let range_orig = tree.range_query(&[-1.0, -1.0], &[2.0, 2.0]);
        let range_rest = restored.range_query(&[-1.0, -1.0], &[2.0, 2.0]);
        assert_eq!(range_orig.len(), range_rest.len());
    }

    #[test]
    fn large_tree_nearest_correctness() {
        // Build grid of 100 points and verify nearest neighbor brute force
        let mut items: Vec<(VecPoint, (i32, i32))> = Vec::new();
        for x in 0..10 {
            for y in 0..10 {
                items.push((pt(x as f64, y as f64), (x, y)));
            }
        }
        let tree = KdTree::build(items.clone(), 2);
        assert_eq!(tree.len(), 100);

        // Query a point and verify nearest is correct
        let query = pt(3.7, 6.2);
        let (_, v, dist_sq) = tree.nearest(&query).unwrap();

        // Brute force check
        let mut best_dist_sq = f64::INFINITY;
        let mut best_val = (0, 0);
        for (p, val) in &items {
            let d = p.dist_sq(&query);
            if d < best_dist_sq {
                best_dist_sq = d;
                best_val = *val;
            }
        }
        assert_eq!(*v, best_val);
        assert!((dist_sq - best_dist_sq).abs() < 1e-10);
    }

    #[test]
    fn five_dimensional_tree() {
        let items = vec![
            (VecPoint::new(vec![1.0, 0.0, 0.0, 0.0, 0.0]), "a"),
            (VecPoint::new(vec![0.0, 1.0, 0.0, 0.0, 0.0]), "b"),
            (VecPoint::new(vec![0.0, 0.0, 1.0, 0.0, 0.0]), "c"),
            (VecPoint::new(vec![0.0, 0.0, 0.0, 1.0, 0.0]), "d"),
            (VecPoint::new(vec![0.0, 0.0, 0.0, 0.0, 1.0]), "e"),
            (VecPoint::new(vec![0.0, 0.0, 0.0, 0.0, 0.0]), "origin"),
        ];
        let tree = KdTree::build(items, 5);
        assert_eq!(tree.dims(), 5);
        assert_eq!(tree.len(), 6);

        let query = VecPoint::new(vec![0.0, 0.0, 0.0, 0.0, 0.0]);
        let (_, v, dist) = tree.nearest(&query).unwrap();
        assert_eq!(*v, "origin");
        assert!(dist < 1e-10);
    }

    #[test]
    fn one_dimensional_tree() {
        let items = vec![
            (VecPoint::new(vec![5.0]), 5),
            (VecPoint::new(vec![2.0]), 2),
            (VecPoint::new(vec![8.0]), 8),
            (VecPoint::new(vec![1.0]), 1),
        ];
        let tree = KdTree::build(items, 1);
        assert_eq!(tree.dims(), 1);

        let query = VecPoint::new(vec![3.0]);
        let (_, v, _) = tree.nearest(&query).unwrap();
        assert_eq!(*v, 2); // closest to 3.0 is 2.0

        let results = tree.range_query(&[1.5], &[5.5]);
        assert_eq!(results.len(), 2); // points 2.0 and 5.0
    }

    #[test]
    fn clone_independence() {
        let items = vec![(pt(0.0, 0.0), 1), (pt(5.0, 5.0), 2)];
        let tree = KdTree::build(items, 2);
        let mut cloned = tree.clone();
        cloned.insert(pt(10.0, 10.0), 3);

        assert_eq!(tree.len(), 2);
        assert_eq!(cloned.len(), 3);
    }

    #[test]
    fn dist_sq_correctness() {
        let a = pt(3.0, 4.0);
        let b = pt(0.0, 0.0);
        assert!((a.dist_sq(&b) - 25.0).abs() < 1e-10); // 3² + 4² = 25

        let c = VecPoint::new3d(1.0, 2.0, 3.0);
        let d = VecPoint::new3d(4.0, 6.0, 3.0);
        assert!((c.dist_sq(&d) - 25.0).abs() < 1e-10); // 3² + 4² + 0² = 25
    }

    #[test]
    fn dist_sq_symmetric() {
        let a = pt(1.0, 7.0);
        let b = pt(4.0, 3.0);
        assert!((a.dist_sq(&b) - b.dist_sq(&a)).abs() < 1e-10);
    }

    #[test]
    fn dist_sq_zero_for_same_point() {
        let a = pt(42.0, 17.0);
        assert!(a.dist_sq(&a) < 1e-10);
    }

    #[test]
    fn radius_query_includes_boundary() {
        // Point at exactly radius distance should be included (<=)
        let items = vec![(pt(0.0, 0.0), 1), (pt(1.0, 0.0), 2)];
        let tree = KdTree::build(items, 2);
        let results = tree.radius_query(&pt(0.0, 0.0), 1.0);
        assert_eq!(results.len(), 2); // both points: 0 dist and 1.0 dist
    }

    #[test]
    fn k_nearest_single_point_tree() {
        let tree = KdTree::build(vec![(pt(5.0, 5.0), 99)], 2);
        let results = tree.k_nearest(&pt(0.0, 0.0), 3);
        assert_eq!(results.len(), 1);
        assert_eq!(*results[0].1, 99);
    }

    #[test]
    fn range_query_single_point_in_range() {
        let tree = KdTree::build(vec![(pt(5.0, 5.0), 1)], 2);
        let results = tree.range_query(&[4.0, 4.0], &[6.0, 6.0]);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn range_query_single_point_out_of_range() {
        let tree = KdTree::build(vec![(pt(5.0, 5.0), 1)], 2);
        let results = tree.range_query(&[0.0, 0.0], &[1.0, 1.0]);
        assert!(results.is_empty());
    }

    #[test]
    fn insert_many_then_nearest() {
        let mut tree: KdTree<VecPoint, i32> = KdTree::new(2);
        for i in 0..50 {
            tree.insert(pt(i as f64, i as f64), i);
        }
        assert_eq!(tree.len(), 50);

        let (_, v, _) = tree.nearest(&pt(25.3, 25.3)).unwrap();
        assert_eq!(*v, 25);
    }

    #[test]
    fn points_returns_all_after_insert() {
        let mut tree: KdTree<VecPoint, i32> = KdTree::new(2);
        tree.insert(pt(1.0, 2.0), 10);
        tree.insert(pt(3.0, 4.0), 20);
        tree.insert(pt(5.0, 6.0), 30);

        let points = tree.points();
        assert_eq!(points.len(), 3);
        let values: Vec<&i32> = points.iter().map(|(_, v)| *v).collect();
        assert!(values.contains(&&10));
        assert!(values.contains(&&20));
        assert!(values.contains(&&30));
    }

    #[test]
    fn display_empty_tree() {
        let tree: KdTree<VecPoint, i32> = KdTree::new(3);
        assert_eq!(format!("{}", tree), "KdTree(0 points, 3D)");
    }

    #[test]
    fn vec_point_constructors() {
        let p2 = VecPoint::new2d(1.0, 2.0);
        assert_eq!(p2.dims(), 2);
        assert_eq!(p2.coord(0), 1.0);
        assert_eq!(p2.coord(1), 2.0);

        let p3 = VecPoint::new3d(3.0, 4.0, 5.0);
        assert_eq!(p3.dims(), 3);
        assert_eq!(p3.coord(2), 5.0);

        let pn = VecPoint::new(vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(pn.dims(), 4);
    }

    #[test]
    fn radius_query_returns_sorted_candidate_distances() {
        // The results should have correct distance values
        let items = vec![(pt(0.0, 0.0), 1), (pt(1.0, 0.0), 2), (pt(2.0, 0.0), 3)];
        let tree = KdTree::build(items, 2);
        let results = tree.radius_query(&pt(0.0, 0.0), 3.0);
        assert_eq!(results.len(), 3);

        for (point, _, dist_sq) in &results {
            let expected = point.dist_sq(&pt(0.0, 0.0));
            assert!((dist_sq - expected).abs() < 1e-10);
        }
    }

    #[test]
    fn large_tree_range_query_correctness() {
        // Grid of points, verify range query matches brute force
        let mut items: Vec<(VecPoint, (i32, i32))> = Vec::new();
        for x in 0..10 {
            for y in 0..10 {
                items.push((pt(x as f64, y as f64), (x, y)));
            }
        }
        let tree = KdTree::build(items.clone(), 2);

        let min = [3.0, 3.0];
        let max = [7.0, 7.0];
        let results = tree.range_query(&min, &max);

        // Brute force count
        let expected: usize = items
            .iter()
            .filter(|(p, _)| {
                p.coord(0) >= 3.0 && p.coord(0) <= 7.0 && p.coord(1) >= 3.0 && p.coord(1) <= 7.0
            })
            .count();

        assert_eq!(results.len(), expected);
        assert_eq!(results.len(), 25); // 5x5 grid from 3..=7
    }
}
