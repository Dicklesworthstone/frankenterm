//! Shortest path algorithms for weighted directed graphs.
//!
//! Provides Dijkstra's algorithm for non-negative weights, Bellman-Ford for
//! graphs with negative weights (with negative cycle detection), and BFS
//! for unweighted graphs. Useful for agent routing, latency analysis,
//! bottleneck identification, and dependency chain analysis.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};

use serde::{Deserialize, Serialize};

/// A weighted directed graph stored as an adjacency list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeightedGraph {
    n: usize,
    adj: Vec<Vec<(usize, f64)>>,
}

impl WeightedGraph {
    /// Create a new graph with `n` nodes and no edges.
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self {
            n,
            adj: vec![Vec::new(); n],
        }
    }

    /// Create from weighted edge triples `(from, to, weight)`.
    #[must_use]
    pub fn from_edges(n: usize, edges: &[(usize, usize, f64)]) -> Self {
        let mut g = Self::new(n);
        for &(u, v, w) in edges {
            g.add_edge(u, v, w);
        }
        g
    }

    /// Add a directed edge with weight.
    ///
    /// # Panics
    /// Panics if `u >= n` or `v >= n`.
    pub fn add_edge(&mut self, u: usize, v: usize, weight: f64) {
        assert!(u < self.n, "node {} out of range [0, {})", u, self.n);
        assert!(v < self.n, "node {} out of range [0, {})", v, self.n);
        self.adj[u].push((v, weight));
    }

    /// Add an undirected edge (two directed edges).
    pub fn add_undirected_edge(&mut self, u: usize, v: usize, weight: f64) {
        self.add_edge(u, v, weight);
        self.add_edge(v, u, weight);
    }

    /// Number of nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.n
    }

    /// Number of directed edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.adj.iter().map(|a| a.len()).sum()
    }

    /// Neighbors of node `u` with weights.
    #[must_use]
    pub fn neighbors(&self, u: usize) -> &[(usize, f64)] {
        &self.adj[u]
    }

    /// All weights are non-negative.
    #[must_use]
    pub fn all_non_negative(&self) -> bool {
        self.adj.iter().all(|a| a.iter().all(|&(_, w)| w >= 0.0))
    }
}

/// Result of a shortest path computation.
#[derive(Debug, Clone)]
pub struct ShortestPathResult {
    /// Distance from source to each node. `f64::INFINITY` if unreachable.
    pub dist: Vec<f64>,
    /// Predecessor on the shortest path. `usize::MAX` if no predecessor.
    pub prev: Vec<usize>,
    /// Source node.
    pub source: usize,
}

impl ShortestPathResult {
    /// Distance to node `target`. Returns `f64::INFINITY` if unreachable.
    #[must_use]
    pub fn distance_to(&self, target: usize) -> f64 {
        self.dist[target]
    }

    /// Whether `target` is reachable from the source.
    #[must_use]
    pub fn is_reachable(&self, target: usize) -> bool {
        self.dist[target] < f64::INFINITY
    }

    /// Reconstruct the path from source to target.
    ///
    /// Returns `None` if target is unreachable.
    #[must_use]
    pub fn path_to(&self, target: usize) -> Option<Vec<usize>> {
        if !self.is_reachable(target) {
            return None;
        }
        let mut path = Vec::new();
        let mut cur = target;
        while cur != usize::MAX {
            path.push(cur);
            cur = self.prev[cur];
        }
        path.reverse();
        Some(path)
    }
}

/// Entry for the Dijkstra priority queue (min-heap by distance).
#[derive(Debug, Clone)]
struct DijkstraEntry {
    node: usize,
    dist: f64,
}

impl PartialEq for DijkstraEntry {
    fn eq(&self, other: &Self) -> bool {
        self.dist == other.dist && self.node == other.node
    }
}

impl Eq for DijkstraEntry {}

impl PartialOrd for DijkstraEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DijkstraEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse for min-heap
        other
            .dist
            .partial_cmp(&self.dist)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.node.cmp(&self.node))
    }
}

/// Dijkstra's algorithm for single-source shortest paths.
///
/// Requires all edge weights to be non-negative.
/// O((V + E) log V) with a binary heap.
#[must_use]
pub fn dijkstra(g: &WeightedGraph, source: usize) -> ShortestPathResult {
    let n = g.node_count();
    let mut dist = vec![f64::INFINITY; n];
    let mut prev = vec![usize::MAX; n];

    dist[source] = 0.0;
    let mut heap = BinaryHeap::new();
    heap.push(DijkstraEntry {
        node: source,
        dist: 0.0,
    });

    while let Some(DijkstraEntry { node: u, dist: d }) = heap.pop() {
        if d > dist[u] {
            continue; // stale entry
        }
        for &(v, w) in g.neighbors(u) {
            let new_dist = dist[u] + w;
            if new_dist < dist[v] {
                dist[v] = new_dist;
                prev[v] = u;
                heap.push(DijkstraEntry {
                    node: v,
                    dist: new_dist,
                });
            }
        }
    }

    ShortestPathResult { dist, prev, source }
}

/// Bellman-Ford algorithm for single-source shortest paths.
///
/// Handles negative edge weights. Returns `None` if a negative-weight cycle
/// is reachable from the source.
/// O(VE) time.
#[must_use]
pub fn bellman_ford(g: &WeightedGraph, source: usize) -> Option<ShortestPathResult> {
    let n = g.node_count();
    let mut dist = vec![f64::INFINITY; n];
    let mut prev = vec![usize::MAX; n];

    dist[source] = 0.0;

    // Relax V-1 times
    for _ in 0..n.saturating_sub(1) {
        let mut changed = false;
        for u in 0..n {
            if dist[u] == f64::INFINITY {
                continue;
            }
            for &(v, w) in g.neighbors(u) {
                let new_dist = dist[u] + w;
                if new_dist < dist[v] {
                    dist[v] = new_dist;
                    prev[v] = u;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // Check for negative cycles
    for u in 0..n {
        if dist[u] == f64::INFINITY {
            continue;
        }
        for &(v, w) in g.neighbors(u) {
            if dist[u] + w < dist[v] {
                return None; // negative cycle
            }
        }
    }

    Some(ShortestPathResult { dist, prev, source })
}

/// BFS-based shortest paths for unweighted graphs.
///
/// Treats all edges as having weight 1.
/// O(V + E) time.
#[must_use]
pub fn bfs_shortest(g: &WeightedGraph, source: usize) -> ShortestPathResult {
    let n = g.node_count();
    let mut dist = vec![f64::INFINITY; n];
    let mut prev = vec![usize::MAX; n];

    dist[source] = 0.0;
    let mut queue = VecDeque::new();
    queue.push_back(source);

    while let Some(u) = queue.pop_front() {
        for &(v, _) in g.neighbors(u) {
            if dist[v] == f64::INFINITY {
                dist[v] = dist[u] + 1.0;
                prev[v] = u;
                queue.push_back(v);
            }
        }
    }

    ShortestPathResult { dist, prev, source }
}

/// All-pairs shortest paths using Floyd-Warshall.
///
/// O(V³) time, O(V²) space.
/// Returns `None` if a negative cycle exists.
#[must_use]
pub fn floyd_warshall(g: &WeightedGraph) -> Option<Vec<Vec<f64>>> {
    let n = g.node_count();
    let mut dist = vec![vec![f64::INFINITY; n]; n];

    for i in 0..n {
        dist[i][i] = 0.0;
    }
    for u in 0..n {
        for &(v, w) in g.neighbors(u) {
            if w < dist[u][v] {
                dist[u][v] = w;
            }
        }
    }

    for k in 0..n {
        for i in 0..n {
            for j in 0..n {
                let through_k = dist[i][k] + dist[k][j];
                if through_k < dist[i][j] {
                    dist[i][j] = through_k;
                }
            }
        }
    }

    // Check for negative cycles (diagonal < 0)
    for i in 0..n {
        if dist[i][i] < 0.0 {
            return None;
        }
    }

    Some(dist)
}

/// Compute the diameter of a graph (longest shortest path between any pair).
///
/// Returns `None` if the graph has a negative cycle.
/// Returns `f64::INFINITY` if the graph is disconnected.
#[must_use]
pub fn graph_diameter(g: &WeightedGraph) -> Option<f64> {
    let dist = floyd_warshall(g)?;
    let mut max_dist = 0.0f64;
    for row in &dist {
        for &d in row {
            if d > max_dist && d < f64::INFINITY {
                max_dist = d;
            }
        }
    }
    Some(max_dist)
}

/// Find the shortest path between two specific nodes.
///
/// Returns `(distance, path)` or `None` if unreachable.
#[must_use]
pub fn shortest_path(g: &WeightedGraph, source: usize, target: usize) -> Option<(f64, Vec<usize>)> {
    let result = dijkstra(g, source);
    let path = result.path_to(target)?;
    Some((result.distance_to(target), path))
}

/// k-shortest paths between source and target (Yen's algorithm).
///
/// Returns up to `k` shortest paths sorted by total distance.
#[must_use]
pub fn k_shortest_paths(
    g: &WeightedGraph,
    source: usize,
    target: usize,
    k: usize,
) -> Vec<(f64, Vec<usize>)> {
    let mut result = Vec::new();

    // Find the first shortest path
    let first = dijkstra(g, source);
    let first_path = match first.path_to(target) {
        Some(p) => p,
        None => return result,
    };
    result.push((first.distance_to(target), first_path));

    let mut candidates: Vec<(f64, Vec<usize>)> = Vec::new();

    for ki in 1..k {
        let prev_path = &result[ki - 1].1;

        for i in 0..prev_path.len().saturating_sub(1) {
            let spur_node = prev_path[i];
            let root_path = &prev_path[..=i];
            let root_cost: f64 = if i == 0 {
                0.0
            } else {
                // Sum minimum edge weights along root path
                let mut cost = 0.0;
                for w in root_path.windows(2) {
                    let min_w = g
                        .neighbors(w[0])
                        .iter()
                        .filter(|&&(v, _)| v == w[1])
                        .map(|&(_, wt)| wt)
                        .fold(f64::INFINITY, f64::min);
                    cost += min_w;
                }
                cost
            };

            // Build modified graph excluding edges from spur_node used by existing paths
            let mut excluded_edges = Vec::new();
            for (_, p) in &result {
                if p.len() > i && p[..=i] == *root_path {
                    if let Some(&next) = p.get(i + 1) {
                        excluded_edges.push((spur_node, next));
                    }
                }
            }

            // Run Dijkstra from spur_node on modified graph
            let n = g.node_count();
            let mut dist = vec![f64::INFINITY; n];
            let mut prev_arr = vec![usize::MAX; n];
            dist[spur_node] = 0.0;

            let mut heap = BinaryHeap::new();
            heap.push(DijkstraEntry {
                node: spur_node,
                dist: 0.0,
            });

            // Exclude root path nodes (except spur_node)
            let root_set: std::collections::HashSet<usize> =
                root_path[..i].iter().copied().collect();

            while let Some(DijkstraEntry { node: u, dist: d }) = heap.pop() {
                if d > dist[u] {
                    continue;
                }
                if u == target {
                    break;
                }
                for &(v, w) in g.neighbors(u) {
                    if root_set.contains(&v) {
                        continue;
                    }
                    if u == spur_node && excluded_edges.contains(&(u, v)) {
                        continue;
                    }
                    let new_dist = dist[u] + w;
                    if new_dist < dist[v] {
                        dist[v] = new_dist;
                        prev_arr[v] = u;
                        heap.push(DijkstraEntry {
                            node: v,
                            dist: new_dist,
                        });
                    }
                }
            }

            if dist[target] < f64::INFINITY {
                // Reconstruct spur path
                let mut spur_path = Vec::new();
                let mut cur = target;
                while cur != usize::MAX {
                    spur_path.push(cur);
                    cur = prev_arr[cur];
                }
                spur_path.reverse();

                // Combine root + spur
                let mut full_path: Vec<usize> = root_path[..i].to_vec();
                full_path.extend(spur_path);
                let total_cost = root_cost + dist[target];

                if !candidates.iter().any(|(_, p)| p == &full_path)
                    && !result.iter().any(|(_, p)| p == &full_path)
                {
                    candidates.push((total_cost, full_path));
                }
            }
        }

        if candidates.is_empty() {
            break;
        }

        // Pick the best candidate
        candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
        result.push(candidates.remove(0));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_chain() -> WeightedGraph {
        // 0 -1-> 1 -2-> 2 -3-> 3
        WeightedGraph::from_edges(4, &[(0, 1, 1.0), (1, 2, 2.0), (2, 3, 3.0)])
    }

    fn diamond_graph() -> WeightedGraph {
        //       0
        //      / \
        //   w=1   w=4
        //    /     \
        //   1--w=2->2
        //    \     /
        //   w=5  w=1
        //      \ /
        //       3
        WeightedGraph::from_edges(
            4,
            &[
                (0, 1, 1.0),
                (0, 2, 4.0),
                (1, 3, 5.0),
                (2, 3, 1.0),
                (1, 2, 2.0),
            ],
        )
    }

    // -- WeightedGraph basics --

    #[test]
    fn new_graph() {
        let g = WeightedGraph::new(5);
        assert_eq!(g.node_count(), 5);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn add_edges() {
        let mut g = WeightedGraph::new(3);
        g.add_edge(0, 1, 1.0);
        g.add_edge(1, 2, 2.0);
        assert_eq!(g.edge_count(), 2);
    }

    #[test]
    fn add_undirected() {
        let mut g = WeightedGraph::new(2);
        g.add_undirected_edge(0, 1, 5.0);
        assert_eq!(g.edge_count(), 2);
    }

    #[test]
    #[should_panic]
    fn add_edge_oob() {
        let mut g = WeightedGraph::new(2);
        g.add_edge(0, 5, 1.0);
    }

    #[test]
    fn all_non_negative_check() {
        let g = simple_chain();
        assert!(g.all_non_negative());

        let g2 = WeightedGraph::from_edges(2, &[(0, 1, -1.0)]);
        assert!(!g2.all_non_negative());
    }

    // -- Dijkstra --

    #[test]
    fn dijkstra_chain() {
        let g = simple_chain();
        let r = dijkstra(&g, 0);
        assert!((r.distance_to(0) - 0.0).abs() < f64::EPSILON);
        assert!((r.distance_to(1) - 1.0).abs() < f64::EPSILON);
        assert!((r.distance_to(2) - 3.0).abs() < f64::EPSILON);
        assert!((r.distance_to(3) - 6.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dijkstra_diamond() {
        let g = diamond_graph();
        let r = dijkstra(&g, 0);
        // 0->1->2->3 = 1+2+1 = 4 (shorter than 0->1->3 = 6)
        assert!((r.distance_to(3) - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dijkstra_unreachable() {
        let g = WeightedGraph::from_edges(3, &[(0, 1, 1.0)]);
        let r = dijkstra(&g, 0);
        assert!(!r.is_reachable(2));
        assert_eq!(r.distance_to(2), f64::INFINITY);
    }

    #[test]
    fn dijkstra_path() {
        let g = simple_chain();
        let r = dijkstra(&g, 0);
        let path = r.path_to(3).unwrap();
        assert_eq!(path, vec![0, 1, 2, 3]);
    }

    #[test]
    fn dijkstra_path_unreachable() {
        let g = WeightedGraph::from_edges(3, &[(0, 1, 1.0)]);
        let r = dijkstra(&g, 0);
        assert!(r.path_to(2).is_none());
    }

    #[test]
    fn dijkstra_self() {
        let g = simple_chain();
        let r = dijkstra(&g, 2);
        assert!((r.distance_to(2) - 0.0).abs() < f64::EPSILON);
    }

    // -- Bellman-Ford --

    #[test]
    fn bf_chain() {
        let g = simple_chain();
        let r = bellman_ford(&g, 0).unwrap();
        assert!((r.distance_to(3) - 6.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bf_negative_edge() {
        let g = WeightedGraph::from_edges(3, &[(0, 1, 2.0), (1, 2, -1.0)]);
        let r = bellman_ford(&g, 0).unwrap();
        assert!((r.distance_to(2) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bf_negative_cycle() {
        let g = WeightedGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, -3.0), (2, 0, 1.0)]);
        assert!(bellman_ford(&g, 0).is_none());
    }

    #[test]
    fn bf_matches_dijkstra() {
        let g = diamond_graph();
        let dij = dijkstra(&g, 0);
        let bf = bellman_ford(&g, 0).unwrap();
        for i in 0..g.node_count() {
            assert!(
                (dij.distance_to(i) - bf.distance_to(i)).abs() < 1e-10,
                "node {}: dij={}, bf={}",
                i,
                dij.distance_to(i),
                bf.distance_to(i)
            );
        }
    }

    // -- BFS --

    #[test]
    fn bfs_basic() {
        let g = simple_chain();
        let r = bfs_shortest(&g, 0);
        assert!((r.distance_to(0) - 0.0).abs() < f64::EPSILON);
        assert!((r.distance_to(1) - 1.0).abs() < f64::EPSILON);
        assert!((r.distance_to(2) - 2.0).abs() < f64::EPSILON);
        assert!((r.distance_to(3) - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bfs_unreachable() {
        let g = WeightedGraph::from_edges(3, &[(0, 1, 1.0)]);
        let r = bfs_shortest(&g, 0);
        assert!(!r.is_reachable(2));
    }

    // -- Floyd-Warshall --

    #[test]
    fn fw_chain() {
        let g = simple_chain();
        let dist = floyd_warshall(&g).unwrap();
        assert!((dist[0][3] - 6.0).abs() < f64::EPSILON);
        assert_eq!(dist[3][0], f64::INFINITY);
    }

    #[test]
    fn fw_negative_cycle() {
        let g = WeightedGraph::from_edges(3, &[(0, 1, 1.0), (1, 2, -3.0), (2, 0, 1.0)]);
        assert!(floyd_warshall(&g).is_none());
    }

    #[test]
    fn fw_matches_dijkstra() {
        let g = diamond_graph();
        let fw = floyd_warshall(&g).unwrap();
        let dij = dijkstra(&g, 0);
        for i in 0..g.node_count() {
            assert!(
                (fw[0][i] - dij.distance_to(i)).abs() < 1e-10,
                "node {}: fw={}, dij={}",
                i,
                fw[0][i],
                dij.distance_to(i)
            );
        }
    }

    // -- Diameter --

    #[test]
    fn diameter_chain() {
        let g = simple_chain();
        let d = graph_diameter(&g).unwrap();
        assert!((d - 6.0).abs() < f64::EPSILON);
    }

    #[test]
    fn diameter_single() {
        let g = WeightedGraph::new(1);
        let d = graph_diameter(&g).unwrap();
        assert!((d - 0.0).abs() < f64::EPSILON);
    }

    // -- shortest_path helper --

    #[test]
    fn sp_basic() {
        let g = simple_chain();
        let (d, path) = shortest_path(&g, 0, 3).unwrap();
        assert!((d - 6.0).abs() < f64::EPSILON);
        assert_eq!(path, vec![0, 1, 2, 3]);
    }

    #[test]
    fn sp_unreachable() {
        let g = WeightedGraph::from_edges(3, &[(0, 1, 1.0)]);
        assert!(shortest_path(&g, 0, 2).is_none());
    }

    // -- k-shortest paths --

    #[test]
    fn ksp_basic() {
        let g = diamond_graph();
        let paths = k_shortest_paths(&g, 0, 3, 3);
        assert!(!paths.is_empty());
        // First path should be shortest
        assert!((paths[0].0 - 4.0).abs() < f64::EPSILON);
        // Paths should be in non-decreasing order
        for w in paths.windows(2) {
            assert!(w[0].0 <= w[1].0 + f64::EPSILON);
        }
    }

    #[test]
    fn ksp_unreachable() {
        let g = WeightedGraph::from_edges(3, &[(0, 1, 1.0)]);
        let paths = k_shortest_paths(&g, 0, 2, 5);
        assert!(paths.is_empty());
    }

    // -- Serde --

    #[test]
    fn serde_roundtrip() {
        let g = diamond_graph();
        let json = serde_json::to_string(&g).unwrap();
        let back: WeightedGraph = serde_json::from_str(&json).unwrap();
        assert_eq!(g, back);
    }

    // -- Edge cases --

    #[test]
    fn empty_graph() {
        let g = WeightedGraph::new(0);
        let fw = floyd_warshall(&g).unwrap();
        assert!(fw.is_empty());
    }

    #[test]
    fn single_node() {
        let g = WeightedGraph::new(1);
        let r = dijkstra(&g, 0);
        assert!((r.distance_to(0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn zero_weight_edge() {
        let g = WeightedGraph::from_edges(2, &[(0, 1, 0.0)]);
        let r = dijkstra(&g, 0);
        assert!((r.distance_to(1) - 0.0).abs() < f64::EPSILON);
    }
}
