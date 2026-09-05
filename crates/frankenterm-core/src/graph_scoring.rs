//! Graph scoring algorithms: PageRank and betweenness centrality.
//!
//! Extracted from `beads_viewer_rust` for use in bv triage scoring
//! and pane dependency analysis.  Generic over any graph that
//! implements [`GraphView`].

use std::collections::{BTreeMap, HashMap, VecDeque};

use tracing::debug;

const MAX_SCORE_NODES: usize = 8_192;
const MAX_SCORE_EDGES: usize = 65_536;
const MAX_BETWEENNESS_WORK: usize = 20_000_000;

/// Invalid graph data must never become a ranking or a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GraphScoreError {
    #[error("graph exceeds the scoring work or size budget")]
    BudgetExceeded,
    #[error("declared node count or node identities are inconsistent")]
    InvalidNodes,
    #[error("graph references an unknown node")]
    MissingNode,
    #[error("incoming and outgoing adjacency disagree")]
    InconsistentAdjacency,
    #[error(
        "PageRank requires finite damping in [0, 1), positive tolerance and 1..=1000 iterations"
    )]
    InvalidConfig,
    #[error("graph score arithmetic is not representable")]
    NumericalOverflow,
}

/// Capture each adjacency once. Sorting fixes accumulation order; validation
/// prevents a malformed external graph from reaching indexed arithmetic.
struct ScoringGraph {
    nodes: Vec<usize>,
    successors: HashMap<usize, Vec<usize>>,
    predecessors: HashMap<usize, Vec<usize>>,
    edge_count: usize,
}

impl ScoringGraph {
    fn capture(graph: &impl GraphView) -> Result<Self, GraphScoreError> {
        let n = graph.node_count();
        if n > MAX_SCORE_NODES {
            return Err(GraphScoreError::BudgetExceeded);
        }
        let mut nodes = graph.nodes();
        nodes.sort_unstable();
        if nodes.len() != n || nodes.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(GraphScoreError::InvalidNodes);
        }
        let mut successors = HashMap::with_capacity(n);
        let mut predecessors = HashMap::with_capacity(n);
        let mut outgoing = BTreeMap::new();
        let mut incoming = BTreeMap::new();
        let mut edge_count = 0;
        let mut incoming_count = 0;
        for &node in &nodes {
            let mut next = graph.successors(node);
            let mut previous = graph.predecessors(node);
            if next.len() > MAX_SCORE_EDGES - edge_count
                || previous.len() > MAX_SCORE_EDGES - incoming_count
            {
                return Err(GraphScoreError::BudgetExceeded);
            }
            edge_count += next.len();
            incoming_count += previous.len();
            next.sort_unstable();
            previous.sort_unstable();
            for &neighbor in &next {
                if nodes.binary_search(&neighbor).is_err() {
                    return Err(GraphScoreError::MissingNode);
                }
                *outgoing.entry((node, neighbor)).or_insert(0usize) += 1;
            }
            for &neighbor in &previous {
                if nodes.binary_search(&neighbor).is_err() {
                    return Err(GraphScoreError::MissingNode);
                }
                *incoming.entry((neighbor, node)).or_insert(0usize) += 1;
            }
            successors.insert(node, next);
            predecessors.insert(node, previous);
        }
        // Multiplicities matter: GraphView supports parallel directed edges.
        if outgoing != incoming {
            return Err(GraphScoreError::InconsistentAdjacency);
        }
        Ok(Self {
            nodes,
            successors,
            predecessors,
            edge_count,
        })
    }
}

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
/// Rejects inconsistent adjacency, more than 8,192 nodes or 65,536 edges,
/// and invalid configuration. Adjacency is captured once in stable order.
pub fn pagerank(
    graph: &impl GraphView,
    config: &PageRankConfig,
) -> Result<PageRankResult, GraphScoreError> {
    if !config.damping.is_finite()
        || !(0.0..1.0).contains(&config.damping)
        || !config.tolerance.is_finite()
        || config.tolerance <= 0.0
        || !(1..=1_000).contains(&config.max_iterations)
    {
        return Err(GraphScoreError::InvalidConfig);
    }
    let graph = ScoringGraph::capture(graph)?;
    let n = graph.nodes.len();
    if n == 0 {
        return Ok(PageRankResult {
            scores: HashMap::new(),
            iterations: 0,
            converged: true,
        });
    }

    let nodes = &graph.nodes;
    let init = 1.0 / n as f64;
    let mut rank: HashMap<usize, f64> = nodes.iter().map(|&node| (node, init)).collect();

    // Pre-compute out-degree for each node.
    let out_degree: HashMap<usize, usize> = nodes
        .iter()
        .map(|&node| (node, graph.successors[&node].len()))
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

        for &node in nodes {
            let mut incoming_sum = 0.0;
            for pred in &graph.predecessors[&node] {
                let deg = out_degree[pred];
                if deg > 0 {
                    incoming_sum += rank[pred] / deg as f64;
                }
            }
            new_rank.insert(
                node,
                config
                    .damping
                    .mul_add(incoming_sum + dangling_sum / n as f64, teleport),
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

    Ok(PageRankResult {
        scores: rank,
        iterations,
        converged,
    })
}

/// Result of betweenness centrality computation.
#[derive(Debug, Clone)]
pub struct BetweennessResult {
    /// Betweenness centrality score per node index.
    pub scores: HashMap<usize, f64>,
}

/// Compute betweenness centrality using Brandes' algorithm.
///
/// Runs in O(V*(V+E)) for unweighted graphs. Scores are not normalized
/// (divide by (n-1)(n-2) for the standard normalization). Rejects inconsistent
/// graphs, more than 20 million estimated node/edge visits, and unrepresentable
/// shortest-path counts rather than returning non-finite rankings.
pub fn betweenness_centrality(
    graph: &impl GraphView,
) -> Result<BetweennessResult, GraphScoreError> {
    let graph = ScoringGraph::capture(graph)?;
    let n = graph.nodes.len();
    if n.saturating_mul(n.saturating_add(graph.edge_count)) > MAX_BETWEENNESS_WORK {
        return Err(GraphScoreError::BudgetExceeded);
    }
    let nodes = &graph.nodes;
    let mut centrality: HashMap<usize, f64> = nodes.iter().map(|&node| (node, 0.0)).collect();

    if n <= 1 {
        debug!(
            algorithm = "betweenness",
            nodes = n,
            "betweenness centrality complete (trivial)"
        );
        return Ok(BetweennessResult { scores: centrality });
    }

    for &source in nodes {
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
            for &w in &graph.successors[&v] {
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
                    if !sigma[&w].is_finite() {
                        return Err(GraphScoreError::NumericalOverflow);
                    }
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

    Ok(BetweennessResult { scores: centrality })
}

/// Normalize betweenness scores by (n-1)(n-2) for a directed graph.
pub fn normalize_betweenness<S: ::std::hash::BuildHasher>(
    scores: &mut HashMap<usize, f64, S>,
    node_count: usize,
) {
    if node_count <= 2 {
        return;
    }
    let factor = 1.0 / ((node_count - 1) as f64 * (node_count - 2) as f64);
    for score in scores.values_mut() {
        *score *= factor;
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct ObservedGraph {
        declared_nodes: usize,
        node_ids: Vec<usize>,
        outgoing: Vec<Vec<usize>>,
        incoming: Vec<Vec<usize>>,
        adjacency_calls: Cell<usize>,
    }

    impl GraphView for ObservedGraph {
        fn node_count(&self) -> usize {
            self.declared_nodes
        }

        fn nodes(&self) -> Vec<usize> {
            self.node_ids.clone()
        }

        fn successors(&self, node: usize) -> Vec<usize> {
            self.adjacency_calls.set(self.adjacency_calls.get() + 1);
            self.outgoing[node].clone()
        }

        fn predecessors(&self, node: usize) -> Vec<usize> {
            self.adjacency_calls.set(self.adjacency_calls.get() + 1);
            self.incoming[node].clone()
        }
    }

    #[test]
    fn malformed_graphs_return_errors_before_score_arithmetic() {
        let mut graph = ObservedGraph {
            declared_nodes: 2,
            node_ids: vec![0, 0],
            outgoing: vec![vec![1], vec![]],
            incoming: vec![vec![], vec![0]],
            adjacency_calls: Cell::new(0),
        };
        assert_eq!(
            pagerank(&graph, &PageRankConfig::default()).unwrap_err(),
            GraphScoreError::InvalidNodes
        );
        assert_eq!(graph.adjacency_calls.get(), 0);
        graph.node_ids = vec![0];
        assert_eq!(
            betweenness_centrality(&graph).unwrap_err(),
            GraphScoreError::InvalidNodes
        );
        graph.node_ids = vec![0, 1];
        graph.outgoing[0] = vec![2];
        assert_eq!(
            pagerank(&graph, &PageRankConfig::default()).unwrap_err(),
            GraphScoreError::MissingNode
        );
        graph.outgoing[0] = vec![1, 1];
        assert_eq!(
            betweenness_centrality(&graph).unwrap_err(),
            GraphScoreError::InconsistentAdjacency
        );
        graph.incoming[1].push(0);
        assert!(betweenness_centrality(&graph).is_ok());
    }

    #[test]
    fn invalid_configuration_is_rejected_even_for_empty_graphs() {
        let graph = AdjGraph::new(0);
        for damping in [f64::NAN, f64::INFINITY, -0.1, 1.0] {
            let config = PageRankConfig {
                damping,
                ..PageRankConfig::default()
            };
            assert_eq!(
                pagerank(&graph, &config).unwrap_err(),
                GraphScoreError::InvalidConfig
            );
        }
        for tolerance in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            let config = PageRankConfig {
                tolerance,
                ..PageRankConfig::default()
            };
            assert_eq!(
                pagerank(&graph, &config).unwrap_err(),
                GraphScoreError::InvalidConfig
            );
        }
        for max_iterations in [0, 1_001, usize::MAX] {
            let config = PageRankConfig {
                max_iterations,
                ..PageRankConfig::default()
            };
            assert_eq!(
                pagerank(&graph, &config).unwrap_err(),
                GraphScoreError::InvalidConfig
            );
        }
    }

    #[test]
    fn graph_size_and_work_budgets_fail_before_expensive_traversal() {
        let huge = AdjGraph::new(usize::MAX);
        assert_eq!(
            pagerank(&huge, &PageRankConfig::default()).unwrap_err(),
            GraphScoreError::BudgetExceeded
        );
        assert_eq!(
            betweenness_centrality(&huge).unwrap_err(),
            GraphScoreError::BudgetExceeded
        );
        let isolated = AdjGraph::new(4_473);
        assert_eq!(
            betweenness_centrality(&isolated).unwrap_err(),
            GraphScoreError::BudgetExceeded
        );
        let mut parallel = AdjGraph::new(2);
        for _ in 0..=MAX_SCORE_EDGES {
            parallel.add_edge(0, 1);
        }
        assert_eq!(
            pagerank(&parallel, &PageRankConfig::default()).unwrap_err(),
            GraphScoreError::BudgetExceeded
        );
    }

    #[test]
    fn iterative_pagerank_reads_each_adjacency_only_once() {
        let graph = ObservedGraph {
            declared_nodes: 3,
            node_ids: vec![2, 0, 1],
            outgoing: vec![vec![1], vec![2], vec![]],
            incoming: vec![vec![], vec![0], vec![1]],
            adjacency_calls: Cell::new(0),
        };
        let result = pagerank(&graph, &PageRankConfig::default()).unwrap();
        assert!(result.iterations > 1);
        assert!(result.converged);
        assert_eq!(graph.adjacency_calls.get(), 6);
        assert!(result.scores[&2] > result.scores[&1]);
        assert!(result.scores[&1] > result.scores[&0]);
    }

    #[test]
    fn graph_iteration_order_does_not_change_score_bits() {
        let mut graph = ObservedGraph {
            declared_nodes: 4,
            node_ids: vec![0, 1, 2, 3],
            outgoing: vec![vec![1, 2, 2], vec![3], vec![3], vec![0]],
            incoming: vec![vec![3], vec![0], vec![0, 0], vec![1, 2]],
            adjacency_calls: Cell::new(0),
        };
        let rank = pagerank(&graph, &PageRankConfig::default()).unwrap();
        let between = betweenness_centrality(&graph).unwrap();
        graph.node_ids.reverse();
        for edges in graph.outgoing.iter_mut().chain(graph.incoming.iter_mut()) {
            edges.reverse();
        }
        let reordered_rank = pagerank(&graph, &PageRankConfig::default()).unwrap();
        let reordered_between = betweenness_centrality(&graph).unwrap();
        assert_eq!(rank.iterations, reordered_rank.iterations);
        for node in 0..4 {
            assert_eq!(
                rank.scores[&node].to_bits(),
                reordered_rank.scores[&node].to_bits()
            );
            assert_eq!(
                between.scores[&node].to_bits(),
                reordered_between.scores[&node].to_bits()
            );
        }
    }

    #[test]
    fn overflowing_shortest_path_counts_are_not_rankings() {
        // Two choices per layer create 2^1024 shortest paths while the graph
        // itself remains small enough for the admitted work budget.
        let layers = 1_025;
        let mut graph = AdjGraph::new(1 + 2 * layers);
        graph.add_edge(0, 1);
        graph.add_edge(0, 2);
        for layer in 1..layers {
            let previous = 1 + 2 * (layer - 1);
            let next = 1 + 2 * layer;
            for source in previous..previous + 2 {
                for destination in next..next + 2 {
                    graph.add_edge(source, destination);
                }
            }
        }
        assert_eq!(
            betweenness_centrality(&graph).unwrap_err(),
            GraphScoreError::NumericalOverflow
        );
    }

    #[test]
    fn normalization_does_not_overflow_integer_node_counts() {
        let mut scores = HashMap::from([(0, 1.0)]);
        normalize_betweenness(&mut scores, usize::MAX);
        assert!(scores[&0].is_finite());
        assert!(scores[&0] > 0.0);
        let n = usize::MAX as f64;
        assert!((scores[&0] * n * n - 1.0).abs() < 1e-14);
    }

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
        assert_eq!(g.successors(0), [] as [usize; 0]);
        assert_eq!(g.predecessors(0), [] as [usize; 0]);
    }

    // -------------------------------------------------------------------------
    // PageRank
    // -------------------------------------------------------------------------

    #[test]
    fn test_pagerank_empty_graph() {
        let g = AdjGraph::new(0);
        let result = pagerank(&g, &PageRankConfig::default()).unwrap();
        assert!(result.scores.is_empty());
        assert_eq!(result.iterations, 0);
        assert!(result.converged);
    }

    #[test]
    fn test_pagerank_single_node() {
        let g = AdjGraph::new(1);
        let result = pagerank(&g, &PageRankConfig::default()).unwrap();
        assert!((result.scores[&0] - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_pagerank_simple_chain() {
        let g = chain(4); // 0→1→2→3
        let result = pagerank(&g, &PageRankConfig::default()).unwrap();
        // Last node should have highest rank (accumulates all flow)
        assert!(result.scores[&3] > result.scores[&0]);
    }

    #[test]
    fn test_pagerank_star_topology() {
        let g = star(5); // 0→{1,2,3,4}
        let result = pagerank(&g, &PageRankConfig::default()).unwrap();
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
        let result = pagerank(&g, &PageRankConfig::default()).unwrap();
        // All nodes in a cycle should have equal rank
        let scores: Vec<f64> = (0..4).map(|i| result.scores[&i]).collect();
        for &s in &scores {
            assert!((s - 0.25).abs() < 0.01);
        }
    }

    #[test]
    fn test_pagerank_converges() {
        let g = chain(10);
        let result = pagerank(&g, &PageRankConfig::default()).unwrap();
        assert!(result.converged);
        assert!(result.iterations < 100);
    }

    #[test]
    fn test_pagerank_scores_sum_to_one() {
        let g = chain(5);
        let result = pagerank(&g, &PageRankConfig::default()).unwrap();
        let total: f64 = result.scores.values().sum();
        assert!((total - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_pagerank_complete_graph_equal() {
        let g = complete(4);
        let result = pagerank(&g, &PageRankConfig::default()).unwrap();
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
        let result = pagerank(&g, &config).unwrap();
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
        let result = pagerank(&g, &config).unwrap();
        assert_eq!(result.iterations, 2);
        assert!(!result.converged);
    }

    #[test]
    fn test_pagerank_disconnected_components() {
        let mut g = AdjGraph::new(4);
        g.add_edge(0, 1); // Component A
        g.add_edge(2, 3); // Component B
        let result = pagerank(&g, &PageRankConfig::default()).unwrap();
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
        let result = pagerank(&g, &PageRankConfig::default()).unwrap();
        // Dangling node mass redistributed uniformly
        assert!(result.scores[&2] > 0.0);
    }

    // -------------------------------------------------------------------------
    // Betweenness Centrality
    // -------------------------------------------------------------------------

    #[test]
    fn test_betweenness_empty_graph() {
        let g = AdjGraph::new(0);
        let result = betweenness_centrality(&g).unwrap();
        assert!(result.scores.is_empty());
    }

    #[test]
    fn test_betweenness_single_node() {
        let g = AdjGraph::new(1);
        let result = betweenness_centrality(&g).unwrap();
        assert_eq!(result.scores[&0], 0.0);
    }

    #[test]
    fn test_betweenness_chain_bridge_node_highest() {
        let g = chain(5); // 0→1→2→3→4
        let result = betweenness_centrality(&g).unwrap();
        // Middle nodes should have highest betweenness
        assert!(result.scores[&2] > result.scores[&0]);
        assert!(result.scores[&2] > result.scores[&4]);
    }

    #[test]
    fn test_betweenness_leaf_node_zero() {
        let g = chain(5); // 0→1→2→3→4
        let result = betweenness_centrality(&g).unwrap();
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
        let result = betweenness_centrality(&g).unwrap();
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
        let result = betweenness_centrality(&g).unwrap();
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
        let result = betweenness_centrality(&g).unwrap();
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
        let result = betweenness_centrality(&g).unwrap();
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
        let result = betweenness_centrality(&g).unwrap();
        // No shortest paths cross components
        assert_eq!(result.scores[&0], 0.0);
        assert_eq!(result.scores[&2], 0.0);
    }

    #[test]
    fn test_betweenness_scores_nonnegative() {
        let g = chain(10);
        let result = betweenness_centrality(&g).unwrap();
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
        let mut result = betweenness_centrality(&g).unwrap();
        normalize_betweenness(&mut result.scores, 5);
        for &score in result.scores.values() {
            assert!(score <= 1.0, "normalized score should be <= 1.0");
            assert!(score >= 0.0, "normalized score should be >= 0.0");
        }
    }

    #[test]
    fn test_normalize_betweenness_small_graph() {
        let g = AdjGraph::new(2);
        let mut result = betweenness_centrality(&g).unwrap();
        // n=2: normalization should be a no-op (divides by 0 protection)
        normalize_betweenness(&mut result.scores, 2);
        assert_eq!(result.scores[&0], 0.0);
    }

    #[test]
    fn test_normalize_betweenness_single_node() {
        let g = AdjGraph::new(1);
        let mut result = betweenness_centrality(&g).unwrap();
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
        let pr = pagerank(&g, &PageRankConfig::default()).unwrap();
        let bc = betweenness_centrality(&g).unwrap();
        // Both should return scores for all nodes
        assert_eq!(pr.scores.len(), 5);
        assert_eq!(bc.scores.len(), 5);
    }

    #[test]
    fn test_both_algorithms_empty() {
        let g = AdjGraph::new(0);
        let pr = pagerank(&g, &PageRankConfig::default()).unwrap();
        let bc = betweenness_centrality(&g).unwrap();
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
        assert_eq!(g.nodes(), [] as [usize; 0]);
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
