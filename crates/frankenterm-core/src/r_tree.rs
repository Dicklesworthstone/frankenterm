//! R-tree for 2D spatial indexing.
//!
//! An R-tree organizes 2D rectangles in a balanced tree structure,
//! supporting efficient range queries, point queries, and nearest
//! neighbor search.
//!
//! # Properties
//!
//! - **O(log n)** average insert, query
//! - **Range query**: find all rectangles overlapping a query region
//! - **Point query**: find all rectangles containing a point
//! - **Nearest neighbor**: find closest rectangle to a point
//!
//! # Use in FrankenTerm
//!
//! Spatial indexing of pane positions in the terminal grid, click
//! target resolution, and layout overlap detection.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Geometry ───────────────────────────────────────────────────────────

/// Axis-aligned bounding rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x_min: f64,
    pub y_min: f64,
    pub x_max: f64,
    pub y_max: f64,
}

impl Rect {
    /// Creates a rectangle from corner coordinates.
    pub fn new(x_min: f64, y_min: f64, x_max: f64, y_max: f64) -> Self {
        Self {
            x_min: x_min.min(x_max),
            y_min: y_min.min(y_max),
            x_max: x_min.max(x_max),
            y_max: y_min.max(y_max),
        }
    }

    /// Creates a point (zero-area rectangle).
    pub fn point(x: f64, y: f64) -> Self {
        Self {
            x_min: x,
            y_min: y,
            x_max: x,
            y_max: y,
        }
    }

    /// Area of the rectangle.
    pub fn area(&self) -> f64 {
        (self.x_max - self.x_min) * (self.y_max - self.y_min)
    }

    /// Tests if this rectangle contains a point.
    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        x >= self.x_min && x <= self.x_max && y >= self.y_min && y <= self.y_max
    }

    /// Tests if this rectangle overlaps another.
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.x_min <= other.x_max
            && self.x_max >= other.x_min
            && self.y_min <= other.y_max
            && self.y_max >= other.y_min
    }

    /// Returns the minimum bounding rectangle of two rectangles.
    #[must_use]
    pub fn union(&self, other: &Rect) -> Rect {
        Rect {
            x_min: self.x_min.min(other.x_min),
            y_min: self.y_min.min(other.y_min),
            x_max: self.x_max.max(other.x_max),
            y_max: self.y_max.max(other.y_max),
        }
    }

    /// Area enlargement needed to include another rectangle.
    pub fn enlargement(&self, other: &Rect) -> f64 {
        self.union(other).area() - self.area()
    }

    /// Minimum distance from a point to this rectangle.
    pub fn min_distance(&self, x: f64, y: f64) -> f64 {
        let dx = if x < self.x_min {
            self.x_min - x
        } else if x > self.x_max {
            x - self.x_max
        } else {
            0.0
        };
        let dy = if y < self.y_min {
            self.y_min - y
        } else if y > self.y_max {
            y - self.y_max
        } else {
            0.0
        };
        dx.hypot(dy)
    }
}

// ── R-tree ─────────────────────────────────────────────────────────────

const MAX_ENTRIES: usize = 8;
const MIN_ENTRIES: usize = 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RTreeNode<V> {
    mbr: Rect,
    children: Vec<usize>,
    entries: Vec<(Rect, V)>, // Only leaf nodes have entries
    is_leaf: bool,
}

/// R-tree for 2D spatial indexing.
///
/// Stores rectangles with associated values and supports efficient
/// spatial queries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RTree<V> {
    nodes: Vec<RTreeNode<V>>,
    root: Option<usize>,
    count: usize,
}

impl<V: Clone> RTree<V> {
    /// Creates an empty R-tree.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            root: None,
            count: 0,
        }
    }

    /// Returns the number of entries.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Returns true if the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn alloc_node(&mut self, is_leaf: bool) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(RTreeNode {
            mbr: Rect::new(0.0, 0.0, 0.0, 0.0),
            children: Vec::new(),
            entries: Vec::new(),
            is_leaf,
        });
        idx
    }

    fn update_mbr(&mut self, node_idx: usize) {
        if self.nodes[node_idx].is_leaf {
            if let Some(first) = self.nodes[node_idx].entries.first() {
                let mut mbr = first.0;
                for (rect, _) in &self.nodes[node_idx].entries[1..] {
                    mbr = mbr.union(rect);
                }
                self.nodes[node_idx].mbr = mbr;
            }
        } else if let Some(&first_child) = self.nodes[node_idx].children.first() {
            let mut mbr = self.nodes[first_child].mbr;
            for &child in &self.nodes[node_idx].children[1..] {
                mbr = mbr.union(&self.nodes[child].mbr);
            }
            self.nodes[node_idx].mbr = mbr;
        }
    }

    /// Inserts a rectangle with associated value.
    pub fn insert(&mut self, rect: Rect, value: V) {
        if self.root.is_none() {
            let leaf = self.alloc_node(true);
            self.nodes[leaf].entries.push((rect, value));
            self.nodes[leaf].mbr = rect;
            self.root = Some(leaf);
            self.count += 1;
            return;
        }

        let root = self.root.unwrap();
        let split = self.insert_recursive(root, rect, value);

        if let Some((new_node, _)) = split {
            // Root was split, create new root
            let new_root = self.alloc_node(false);
            let old_root = self.root.unwrap();
            self.nodes[new_root].children.push(old_root);
            self.nodes[new_root].children.push(new_node);
            self.update_mbr(new_root);
            self.root = Some(new_root);
        }

        self.count += 1;
    }

    fn insert_recursive(
        &mut self,
        node_idx: usize,
        rect: Rect,
        value: V,
    ) -> Option<(usize, Rect)> {
        if self.nodes[node_idx].is_leaf {
            self.nodes[node_idx].entries.push((rect, value));
            self.update_mbr(node_idx);

            if self.nodes[node_idx].entries.len() > MAX_ENTRIES {
                return Some(self.split_leaf(node_idx));
            }
            return None;
        }

        // Choose child with minimum enlargement
        let best_child = self.choose_subtree(node_idx, &rect);
        let split = self.insert_recursive(best_child, rect, value);

        if let Some((new_child, _)) = split {
            self.nodes[node_idx].children.push(new_child);
            self.update_mbr(node_idx);

            if self.nodes[node_idx].children.len() > MAX_ENTRIES {
                return Some(self.split_internal(node_idx));
            }
        }

        self.update_mbr(node_idx);
        None
    }

    fn choose_subtree(&self, node_idx: usize, rect: &Rect) -> usize {
        let mut best_idx = self.nodes[node_idx].children[0];
        let mut best_enlargement = self.nodes[best_idx].mbr.enlargement(rect);
        let mut best_area = self.nodes[best_idx].mbr.area();

        for &child in &self.nodes[node_idx].children[1..] {
            let enlargement = self.nodes[child].mbr.enlargement(rect);
            let area = self.nodes[child].mbr.area();
            if enlargement < best_enlargement
                || ((enlargement - best_enlargement).abs() < f64::EPSILON && area < best_area)
            {
                best_idx = child;
                best_enlargement = enlargement;
                best_area = area;
            }
        }

        best_idx
    }

    fn split_leaf(&mut self, node_idx: usize) -> (usize, Rect) {
        let entries = std::mem::take(&mut self.nodes[node_idx].entries);
        let (left, right) = self.split_entries(entries);

        self.nodes[node_idx].entries = left;
        self.update_mbr(node_idx);

        let new_node = self.alloc_node(true);
        self.nodes[new_node].entries = right;
        self.update_mbr(new_node);

        let new_mbr = self.nodes[new_node].mbr;
        (new_node, new_mbr)
    }

    #[allow(clippy::unused_self, clippy::type_complexity)]
    fn split_entries(&self, mut entries: Vec<(Rect, V)>) -> (Vec<(Rect, V)>, Vec<(Rect, V)>) {
        // Simple split: sort by x center, split in half
        entries.sort_by(|a, b| {
            let ca = f64::midpoint(a.0.x_min, a.0.x_max);
            let cb = f64::midpoint(b.0.x_min, b.0.x_max);
            ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
        });

        let mid = entries.len().max(MIN_ENTRIES * 2) / 2;
        let mid = mid.max(MIN_ENTRIES).min(entries.len() - MIN_ENTRIES);
        let right = entries.split_off(mid);
        (entries, right)
    }

    fn split_internal(&mut self, node_idx: usize) -> (usize, Rect) {
        let children = std::mem::take(&mut self.nodes[node_idx].children);

        // Sort children by x center of their MBR
        let mut indexed: Vec<(usize, f64)> = children
            .iter()
            .map(|&c| {
                let mbr = &self.nodes[c].mbr;
                (c, f64::midpoint(mbr.x_min, mbr.x_max))
            })
            .collect();
        indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let mid = indexed.len().max(MIN_ENTRIES * 2) / 2;
        let mid = mid.max(MIN_ENTRIES).min(indexed.len() - MIN_ENTRIES);

        let left_children: Vec<usize> = indexed[..mid].iter().map(|&(c, _)| c).collect();
        let right_children: Vec<usize> = indexed[mid..].iter().map(|&(c, _)| c).collect();

        self.nodes[node_idx].children = left_children;
        self.update_mbr(node_idx);

        let new_node = self.alloc_node(false);
        self.nodes[new_node].children = right_children;
        self.update_mbr(new_node);

        let new_mbr = self.nodes[new_node].mbr;
        (new_node, new_mbr)
    }

    /// Finds all entries whose rectangles overlap the query rectangle.
    pub fn query(&self, query_rect: &Rect) -> Vec<(&Rect, &V)> {
        let mut results = Vec::new();
        if let Some(root) = self.root {
            self.query_recursive(root, query_rect, &mut results);
        }
        results
    }

    fn query_recursive<'a>(
        &'a self,
        node_idx: usize,
        query_rect: &Rect,
        results: &mut Vec<(&'a Rect, &'a V)>,
    ) {
        let node = &self.nodes[node_idx];

        if !node.mbr.overlaps(query_rect) {
            return;
        }

        if node.is_leaf {
            for (rect, value) in &node.entries {
                if rect.overlaps(query_rect) {
                    results.push((rect, value));
                }
            }
        } else {
            for &child in &node.children {
                self.query_recursive(child, query_rect, results);
            }
        }
    }

    /// Finds all entries containing the given point.
    pub fn query_point(&self, x: f64, y: f64) -> Vec<(&Rect, &V)> {
        let point = Rect::point(x, y);
        let mut results = Vec::new();
        if let Some(root) = self.root {
            self.query_point_recursive(root, x, y, &point, &mut results);
        }
        results
    }

    #[allow(clippy::only_used_in_recursion)]
    fn query_point_recursive<'a>(
        &'a self,
        node_idx: usize,
        x: f64,
        y: f64,
        point_rect: &Rect,
        results: &mut Vec<(&'a Rect, &'a V)>,
    ) {
        let node = &self.nodes[node_idx];

        if !node.mbr.contains_point(x, y) {
            return;
        }

        if node.is_leaf {
            for (rect, value) in &node.entries {
                if rect.contains_point(x, y) {
                    results.push((rect, value));
                }
            }
        } else {
            for &child in &node.children {
                self.query_point_recursive(child, x, y, point_rect, results);
            }
        }
    }

    /// Finds the nearest entry to the given point.
    /// Returns (rectangle, value, distance).
    pub fn nearest(&self, x: f64, y: f64) -> Option<(&Rect, &V, f64)> {
        let root_idx = self.root?;

        let mut best_dist = f64::INFINITY;
        let mut best: Option<(&Rect, &V)> = None;
        self.nearest_recursive(root_idx, x, y, &mut best_dist, &mut best);
        best.map(|(r, v)| (r, v, best_dist))
    }

    fn nearest_recursive<'a>(
        &'a self,
        node_idx: usize,
        x: f64,
        y: f64,
        best_dist: &mut f64,
        best: &mut Option<(&'a Rect, &'a V)>,
    ) {
        let node = &self.nodes[node_idx];

        if node.mbr.min_distance(x, y) >= *best_dist {
            return;
        }

        if node.is_leaf {
            for (rect, value) in &node.entries {
                let dist = rect.min_distance(x, y);
                if dist < *best_dist {
                    *best_dist = dist;
                    *best = Some((rect, value));
                }
            }
        } else {
            // Sort children by min distance for better pruning
            let mut children_with_dist: Vec<(usize, f64)> = node
                .children
                .iter()
                .map(|&c| (c, self.nodes[c].mbr.min_distance(x, y)))
                .collect();
            children_with_dist.sort_by(|a, b| {
                a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
            });

            for (child, min_dist) in children_with_dist {
                if min_dist >= *best_dist {
                    break;
                }
                self.nearest_recursive(child, x, y, best_dist, best);
            }
        }
    }

    /// Returns all entries in the tree.
    pub fn entries(&self) -> Vec<(&Rect, &V)> {
        let mut result = Vec::with_capacity(self.count);
        if let Some(root) = self.root {
            self.collect_entries(root, &mut result);
        }
        result
    }

    fn collect_entries<'a>(
        &'a self,
        node_idx: usize,
        out: &mut Vec<(&'a Rect, &'a V)>,
    ) {
        let node = &self.nodes[node_idx];
        if node.is_leaf {
            for (rect, value) in &node.entries {
                out.push((rect, value));
            }
        } else {
            for &child in &node.children {
                self.collect_entries(child, out);
            }
        }
    }
}

impl<V: Clone> Default for RTree<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V: Clone + fmt::Debug> fmt::Display for RTree<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RTree({} entries)", self.count)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        let tree: RTree<i32> = RTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
    }

    #[test]
    fn default_is_empty() {
        let tree: RTree<i32> = RTree::default();
        assert!(tree.is_empty());
    }

    #[test]
    fn single_insert() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 10.0, 10.0), 1);
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn point_query() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 10.0, 10.0), "a");
        tree.insert(Rect::new(5.0, 5.0, 15.0, 15.0), "b");
        tree.insert(Rect::new(20.0, 20.0, 30.0, 30.0), "c");

        let results = tree.query_point(7.0, 7.0);
        assert_eq!(results.len(), 2);

        let results = tree.query_point(25.0, 25.0);
        assert_eq!(results.len(), 1);
        assert_eq!(*results[0].1, "c");
    }

    #[test]
    fn range_query() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 5.0, 5.0), 1);
        tree.insert(Rect::new(10.0, 10.0, 15.0, 15.0), 2);
        tree.insert(Rect::new(3.0, 3.0, 12.0, 12.0), 3);

        let results = tree.query(&Rect::new(4.0, 4.0, 11.0, 11.0));
        let vals: Vec<i32> = results.iter().map(|(_, v)| **v).collect();
        assert!(vals.contains(&1)); // overlaps [0,5]x[0,5]
        assert!(vals.contains(&3)); // overlaps [3,12]x[3,12]
        assert!(vals.contains(&2)); // overlaps [10,15]x[10,15]
    }

    #[test]
    fn nearest() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 1.0, 1.0), "a");
        tree.insert(Rect::new(10.0, 10.0, 11.0, 11.0), "b");

        let (_, val, dist) = tree.nearest(2.0, 2.0).unwrap();
        assert_eq!(*val, "a");
        let expected_dist = (1.0f64 + 1.0f64).sqrt();
        assert!((dist - expected_dist).abs() < 1e-10);
    }

    #[test]
    fn nearest_point_inside() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 10.0, 10.0), "inside");

        let (_, val, dist) = tree.nearest(5.0, 5.0).unwrap();
        assert_eq!(*val, "inside");
        assert!((dist - 0.0).abs() < 1e-10);
    }

    #[test]
    fn many_inserts() {
        let mut tree = RTree::new();
        for i in 0..100 {
            let x = i as f64 * 5.0;
            tree.insert(Rect::new(x, 0.0, x + 3.0, 3.0), i);
        }
        assert_eq!(tree.len(), 100);

        // Query a specific region
        let results = tree.query(&Rect::new(10.0, 0.0, 20.0, 3.0));
        assert!(!results.is_empty());
    }

    #[test]
    fn entries() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 1.0, 1.0), 1);
        tree.insert(Rect::new(2.0, 2.0, 3.0, 3.0), 2);
        let entries = tree.entries();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn serde_roundtrip() {
        let mut tree = RTree::new();
        for i in 0..20 {
            let x = i as f64;
            tree.insert(Rect::new(x, x, x + 1.0, x + 1.0), i);
        }
        let json = serde_json::to_string(&tree).unwrap();
        let restored: RTree<i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), tree.len());
    }

    #[test]
    fn display_format() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 1.0, 1.0), 1);
        assert_eq!(format!("{}", tree), "RTree(1 entries)");
    }

    #[test]
    fn rect_area() {
        let r = Rect::new(0.0, 0.0, 3.0, 4.0);
        assert!((r.area() - 12.0).abs() < 1e-10);
    }

    #[test]
    fn rect_contains_point() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains_point(5.0, 5.0));
        assert!(r.contains_point(0.0, 0.0));
        assert!(!r.contains_point(11.0, 5.0));
    }

    #[test]
    fn rect_overlaps() {
        let a = Rect::new(0.0, 0.0, 5.0, 5.0);
        let b = Rect::new(3.0, 3.0, 8.0, 8.0);
        let c = Rect::new(10.0, 10.0, 15.0, 15.0);
        assert!(a.overlaps(&b));
        assert!(!a.overlaps(&c));
    }

    #[test]
    fn rect_min_distance() {
        let r = Rect::new(0.0, 0.0, 5.0, 5.0);
        assert!((r.min_distance(3.0, 3.0) - 0.0).abs() < 1e-10); // Inside
        assert!((r.min_distance(8.0, 0.0) - 3.0).abs() < 1e-10); // Right
        assert!((r.min_distance(0.0, -3.0) - 3.0).abs() < 1e-10); // Below
    }

    #[test]
    fn nearest_empty() {
        let tree: RTree<i32> = RTree::new();
        assert!(tree.nearest(0.0, 0.0).is_none());
    }

    #[test]
    fn query_empty() {
        let tree: RTree<i32> = RTree::new();
        let results = tree.query(&Rect::new(0.0, 0.0, 10.0, 10.0));
        assert!(results.is_empty());
    }

    // ── Expanded test coverage ──────────────────────────────────────

    #[test]
    fn rect_new_swapped_coords() {
        // Rect::new auto-corrects swapped coordinates
        let r = Rect::new(10.0, 10.0, 0.0, 0.0);
        assert!(r.x_min <= r.x_max);
        assert_eq!(r.x_min, 0.0);
        assert_eq!(r.x_max, 10.0);
        assert_eq!(r.y_min, 0.0);
        assert_eq!(r.y_max, 10.0);
    }

    #[test]
    fn rect_point_is_zero_area() {
        let p = Rect::point(5.0, 3.0);
        assert!((p.area() - 0.0).abs() < 1e-10);
        assert!(p.contains_point(5.0, 3.0));
        assert!(!p.contains_point(5.1, 3.0));
    }

    #[test]
    fn rect_union_basic() {
        let a = Rect::new(0.0, 0.0, 5.0, 5.0);
        let b = Rect::new(3.0, 3.0, 10.0, 10.0);
        let u = a.union(&b);
        assert!((u.x_min - 0.0).abs() < 1e-10);
        assert!((u.y_min - 0.0).abs() < 1e-10);
        assert!((u.x_max - 10.0).abs() < 1e-10);
        assert!((u.y_max - 10.0).abs() < 1e-10);
    }

    #[test]
    fn rect_union_with_self() {
        let a = Rect::new(1.0, 2.0, 3.0, 4.0);
        let u = a.union(&a);
        assert_eq!(u, a);
    }

    #[test]
    fn rect_enlargement() {
        let a = Rect::new(0.0, 0.0, 5.0, 5.0);
        let b = Rect::new(0.0, 0.0, 5.0, 5.0); // same rect
        assert!((a.enlargement(&b) - 0.0).abs() < 1e-10);

        let c = Rect::new(5.0, 5.0, 10.0, 10.0);
        // union would be [0,10]x[0,10] = 100, original = 25, enlargement = 75
        assert!((a.enlargement(&c) - 75.0).abs() < 1e-10);
    }

    #[test]
    fn rect_overlaps_symmetric() {
        let a = Rect::new(0.0, 0.0, 5.0, 5.0);
        let b = Rect::new(3.0, 3.0, 8.0, 8.0);
        assert_eq!(a.overlaps(&b), b.overlaps(&a));
    }

    #[test]
    fn rect_overlaps_touching_edges() {
        // Rects sharing an edge should overlap (>=/<= check)
        let a = Rect::new(0.0, 0.0, 5.0, 5.0);
        let b = Rect::new(5.0, 0.0, 10.0, 5.0);
        assert!(a.overlaps(&b));
    }

    #[test]
    fn rect_overlaps_touching_corner() {
        let a = Rect::new(0.0, 0.0, 5.0, 5.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert!(a.overlaps(&b));
    }

    #[test]
    fn rect_no_overlap_separated() {
        let a = Rect::new(0.0, 0.0, 1.0, 1.0);
        let b = Rect::new(2.0, 2.0, 3.0, 3.0);
        assert!(!a.overlaps(&b));
        assert!(!b.overlaps(&a));
    }

    #[test]
    fn rect_min_distance_corners() {
        let r = Rect::new(0.0, 0.0, 5.0, 5.0);
        // Upper-right diagonal
        let dist = r.min_distance(8.0, 8.0);
        let expected = (3.0f64).hypot(3.0f64);
        assert!((dist - expected).abs() < 1e-10);
    }

    #[test]
    fn rect_min_distance_on_boundary() {
        let r = Rect::new(0.0, 0.0, 5.0, 5.0);
        assert!((r.min_distance(5.0, 3.0) - 0.0).abs() < 1e-10);
        assert!((r.min_distance(0.0, 0.0) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn rect_contains_point_on_boundary() {
        let r = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains_point(0.0, 0.0));
        assert!(r.contains_point(10.0, 10.0));
        assert!(r.contains_point(5.0, 0.0));
        assert!(r.contains_point(0.0, 5.0));
    }

    #[test]
    fn point_query_empty_tree() {
        let tree: RTree<i32> = RTree::new();
        let results = tree.query_point(5.0, 5.0);
        assert!(results.is_empty());
    }

    #[test]
    fn point_query_outside_all_rects() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 5.0, 5.0), 1);
        tree.insert(Rect::new(10.0, 10.0, 15.0, 15.0), 2);

        let results = tree.query_point(7.0, 7.0);
        assert!(results.is_empty());
    }

    #[test]
    fn point_query_on_boundary() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 10.0, 10.0), 1);

        let results = tree.query_point(10.0, 10.0);
        assert_eq!(results.len(), 1);
        assert_eq!(*results[0].1, 1);
    }

    #[test]
    fn insert_triggers_split() {
        // Insert more than MAX_ENTRIES to force splits
        let mut tree = RTree::new();
        for i in 0..20 {
            tree.insert(
                Rect::new(i as f64, 0.0, i as f64 + 1.0, 1.0),
                i,
            );
        }
        assert_eq!(tree.len(), 20);

        // All entries should still be retrievable
        let entries = tree.entries();
        assert_eq!(entries.len(), 20);
    }

    #[test]
    fn query_after_many_splits() {
        let mut tree = RTree::new();
        for i in 0..50 {
            let x = (i % 10) as f64 * 10.0;
            let y = (i / 10) as f64 * 10.0;
            tree.insert(Rect::new(x, y, x + 5.0, y + 5.0), i);
        }
        assert_eq!(tree.len(), 50);

        // Query a region that should contain specific entries
        let results = tree.query(&Rect::new(0.0, 0.0, 15.0, 5.0));
        assert!(!results.is_empty());
    }

    #[test]
    fn nearest_single_entry() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(10.0, 10.0, 20.0, 20.0), 42);

        let (_, val, _) = tree.nearest(0.0, 0.0).unwrap();
        assert_eq!(*val, 42);
    }

    #[test]
    fn nearest_prefers_closest() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 1.0, 1.0), "near");
        tree.insert(Rect::new(100.0, 100.0, 101.0, 101.0), "far");
        tree.insert(Rect::new(50.0, 50.0, 51.0, 51.0), "mid");

        let (_, val, _) = tree.nearest(0.5, 0.5).unwrap();
        assert_eq!(*val, "near");
    }

    #[test]
    fn entries_empty_tree() {
        let tree: RTree<i32> = RTree::new();
        assert!(tree.entries().is_empty());
    }

    #[test]
    fn entries_count_matches_len() {
        let mut tree = RTree::new();
        for i in 0..15 {
            tree.insert(Rect::new(i as f64, 0.0, i as f64 + 1.0, 1.0), i);
        }
        assert_eq!(tree.entries().len(), tree.len());
    }

    #[test]
    fn serde_roundtrip_preserves_queries() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 5.0, 5.0), "a");
        tree.insert(Rect::new(10.0, 10.0, 15.0, 15.0), "b");

        let json = serde_json::to_string(&tree).unwrap();
        let restored: RTree<&str> = serde_json::from_str(&json).unwrap();

        let orig_results = tree.query_point(3.0, 3.0);
        let rest_results = restored.query_point(3.0, 3.0);
        assert_eq!(orig_results.len(), rest_results.len());
    }

    #[test]
    fn clone_independence() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 5.0, 5.0), 1);

        let mut cloned = tree.clone();
        cloned.insert(Rect::new(10.0, 10.0, 15.0, 15.0), 2);

        assert_eq!(tree.len(), 1);
        assert_eq!(cloned.len(), 2);
    }

    #[test]
    fn display_empty() {
        let tree: RTree<i32> = RTree::new();
        assert_eq!(format!("{}", tree), "RTree(0 entries)");
    }

    #[test]
    fn display_many() {
        let mut tree = RTree::new();
        for i in 0..25 {
            tree.insert(Rect::new(0.0, 0.0, 1.0, 1.0), i);
        }
        assert_eq!(format!("{}", tree), "RTree(25 entries)");
    }

    #[test]
    fn overlapping_rects_query() {
        // Multiple overlapping rects, query should find all overlapping ones
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 10.0, 10.0), 1);
        tree.insert(Rect::new(2.0, 2.0, 12.0, 12.0), 2);
        tree.insert(Rect::new(4.0, 4.0, 14.0, 14.0), 3);
        tree.insert(Rect::new(50.0, 50.0, 60.0, 60.0), 4);

        let results = tree.query(&Rect::new(3.0, 3.0, 5.0, 5.0));
        assert_eq!(results.len(), 3); // All except rect 4
    }

    #[test]
    fn point_rects_in_tree() {
        // Insert zero-area point rects
        let mut tree = RTree::new();
        tree.insert(Rect::point(1.0, 1.0), "p1");
        tree.insert(Rect::point(2.0, 2.0), "p2");
        tree.insert(Rect::point(3.0, 3.0), "p3");
        assert_eq!(tree.len(), 3);

        let results = tree.query_point(2.0, 2.0);
        assert_eq!(results.len(), 1);
        assert_eq!(*results[0].1, "p2");
    }

    #[test]
    fn nearest_large_dataset_brute_force() {
        let mut tree = RTree::new();
        let mut rects = Vec::new();
        for i in 0..30 {
            let x = (i * 7 % 20) as f64;
            let y = (i * 11 % 20) as f64;
            let r = Rect::new(x, y, x + 2.0, y + 2.0);
            rects.push((r, i));
            tree.insert(r, i);
        }

        let qx = 5.5;
        let qy = 7.3;

        let (_, tree_val, tree_dist) = tree.nearest(qx, qy).unwrap();

        // Brute force
        let mut best_dist = f64::INFINITY;
        let mut best_val = 0;
        for (r, v) in &rects {
            let d = r.min_distance(qx, qy);
            if d < best_dist {
                best_dist = d;
                best_val = *v;
            }
        }

        assert_eq!(*tree_val, best_val);
        assert!((tree_dist - best_dist).abs() < 1e-10);
    }

    #[test]
    fn query_with_negative_coordinates() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(-10.0, -10.0, -5.0, -5.0), "neg");
        tree.insert(Rect::new(5.0, 5.0, 10.0, 10.0), "pos");

        let results = tree.query_point(-7.0, -7.0);
        assert_eq!(results.len(), 1);
        assert_eq!(*results[0].1, "neg");

        let results = tree.query(&Rect::new(-11.0, -11.0, -4.0, -4.0));
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn range_query_no_overlap() {
        let mut tree = RTree::new();
        tree.insert(Rect::new(0.0, 0.0, 5.0, 5.0), 1);
        tree.insert(Rect::new(10.0, 10.0, 15.0, 15.0), 2);

        let results = tree.query(&Rect::new(6.0, 6.0, 9.0, 9.0));
        assert!(results.is_empty());
    }

    #[test]
    fn rect_area_zero_width() {
        let r = Rect::new(5.0, 0.0, 5.0, 10.0);
        assert!((r.area() - 0.0).abs() < 1e-10);
    }
}
