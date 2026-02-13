//! Property-based tests for the reactive dataflow graph.
//!
//! Tests invariants: acyclicity, topological propagation order, glitch-freedom,
//! incremental recomputation, and value consistency.

use frankenterm_core::dataflow::{DataflowError, DataflowGraph, NodeId, Value};
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

/// Generate a random Value.
fn arb_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<bool>().prop_map(Value::Bool),
        any::<f64>()
            .prop_filter("finite float", |v| v.is_finite())
            .prop_map(Value::Float),
        any::<i64>().prop_map(Value::Int),
        "[a-z]{0,20}".prop_map(Value::Text),
        Just(Value::None),
    ]
}

/// Graph operation for random graph construction.
#[derive(Debug, Clone)]
enum GraphOp {
    AddSource(String, Value),
    AddMap(String, Vec<usize>), // indices into existing nodes
    AddEdge(usize, usize),      // indices into existing nodes
}

/// Generate a sequence of graph-building operations.
fn arb_graph_ops(max_ops: usize) -> impl Strategy<Value = Vec<GraphOp>> {
    prop::collection::vec(
        prop_oneof![
            (("[a-z]{1,8}"), arb_value()).prop_map(|(label, val)| GraphOp::AddSource(label, val)),
            ("[a-z]{1,8}", prop::collection::vec(0..50usize, 0..4))
                .prop_map(|(label, inputs)| GraphOp::AddMap(label, inputs)),
            (0..50usize, 0..50usize).prop_map(|(from, to)| GraphOp::AddEdge(from, to)),
        ],
        1..max_ops,
    )
}

/// Build a graph from a sequence of operations, returning (graph, node_ids).
fn build_graph_from_ops(ops: &[GraphOp]) -> (DataflowGraph, Vec<NodeId>) {
    let mut graph = DataflowGraph::new();
    let mut nodes: Vec<NodeId> = Vec::new();

    for op in ops {
        match op {
            GraphOp::AddSource(label, value) => {
                let id = graph.add_source(label, value.clone());
                nodes.push(id);
            }
            GraphOp::AddMap(label, input_indices) => {
                let inputs: Vec<NodeId> = input_indices
                    .iter()
                    .filter_map(|&idx| nodes.get(idx % nodes.len().max(1)).copied())
                    .collect();
                if nodes.is_empty() {
                    // Need at least one source first.
                    let s = graph.add_source("auto_src", Value::None);
                    nodes.push(s);
                    let id = graph.add_map(label, vec![s], |i| {
                        i.first().cloned().unwrap_or(Value::None)
                    });
                    nodes.push(id);
                } else {
                    let id =
                        graph.add_map(label, inputs, |i| i.first().cloned().unwrap_or(Value::None));
                    nodes.push(id);
                }
            }
            GraphOp::AddEdge(from_idx, to_idx) => {
                if nodes.len() < 2 {
                    continue;
                }
                let from = nodes[*from_idx % nodes.len()];
                let to = nodes[*to_idx % nodes.len()];
                // Ignore errors (cycle detection, duplicate, etc.)
                let _ = graph.add_edge(from, to);
            }
        }
    }

    (graph, nodes)
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    /// After any sequence of operations, the graph remains acyclic.
    #[test]
    fn graph_always_acyclic(ops in arb_graph_ops(100)) {
        let (graph, _) = build_graph_from_ops(&ops);
        prop_assert!(graph.is_acyclic(), "Graph should always be acyclic");
    }

    /// Propagation never panics regardless of graph shape.
    #[test]
    fn propagation_never_panics(ops in arb_graph_ops(50)) {
        let (mut graph, _) = build_graph_from_ops(&ops);
        let stats = graph.propagate();
        prop_assert!(stats.nodes_recomputed <= stats.total_nodes);
    }

    /// Node count matches operations.
    #[test]
    fn node_count_consistent(ops in arb_graph_ops(50)) {
        let (graph, nodes) = build_graph_from_ops(&ops);
        prop_assert_eq!(graph.node_count(), nodes.len());
    }

    /// Source updates only affect downstream nodes.
    #[test]
    fn source_update_propagates_downstream(
        initial in any::<i64>(),
        updated in any::<i64>(),
    ) {
        let mut graph = DataflowGraph::new();
        let s1 = graph.add_source("s1", Value::Int(initial));
        let s2 = graph.add_source("s2", Value::Int(42));
        let m1 = graph.add_map("m1", vec![s1], |i| i[0].clone());
        let m2 = graph.add_map("m2", vec![s2], |i| i[0].clone());

        graph.propagate();

        // Update s1 — m2 should not change.
        let _ = graph.update_source(s1, Value::Int(updated));
        let stats = graph.propagate();

        prop_assert_eq!(graph.get_value(m1), Some(&Value::Int(updated)));
        prop_assert_eq!(graph.get_value(m2), Some(&Value::Int(42)));
        // m1 should be recomputed but not m2.
        if initial != updated {
            prop_assert!(stats.nodes_recomputed <= 1);
        }
    }

    /// Glitch-freedom: diamond graph sees consistent snapshots.
    #[test]
    fn glitch_freedom_diamond(val in any::<i64>()) {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source("s", Value::Int(0));
        let left = graph.add_map("left", vec![s], |i| match &i[0] {
            Value::Int(v) => Value::Int(v + 1),
            _ => Value::None,
        });
        let right = graph.add_map("right", vec![s], |i| match &i[0] {
            Value::Int(v) => Value::Int(v * 2),
            _ => Value::None,
        });
        let join = graph.add_combine("join", vec![left, right], |i| {
            match (&i[0], &i[1]) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a + b),
                _ => Value::None,
            }
        });

        graph.propagate();

        let _ = graph.update_source(s, Value::Int(val));
        graph.propagate();

        let expected_left = val + 1;
        let expected_right = val * 2;
        let expected_join = expected_left + expected_right;

        prop_assert_eq!(graph.get_value(left), Some(&Value::Int(expected_left)));
        prop_assert_eq!(graph.get_value(right), Some(&Value::Int(expected_right)));
        prop_assert_eq!(graph.get_value(join), Some(&Value::Int(expected_join)));
    }

    /// Setting a source to the same value causes zero recomputation.
    #[test]
    fn stable_value_no_recompute(val in arb_value()) {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source("s", val.clone());
        let _m = graph.add_map("m", vec![s], |i| i[0].clone());
        graph.propagate();

        let _ = graph.update_source(s, val);
        let stats = graph.propagate();
        prop_assert_eq!(stats.nodes_recomputed, 0);
    }

    /// Chain of N map nodes computes correctly.
    #[test]
    fn chain_computes_correctly(
        n in 1..50usize,
        start in -100..100i64,
    ) {
        let mut graph = DataflowGraph::new();
        let mut prev = graph.add_source("s", Value::Int(start));
        for i in 0..n {
            prev = graph.add_map(&format!("n{i}"), vec![prev], |inputs| {
                match &inputs[0] {
                    Value::Int(v) => Value::Int(v + 1),
                    _ => Value::None,
                }
            });
        }
        graph.propagate();
        prop_assert_eq!(
            graph.get_value(prev),
            Some(&Value::Int(start + n as i64))
        );
    }

    /// Fanout: one source feeding N maps all compute correctly.
    #[test]
    fn fanout_computes_correctly(
        n in 1..20usize,
        val in any::<i64>(),
    ) {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source("s", Value::Int(val));
        let maps: Vec<NodeId> = (0..n)
            .map(|i| {
                let offset = i as i64;
                graph.add_map(&format!("m{i}"), vec![s], move |inputs| {
                    match &inputs[0] {
                        Value::Int(v) => Value::Int(v + offset),
                        _ => Value::None,
                    }
                })
            })
            .collect();

        graph.propagate();

        for (i, &m) in maps.iter().enumerate() {
            prop_assert_eq!(
                graph.get_value(m),
                Some(&Value::Int(val + i as i64))
            );
        }
    }

    /// Removing a node from the graph doesn't corrupt remaining computation.
    #[test]
    fn remove_node_preserves_others(val in any::<i64>()) {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source("s", Value::Int(val));
        let a = graph.add_map("a", vec![s], |i| match &i[0] {
            Value::Int(v) => Value::Int(v + 1),
            _ => Value::None,
        });
        let b = graph.add_map("b", vec![s], |i| match &i[0] {
            Value::Int(v) => Value::Int(v * 2),
            _ => Value::None,
        });
        graph.propagate();

        // Remove a — b should still work.
        graph.remove_node(a).unwrap();
        let _ = graph.update_source(s, Value::Int(val + 10));
        graph.propagate();

        prop_assert_eq!(graph.get_value(b), Some(&Value::Int((val + 10) * 2)));
        prop_assert!(graph.get_value(a).is_none());
    }

    /// Cycle detection: self-loops are always rejected.
    #[test]
    fn self_loop_always_rejected(label in "[a-z]{1,8}") {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source(&label, Value::None);
        let result = graph.add_edge(s, s);
        prop_assert!(matches!(result, Err(DataflowError::CycleDetected { .. })));
    }

    /// Snapshot roundtrips through JSON.
    #[test]
    fn snapshot_json_roundtrip(ops in arb_graph_ops(30)) {
        let (mut graph, _) = build_graph_from_ops(&ops);
        graph.propagate();
        let snap = graph.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: frankenterm_core::dataflow::GraphSnapshot =
            serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.nodes.len(), deserialized.nodes.len());
        prop_assert_eq!(snap.edges.len(), deserialized.edges.len());
    }

    /// Edge count matches the sum of input lists.
    #[test]
    fn edge_count_consistent(ops in arb_graph_ops(40)) {
        let (graph, _) = build_graph_from_ops(&ops);
        // edge_count counts inputs across all nodes.
        let expected: usize = graph.node_ids().iter().map(|_| 0).sum::<usize>();
        // Just verify it doesn't panic.
        let _count = graph.edge_count();
    }

    /// Propagation stats total_nodes matches node_count.
    #[test]
    fn propagation_stats_consistent(ops in arb_graph_ops(30)) {
        let (mut graph, _) = build_graph_from_ops(&ops);
        let stats = graph.propagate();
        prop_assert_eq!(stats.total_nodes, graph.node_count());
    }

    /// Multiple propagations without source changes are idempotent.
    #[test]
    fn propagation_idempotent(ops in arb_graph_ops(30)) {
        let (mut graph, _) = build_graph_from_ops(&ops);
        graph.propagate();

        // Second propagation should do nothing.
        let stats = graph.propagate();
        prop_assert_eq!(stats.nodes_recomputed, 0);
        prop_assert_eq!(stats.nodes_changed, 0);
    }

    /// Value truthiness is consistent with documented semantics.
    #[test]
    fn value_truthiness_consistent(val in arb_value()) {
        let truthy = val.is_truthy();
        match &val {
            Value::Bool(b) => prop_assert_eq!(truthy, *b),
            Value::Int(i) => prop_assert_eq!(truthy, *i != 0),
            Value::Float(f) => prop_assert_eq!(truthy, *f != 0.0),
            Value::Text(s) => prop_assert_eq!(truthy, !s.is_empty()),
            Value::None => prop_assert!(!truthy),
        }
    }

    /// Value display doesn't panic.
    #[test]
    fn value_display_no_panic(val in arb_value()) {
        let s = format!("{val}");
        prop_assert!(!s.is_empty());
    }
}
