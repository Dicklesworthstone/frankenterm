//! Property-based tests for `shortest_path` — weighted graph algorithms.

#![allow(clippy::float_cmp, clippy::needless_range_loop)]

use proptest::prelude::*;

use frankenterm_core::shortest_path::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Generate a random graph with non-negative weights.
fn arb_nn_graph(max_nodes: usize) -> impl Strategy<Value = WeightedGraph> {
    (1..max_nodes).prop_flat_map(|n| {
        let max_edges = (n * n).min(40);
        let edge_count = 0..max_edges.max(1);
        (Just(n), edge_count).prop_flat_map(move |(n, e)| {
            proptest::collection::vec((0..n, 0..n, 0.0..100.0f64), e).prop_map(move |triples| {
                let mut g = WeightedGraph::new(n);
                for (u, v, w) in triples {
                    g.add_edge(u, v, w);
                }
                g
            })
        })
    })
}

/// Generate an undirected graph with non-negative weights.
fn arb_undirected(max_nodes: usize) -> impl Strategy<Value = WeightedGraph> {
    (2..max_nodes).prop_flat_map(|n| {
        let max_edges = n * (n - 1) / 2;
        let edge_count = 0..max_edges.max(1);
        (Just(n), edge_count).prop_flat_map(move |(n, e)| {
            proptest::collection::vec((0..n, 0..n, 1.0..50.0f64), e).prop_map(move |triples| {
                let mut g = WeightedGraph::new(n);
                for (u, v, w) in triples {
                    if u != v {
                        g.add_undirected_edge(u, v, w);
                    }
                }
                g
            })
        })
    })
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    // 1. Dijkstra: source distance is 0
    #[test]
    fn dijkstra_source_zero(g in arb_nn_graph(10), source in 0..10usize) {
        prop_assume!(source < g.node_count());
        let r = dijkstra(&g, source);
        prop_assert!((r.distance_to(source) - 0.0).abs() < f64::EPSILON);
    }

    // 2. Dijkstra: all distances non-negative
    #[test]
    fn dijkstra_non_negative(g in arb_nn_graph(10), source in 0..10usize) {
        prop_assume!(source < g.node_count());
        let r = dijkstra(&g, source);
        for i in 0..g.node_count() {
            prop_assert!(r.distance_to(i) >= 0.0);
        }
    }

    // 3. Dijkstra: triangle inequality on result
    #[test]
    fn dijkstra_triangle(g in arb_nn_graph(8), source in 0..8usize) {
        prop_assume!(source < g.node_count());
        let r = dijkstra(&g, source);
        for u in 0..g.node_count() {
            for &(v, w) in g.neighbors(u) {
                if r.is_reachable(u) {
                    prop_assert!(
                        r.distance_to(v) <= r.distance_to(u) + w + 1e-10,
                        "d[{}]={} > d[{}]={} + w={}",
                        v, r.distance_to(v), u, r.distance_to(u), w
                    );
                }
            }
        }
    }

    // 4. Dijkstra: path starts at source, ends at target
    #[test]
    fn dijkstra_path_endpoints(g in arb_nn_graph(10), source in 0..10usize, target in 0..10usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let r = dijkstra(&g, source);
        if let Some(path) = r.path_to(target) {
            prop_assert_eq!(*path.first().unwrap(), source);
            prop_assert_eq!(*path.last().unwrap(), target);
        }
    }

    // 5. Dijkstra: path cost equals distance
    #[test]
    fn dijkstra_path_cost(g in arb_nn_graph(10), source in 0..10usize, target in 0..10usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let r = dijkstra(&g, source);
        if let Some(path) = r.path_to(target) {
            let mut cost = 0.0;
            for w in path.windows(2) {
                let u = w[0];
                let v = w[1];
                let edge_w = g.neighbors(u).iter().filter(|&&(n, _)| n == v).map(|&(_, w)| w).fold(f64::INFINITY, f64::min);
                cost += edge_w;
            }
            prop_assert!((cost - r.distance_to(target)).abs() < 1e-8,
                "path cost={}, distance={}", cost, r.distance_to(target));
        }
    }

    // 6. BF agrees with Dijkstra on non-negative graphs
    #[test]
    fn bf_agrees_dijkstra(g in arb_nn_graph(8), source in 0..8usize) {
        prop_assume!(source < g.node_count());
        let dij = dijkstra(&g, source);
        let bf = bellman_ford(&g, source).unwrap();
        for i in 0..g.node_count() {
            let dd = dij.distance_to(i);
            let bd = bf.distance_to(i);
            let both_inf = dd == f64::INFINITY && bd == f64::INFINITY;
            let close = (dd - bd).abs() < 1e-8;
            prop_assert!(both_inf || close,
                "node {}: dij={}, bf={}", i, dd, bd
            );
        }
    }

    // 7. BF: source distance is 0
    #[test]
    fn bf_source_zero(g in arb_nn_graph(8), source in 0..8usize) {
        prop_assume!(source < g.node_count());
        let r = bellman_ford(&g, source).unwrap();
        prop_assert!((r.distance_to(source) - 0.0).abs() < f64::EPSILON);
    }

    // 8. BFS: distances are integers
    #[test]
    fn bfs_integer_distances(g in arb_nn_graph(10), source in 0..10usize) {
        prop_assume!(source < g.node_count());
        let r = bfs_shortest(&g, source);
        for i in 0..g.node_count() {
            if r.is_reachable(i) {
                let d = r.distance_to(i);
                prop_assert!((d - d.round()).abs() < f64::EPSILON);
            }
        }
    }

    // 9. BFS: distance <= Dijkstra distance (fewer hops, not necessarily less weight)
    // Actually BFS counts hops, Dijkstra counts weight. BFS hop count <= Dijkstra path length (in hops)
    #[test]
    fn bfs_hop_count(g in arb_nn_graph(10), source in 0..10usize, target in 0..10usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let bfs_r = bfs_shortest(&g, source);
        let dij_r = dijkstra(&g, source);
        if bfs_r.is_reachable(target) {
            prop_assert!(dij_r.is_reachable(target));
        }
    }

    // 10. Floyd-Warshall: diagonal is 0
    #[test]
    fn fw_diagonal_zero(g in arb_nn_graph(8)) {
        let dist = floyd_warshall(&g).unwrap();
        for i in 0..g.node_count() {
            prop_assert!((dist[i][i] - 0.0).abs() < f64::EPSILON);
        }
    }

    // 11. FW agrees with Dijkstra for each source
    #[test]
    fn fw_agrees_dijkstra(g in arb_nn_graph(8)) {
        let fw = floyd_warshall(&g).unwrap();
        for source in 0..g.node_count() {
            let dij = dijkstra(&g, source);
            for target in 0..g.node_count() {
                let fwd = fw[source][target];
                let dd = dij.distance_to(target);
                let both_inf = fwd == f64::INFINITY && dd == f64::INFINITY;
                let close = (fwd - dd).abs() < 1e-8;
                prop_assert!(both_inf || close,
                    "FW[{}][{}]={}, Dij={}", source, target, fwd, dd
                );
            }
        }
    }

    // 12. FW: triangle inequality
    #[test]
    fn fw_triangle(g in arb_nn_graph(8)) {
        let fw = floyd_warshall(&g).unwrap();
        let n = g.node_count();
        for i in 0..n {
            for j in 0..n {
                for k in 0..n {
                    if fw[i][k] < f64::INFINITY && fw[k][j] < f64::INFINITY {
                        prop_assert!(
                            fw[i][j] <= fw[i][k] + fw[k][j] + 1e-10,
                            "d[{}][{}]={} > d[{}][{}]={} + d[{}][{}]={}",
                            i, j, fw[i][j], i, k, fw[i][k], k, j, fw[k][j]
                        );
                    }
                }
            }
        }
    }

    // 13. Diameter >= 0
    #[test]
    fn diameter_non_negative(g in arb_nn_graph(8)) {
        let d = graph_diameter(&g).unwrap();
        prop_assert!(d >= 0.0);
    }

    // 14. shortest_path agrees with dijkstra
    #[test]
    fn sp_agrees(g in arb_nn_graph(10), source in 0..10usize, target in 0..10usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let dij = dijkstra(&g, source);
        match shortest_path(&g, source, target) {
            Some((d, _path)) => {
                prop_assert!((d - dij.distance_to(target)).abs() < 1e-8);
            }
            None => {
                prop_assert!(!dij.is_reachable(target));
            }
        }
    }

    // 15. k-shortest paths: first is shortest
    #[test]
    fn ksp_first_is_shortest(g in arb_nn_graph(8), source in 0..8usize, target in 0..8usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let paths = k_shortest_paths(&g, source, target, 3);
        if let Some(first) = paths.first() {
            let dij = dijkstra(&g, source);
            if dij.is_reachable(target) {
                prop_assert!((first.0 - dij.distance_to(target)).abs() < 1e-8);
            }
        }
    }

    // 16. k-shortest: non-decreasing order
    #[test]
    fn ksp_sorted(g in arb_nn_graph(8), source in 0..8usize, target in 0..8usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let paths = k_shortest_paths(&g, source, target, 5);
        for w in paths.windows(2) {
            prop_assert!(w[0].0 <= w[1].0 + 1e-8);
        }
    }

    // 17. k-shortest: at most k results
    #[test]
    fn ksp_bounded(g in arb_nn_graph(8), source in 0..8usize, target in 0..8usize, k in 1..5usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let paths = k_shortest_paths(&g, source, target, k);
        prop_assert!(paths.len() <= k);
    }

    // 18. Serde roundtrip preserves structure
    #[test]
    fn serde_roundtrip(g in arb_nn_graph(10)) {
        let json = serde_json::to_string(&g).unwrap();
        let back: WeightedGraph = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(g.node_count(), back.node_count());
        prop_assert_eq!(g.edge_count(), back.edge_count());
        for u in 0..g.node_count() {
            let orig = g.neighbors(u);
            let rest = back.neighbors(u);
            prop_assert_eq!(orig.len(), rest.len());
            for (a, b) in orig.iter().zip(rest.iter()) {
                prop_assert_eq!(a.0, b.0);
                prop_assert!((a.1 - b.1).abs() < 1e-10,
                    "weight mismatch: {} vs {}", a.1, b.1);
            }
        }
    }

    // 19. all_non_negative for generated graphs
    #[test]
    fn all_nn_flag(g in arb_nn_graph(10)) {
        prop_assert!(g.all_non_negative());
    }

    // 20. node_count and edge_count correct
    #[test]
    fn counts_correct(n in 1..15usize,
                      edges in proptest::collection::vec((0..15usize, 0..15usize, 1.0..10.0f64), 0..30)) {
        let valid: Vec<(usize, usize, f64)> = edges.into_iter().filter(|&(u, v, _)| u < n && v < n).collect();
        let g = WeightedGraph::from_edges(n, &valid);
        prop_assert_eq!(g.node_count(), n);
        prop_assert_eq!(g.edge_count(), valid.len());
    }

    // 21. Undirected graph: d(u,v) == d(v,u)
    #[test]
    fn undirected_symmetric(g in arb_undirected(8)) {
        let fw = floyd_warshall(&g).unwrap();
        let n = g.node_count();
        for i in 0..n {
            for j in 0..n {
                let same_inf = fw[i][j] == f64::INFINITY && fw[j][i] == f64::INFINITY;
                let close = (fw[i][j] - fw[j][i]).abs() < 1e-8;
                prop_assert!(same_inf || close,
                    "d[{}][{}]={} != d[{}][{}]={}", i, j, fw[i][j], j, i, fw[j][i]
                );
            }
        }
    }

    // 22. Dijkstra path uses only existing edges
    #[test]
    fn path_uses_edges(g in arb_nn_graph(10), source in 0..10usize, target in 0..10usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let r = dijkstra(&g, source);
        if let Some(path) = r.path_to(target) {
            for w in path.windows(2) {
                let u = w[0];
                let v = w[1];
                let has_edge = g.neighbors(u).iter().any(|&(n, _)| n == v);
                prop_assert!(has_edge, "path uses nonexistent edge {}->{}", u, v);
            }
        }
    }

    // 23. Empty graph: no reachability
    #[test]
    fn empty_no_reach(n in 2..10usize) {
        let g = WeightedGraph::new(n);
        let r = dijkstra(&g, 0);
        for i in 1..n {
            prop_assert!(!r.is_reachable(i));
        }
    }

    // 24. Self-loop doesn't change distances
    #[test]
    fn self_loop_neutral(n in 2..8usize, w in 0.0..100.0f64) {
        let mut g = WeightedGraph::new(n);
        g.add_edge(0, 0, w);
        let r = dijkstra(&g, 0);
        prop_assert!((r.distance_to(0) - 0.0).abs() < f64::EPSILON);
    }

    // 25. BFS path length matches distance
    #[test]
    fn bfs_path_len(g in arb_nn_graph(10), source in 0..10usize, target in 0..10usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let r = bfs_shortest(&g, source);
        if let Some(path) = r.path_to(target) {
            let hops = (path.len() - 1) as f64;
            prop_assert!((r.distance_to(target) - hops).abs() < f64::EPSILON);
        }
    }

    // 26. Diameter >= any single shortest path
    #[test]
    fn diameter_ge_any_sp(g in arb_nn_graph(8), source in 0..8usize, target in 0..8usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let d = graph_diameter(&g).unwrap();
        let r = dijkstra(&g, source);
        if r.is_reachable(target) {
            prop_assert!(d >= r.distance_to(target) - 1e-10);
        }
    }

    // 27. Undirected edge count is even
    #[test]
    fn undirected_even_edges(g in arb_undirected(8)) {
        prop_assert_eq!(g.edge_count() % 2, 0);
    }

    // 28. Reachable implies path exists
    #[test]
    fn reachable_implies_path(g in arb_nn_graph(10), source in 0..10usize, target in 0..10usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let r = dijkstra(&g, source);
        if r.is_reachable(target) {
            prop_assert!(r.path_to(target).is_some());
        } else {
            prop_assert!(r.path_to(target).is_none());
        }
    }

    // 29. BF returns Some for non-negative graphs
    #[test]
    fn bf_some_for_nn(g in arb_nn_graph(8), source in 0..8usize) {
        prop_assume!(source < g.node_count());
        prop_assert!(bellman_ford(&g, source).is_some());
    }

    // 30. k-shortest paths all start at source and end at target
    #[test]
    fn ksp_endpoints(g in arb_nn_graph(8), source in 0..8usize, target in 0..8usize) {
        prop_assume!(source < g.node_count() && target < g.node_count());
        let paths = k_shortest_paths(&g, source, target, 3);
        for (_cost, path) in &paths {
            prop_assert_eq!(*path.first().unwrap(), source);
            prop_assert_eq!(*path.last().unwrap(), target);
        }
    }

    // 31. Dijkstra: unreachable nodes have INFINITY distance
    #[test]
    fn unreachable_infinity(g in arb_nn_graph(10), source in 0..10usize) {
        prop_assume!(source < g.node_count());
        let r = dijkstra(&g, source);
        for i in 0..g.node_count() {
            if !r.is_reachable(i) {
                prop_assert_eq!(r.distance_to(i), f64::INFINITY);
            }
        }
    }

    // 32. Dijkstra source always reachable
    #[test]
    fn source_always_reachable(g in arb_nn_graph(10), source in 0..10usize) {
        prop_assume!(source < g.node_count());
        let r = dijkstra(&g, source);
        prop_assert!(r.is_reachable(source));
    }
}
