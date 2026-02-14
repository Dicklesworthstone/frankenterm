//! Property-based tests for the reactive dataflow graph.
//!
//! Tests invariants: acyclicity, topological propagation order, glitch-freedom,
//! incremental recomputation, value consistency, error handling, merge, query
//! API, and serde roundtrips.

use frankenterm_core::dataflow::{DataflowError, DataflowGraph, NodeId, Value};
use proptest::prelude::*;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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
    Source(String, Value),
    Map(String, Vec<usize>), // indices into existing nodes
    Edge(usize, usize),      // indices into existing nodes
}

/// Generate a sequence of graph-building operations.
fn arb_graph_ops(max_ops: usize) -> impl Strategy<Value = Vec<GraphOp>> {
    prop::collection::vec(
        prop_oneof![
            (("[a-z]{1,8}"), arb_value()).prop_map(|(label, val)| GraphOp::Source(label, val)),
            ("[a-z]{1,8}", prop::collection::vec(0..50usize, 0..4))
                .prop_map(|(label, inputs)| GraphOp::Map(label, inputs)),
            (0..50usize, 0..50usize).prop_map(|(from, to)| GraphOp::Edge(from, to)),
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
            GraphOp::Source(label, value) => {
                let id = graph.add_source(label, value.clone());
                nodes.push(id);
            }
            GraphOp::Map(label, input_indices) => {
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
            GraphOp::Edge(from_idx, to_idx) => {
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
// Property tests — core graph invariants
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
            Value::Int(v) => Value::Int(v.saturating_add(1)),
            _ => Value::None,
        });
        let right = graph.add_map("right", vec![s], |i| match &i[0] {
            Value::Int(v) => Value::Int(v.saturating_mul(2)),
            _ => Value::None,
        });
        let join = graph.add_combine("join", vec![left, right], |i| {
            match (&i[0], &i[1]) {
                (Value::Int(a), Value::Int(b)) => Value::Int(a.saturating_add(*b)),
                _ => Value::None,
            }
        });

        graph.propagate();

        let _ = graph.update_source(s, Value::Int(val));
        graph.propagate();

        let expected_left = val.saturating_add(1);
        let expected_right = val.saturating_mul(2);
        let expected_join = expected_left.saturating_add(expected_right);

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
                        Value::Int(v) => Value::Int(v.saturating_add(offset)),
                        _ => Value::None,
                    }
                })
            })
            .collect();

        graph.propagate();

        for (i, &m) in maps.iter().enumerate() {
            prop_assert_eq!(
                graph.get_value(m),
                Some(&Value::Int(val.saturating_add(i as i64)))
            );
        }
    }

    /// Removing a node from the graph doesn't corrupt remaining computation.
    #[test]
    fn remove_node_preserves_others(val in any::<i64>()) {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source("s", Value::Int(val));
        let a = graph.add_map("a", vec![s], |i| match &i[0] {
            Value::Int(v) => Value::Int(v.saturating_add(1)),
            _ => Value::None,
        });
        let b = graph.add_map("b", vec![s], |i| match &i[0] {
            Value::Int(v) => Value::Int(v.saturating_mul(2)),
            _ => Value::None,
        });
        graph.propagate();

        // Remove a — b should still work.
        graph.remove_node(a).unwrap();
        let _ = graph.update_source(s, Value::Int(val.saturating_add(10)));
        graph.propagate();

        prop_assert_eq!(graph.get_value(b), Some(&Value::Int(val.saturating_add(10).saturating_mul(2))));
        prop_assert!(graph.get_value(a).is_none());
    }

    /// Cycle detection: self-loops are always rejected.
    #[test]
    fn self_loop_always_rejected(label in "[a-z]{1,8}") {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source(&label, Value::None);
        let result = graph.add_edge(s, s);
        let cycle_detected = matches!(result, Err(DataflowError::CycleDetected { .. }));
        prop_assert!(cycle_detected);
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
        // edge_count should match serialized edge count from the snapshot.
        let expected = graph.snapshot().edges.len();
        let count = graph.edge_count();
        prop_assert_eq!(count, expected);
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
        // Empty text values produce empty display strings, which is correct.
        match &val {
            Value::Text(t) if t.is_empty() => prop_assert!(s.is_empty()),
            _ => prop_assert!(!s.is_empty()),
        }
    }
}

// =============================================================================
// Property tests — Value serde and equality
// =============================================================================

proptest! {
    /// Value serde roundtrip preserves equality.
    #[test]
    fn value_serde_roundtrip(val in arb_value()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: Value = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &val);
    }

    /// Value JSON has a "type" tag.
    #[test]
    fn value_json_has_type_tag(val in arb_value()) {
        let json = serde_json::to_string(&val).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(
            parsed.get("type").is_some(),
            "Value JSON should have a 'type' field: {}", json
        );
        let tag = parsed["type"].as_str().unwrap();
        let expected_tag = match &val {
            Value::Bool(_) => "bool",
            Value::Float(_) => "float",
            Value::Int(_) => "int",
            Value::Text(_) => "text",
            Value::None => "none",
        };
        prop_assert_eq!(tag, expected_tag);
    }

    /// Value equality is reflexive.
    #[test]
    fn value_equality_reflexive(val in arb_value()) {
        prop_assert_eq!(&val, &val);
    }

    /// Value clone equals original.
    #[test]
    fn value_clone_equals(val in arb_value()) {
        let cloned = val.clone();
        prop_assert_eq!(&cloned, &val);
    }
}

// =============================================================================
// Property tests — query API
// =============================================================================

proptest! {
    /// get_label returns the correct label for each node.
    #[test]
    fn get_label_correct(label in "[a-z]{1,10}", val in arb_value()) {
        let mut graph = DataflowGraph::new();
        let id = graph.add_source(&label, val);
        prop_assert_eq!(graph.get_label(id), Some(label.as_str()));
    }

    /// is_stable returns true after propagation.
    #[test]
    fn is_stable_after_propagation(ops in arb_graph_ops(30)) {
        let (mut graph, _) = build_graph_from_ops(&ops);
        graph.propagate();
        prop_assert!(graph.is_stable(), "should be stable after propagation");
    }

    /// node_ids returns all node IDs in the graph.
    #[test]
    fn node_ids_complete(ops in arb_graph_ops(30)) {
        let (graph, created_nodes) = build_graph_from_ops(&ops);
        let ids = graph.node_ids();
        let id_set: HashSet<NodeId> = ids.into_iter().collect();
        for &n in &created_nodes {
            prop_assert!(id_set.contains(&n), "missing node ID");
        }
        prop_assert_eq!(id_set.len(), graph.node_count());
    }

    /// propagation_count increments with each propagation.
    #[test]
    fn propagation_count_increments(n in 1..10usize) {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source("s", Value::Int(0));
        prop_assert_eq!(graph.propagation_count(), 0);
        for i in 0..n {
            let _ = graph.update_source(s, Value::Int(i as i64));
            graph.propagate();
        }
        prop_assert_eq!(graph.propagation_count(), n as u64);
    }

    /// NodeId Display format contains "node:".
    #[test]
    fn node_id_display_format(_dummy in 0..1u8) {
        let mut graph = DataflowGraph::new();
        let id = graph.add_source("test", Value::None);
        let display = format!("{}", id);
        prop_assert!(display.starts_with("node:"), "NodeId display should start with 'node:', got '{}'", display);
    }
}

// =============================================================================
// Property tests — error handling
// =============================================================================

proptest! {
    /// update_source on a map node returns NotASource error.
    #[test]
    fn update_source_on_map_returns_error(val in arb_value()) {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source("s", Value::None);
        let m = graph.add_map("m", vec![s], |i| i[0].clone());
        let result = graph.update_source(m, val);
        prop_assert!(
            matches!(result, Err(DataflowError::NotASource(_))),
            "update_source on map should return NotASource"
        );
    }

    /// Cycle detection across 2 nodes: A→B→A is rejected.
    #[test]
    fn cycle_across_two_nodes_rejected(val in arb_value()) {
        let mut graph = DataflowGraph::new();
        let a = graph.add_source("a", val.clone());
        let b = graph.add_map("b", vec![a], |i| i[0].clone());
        // b already depends on a; adding a→b would create a cycle
        // (well, a already feeds b, so edge b→a would create the cycle)
        let result = graph.add_edge(b, a);
        prop_assert!(
            matches!(result, Err(DataflowError::CycleDetected { .. })),
            "adding b->a when a->b exists should detect a cycle"
        );
    }

    /// Duplicate edge returns DuplicateEdge error.
    #[test]
    fn duplicate_edge_rejected(_dummy in 0..1u8) {
        let mut graph = DataflowGraph::new();
        let a = graph.add_source("a", Value::None);
        let b = graph.add_source("b", Value::None);
        graph.add_edge(a, b).unwrap();
        let result = graph.add_edge(a, b);
        prop_assert!(
            matches!(result, Err(DataflowError::DuplicateEdge { .. })),
            "adding duplicate edge should return DuplicateEdge"
        );
    }

    /// DataflowError Display is non-empty for all variants.
    #[test]
    fn error_display_non_empty(_dummy in 0..1u8) {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source("s", Value::None);

        let err1 = graph.add_edge(s, s).unwrap_err();
        prop_assert!(!err1.to_string().is_empty());

        let m = graph.add_map("m", vec![s], |i| i[0].clone());
        let err2 = graph.update_source(m, Value::None).unwrap_err();
        prop_assert!(!err2.to_string().is_empty());
    }

    /// remove_node on unknown ID returns NodeNotFound.
    #[test]
    fn remove_unknown_node_errors(_dummy in 0..1u8) {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source("s", Value::None);
        graph.remove_node(s).unwrap();
        // Now s is gone; removing again should fail
        let result = graph.remove_node(s);
        prop_assert!(matches!(result, Err(DataflowError::NodeNotFound(_))));
    }
}

// =============================================================================
// Property tests — sink and merge
// =============================================================================

proptest! {
    /// add_sink fires callback on value change.
    #[test]
    fn sink_fires_on_change(initial in any::<i64>(), updated in any::<i64>()) {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source("s", Value::Int(initial));
        let m = graph.add_map("m", vec![s], |i| i[0].clone());
        graph.propagate();

        let fire_count = Arc::new(AtomicUsize::new(0));
        let fc = Arc::clone(&fire_count);
        graph.add_sink(m, move |_val| {
            fc.fetch_add(1, Ordering::Relaxed);
        }).unwrap();

        let _ = graph.update_source(s, Value::Int(updated));
        graph.propagate();

        if initial != updated {
            prop_assert!(
                fire_count.load(Ordering::Relaxed) >= 1,
                "sink should fire when value changes"
            );
        }
    }

    /// add_sink on unknown node returns NodeNotFound.
    #[test]
    fn sink_unknown_node_errors(_dummy in 0..1u8) {
        let mut graph = DataflowGraph::new();
        let s = graph.add_source("s", Value::None);
        graph.remove_node(s).unwrap();
        let result = graph.add_sink(s, |_| {});
        prop_assert!(matches!(result, Err(DataflowError::NodeNotFound(_))));
    }

    /// Merge increases node count by the other graph's node count.
    #[test]
    fn merge_increases_node_count(
        n1 in 1..10usize,
        n2 in 1..10usize,
    ) {
        let mut g1 = DataflowGraph::new();
        for i in 0..n1 {
            g1.add_source(&format!("g1_s{}", i), Value::Int(i as i64));
        }

        let mut g2 = DataflowGraph::new();
        for i in 0..n2 {
            g2.add_source(&format!("g2_s{}", i), Value::Int(i as i64));
        }

        let before = g1.node_count();
        let id_map = g1.merge(&g2);

        prop_assert_eq!(g1.node_count(), before + n2);
        prop_assert_eq!(id_map.len(), n2, "id_map should have one entry per merged node");
    }

    /// Merge preserves values from the merged graph.
    #[test]
    fn merge_preserves_values(val in any::<i64>()) {
        let mut g1 = DataflowGraph::new();
        g1.add_source("g1_s", Value::Int(0));

        let mut g2 = DataflowGraph::new();
        let g2_s = g2.add_source("g2_s", Value::Int(val));

        let id_map = g1.merge(&g2);
        let new_id = id_map[&g2_s];

        prop_assert_eq!(g1.get_value(new_id), Some(&Value::Int(val)));
        prop_assert_eq!(g1.get_label(new_id), Some("g2_s"));
    }

    /// Merge remaps edges correctly.
    #[test]
    fn merge_remaps_edges(_dummy in 0..1u8) {
        let mut g1 = DataflowGraph::new();
        g1.add_source("g1_s", Value::Int(0));

        let mut g2 = DataflowGraph::new();
        let g2_s = g2.add_source("g2_s", Value::Int(1));
        let g2_m = g2.add_map("g2_m", vec![g2_s], |i| i[0].clone());
        g2.propagate();

        let edges_before = g1.edge_count();
        let id_map = g1.merge(&g2);

        // Should have at least one new edge (g2_s → g2_m)
        prop_assert!(g1.edge_count() > edges_before, "merge should add edges");

        // Merged graph should remain acyclic
        prop_assert!(g1.is_acyclic(), "merged graph should be acyclic");

        // Values should be accessible via remapped IDs
        let new_s = id_map[&g2_s];
        let new_m = id_map[&g2_m];
        prop_assert_eq!(g1.get_value(new_s), Some(&Value::Int(1)));
        // g2_m was computed from g2_s; after merge, nodes become sources
        // but retain their last-computed value
        prop_assert_eq!(g1.get_value(new_m), Some(&Value::Int(1)));
    }
}
