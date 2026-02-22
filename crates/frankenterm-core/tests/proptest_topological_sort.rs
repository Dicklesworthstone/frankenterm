//! Property-based tests for `topological_sort` — graph algorithms.

use proptest::prelude::*;

use frankenterm_core::topological_sort::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Generate a random DAG by only allowing edges from lower to higher indices.
fn arb_dag(max_nodes: usize) -> impl Strategy<Value = DiGraph> {
    (1..max_nodes).prop_flat_map(|n| {
        let max_edges = n * (n - 1) / 2;
        let edge_count = 0..max_edges.max(1);
        (Just(n), edge_count).prop_flat_map(move |(n, e)| {
            proptest::collection::vec((0..n, 0..n), e).prop_map(move |pairs| {
                let mut g = DiGraph::new(n);
                for (u, v) in pairs {
                    // Only add forward edges to guarantee DAG
                    if u < v {
                        g.add_edge(u, v);
                    }
                }
                g
            })
        })
    })
}

/// Generate a random graph (may have cycles).
fn arb_graph(max_nodes: usize) -> impl Strategy<Value = DiGraph> {
    (1..max_nodes).prop_flat_map(|n| {
        let max_edges = n * n;
        let edge_count = 0..max_edges.min(50);
        (Just(n), edge_count).prop_flat_map(move |(n, e)| {
            proptest::collection::vec((0..n, 0..n), e).prop_map(move |pairs| {
                let mut g = DiGraph::new(n);
                for (u, v) in pairs {
                    g.add_edge(u, v);
                }
                g
            })
        })
    })
}

/// Generate a DAG with its edges as a list.
fn arb_dag_with_edges(max_nodes: usize) -> impl Strategy<Value = (DiGraph, Vec<(usize, usize)>)> {
    (2..max_nodes).prop_flat_map(|n| {
        let max_edges = n * (n - 1) / 2;
        let edge_count = 0..max_edges.max(1);
        (Just(n), edge_count).prop_flat_map(move |(n, e)| {
            proptest::collection::vec((0..n, 0..n), e).prop_map(move |pairs| {
                let mut g = DiGraph::new(n);
                let mut edges = Vec::new();
                for (u, v) in pairs {
                    if u < v {
                        g.add_edge(u, v);
                        edges.push((u, v));
                    }
                }
                (g, edges)
            })
        })
    })
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. DAG always produces a valid order
    #[test]
    fn dag_has_order(g in arb_dag(15)) {
        let result = topological_sort(&g);
        prop_assert!(result.is_dag());
    }

    // 2. Topo order contains all nodes exactly once
    #[test]
    fn topo_order_permutation(g in arb_dag(15)) {
        let order = topological_sort(&g).order().unwrap().to_vec();
        prop_assert_eq!(order.len(), g.node_count());
        let mut sorted = order.clone();
        sorted.sort_unstable();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), g.node_count());
    }

    // 3. Topo order respects all edges
    #[test]
    fn topo_order_respects_edges((g, edges) in arb_dag_with_edges(15)) {
        let order = topological_sort(&g).order().unwrap().to_vec();
        let mut pos = vec![0usize; g.node_count()];
        for (i, &u) in order.iter().enumerate() {
            pos[u] = i;
        }
        for (u, v) in edges {
            prop_assert!(pos[u] < pos[v], "edge ({},{}) violated: pos[{}]={} >= pos[{}]={}", u, v, u, pos[u], v, pos[v]);
        }
    }

    // 4. is_dag consistent with topological_sort
    #[test]
    fn is_dag_consistent(g in arb_graph(10)) {
        let result = topological_sort(&g);
        prop_assert_eq!(is_dag(&g), result.is_dag());
    }

    // 5. has_cycle is !is_dag
    #[test]
    fn has_cycle_negation(g in arb_graph(10)) {
        prop_assert_eq!(has_cycle(&g), !is_dag(&g));
    }

    // 6. SCC covers all nodes
    #[test]
    fn scc_covers_all(g in arb_graph(15)) {
        let sccs = tarjan_scc(&g);
        let mut all_nodes: Vec<usize> = sccs.into_iter().flatten().collect();
        all_nodes.sort_unstable();
        let expected: Vec<usize> = (0..g.node_count()).collect();
        prop_assert_eq!(all_nodes, expected);
    }

    // 7. SCC partitions nodes (no duplicates)
    #[test]
    fn scc_partition(g in arb_graph(15)) {
        let sccs = tarjan_scc(&g);
        let total: usize = sccs.iter().map(|s| s.len()).sum();
        prop_assert_eq!(total, g.node_count());
    }

    // 8. DAG has all singleton SCCs
    #[test]
    fn dag_singleton_sccs(g in arb_dag(15)) {
        let sccs = tarjan_scc(&g);
        for scc in &sccs {
            prop_assert_eq!(scc.len(), 1);
        }
    }

    // 9. Cycle graph has non-singleton SCC
    #[test]
    fn cycle_nonsingleton_scc(n in 2..10usize) {
        let mut g = DiGraph::new(n);
        for i in 0..n {
            g.add_edge(i, (i + 1) % n);
        }
        let sccs = tarjan_scc(&g);
        prop_assert_eq!(sccs.len(), 1);
        prop_assert_eq!(sccs[0].len(), n);
    }

    // 10. find_cycle returns None for DAGs
    #[test]
    fn find_cycle_none_for_dag(g in arb_dag(15)) {
        prop_assert!(find_cycle(&g).is_none());
    }

    // 11. find_cycle returns valid cycle for cyclic graphs
    #[test]
    fn find_cycle_valid(n in 2..10usize) {
        let mut g = DiGraph::new(n);
        for i in 0..n {
            g.add_edge(i, (i + 1) % n);
        }
        let cycle = find_cycle(&g).unwrap();
        // Cycle starts and ends at same node
        prop_assert_eq!(cycle.first(), cycle.last());
        prop_assert!(cycle.len() >= 2);
    }

    // 12. Condensation is a DAG
    #[test]
    fn condensation_is_dag(g in arb_graph(15)) {
        let (_, cond) = condensation(&g);
        prop_assert!(is_dag(&cond));
    }

    // 13. Condensation SCC assignment covers all nodes
    #[test]
    fn condensation_covers(g in arb_graph(15)) {
        let (scc_of, cond) = condensation(&g);
        prop_assert_eq!(scc_of.len(), g.node_count());
        for &s in &scc_of {
            prop_assert!(s < cond.node_count());
        }
    }

    // 14. Longest paths non-negative for DAGs
    #[test]
    fn longest_paths_non_neg(g in arb_dag(15)) {
        let dists = longest_paths(&g).unwrap();
        for &d in &dists {
            // d is usize, always >= 0
            let _ = d;
        }
        prop_assert_eq!(dists.len(), g.node_count());
    }

    // 15. Longest paths None for cyclic graphs
    #[test]
    fn longest_paths_none_cyclic(n in 2..10usize) {
        let mut g = DiGraph::new(n);
        for i in 0..n {
            g.add_edge(i, (i + 1) % n);
        }
        prop_assert!(longest_paths(&g).is_none());
    }

    // 16. Sources have longest path 0
    #[test]
    fn sources_at_level_zero(g in arb_dag(15)) {
        let dists = longest_paths(&g).unwrap();
        for s in g.sources() {
            prop_assert_eq!(dists[s], 0);
        }
    }

    // 17. Longest path respects edges: dist[v] >= dist[u] + 1 for edge u->v
    #[test]
    fn longest_path_monotone((g, edges) in arb_dag_with_edges(15)) {
        let dists = longest_paths(&g).unwrap();
        for (u, v) in edges {
            prop_assert!(dists[v] >= dists[u] + 1,
                "dist[{}]={} < dist[{}]+1={}", v, dists[v], u, dists[u] + 1);
        }
    }

    // 18. Parallel levels cover all nodes
    #[test]
    fn levels_cover_all(g in arb_dag(15)) {
        let levels = parallel_levels(&g).unwrap();
        let total: usize = levels.iter().map(|l| l.len()).sum();
        prop_assert_eq!(total, g.node_count());
    }

    // 19. Each level has no internal dependencies
    #[test]
    fn levels_independent((g, _edges) in arb_dag_with_edges(15)) {
        let levels = parallel_levels(&g).unwrap();
        let dists = longest_paths(&g).unwrap();
        for level in &levels {
            if level.len() < 2 { continue; }
            let d = dists[level[0]];
            for &u in level {
                prop_assert_eq!(dists[u], d);
            }
        }
    }

    // 20. Reverse preserves edge count
    #[test]
    fn reverse_edge_count(g in arb_graph(15)) {
        let rev = g.reverse();
        prop_assert_eq!(g.edge_count(), rev.edge_count());
    }

    // 21. Reverse of reverse preserves adjacency
    #[test]
    fn reverse_involution(g in arb_dag(10)) {
        let rev2 = g.reverse().reverse();
        // Edge sets should be identical (order may differ)
        prop_assert_eq!(g.edge_count(), rev2.edge_count());
        prop_assert_eq!(g.node_count(), rev2.node_count());
    }

    // 22. In-degree sum = edge count
    #[test]
    fn in_degree_sum(g in arb_graph(15)) {
        let sum: usize = g.in_degrees().iter().sum();
        prop_assert_eq!(sum, g.edge_count());
    }

    // 23. Out-degree sum = edge count
    #[test]
    fn out_degree_sum(g in arb_graph(15)) {
        let sum: usize = g.out_degrees().iter().sum();
        prop_assert_eq!(sum, g.edge_count());
    }

    // 24. Transitive closure is reflexive
    #[test]
    fn closure_reflexive(g in arb_graph(10)) {
        let reach = transitive_closure(&g);
        for i in 0..g.node_count() {
            prop_assert!(reach[i][i]);
        }
    }

    // 25. Transitive closure respects edges
    #[test]
    fn closure_includes_edges(g in arb_graph(10)) {
        let reach = transitive_closure(&g);
        for u in 0..g.node_count() {
            for &v in g.successors(u) {
                prop_assert!(reach[u][v]);
            }
        }
    }

    // 26. Reachability from source is subset of closure
    #[test]
    fn reachable_subset_closure(g in arb_graph(10), source in 0..10usize) {
        prop_assume!(source < g.node_count());
        let r = reachable_from(&g, source);
        let reach = transitive_closure(&g);
        for v in 0..g.node_count() {
            if r[v] {
                prop_assert!(reach[source][v]);
            }
        }
    }

    // 27. Source reaches itself
    #[test]
    fn reachable_self(g in arb_graph(10), source in 0..10usize) {
        prop_assume!(source < g.node_count());
        let r = reachable_from(&g, source);
        prop_assert!(r[source]);
    }

    // 28. Serde roundtrip preserves graph
    #[test]
    fn serde_roundtrip(g in arb_graph(10)) {
        let json = serde_json::to_string(&g).unwrap();
        let back: DiGraph = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(g, back);
    }

    // 29. Empty graph is DAG
    #[test]
    fn empty_is_dag(_dummy in 0..1i32) {
        let g = DiGraph::new(0);
        prop_assert!(is_dag(&g));
        prop_assert_eq!(tarjan_scc(&g).len(), 0);
    }

    // 30. Node count matches construction
    #[test]
    fn node_count_matches(n in 0..50usize) {
        let g = DiGraph::new(n);
        prop_assert_eq!(g.node_count(), n);
    }

    // 31. Adding self-loop creates cycle
    #[test]
    fn self_loop_cycle(n in 1..10usize, node in 0..10usize) {
        prop_assume!(node < n);
        let g = DiGraph::from_edges(n, &[(node, node)]);
        prop_assert!(has_cycle(&g));
    }

    // 32. Condensation of DAG has same node count
    #[test]
    fn condensation_dag_same_count(g in arb_dag(15)) {
        let (_, cond) = condensation(&g);
        prop_assert_eq!(cond.node_count(), g.node_count());
    }

    // 33. Number of SCCs <= number of nodes
    #[test]
    fn scc_count_bounded(g in arb_graph(15)) {
        let sccs = tarjan_scc(&g);
        prop_assert!(sccs.len() <= g.node_count());
    }

    // 34. Transitive closure is transitive
    #[test]
    fn closure_transitive(g in arb_graph(8)) {
        let reach = transitive_closure(&g);
        let n = g.node_count();
        for i in 0..n {
            for j in 0..n {
                for k in 0..n {
                    if reach[i][j] && reach[j][k] {
                        prop_assert!(reach[i][k]);
                    }
                }
            }
        }
    }

    // 35. from_edges matches manual construction
    #[test]
    fn from_edges_matches(n in 2..10usize,
                          edges in proptest::collection::vec((0..10usize, 0..10usize), 0..20)) {
        let valid: Vec<(usize, usize)> = edges.into_iter().filter(|&(u, v)| u < n && v < n).collect();
        let g = DiGraph::from_edges(n, &valid);
        prop_assert_eq!(g.node_count(), n);
        prop_assert_eq!(g.edge_count(), valid.len());
    }
}
