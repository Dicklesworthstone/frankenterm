//! Graph scoring algorithms: PageRank and betweenness centrality.
//!
//! Extracted from `beads_viewer_rust` for use in bv triage scoring
//! and pane dependency analysis.  Generic over any graph that
//! implements [`GraphView`].

use std::collections::{HashMap, VecDeque};

use tracing::debug;

/// Read-only view into a directed graph.
///
/// Implementors provide node iteration and adjacency queries.
/// Node identity is represented as `usize` indices.
pub trait GraphView {
    /// Number of nodes in the graph.
    fn node_count(&self) -> usize;

    /// Iterate over all node indices.
    fn nodes(&self) -> Vec<usize>;

    /// Outgoing neighbors of `node`.
    fn successors(&self, node: usize) -> Vec<usize>;

    /// Incoming neighbors of `node` (needed for PageRank).
    fn predecessors(&self, node: usize) -> Vec<usize>;
}

/// Simple adjacency-list directed graph for testing and lightweight use.
#[derive(Debug, Clone, Default)]
pub struct AdjGraph {
    node_count: usize,
    edges: Vec<(usize, usize)>,
}

impl AdjGraph {
    /// Create a graph with `n` nodes and no edges.
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self {
            node_count: n,
            edges: Vec::new(),
        }
    }

    /// Add a directed edge from `src` to `dst`.
    pub fn add_edge(&mut self, src: usize, dst: usize) {
        self.edges.push((src, dst));
    }
}

impl GraphView for AdjGraph {
    fn node_count(&self) -> usize {
        self.node_count
    }

    fn nodes(&self) -> Vec<usize> {
        (0..self.node_count).collect()
    }

    fn successors(&self, node: usize) -> Vec<usize> {
        self.edges
            .iter()
            .filter_map(|&(s, d)| if s == node { Some(d) } else { None })
            .collect()
    }

    fn predecessors(&self, node: usize) -> Vec<usize> {
        self.edges
            .iter()
            .filter_map(|&(s, d)| if d == node { Some(s) } else { None })
            .collect()
    }
}

/// PageRank configuration.
#[derive(Debug, Clone)]
pub struct PageRankConfig {
    /// Damping factor (typically 0.85).
    pub damping: f64,
    /// Maximum iterations before convergence.
    pub max_iterations: usize,
    /// Convergence tolerance (L1 norm of rank delta).
    pub tolerance: f64,
}

impl Default for PageRankConfig {
    fn default() -> Self {
        Self {
            damping: 0.85,
            max_iterations: 100,
            tolerance: 1e-6,
        }
    }
}

/// Result of a PageRank computation.
#[derive(Debug, Clone)]
pub struct PageRankResult {
    /// PageRank score per node index.
    pub scores: HashMap<usize, f64>,
    /// Actual iterations performed.
    pub iterations: usize,
    /// Whether the algorithm converged within tolerance.
    pub converged: bool,
}

/// Compute PageRank scores using the iterative power method.
///
/// Returns a map from node index to rank score (scores sum to ~1.0).
pub fn pagerank(graph: &impl GraphView, config: &PageRankConfig) -> PageRankResult {
    let n = graph.node_count();
    if n == 0 {
        return PageRankResult {
            scores: HashMap::new(),
            iterations: 0,
            converged: true,
        };
    }

    let nodes = graph.nodes();
    let init = 1.0 / n as f64;
    let mut rank: HashMap<usize, f64> = nodes.iter().map(|&node| (node, init)).collect();

    // Pre-compute out-degree for each node.
    let out_degree: HashMap<usize, usize> = nodes
        .iter()
        .map(|&node| (node, graph.successors(node).len()))
        .collect();

    let teleport = (1.0 - config.damping) / n as f64;
    let mut converged = false;
    let mut iterations = 0;

    for _ in 0..config.max_iterations {
        iterations += 1;
        let mut new_rank: HashMap<usize, f64> = HashMap::with_capacity(n);

        // Accumulate dangling node mass (nodes with no outgoing edges).
        let dangling_sum: f64 = nodes
            .iter()
            .filter(|&&node| out_degree[&node] == 0)
            .map(|&node| rank[&node])
            .sum();

        for &node in &nodes {
            let mut incoming_sum = 0.0;
            for pred in graph.predecessors(node) {
                let deg = out_degree[&pred];
                if deg > 0 {
                    incoming_sum += rank[&pred] / deg as f64;
                }
            }
            new_rank.insert(
                node,
                config.damping.mul_add(incoming_sum + dangling_sum / n as f64, teleport),
            );
        }

        // Check convergence (L1 norm).
        let delta: f64 = nodes
            .iter()
            .map(|&node| (new_rank[&node] - rank[&node]).abs())
            .sum();

        rank = new_rank;

        if delta < config.tolerance {
            converged = true;
            break;
        }
    }

    debug!(
        algorithm = "pagerank",
        nodes = n,
        iterations,
        converged,
        "pagerank complete"
    );

    PageRankResult {
        scores: rank,
        iterations,
        converged,
    }
}

/// Result of betweenness centrality computation.
#[derive(Debug, Clone)]
pub struct BetweennessResult {
    /// Betweenness centrality score per node index.
    pub scores: HashMap<usize, f64>,
}

/// Compute betweenness centrality using Brandes' algorithm.
///
/// Runs in O(V*E) for unweighted graphs.  Scores are NOT normalized
/// (divide by (n-1)(n-2) for the standard normalization).
pub fn betweenness_centrality(graph: &impl GraphView) -> BetweennessResult {
    let n = graph.node_count();
    let nodes = graph.nodes();
    let mut centrality: HashMap<usize, f64> = nodes.iter().map(|&node| (node, 0.0)).collect();

    if n <= 1 {
        debug!(
            algorithm = "betweenness",
            nodes = n,
            "betweenness centrality complete (trivial)"
        );
        return BetweennessResult { scores: centrality };
    }

    for &source in &nodes {
        // BFS from source.
        let mut stack: Vec<usize> = Vec::new();
        let mut predecessors_map: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut sigma: HashMap<usize, f64> = nodes.iter().map(|&node| (node, 0.0)).collect();
        let mut dist: HashMap<usize, i64> = nodes.iter().map(|&node| (node, -1)).collect();

        sigma.insert(source, 1.0);
        dist.insert(source, 0);
        let mut queue: VecDeque<usize> = VecDeque::new();
        queue.push_back(source);

        while let Some(v) = queue.pop_front() {
            stack.push(v);
            let d_v = *dist.get(&v).unwrap_or(&0);
            for w in graph.successors(v) {
                let d_w = *dist.get(&w).unwrap_or(&-1);
                // First visit?
                if d_w < 0 {
                    dist.insert(w, d_v + 1);
                    queue.push_back(w);
                }
                // Shortest path through v?
                if *dist.get(&w).unwrap_or(&-1) == d_v + 1 {
                    let sigma_v = *sigma.get(&v).unwrap_or(&0.0);
                    *sigma.entry(w).or_insert(0.0) += sigma_v;
                    predecessors_map.entry(w).or_default().push(v);
                }
            }
        }

        // Accumulate dependencies.
        let mut delta: HashMap<usize, f64> = nodes.iter().map(|&node| (node, 0.0)).collect();
        while let Some(w) = stack.pop() {
            if let Some(preds) = predecessors_map.get(&w) {
                let sigma_w = *sigma.get(&w).unwrap_or(&1.0); // avoid division by zero
                let delta_w = *delta.get(&w).unwrap_or(&0.0);
                for &v in preds {
                    let sigma_v = *sigma.get(&v).unwrap_or(&0.0);
                    let d = (sigma_v / sigma_w) * (1.0 + delta_w);
                    *delta.entry(v).or_insert(0.0) += d;
                }
            }
            if w != source {
                let delta_w = *delta.get(&w).unwrap_or(&0.0);
                *centrality.entry(w).or_insert(0.0) += delta_w;
            }
        }
    }

    debug!(
        algorithm = "betweenness",
        nodes = n,
        "betweenness centrality complete"
    );

    BetweennessResult { scores: centrality }
}

/// Normalize betweenness scores by (n-1)(n-2) for a directed graph.
pub fn normalize_betweenness<S: ::std::hash::BuildHasher>(scores: &mut HashMap<usize, f64, S>, node_count: usize) {
    if node_count <= 2 {
        return;
    }
    let factor = 1.0 / ((node_count - 1) * (node_count - 2)) as f64;
    for score in scores.values_mut() {
        *score *= factor;
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(n: usize) -> AdjGraph {
        let mut g = AdjGraph::new(n);
        for i in 0..n.saturating_sub(1) {
            g.add_edge(i, i + 1);
        }
        g
    }

    fn star(n: usize) -> AdjGraph {
        let mut g = AdjGraph::new(n);
        for i in 1..n {
            g.add_edge(0, i);
        }
        g
    }

    fn cycle(n: usize) -> AdjGraph {
        let mut g = AdjGraph::new(n);
        for i in 0..n {
            g.add_edge(i, (i + 1) % n);
        }
        g
    }

    fn complete(n: usize) -> AdjGraph {
        let mut g = AdjGraph::new(n);
        for i in 0..n {
            for j in 0..n {
                if i != j {
                    g.add_edge(i, j);
                }
            }
        }
        g
    }

    // -------------------------------------------------------------------------
    // AdjGraph
    // -------------------------------------------------------------------------

    #[test]
    fn test_adj_graph_new() {
        let g = AdjGraph::new(5);
        assert_eq!(g.node_count(), 5);
        assert_eq!(g.nodes().len(), 5);
    }

    #[test]
    fn test_adj_graph_successors() {
        let mut g = AdjGraph::new(3);
        g.add_edge(0, 1);
        g.add_edge(0, 2);
        let succ = g.successors(0);
        assert_eq!(succ.len(), 2);
        assert!(succ.contains(&1));
        assert!(succ.contains(&2));
    }

    #[test]
    fn test_adj_graph_predecessors() {
        let mut g = AdjGraph::new(3);
        g.add_edge(0, 2);
        g.add_edge(1, 2);
        let preds = g.predecessors(2);
        assert_eq!(preds.len(), 2);
        assert!(preds.contains(&0));
        assert!(preds.contains(&1));
    }

    #[test]
    fn test_adj_graph_no_edges() {
        let g = AdjGraph::new(3);
        assert!(g.successors(0).is_empty());
        assert!(g.predecessors(0).is_empty());
    }

    // -------------------------------------------------------------------------
    // PageRank
    // -------------------------------------------------------------------------

    #[test]
    fn test_pagerank_empty_graph() {
        let g = AdjGraph::new(0);
        let result = pagerank(&g, &PageRankConfig::default());
        assert!(result.scores.is_empty());
        assert_eq!(result.iterations, 0);
        assert!(result.converged);
    }

    #[test]
    fn test_pagerank_single_node() {
        let g = AdjGraph::new(1);
        let result = pagerank(&g, &PageRankConfig::default());
        assert!((result.scores[&0] - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_pagerank_simple_chain() {
        let g = chain(4); // 0→1→2→3
        let result = pagerank(&g, &PageRankConfig::default());
        // Last node should have highest rank (accumulates all flow)
        assert!(result.scores[&3] > result.scores[&0]);
    }

    #[test]
    fn test_pagerank_star_topology() {
        let g = star(5); // 0→{1,2,3,4}
        let result = pagerank(&g, &PageRankConfig::default());
        // Leaf nodes should all have similar scores
        let leaf_scores: Vec<f64> = (1..5).map(|i| result.scores[&i]).collect();
        let max_diff = leaf_scores
            .iter()
            .map(|&s| (s - leaf_scores[0]).abs())
            .fold(0.0_f64, f64::max);
        assert!(max_diff < 0.01, "leaf scores should be equal");
    }

    #[test]
    fn test_pagerank_cycle() {
        let g = cycle(4);
        let result = pagerank(&g, &PageRankConfig::default());
        // All nodes in a cycle should have equal rank
        let scores: Vec<f64> = (0..4).map(|i| result.scores[&i]).collect();
        for &s in &scores {
            assert!((s - 0.25).abs() < 0.01);
        }
    }

    #[test]
    fn test_pagerank_converges() {
        let g = chain(10);
        let result = pagerank(&g, &PageRankConfig::default());
        assert!(result.converged);
        assert!(result.iterations < 100);
    }

    #[test]
    fn test_pagerank_scores_sum_to_one() {
        let g = chain(5);
        let result = pagerank(&g, &PageRankConfig::default());
        let total: f64 = result.scores.values().sum();
        assert!((total - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_pagerank_complete_graph_equal() {
        let g = complete(4);
        let result = pagerank(&g, &PageRankConfig::default());
        for &node in &g.nodes() {
            assert!(
                (result.scores[&node] - 0.25).abs() < 0.01,
                "complete graph nodes should have equal rank"
            );
        }
    }

    #[test]
    fn test_pagerank_custom_damping() {
        let g = chain(3);
        let config = PageRankConfig {
            damping: 0.5,
            ..Default::default()
        };
        let result = pagerank(&g, &config);
        assert!(result.converged);
        let total: f64 = result.scores.values().sum();
        assert!((total - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_pagerank_low_max_iterations() {
        let g = chain(100);
        let config = PageRankConfig {
            max_iterations: 2,
            tolerance: 1e-15,
            ..Default::default()
        };
        let result = pagerank(&g, &config);
        assert_eq!(result.iterations, 2);
        assert!(!result.converged);
    }

    #[test]
    fn test_pagerank_disconnected_components() {
        let mut g = AdjGraph::new(4);
        g.add_edge(0, 1); // Component A
        g.add_edge(2, 3); // Component B
        let result = pagerank(&g, &PageRankConfig::default());
        assert_eq!(result.scores.len(), 4);
        let total: f64 = result.scores.values().sum();
        assert!((total - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_pagerank_dangling_nodes() {
        // Node 2 has no outgoing edges (dangling)
        let mut g = AdjGraph::new(3);
        g.add_edge(0, 1);
        g.add_edge(1, 2);
        let result = pagerank(&g, &PageRankConfig::default());
        // Dangling node mass redistributed uniformly
        assert!(result.scores[&2] > 0.0);
    }

    // -------------------------------------------------------------------------
    // Betweenness Centrality
    // -------------------------------------------------------------------------

    #[test]
    fn test_betweenness_empty_graph() {
        let g = AdjGraph::new(0);
        let result = betweenness_centrality(&g);
        assert!(result.scores.is_empty());
    }

    #[test]
    fn test_betweenness_single_node() {
        let g = AdjGraph::new(1);
        let result = betweenness_centrality(&g);
        assert_eq!(result.scores[&0], 0.0);
    }

    #[test]
    fn test_betweenness_chain_bridge_node_highest() {
        let g = chain(5); // 0→1→2→3→4
        let result = betweenness_centrality(&g);
        // Middle nodes should have highest betweenness
        assert!(result.scores[&2] > result.scores[&0]);
        assert!(result.scores[&2] > result.scores[&4]);
    }

    #[test]
    fn test_betweenness_leaf_node_zero() {
        let g = chain(5); // 0→1→2→3→4
        let result = betweenness_centrality(&g);
        // Source node (0) has no shortest paths through it (as intermediate)
        assert_eq!(result.scores[&0], 0.0);
    }

    #[test]
    fn test_betweenness_star_center_highest() {
        // Bidirectional star: center connects to all leaves and vice versa
        let mut g = AdjGraph::new(5);
        for i in 1..5 {
            g.add_edge(0, i);
            g.add_edge(i, 0);
        }
        let result = betweenness_centrality(&g);
        // Center node should have highest betweenness
        for i in 1..5 {
            assert!(
                result.scores[&0] >= result.scores[&i],
                "center should have highest betweenness"
            );
        }
    }

    #[test]
    fn test_betweenness_cycle_equal() {
        let g = cycle(4);
        let result = betweenness_centrality(&g);
        // All nodes in a cycle should have equal betweenness
        let first = result.scores[&0];
        for i in 1..4 {
            assert!(
                (result.scores[&i] - first).abs() < 0.01,
                "cycle nodes should have equal betweenness"
            );
        }
    }

    #[test]
    fn test_betweenness_complete_graph_equal() {
        let g = complete(4);
        let result = betweenness_centrality(&g);
        let first = result.scores[&0];
        for i in 1..4 {
            assert!(
                (result.scores[&i] - first).abs() < 0.01,
                "complete graph should have equal betweenness"
            );
        }
    }

    #[test]
    fn test_betweenness_bridge_graph() {
        // 0→1→2→3→4 with 5→2→6 (node 2 is a bridge between two components)
        let mut g = AdjGraph::new(7);
        g.add_edge(0, 1);
        g.add_edge(1, 2);
        g.add_edge(2, 3);
        g.add_edge(3, 4);
        g.add_edge(5, 2);
        g.add_edge(2, 6);
        let result = betweenness_centrality(&g);
        // Node 2 is the bridge — should have highest betweenness
        for &i in &[0, 1, 3, 4, 5, 6] {
            assert!(
                result.scores[&2] >= result.scores[&i],
                "bridge node 2 should have highest betweenness"
            );
        }
    }

    #[test]
    fn test_betweenness_disconnected() {
        let mut g = AdjGraph::new(4);
        g.add_edge(0, 1);
        g.add_edge(2, 3);
        let result = betweenness_centrality(&g);
        // No shortest paths cross components
        assert_eq!(result.scores[&0], 0.0);
        assert_eq!(result.scores[&2], 0.0);
    }

    #[test]
    fn test_betweenness_scores_nonnegative() {
        let g = chain(10);
        let result = betweenness_centrality(&g);
        for &score in result.scores.values() {
            assert!(score >= 0.0, "betweenness should be non-negative");
        }
    }

    // -------------------------------------------------------------------------
    // Normalization
    // -------------------------------------------------------------------------

    #[test]
    fn test_normalize_betweenness() {
        let g = chain(5);
        let mut result = betweenness_centrality(&g);
        normalize_betweenness(&mut result.scores, 5);
        for &score in result.scores.values() {
            assert!(score <= 1.0, "normalized score should be <= 1.0");
            assert!(score >= 0.0, "normalized score should be >= 0.0");
        }
    }

    #[test]
    fn test_normalize_betweenness_small_graph() {
        let g = AdjGraph::new(2);
        let mut result = betweenness_centrality(&g);
        // n=2: normalization should be a no-op (divides by 0 protection)
        normalize_betweenness(&mut result.scores, 2);
        assert_eq!(result.scores[&0], 0.0);
    }

    #[test]
    fn test_normalize_betweenness_single_node() {
        let g = AdjGraph::new(1);
        let mut result = betweenness_centrality(&g);
        normalize_betweenness(&mut result.scores, 1);
        assert_eq!(result.scores[&0], 0.0);
    }

    // -------------------------------------------------------------------------
    // PageRankConfig
    // -------------------------------------------------------------------------

    #[test]
    fn test_pagerank_config_default() {
        let config = PageRankConfig::default();
        assert!((config.damping - 0.85).abs() < 1e-10);
        assert_eq!(config.max_iterations, 100);
        assert!((config.tolerance - 1e-6).abs() < 1e-12);
    }

    // -------------------------------------------------------------------------
    // Integration: both algorithms on same graph
    // -------------------------------------------------------------------------

    #[test]
    fn test_both_algorithms_chain() {
        let g = chain(5);
        let pr = pagerank(&g, &PageRankConfig::default());
        let bc = betweenness_centrality(&g);
        // Both should return scores for all nodes
        assert_eq!(pr.scores.len(), 5);
        assert_eq!(bc.scores.len(), 5);
    }

    #[test]
    fn test_both_algorithms_empty() {
        let g = AdjGraph::new(0);
        let pr = pagerank(&g, &PageRankConfig::default());
        let bc = betweenness_centrality(&g);
        assert!(pr.scores.is_empty());
        assert!(bc.scores.is_empty());
    }

    // -------------------------------------------------------------------------
    // GraphView trait
    // -------------------------------------------------------------------------

    #[test]
    fn test_graph_view_default_adj_graph() {
        let g = AdjGraph::default();
        assert_eq!(g.node_count(), 0);
        assert!(g.nodes().is_empty());
    }

    #[test]
    fn test_graph_view_clone() {
        let g = chain(3);
        let g2 = g.clone();
        assert_eq!(g.node_count(), g2.node_count());
    }

    #[test]
    fn test_adj_graph_debug() {
        let g = AdjGraph::new(2);
        let dbg = format!("{:?}", g);
        assert!(dbg.contains("AdjGraph"));
    }
}
