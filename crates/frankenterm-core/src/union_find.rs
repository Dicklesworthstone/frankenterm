//! Disjoint Set Union (Union-Find) with path compression and union by rank.
//!
//! Provides near-constant-time union and find operations using path compression
//! and union by rank, achieving O(α(n)) amortized per operation where α is the
//! inverse Ackermann function (effectively ≤ 4 for all practical inputs).
//!
//! # Use Cases
//!
//! - Group correlated panes in causal DAG analysis
//! - Track connected session components during topology changes
//! - Cluster related error events across agent swarms
//! - Efficient equivalence class tracking for pattern deduplication
//!
//! Bead: ft-283h4.23

use serde::{Deserialize, Serialize};

/// Configuration for Union-Find.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnionFindConfig {
    /// Initial capacity (number of elements).
    pub capacity: usize,
}

impl Default for UnionFindConfig {
    fn default() -> Self {
        Self { capacity: 64 }
    }
}

/// Statistics about the Union-Find structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnionFindStats {
    /// Total number of elements.
    pub element_count: usize,
    /// Number of disjoint components.
    pub component_count: usize,
    /// Size of the largest component.
    pub largest_component: usize,
    /// Total union operations performed.
    pub union_count: u64,
    /// Total find operations performed.
    pub find_count: u64,
    /// Memory used in bytes (approximate).
    pub memory_bytes: usize,
}

/// Disjoint Set Union with path compression and union by rank.
///
/// Each element is identified by a `usize` index in `[0, len())`.
/// New elements are added via `make_set()` which returns the new element's index.
///
/// # Example
/// ```
/// use frankenterm_core::union_find::UnionFind;
///
/// let mut uf = UnionFind::new(5);
/// uf.union(0, 1);
/// uf.union(2, 3);
/// assert!(uf.connected(0, 1));
/// assert!(!uf.connected(0, 2));
/// assert_eq!(uf.component_count(), 3); // {0,1}, {2,3}, {4}
/// ```
#[derive(Debug, Clone)]
pub struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
    size: Vec<usize>,
    components: usize,
    union_ops: u64,
    find_ops: u64,
}

impl UnionFind {
    /// Create a Union-Find with `n` elements, each in its own set.
    pub fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
            size: vec![1; n],
            components: n,
            union_ops: 0,
            find_ops: 0,
        }
    }

    /// Create from config.
    pub fn with_config(config: UnionFindConfig) -> Self {
        Self::new(config.capacity)
    }

    /// Add a new element in its own singleton set. Returns its index.
    pub fn make_set(&mut self) -> usize {
        let idx = self.parent.len();
        self.parent.push(idx);
        self.rank.push(0);
        self.size.push(1);
        self.components += 1;
        idx
    }

    /// Find the representative (root) of the set containing `x`.
    /// Uses path compression for amortized near-constant time.
    ///
    /// # Panics
    /// Panics if `x >= len()`.
    pub fn find(&mut self, x: usize) -> usize {
        assert!(
            x < self.parent.len(),
            "index {} out of range [0, {})",
            x,
            self.parent.len()
        );
        self.find_ops += 1;
        self.find_inner(x)
    }

    /// Find without mutating (no path compression). Slower but usable with `&self`.
    pub fn find_immutable(&self, x: usize) -> usize {
        assert!(
            x < self.parent.len(),
            "index {} out of range [0, {})",
            x,
            self.parent.len()
        );
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        root
    }

    /// Union the sets containing `x` and `y`.
    /// Uses union by rank for balanced trees.
    /// Returns `true` if the sets were different (actual merge happened).
    ///
    /// # Panics
    /// Panics if `x >= len()` or `y >= len()`.
    pub fn union(&mut self, x: usize, y: usize) -> bool {
        self.union_ops += 1;
        let rx = self.find(x);
        let ry = self.find(y);
        if rx == ry {
            return false;
        }
        // Union by rank: attach smaller tree under larger
        match self.rank[rx].cmp(&self.rank[ry]) {
            std::cmp::Ordering::Less => {
                self.parent[rx] = ry;
                self.size[ry] += self.size[rx];
            }
            std::cmp::Ordering::Greater => {
                self.parent[ry] = rx;
                self.size[rx] += self.size[ry];
            }
            std::cmp::Ordering::Equal => {
                self.parent[ry] = rx;
                self.size[rx] += self.size[ry];
                self.rank[rx] += 1;
            }
        }
        self.components -= 1;
        true
    }

    /// Check if `x` and `y` are in the same set.
    pub fn connected(&mut self, x: usize, y: usize) -> bool {
        self.find(x) == self.find(y)
    }

    /// Check connectivity without mutation (uses immutable find).
    pub fn connected_immutable(&self, x: usize, y: usize) -> bool {
        self.find_immutable(x) == self.find_immutable(y)
    }

    /// Number of disjoint components.
    pub fn component_count(&self) -> usize {
        self.components
    }

    /// Size of the component containing `x`.
    pub fn component_size(&mut self, x: usize) -> usize {
        let root = self.find(x);
        self.size[root]
    }

    /// Total number of elements.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    /// Whether there are no elements.
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Get all elements in the same component as `x`.
    pub fn component_members(&mut self, x: usize) -> Vec<usize> {
        let root = self.find(x);
        (0..self.parent.len())
            .filter(|&i| self.find(i) == root)
            .collect()
    }

    /// Get all components as vectors of element indices.
    pub fn all_components(&mut self) -> Vec<Vec<usize>> {
        let mut groups: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        for i in 0..self.parent.len() {
            let root = self.find(i);
            groups.entry(root).or_default().push(i);
        }
        let mut result: Vec<Vec<usize>> = groups.into_values().collect();
        result.sort_by_key(|v| v[0]);
        result
    }

    /// Get statistics about the structure.
    pub fn stats(&mut self) -> UnionFindStats {
        let largest = if self.parent.is_empty() {
            0
        } else {
            let mut max_size = 0;
            for i in 0..self.parent.len() {
                let root = self.find(i);
                max_size = max_size.max(self.size[root]);
            }
            max_size
        };

        UnionFindStats {
            element_count: self.parent.len(),
            component_count: self.components,
            largest_component: largest,
            union_count: self.union_ops,
            find_count: self.find_ops,
            memory_bytes: self.memory_bytes(),
        }
    }

    /// Approximate memory usage in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.parent.len() * std::mem::size_of::<usize>() * 2 // parent + size
            + self.rank.len() * std::mem::size_of::<u8>() // rank
    }

    /// Reset: each element becomes its own singleton set again.
    pub fn reset(&mut self) {
        for i in 0..self.parent.len() {
            self.parent[i] = i;
            self.rank[i] = 0;
            self.size[i] = 1;
        }
        self.components = self.parent.len();
        self.union_ops = 0;
        self.find_ops = 0;
    }

    // ── Internal ──────────────────────────────────────────────────

    fn find_inner(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find_inner(self.parent[x]); // path compression
        }
        self.parent[x]
    }
}

impl Default for UnionFind {
    fn default() -> Self {
        Self::new(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_union_find() {
        let uf = UnionFind::new(0);
        assert!(uf.is_empty());
        assert_eq!(uf.len(), 0);
        assert_eq!(uf.component_count(), 0);
    }

    #[test]
    fn singleton_elements() {
        let uf = UnionFind::new(5);
        assert_eq!(uf.len(), 5);
        assert_eq!(uf.component_count(), 5);
        assert!(!uf.is_empty());
    }

    #[test]
    fn find_self() {
        let mut uf = UnionFind::new(3);
        for i in 0..3 {
            assert_eq!(uf.find(i), i);
        }
    }

    #[test]
    fn basic_union() {
        let mut uf = UnionFind::new(4);
        assert!(uf.union(0, 1));
        assert!(uf.connected(0, 1));
        assert!(!uf.connected(0, 2));
        assert_eq!(uf.component_count(), 3);
    }

    #[test]
    fn union_already_connected() {
        let mut uf = UnionFind::new(3);
        assert!(uf.union(0, 1));
        assert!(!uf.union(0, 1)); // already connected
        assert_eq!(uf.component_count(), 2);
    }

    #[test]
    fn transitive_connectivity() {
        let mut uf = UnionFind::new(5);
        uf.union(0, 1);
        uf.union(1, 2);
        assert!(uf.connected(0, 2));
        assert_eq!(uf.component_count(), 3);
    }

    #[test]
    fn merge_two_groups() {
        let mut uf = UnionFind::new(6);
        uf.union(0, 1);
        uf.union(2, 3);
        assert!(!uf.connected(0, 2));
        uf.union(1, 3);
        assert!(uf.connected(0, 2));
        assert!(uf.connected(0, 3));
        assert_eq!(uf.component_count(), 3); // {0,1,2,3}, {4}, {5}
    }

    #[test]
    fn all_connected() {
        let mut uf = UnionFind::new(4);
        uf.union(0, 1);
        uf.union(1, 2);
        uf.union(2, 3);
        assert_eq!(uf.component_count(), 1);
        for i in 0..4 {
            for j in 0..4 {
                assert!(uf.connected(i, j));
            }
        }
    }

    #[test]
    fn component_size_singleton() {
        let mut uf = UnionFind::new(3);
        assert_eq!(uf.component_size(0), 1);
    }

    #[test]
    fn component_size_after_union() {
        let mut uf = UnionFind::new(5);
        uf.union(0, 1);
        uf.union(0, 2);
        assert_eq!(uf.component_size(0), 3);
        assert_eq!(uf.component_size(1), 3);
        assert_eq!(uf.component_size(2), 3);
        assert_eq!(uf.component_size(3), 1);
    }

    #[test]
    fn make_set_grows() {
        let mut uf = UnionFind::new(2);
        let idx = uf.make_set();
        assert_eq!(idx, 2);
        assert_eq!(uf.len(), 3);
        assert_eq!(uf.component_count(), 3);
    }

    #[test]
    fn make_set_then_union() {
        let mut uf = UnionFind::new(0);
        let a = uf.make_set();
        let b = uf.make_set();
        let c = uf.make_set();
        uf.union(a, b);
        assert!(uf.connected(a, b));
        assert!(!uf.connected(a, c));
        assert_eq!(uf.component_count(), 2);
    }

    #[test]
    fn immutable_find() {
        let mut uf = UnionFind::new(4);
        uf.union(0, 1);
        uf.union(2, 3);
        assert!(uf.connected_immutable(0, 1));
        assert!(!uf.connected_immutable(0, 2));
    }

    #[test]
    fn component_members_basic() {
        let mut uf = UnionFind::new(5);
        uf.union(0, 2);
        uf.union(2, 4);
        let members = uf.component_members(0);
        assert_eq!(members.len(), 3);
        assert!(members.contains(&0));
        assert!(members.contains(&2));
        assert!(members.contains(&4));
    }

    #[test]
    fn all_components_basic() {
        let mut uf = UnionFind::new(4);
        uf.union(0, 1);
        uf.union(2, 3);
        let comps = uf.all_components();
        assert_eq!(comps.len(), 2);
    }

    #[test]
    fn reset_restores_singletons() {
        let mut uf = UnionFind::new(5);
        uf.union(0, 1);
        uf.union(2, 3);
        uf.reset();
        assert_eq!(uf.component_count(), 5);
        assert!(!uf.connected(0, 1));
        assert!(!uf.connected(2, 3));
    }

    #[test]
    fn stats_basic() {
        let mut uf = UnionFind::new(5);
        uf.union(0, 1);
        uf.union(0, 2);
        let stats = uf.stats();
        assert_eq!(stats.element_count, 5);
        assert_eq!(stats.component_count, 3);
        assert_eq!(stats.largest_component, 3);
        assert!(stats.union_count >= 2);
    }

    #[test]
    fn stats_serde() {
        let stats = UnionFindStats {
            element_count: 100,
            component_count: 10,
            largest_component: 50,
            union_count: 200,
            find_count: 500,
            memory_bytes: 2400,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: UnionFindStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn config_serde() {
        let config = UnionFindConfig { capacity: 128 };
        let json = serde_json::to_string(&config).unwrap();
        let back: UnionFindConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn default_union_find() {
        let uf = UnionFind::default();
        assert!(uf.is_empty());
        assert_eq!(uf.component_count(), 0);
    }

    #[test]
    fn memory_bytes_scales() {
        let uf1 = UnionFind::new(10);
        let uf2 = UnionFind::new(100);
        assert!(uf2.memory_bytes() > uf1.memory_bytes());
    }

    #[test]
    fn clone_independence() {
        let mut uf = UnionFind::new(5);
        uf.union(0, 1);
        let mut clone = uf.clone();
        clone.union(2, 3);
        assert!(!uf.connected(2, 3));
        assert!(clone.connected(2, 3));
    }

    #[test]
    fn large_chain() {
        let n = 100;
        let mut uf = UnionFind::new(n);
        for i in 0..n - 1 {
            uf.union(i, i + 1);
        }
        assert_eq!(uf.component_count(), 1);
        assert!(uf.connected(0, n - 1));
        assert_eq!(uf.component_size(0), n);
    }

    #[test]
    fn star_topology() {
        let n = 50;
        let mut uf = UnionFind::new(n);
        for i in 1..n {
            uf.union(0, i);
        }
        assert_eq!(uf.component_count(), 1);
        assert_eq!(uf.component_size(0), n);
    }

    #[test]
    fn two_equal_groups() {
        let mut uf = UnionFind::new(10);
        // Group A: 0-4
        for i in 1..5 {
            uf.union(0, i);
        }
        // Group B: 5-9
        for i in 6..10 {
            uf.union(5, i);
        }
        assert_eq!(uf.component_count(), 2);
        assert!(!uf.connected(0, 5));
        // Merge groups
        uf.union(4, 5);
        assert_eq!(uf.component_count(), 1);
        assert!(uf.connected(0, 9));
    }

    #[test]
    fn stats_empty_structure_reports_zero_largest_component() {
        let mut uf = UnionFind::new(0);
        let stats = uf.stats();
        assert_eq!(stats.element_count, 0);
        assert_eq!(stats.component_count, 0);
        assert_eq!(stats.largest_component, 0);
        assert_eq!(stats.union_count, 0);
        assert_eq!(stats.find_count, 0);
    }

    #[test]
    fn union_same_element_is_noop() {
        let mut uf = UnionFind::new(3);
        assert!(!uf.union(1, 1));
        assert_eq!(uf.component_count(), 3);
    }

    #[test]
    fn all_components_are_sorted_by_first_member() {
        let mut uf = UnionFind::new(6);
        uf.union(2, 4);
        uf.union(0, 1);
        let components = uf.all_components();
        assert_eq!(components, vec![vec![0, 1], vec![2, 4], vec![3], vec![5]]);
    }

    #[test]
    fn all_components_empty_is_empty_vec() {
        let mut uf = UnionFind::new(0);
        assert!(uf.all_components().is_empty());
    }

    #[test]
    fn component_members_singleton_contains_only_self() {
        let mut uf = UnionFind::new(4);
        uf.union(0, 1);
        let members = uf.component_members(3);
        assert_eq!(members, vec![3]);
    }

    #[test]
    fn reset_clears_counters_and_connectivity() {
        let mut uf = UnionFind::new(4);
        uf.union(0, 1);
        uf.union(2, 3);
        assert!(uf.connected(0, 1));
        assert!(uf.connected(2, 3));

        uf.reset();

        let stats = uf.stats();
        assert_eq!(stats.union_count, 0);
        // stats() performs one find per element when computing largest_component.
        assert_eq!(stats.find_count, uf.len() as u64);
        assert_eq!(stats.component_count, uf.len());
        assert!(!uf.connected(0, 1));
        assert!(!uf.connected(2, 3));
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn find_out_of_bounds() {
        let mut uf = UnionFind::new(3);
        uf.find(3);
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn find_immutable_out_of_bounds() {
        let uf = UnionFind::new(3);
        uf.find_immutable(3);
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn union_out_of_bounds() {
        let mut uf = UnionFind::new(3);
        uf.union(0, 5);
    }
}
