//! Property-based tests for `graph_scoring` — PageRank and betweenness centrality.

#![allow(clippy::float_cmp)]

use proptest::prelude::*;

use frankenterm_core::graph_scoring::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_pagerank_config() -> impl Strategy<Value = PageRankConfig> {
    (0.5..0.99f64, 10..200usize, -10.0..-4.0f64).prop_map(|(damping, max_iter, tol_exp)| {
        PageRankConfig {
            damping,
            max_iterations: max_iter,
            tolerance: 10.0f64.powf(tol_exp),
        }
    })
}

/// Generate a random graph with n nodes and random edges.
fn arb_graph(max_nodes: usize) -> impl Strategy<Value = AdjGraph> {
    (1..max_nodes).prop_flat_map(|n| {
        let max_edges = n * n;
        let edge_count = 0..max_edges.min(20);
        (Just(n), proptest::collection::vec((0..n, 0..n), edge_count)).prop_map(|(n, edges)| {
            let mut g = AdjGraph::new(n);
            for (src, dst) in edges {
                if src != dst {
                    g.add_edge(src, dst);
                }
            }
            g
        })
    })
}

/// Generate a chain graph 0→1→...→(n-1).
fn arb_chain(max_n: usize) -> impl Strategy<Value = AdjGraph> {
    (2..max_n).prop_map(|n| {
        let mut g = AdjGraph::new(n);
        for i in 0..n - 1 {
            g.add_edge(i, i + 1);
        }
        g
    })
}

/// Generate a cycle graph.
fn arb_cycle(max_n: usize) -> impl Strategy<Value = AdjGraph> {
    (3..max_n).prop_map(|n| {
        let mut g = AdjGraph::new(n);
        for i in 0..n {
            g.add_edge(i, (i + 1) % n);
        }
        g
    })
}

/// Generate a complete graph.
fn arb_complete(max_n: usize) -> impl Strategy<Value = AdjGraph> {
    (2..max_n).prop_map(|n| {
        let mut g = AdjGraph::new(n);
        for i in 0..n {
            for j in 0..n {
                if i != j {
                    g.add_edge(i, j);
                }
            }
        }
        g
    })
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. PageRank scores sum to ~1.0 for any graph
    #[test]
    fn pagerank_scores_sum_to_one(graph in arb_graph(15)) {
        let result = pagerank(&graph, &PageRankConfig::default());
        if graph.node_count() > 0 {
            let total: f64 = result.scores.values().sum();
            prop_assert!((total - 1.0).abs() < 0.02, "sum={}", total);
        }
    }

    // 2. PageRank returns scores for all nodes
    #[test]
    fn pagerank_covers_all_nodes(graph in arb_graph(15)) {
        let result = pagerank(&graph, &PageRankConfig::default());
        prop_assert_eq!(result.scores.len(), graph.node_count());
    }

    // 3. PageRank scores are non-negative
    #[test]
    fn pagerank_scores_nonneg(graph in arb_graph(15)) {
        let result = pagerank(&graph, &PageRankConfig::default());
        for (&_node, &score) in &result.scores {
            prop_assert!(score >= 0.0);
        }
    }

    // 4. PageRank on empty graph returns empty scores
    #[test]
    fn pagerank_empty(_dummy in 0..1u8) {
        let g = AdjGraph::new(0);
        let result = pagerank(&g, &PageRankConfig::default());
        prop_assert!(result.scores.is_empty());
        prop_assert!(result.converged);
        prop_assert_eq!(result.iterations, 0);
    }

    // 5. PageRank on single node returns 1.0
    #[test]
    fn pagerank_single(_dummy in 0..1u8) {
        let g = AdjGraph::new(1);
        let result = pagerank(&g, &PageRankConfig::default());
        prop_assert!((result.scores[&0] - 1.0).abs() < 0.01);
    }

    // 6. PageRank on cycle: all nodes equal
    #[test]
    fn pagerank_cycle_equal(g in arb_cycle(10)) {
        let n = g.node_count();
        let expected = 1.0 / n as f64;
        let result = pagerank(&g, &PageRankConfig::default());
        for i in 0..n {
            prop_assert!((result.scores[&i] - expected).abs() < 0.02);
        }
    }

    // 7. PageRank on complete graph: all nodes equal
    #[test]
    fn pagerank_complete_equal(g in arb_complete(8)) {
        let n = g.node_count();
        let expected = 1.0 / n as f64;
        let result = pagerank(&g, &PageRankConfig::default());
        for i in 0..n {
            prop_assert!((result.scores[&i] - expected).abs() < 0.02);
        }
    }

    // 8. PageRank on chain: last node has highest rank
    #[test]
    fn pagerank_chain_last_highest(g in arb_chain(10)) {
        let n = g.node_count();
        let result = pagerank(&g, &PageRankConfig::default());
        let last_rank = result.scores[&(n - 1)];
        let first_rank = result.scores[&0];
        prop_assert!(last_rank > first_rank);
    }

    // 9. PageRank converges for reasonable configs
    #[test]
    fn pagerank_converges(graph in arb_graph(10), config in arb_pagerank_config()) {
        let result = pagerank(&graph, &config);
        // Should at least run and produce results
        prop_assert_eq!(result.scores.len(), graph.node_count());
    }

    // 10. PageRank with custom damping still sums to ~1.0
    #[test]
    fn pagerank_custom_damping_sums(
        graph in arb_graph(10),
        damping in 0.5..0.99f64,
    ) {
        let config = PageRankConfig { damping, ..Default::default() };
        let result = pagerank(&graph, &config);
        if graph.node_count() > 0 {
            let total: f64 = result.scores.values().sum();
            prop_assert!((total - 1.0).abs() < 0.02, "sum={}", total);
        }
    }

    // 11. Betweenness centrality returns scores for all nodes
    #[test]
    fn betweenness_covers_all_nodes(graph in arb_graph(10)) {
        let result = betweenness_centrality(&graph);
        prop_assert_eq!(result.scores.len(), graph.node_count());
    }

    // 12. Betweenness scores are non-negative
    #[test]
    fn betweenness_nonneg(graph in arb_graph(10)) {
        let result = betweenness_centrality(&graph);
        for (&_node, &score) in &result.scores {
            prop_assert!(score >= 0.0);
        }
    }

    // 13. Betweenness on empty graph returns empty
    #[test]
    fn betweenness_empty(_dummy in 0..1u8) {
        let g = AdjGraph::new(0);
        let result = betweenness_centrality(&g);
        prop_assert!(result.scores.is_empty());
    }

    // 14. Betweenness on single node: score is 0
    #[test]
    fn betweenness_single(_dummy in 0..1u8) {
        let g = AdjGraph::new(1);
        let result = betweenness_centrality(&g);
        prop_assert_eq!(result.scores[&0], 0.0);
    }

    // 15. Betweenness on cycle: all nodes equal
    #[test]
    fn betweenness_cycle_equal(g in arb_cycle(8)) {
        let n = g.node_count();
        let result = betweenness_centrality(&g);
        let first = result.scores[&0];
        for i in 1..n {
            prop_assert!((result.scores[&i] - first).abs() < 0.01);
        }
    }

    // 16. Betweenness on complete graph: all nodes equal
    #[test]
    fn betweenness_complete_equal(g in arb_complete(6)) {
        let n = g.node_count();
        let result = betweenness_centrality(&g);
        let first = result.scores[&0];
        for i in 1..n {
            prop_assert!((result.scores[&i] - first).abs() < 0.01);
        }
    }

    // 17. Betweenness chain: source node is 0
    #[test]
    fn betweenness_chain_source_zero(g in arb_chain(8)) {
        let result = betweenness_centrality(&g);
        prop_assert_eq!(result.scores[&0], 0.0);
    }

    // 18. Betweenness chain: last node is 0 (no paths through terminal node)
    #[test]
    fn betweenness_chain_terminal_zero(g in arb_chain(8)) {
        let n = g.node_count();
        let result = betweenness_centrality(&g);
        prop_assert_eq!(result.scores[&(n - 1)], 0.0);
    }

    // 19. Betweenness chain: middle nodes have higher scores than endpoints
    #[test]
    fn betweenness_chain_middle_highest(g in arb_chain(8)) {
        let n = g.node_count();
        if n >= 3 {
            let result = betweenness_centrality(&g);
            let mid = n / 2;
            prop_assert!(result.scores[&mid] > result.scores[&0]);
            prop_assert!(result.scores[&mid] > result.scores[&(n - 1)]);
        }
    }

    // 20. normalize_betweenness produces values in [0, 1]
    #[test]
    fn normalize_produces_unit_range(g in arb_chain(10)) {
        let n = g.node_count();
        let mut result = betweenness_centrality(&g);
        normalize_betweenness(&mut result.scores, n);
        for &score in result.scores.values() {
            prop_assert!(score >= 0.0);
            prop_assert!(score <= 1.0);
        }
    }

    // 21. normalize_betweenness on small graph is a no-op
    #[test]
    fn normalize_small_noop(n in 0..3usize) {
        let g = AdjGraph::new(n);
        let mut result = betweenness_centrality(&g);
        let before: Vec<f64> = result.scores.values().copied().collect();
        normalize_betweenness(&mut result.scores, n);
        let after: Vec<f64> = result.scores.values().copied().collect();
        prop_assert_eq!(before, after);
    }

    // 22. AdjGraph: node_count matches construction
    #[test]
    fn adj_graph_node_count(n in 0..100usize) {
        let g = AdjGraph::new(n);
        prop_assert_eq!(g.node_count(), n);
    }

    // 23. AdjGraph: nodes() returns 0..n
    #[test]
    fn adj_graph_nodes_sequential(n in 0..50usize) {
        let g = AdjGraph::new(n);
        let nodes = g.nodes();
        let expected: Vec<usize> = (0..n).collect();
        prop_assert_eq!(nodes, expected);
    }

    // 24. AdjGraph: successors/predecessors reflect edges
    #[test]
    fn adj_graph_edge_consistency(n in 2..20usize, src in 0..20usize, dst in 0..20usize) {
        prop_assume!(src < n && dst < n && src != dst);
        let mut g = AdjGraph::new(n);
        g.add_edge(src, dst);
        prop_assert!(g.successors(src).contains(&dst));
        prop_assert!(g.predecessors(dst).contains(&src));
    }

    // 25. AdjGraph: no edges → empty successors and predecessors
    #[test]
    fn adj_graph_no_edges_empty(n in 1..20usize, node in 0..20usize) {
        prop_assume!(node < n);
        let g = AdjGraph::new(n);
        prop_assert!(g.successors(node).is_empty());
        prop_assert!(g.predecessors(node).is_empty());
    }

    // 26. PageRankConfig default values
    #[test]
    fn pagerank_config_defaults(_dummy in 0..1u8) {
        let config = PageRankConfig::default();
        prop_assert!((config.damping - 0.85).abs() < 1e-10);
        prop_assert_eq!(config.max_iterations, 100);
        prop_assert!((config.tolerance - 1e-6).abs() < 1e-12);
    }

    // 27. PageRank with max_iterations=1 terminates
    #[test]
    fn pagerank_single_iteration(graph in arb_graph(10)) {
        let config = PageRankConfig {
            max_iterations: 1,
            tolerance: 1e-20,
            ..Default::default()
        };
        let result = pagerank(&graph, &config);
        if graph.node_count() > 0 {
            prop_assert_eq!(result.iterations, 1);
        }
    }

    // 28. Both algorithms return same node count
    #[test]
    fn both_algorithms_same_nodes(graph in arb_graph(10)) {
        let pr = pagerank(&graph, &PageRankConfig::default());
        let bc = betweenness_centrality(&graph);
        prop_assert_eq!(pr.scores.len(), bc.scores.len());
    }

    // 29. AdjGraph clone produces equivalent graph
    #[test]
    fn adj_graph_clone(graph in arb_graph(10)) {
        let cloned = graph.clone();
        prop_assert_eq!(graph.node_count(), cloned.node_count());
        for node in graph.nodes() {
            let mut s1 = graph.successors(node);
            let mut s2 = cloned.successors(node);
            s1.sort();
            s2.sort();
            prop_assert_eq!(s1, s2);
        }
    }

    // 30. PageRank deterministic: same graph + config → same scores
    #[test]
    fn pagerank_deterministic(graph in arb_graph(10)) {
        let config = PageRankConfig::default();
        let r1 = pagerank(&graph, &config);
        let r2 = pagerank(&graph, &config);
        for node in graph.nodes() {
            let diff = (r1.scores[&node] - r2.scores[&node]).abs();
            prop_assert!(diff < 1e-10);
        }
    }
}
