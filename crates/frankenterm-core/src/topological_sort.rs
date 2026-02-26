//! Graph algorithms: topological sort, strongly connected components, and cycle detection.
//!
//! Provides Kahn's algorithm for topological ordering of DAGs, Tarjan's algorithm
//! for finding strongly connected components, and utilities for cycle detection
//! and dependency analysis. Useful for workflow execution ordering, deadlock
//! detection, and session replay sequencing.
//!
//! # Node representation
//!
//! Nodes are identified by `usize` indices in `[0, n)`. The graph is stored as
//! an adjacency list.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// A directed graph stored as an adjacency list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiGraph {
    /// Number of nodes.
    n: usize,
    /// Adjacency list: `adj[u]` contains successors of `u`.
    adj: Vec<Vec<usize>>,
}

impl DiGraph {
    /// Create a new graph with `n` nodes and no edges.
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self {
            n,
            adj: vec![Vec::new(); n],
        }
    }

    /// Create from an edge list. Nodes are `0..n`.
    #[must_use]
    pub fn from_edges(n: usize, edges: &[(usize, usize)]) -> Self {
        let mut g = Self::new(n);
        for &(u, v) in edges {
            g.add_edge(u, v);
        }
        g
    }

    /// Add a directed edge from `u` to `v`.
    ///
    /// # Panics
    /// Panics if `u >= n` or `v >= n`.
    pub fn add_edge(&mut self, u: usize, v: usize) {
        assert!(u < self.n, "node {} out of range [0, {})", u, self.n);
        assert!(v < self.n, "node {} out of range [0, {})", v, self.n);
        self.adj[u].push(v);
    }

    /// Number of nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.n
    }

    /// Number of edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.adj.iter().map(|a| a.len()).sum()
    }

    /// Successors of node `u`.
    #[must_use]
    pub fn successors(&self, u: usize) -> &[usize] {
        &self.adj[u]
    }

    /// In-degree of each node.
    #[must_use]
    pub fn in_degrees(&self) -> Vec<usize> {
        let mut deg = vec![0usize; self.n];
        for neighbors in &self.adj {
            for &v in neighbors {
                deg[v] += 1;
            }
        }
        deg
    }

    /// Out-degree of each node.
    #[must_use]
    pub fn out_degrees(&self) -> Vec<usize> {
        self.adj.iter().map(|a| a.len()).collect()
    }

    /// Reverse (transpose) graph.
    #[must_use]
    pub fn reverse(&self) -> DiGraph {
        let mut rev = DiGraph::new(self.n);
        for u in 0..self.n {
            for &v in &self.adj[u] {
                rev.adj[v].push(u);
            }
        }
        rev
    }

    /// Whether the graph contains a node with no predecessors.
    #[must_use]
    pub fn has_source(&self) -> bool {
        let deg = self.in_degrees();
        deg.contains(&0)
    }

    /// Whether the graph contains a node with no successors.
    #[must_use]
    pub fn has_sink(&self) -> bool {
        self.adj.iter().any(|a| a.is_empty())
    }

    /// Source nodes (in-degree 0).
    #[must_use]
    pub fn sources(&self) -> Vec<usize> {
        let deg = self.in_degrees();
        (0..self.n).filter(|&u| deg[u] == 0).collect()
    }

    /// Sink nodes (out-degree 0).
    #[must_use]
    pub fn sinks(&self) -> Vec<usize> {
        (0..self.n).filter(|&u| self.adj[u].is_empty()).collect()
    }
}

/// Result of a topological sort attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopoResult {
    /// A valid topological ordering.
    Order(Vec<usize>),
    /// The graph has a cycle; the cycle nodes are returned.
    Cycle(Vec<usize>),
}

impl TopoResult {
    /// Returns the ordering if acyclic, or `None` if cyclic.
    #[must_use]
    pub fn order(&self) -> Option<&[usize]> {
        match self {
            TopoResult::Order(v) => Some(v),
            TopoResult::Cycle(_) => None,
        }
    }

    /// Returns true if the graph is a DAG (acyclic).
    #[must_use]
    pub fn is_dag(&self) -> bool {
        matches!(self, TopoResult::Order(_))
    }
}

/// Topological sort using Kahn's algorithm (BFS-based).
///
/// Returns `TopoResult::Order` if the graph is a DAG, or
/// `TopoResult::Cycle` with remaining nodes if cyclic.
///
/// Kahn's algorithm naturally produces a deterministic ordering
/// (smallest available node first when using a sorted queue).
#[must_use]
pub fn topological_sort(g: &DiGraph) -> TopoResult {
    let n = g.node_count();
    let mut in_deg = g.in_degrees();

    let mut queue: VecDeque<usize> = VecDeque::new();
    // Process sources in order for determinism
    let mut sources: Vec<usize> = (0..n).filter(|&u| in_deg[u] == 0).collect();
    sources.sort_unstable();
    for s in sources {
        queue.push_back(s);
    }

    let mut order = Vec::with_capacity(n);

    while let Some(u) = queue.pop_front() {
        order.push(u);
        // Collect and sort neighbors for determinism
        let mut neighbors: Vec<usize> = g.successors(u).to_vec();
        neighbors.sort_unstable();
        for v in neighbors {
            in_deg[v] -= 1;
            if in_deg[v] == 0 {
                queue.push_back(v);
            }
        }
    }

    if order.len() == n {
        TopoResult::Order(order)
    } else {
        // Remaining nodes form the cycle
        let in_order: Vec<bool> = {
            let mut v = vec![false; n];
            for &u in &order {
                v[u] = true;
            }
            v
        };
        let cycle_nodes: Vec<usize> = (0..n).filter(|&u| !in_order[u]).collect();
        TopoResult::Cycle(cycle_nodes)
    }
}

/// Check if the graph is a DAG (has no cycles).
#[must_use]
pub fn is_dag(g: &DiGraph) -> bool {
    topological_sort(g).is_dag()
}

/// Find all strongly connected components using Tarjan's algorithm.
///
/// Returns components in reverse topological order (sinks first).
/// Each component is a `Vec<usize>` of node indices.
#[must_use]
pub fn tarjan_scc(g: &DiGraph) -> Vec<Vec<usize>> {
    let n = g.node_count();
    let mut index_counter = 0usize;
    let mut stack = Vec::new();
    let mut on_stack = vec![false; n];
    let mut indices = vec![usize::MAX; n];
    let mut lowlinks = vec![0usize; n];
    let mut result = Vec::new();

    fn strongconnect(
        u: usize,
        g: &DiGraph,
        index_counter: &mut usize,
        stack: &mut Vec<usize>,
        on_stack: &mut [bool],
        indices: &mut [usize],
        lowlinks: &mut [usize],
        result: &mut Vec<Vec<usize>>,
    ) {
        // Use iterative DFS to avoid stack overflow
        #[derive(Debug)]
        struct Frame {
            node: usize,
            neighbor_idx: usize,
        }

        let mut call_stack = vec![Frame {
            node: u,
            neighbor_idx: 0,
        }];

        indices[u] = *index_counter;
        lowlinks[u] = *index_counter;
        *index_counter += 1;
        stack.push(u);
        on_stack[u] = true;

        while let Some(frame) = call_stack.last_mut() {
            let node = frame.node;
            let successors = g.successors(node);

            if frame.neighbor_idx < successors.len() {
                let w = successors[frame.neighbor_idx];
                frame.neighbor_idx += 1;

                if indices[w] == usize::MAX {
                    // Not yet visited — push new frame
                    indices[w] = *index_counter;
                    lowlinks[w] = *index_counter;
                    *index_counter += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    call_stack.push(Frame {
                        node: w,
                        neighbor_idx: 0,
                    });
                } else if on_stack[w] {
                    lowlinks[node] = lowlinks[node].min(indices[w]);
                }
            } else {
                // Done processing all neighbors
                if lowlinks[node] == indices[node] {
                    let mut component = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        component.push(w);
                        if w == node {
                            break;
                        }
                    }
                    component.sort_unstable();
                    result.push(component);
                }

                call_stack.pop();
                // Update parent's lowlink
                if let Some(parent) = call_stack.last() {
                    lowlinks[parent.node] = lowlinks[parent.node].min(lowlinks[node]);
                }
            }
        }
    }

    for u in 0..n {
        if indices[u] == usize::MAX {
            strongconnect(
                u,
                g,
                &mut index_counter,
                &mut stack,
                &mut on_stack,
                &mut indices,
                &mut lowlinks,
                &mut result,
            );
        }
    }

    result
}

/// Detect if the graph contains a cycle.
#[must_use]
pub fn has_cycle(g: &DiGraph) -> bool {
    !is_dag(g)
}

/// Find one cycle in the graph, if any exists.
///
/// Returns `None` if the graph is a DAG.
#[must_use]
pub fn find_cycle(g: &DiGraph) -> Option<Vec<usize>> {
    let n = g.node_count();
    let mut color = vec![0u8; n]; // 0=white, 1=gray, 2=black
    let mut parent = vec![usize::MAX; n];

    for start in 0..n {
        if color[start] != 0 {
            continue;
        }

        let mut stack = vec![(start, 0usize)];
        color[start] = 1;

        while let Some((u, idx)) = stack.last_mut() {
            let successors = g.successors(*u);
            if *idx < successors.len() {
                let v = successors[*idx];
                *idx += 1;

                if color[v] == 1 {
                    // Found a back edge — reconstruct cycle
                    let mut cycle = vec![v];
                    let mut cur = *u;
                    while cur != v {
                        cycle.push(cur);
                        cur = parent[cur];
                    }
                    cycle.push(v);
                    cycle.reverse();
                    return Some(cycle);
                } else if color[v] == 0 {
                    parent[v] = *u;
                    color[v] = 1;
                    stack.push((v, 0));
                }
            } else {
                color[*u] = 2;
                stack.pop();
            }
        }
    }

    None
}

/// Compute the condensation DAG (SCC graph).
///
/// Returns `(scc_assignment, condensed_graph)` where `scc_assignment[u]` is the
/// SCC index for node `u`.
#[must_use]
pub fn condensation(g: &DiGraph) -> (Vec<usize>, DiGraph) {
    let sccs = tarjan_scc(g);
    let n = g.node_count();
    let num_sccs = sccs.len();

    let mut scc_of = vec![0usize; n];
    for (i, scc) in sccs.iter().enumerate() {
        for &u in scc {
            scc_of[u] = i;
        }
    }

    let mut condensed = DiGraph::new(num_sccs);
    let mut edges = Vec::new();
    for u in 0..n {
        for &v in g.successors(u) {
            let su = scc_of[u];
            let sv = scc_of[v];
            if su != sv {
                edges.push((su, sv));
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    for (su, sv) in edges {
        condensed.add_edge(su, sv);
    }

    (scc_of, condensed)
}

/// BFS from sources to compute the longest path to each node (critical path length).
///
/// Returns `None` if the graph has a cycle.
#[must_use]
pub fn longest_paths(g: &DiGraph) -> Option<Vec<usize>> {
    let result = topological_sort(g);
    let order = result.order()?;
    let n = g.node_count();

    let mut dist = vec![0usize; n];
    for &u in order {
        for &v in g.successors(u) {
            dist[v] = dist[v].max(dist[u] + 1);
        }
    }

    Some(dist)
}

/// Compute the transitive closure of the graph.
///
/// Returns a matrix `reach[u][v]` indicating if `v` is reachable from `u`.
#[must_use]
pub fn transitive_closure(g: &DiGraph) -> Vec<Vec<bool>> {
    let n = g.node_count();
    let mut reach = vec![vec![false; n]; n];

    // Initialize direct edges
    for (u, row) in reach.iter_mut().enumerate().take(n) {
        row[u] = true;
        for &v in g.successors(u) {
            row[v] = true;
        }
    }

    // Floyd-Warshall style
    for k in 0..n {
        for i in 0..n {
            for j in 0..n {
                if reach[i][k] && reach[k][j] {
                    reach[i][j] = true;
                }
            }
        }
    }

    reach
}

/// BFS-based reachability from a single source node.
#[must_use]
pub fn reachable_from(g: &DiGraph, source: usize) -> Vec<bool> {
    let n = g.node_count();
    let mut visited = vec![false; n];
    let mut queue = VecDeque::new();
    visited[source] = true;
    queue.push_back(source);

    while let Some(u) = queue.pop_front() {
        for &v in g.successors(u) {
            if !visited[v] {
                visited[v] = true;
                queue.push_back(v);
            }
        }
    }

    visited
}

/// Compute topological levels (parallel execution tiers).
///
/// Nodes at the same level can be executed concurrently.
/// Returns `None` if the graph has a cycle.
#[must_use]
pub fn parallel_levels(g: &DiGraph) -> Option<Vec<Vec<usize>>> {
    let dists = longest_paths(g)?;
    let max_level = dists.iter().copied().max().unwrap_or(0);

    let mut levels = vec![Vec::new(); max_level + 1];
    for (u, &d) in dists.iter().enumerate() {
        levels[d].push(u);
    }

    Some(levels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diamond() -> DiGraph {
        //   0
        //  / \
        // 1   2
        //  \ /
        //   3
        DiGraph::from_edges(4, &[(0, 1), (0, 2), (1, 3), (2, 3)])
    }

    fn linear() -> DiGraph {
        // 0 -> 1 -> 2 -> 3
        DiGraph::from_edges(4, &[(0, 1), (1, 2), (2, 3)])
    }

    fn cycle3() -> DiGraph {
        // 0 -> 1 -> 2 -> 0
        DiGraph::from_edges(3, &[(0, 1), (1, 2), (2, 0)])
    }

    // -- DiGraph basics --

    #[test]
    fn new_graph() {
        let g = DiGraph::new(5);
        assert_eq!(g.node_count(), 5);
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn add_edge_basic() {
        let mut g = DiGraph::new(3);
        g.add_edge(0, 1);
        g.add_edge(1, 2);
        assert_eq!(g.edge_count(), 2);
        assert_eq!(g.successors(0), &[1]);
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn add_edge_out_of_range() {
        let mut g = DiGraph::new(3);
        g.add_edge(0, 5);
    }

    #[test]
    fn in_out_degrees() {
        let g = diamond();
        assert_eq!(g.in_degrees(), vec![0, 1, 1, 2]);
        assert_eq!(g.out_degrees(), vec![2, 1, 1, 0]);
    }

    #[test]
    fn sources_and_sinks() {
        let g = diamond();
        assert_eq!(g.sources(), vec![0]);
        assert_eq!(g.sinks(), vec![3]);
    }

    #[test]
    fn reverse_graph() {
        let g = linear();
        let rev = g.reverse();
        assert_eq!(rev.successors(3), &[2]);
        assert_eq!(rev.successors(0).len(), 0);
    }

    // -- Topological Sort --

    #[test]
    fn topo_linear() {
        let g = linear();
        let result = topological_sort(&g);
        assert_eq!(result.order(), Some([0, 1, 2, 3].as_slice()));
    }

    #[test]
    fn topo_diamond() {
        let g = diamond();
        let result = topological_sort(&g);
        let order = result.order().unwrap();
        assert_eq!(order.len(), 4);
        assert_eq!(order[0], 0);
        assert_eq!(order[3], 3);
    }

    #[test]
    fn topo_cycle() {
        let g = cycle3();
        let result = topological_sort(&g);
        assert!(!result.is_dag());
        if let TopoResult::Cycle(nodes) = result {
            assert_eq!(nodes.len(), 3);
        }
    }

    #[test]
    fn topo_empty() {
        let g = DiGraph::new(0);
        assert!(topological_sort(&g).is_dag());
    }

    #[test]
    fn topo_disconnected() {
        let g = DiGraph::new(3); // no edges
        let result = topological_sort(&g);
        let order = result.order().unwrap();
        assert_eq!(order.len(), 3);
    }

    // -- is_dag / has_cycle --

    #[test]
    fn dag_check() {
        assert!(is_dag(&diamond()));
        assert!(!is_dag(&cycle3()));
    }

    #[test]
    fn has_cycle_check() {
        assert!(!has_cycle(&diamond()));
        assert!(has_cycle(&cycle3()));
    }

    // -- Tarjan SCC --

    #[test]
    fn scc_dag() {
        let g = diamond();
        let sccs = tarjan_scc(&g);
        // DAG: each node is its own SCC
        assert_eq!(sccs.len(), 4);
        for scc in &sccs {
            assert_eq!(scc.len(), 1);
        }
    }

    #[test]
    fn scc_cycle() {
        let g = cycle3();
        let sccs = tarjan_scc(&g);
        assert_eq!(sccs.len(), 1);
        assert_eq!(sccs[0].len(), 3);
    }

    #[test]
    fn scc_mixed() {
        // 0->1->2->0 (cycle), 2->3
        let g = DiGraph::from_edges(4, &[(0, 1), (1, 2), (2, 0), (2, 3)]);
        let sccs = tarjan_scc(&g);
        assert_eq!(sccs.len(), 2);
    }

    #[test]
    fn scc_empty() {
        let g = DiGraph::new(0);
        let sccs = tarjan_scc(&g);
        assert!(sccs.is_empty());
    }

    // -- find_cycle --

    #[test]
    fn find_cycle_exists() {
        let g = cycle3();
        let cycle = find_cycle(&g);
        assert!(cycle.is_some());
        let c = cycle.unwrap();
        assert!(c.len() >= 2);
        assert_eq!(c.first(), c.last()); // cycle returns to start
    }

    #[test]
    fn find_cycle_none() {
        let g = diamond();
        assert!(find_cycle(&g).is_none());
    }

    // -- Condensation --

    #[test]
    fn condensation_dag() {
        let g = diamond();
        let (scc_of, cond) = condensation(&g);
        assert_eq!(cond.node_count(), 4);
        // each node in own SCC
        let unique: std::collections::HashSet<usize> = scc_of.iter().copied().collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn condensation_cycle() {
        let g = cycle3();
        let (scc_of, cond) = condensation(&g);
        assert_eq!(cond.node_count(), 1);
        assert!(scc_of.iter().all(|&s| s == scc_of[0]));
    }

    // -- Longest paths --

    #[test]
    fn longest_linear() {
        let g = linear();
        let dists = longest_paths(&g).unwrap();
        assert_eq!(dists, vec![0, 1, 2, 3]);
    }

    #[test]
    fn longest_diamond() {
        let g = diamond();
        let dists = longest_paths(&g).unwrap();
        assert_eq!(dists[0], 0);
        assert_eq!(dists[3], 2);
    }

    #[test]
    fn longest_cycle_none() {
        let g = cycle3();
        assert!(longest_paths(&g).is_none());
    }

    // -- Transitive closure --

    #[test]
    fn closure_linear() {
        let g = linear();
        let reach = transitive_closure(&g);
        assert!(reach[0][3]); // 0 reaches 3
        assert!(!reach[3][0]); // 3 does not reach 0
    }

    #[test]
    fn closure_reflexive() {
        let g = diamond();
        let reach = transitive_closure(&g);
        for (i, row) in reach.iter().enumerate().take(4) {
            assert!(row[i]);
        }
    }

    // -- Reachability --

    #[test]
    fn reachable_linear() {
        let g = linear();
        let r = reachable_from(&g, 0);
        assert!(r.iter().all(|&v| v)); // all reachable from 0
    }

    #[test]
    fn reachable_sink() {
        let g = linear();
        let r = reachable_from(&g, 3);
        assert!(r[3]); // only self
        assert!(!r[0]);
    }

    // -- Parallel levels --

    #[test]
    fn levels_diamond() {
        let g = diamond();
        let levels = parallel_levels(&g).unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec![0]);
        assert!(levels[1].contains(&1) && levels[1].contains(&2));
        assert_eq!(levels[2], vec![3]);
    }

    #[test]
    fn levels_linear() {
        let g = linear();
        let levels = parallel_levels(&g).unwrap();
        assert_eq!(levels.len(), 4);
        for (i, level) in levels.iter().enumerate() {
            assert_eq!(level, &[i]);
        }
    }

    #[test]
    fn levels_cycle_none() {
        let g = cycle3();
        assert!(parallel_levels(&g).is_none());
    }

    // -- Serde --

    #[test]
    fn serde_roundtrip() {
        let g = diamond();
        let json = serde_json::to_string(&g).unwrap();
        let back: DiGraph = serde_json::from_str(&json).unwrap();
        assert_eq!(g, back);
    }

    // -- Edge cases --

    #[test]
    fn self_loop_is_cycle() {
        let g = DiGraph::from_edges(1, &[(0, 0)]);
        assert!(has_cycle(&g));
    }

    #[test]
    fn scc_self_loop() {
        let g = DiGraph::from_edges(2, &[(0, 0), (0, 1)]);
        let sccs = tarjan_scc(&g);
        // Node 0 has self-loop → SCC of size 1 (self-loop counts)
        assert_eq!(sccs.iter().filter(|s| s.contains(&0)).count(), 1);
    }

    #[test]
    fn from_edges_basic() {
        let g = DiGraph::from_edges(3, &[(0, 1), (1, 2)]);
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.edge_count(), 2);
    }

    #[test]
    fn has_source_sink() {
        let g = diamond();
        assert!(g.has_source());
        assert!(g.has_sink());

        let g2 = cycle3();
        assert!(!g2.has_source());
        assert!(!g2.has_sink());
    }
}
