//! Property-based tests for replay_decision_graph (ft-og6q6.5.1).
//!
//! Invariants tested:
//! - DG-1: Node count matches input event count
//! - DG-2: Graph is always a DAG
//! - DG-3: Roots have no incoming edges
//! - DG-4: Canonical order is sorted by (timestamp, pane_id, rule_id)
//! - DG-5: L1 equivalence ignores transient fields
//! - DG-6: L1 non-equivalence on output_hash change
//! - DG-7: Causal chain of root is empty
//! - DG-8: Effects of every node are descendants (no back-edges)
//! - DG-9: JSON roundtrip preserves node count and edges
//! - DG-10: JSON roundtrip preserves L1 equivalence
//! - DG-11: DecisionType serde roundtrip
//! - DG-12: EdgeType serde roundtrip
//! - DG-13: CausalEdge serde roundtrip
//! - DG-14: DecisionNode serde roundtrip
//! - DG-15: All edges reference valid nodes
//! - DG-16: nodes_by_type partition covers all nodes
//! - DG-17: TriggeredBy edges only point forward (from < to)
//! - DG-18: Empty graph is L1 equivalent to itself
//! - DG-19: Causal chain + self + effects covers reachable subgraph
//! - DG-20: Root count <= node count

use proptest::prelude::*;
use std::collections::BTreeSet;

use frankenterm_core::replay_decision_graph::{
    CausalEdge, DecisionEvent, DecisionGraph, DecisionNode, DecisionType, EdgeType,
};

// ── Strategies ────────────────────────────────────────────────────────────

fn arb_decision_type() -> impl Strategy<Value = DecisionType> {
    prop_oneof![
        Just(DecisionType::PatternMatch),
        Just(DecisionType::WorkflowStep),
        Just(DecisionType::PolicyDecision),
        Just(DecisionType::AlertFired),
        Just(DecisionType::OverrideApplied),
        Just(DecisionType::BarrierDecision),
        Just(DecisionType::NoOp),
    ]
}

fn arb_edge_type() -> impl Strategy<Value = EdgeType> {
    prop_oneof![
        Just(EdgeType::TriggeredBy),
        Just(EdgeType::PrecededBy),
        Just(EdgeType::OverriddenBy),
    ]
}

fn arb_event(index: usize) -> impl Strategy<Value = DecisionEvent> {
    (
        arb_decision_type(),
        "[a-z]{1,6}",
        0u64..10000,
        0u64..5,
    )
        .prop_map(move |(dt, rule_id, ts_offset, pane_id)| {
            let triggered_by = if index > 0 && index % 3 == 0 {
                Some((index - 1) as u64)
            } else {
                None
            };
            DecisionEvent {
                decision_type: dt,
                rule_id,
                definition_hash: format!("def_{}", index),
                input_hash: format!("in_{}", index),
                output_hash: format!("out_{}", index),
                timestamp_ms: ts_offset + (index as u64) * 100,
                pane_id,
                triggered_by,
                overrides: None,
                wall_clock_ms: ts_offset * 2,
                replay_run_id: "run_prop".into(),
            }
        })
}

fn arb_events(max_len: usize) -> impl Strategy<Value = Vec<DecisionEvent>> {
    (1..max_len).prop_flat_map(|n| {
        let strats: Vec<_> = (0..n).map(|i| arb_event(i).boxed()).collect();
        strats
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ── DG-1: Node count matches input ─────────────────────────────────

    #[test]
    fn dg1_node_count(events in arb_events(20)) {
        let graph = DecisionGraph::from_decisions(&events);
        prop_assert_eq!(graph.node_count(), events.len());
    }

    // ── DG-2: Graph is always a DAG ────────────────────────────────────

    #[test]
    fn dg2_always_dag(events in arb_events(20)) {
        let graph = DecisionGraph::from_decisions(&events);
        prop_assert!(graph.is_dag());
    }

    // ── DG-3: Roots have no incoming edges ─────────────────────────────

    #[test]
    fn dg3_roots_no_incoming(events in arb_events(15)) {
        let graph = DecisionGraph::from_decisions(&events);
        let to_nodes: BTreeSet<u64> = graph.edges().iter().map(|e| e.to_node).collect();
        for root in graph.roots() {
            prop_assert!(
                !to_nodes.contains(&root.node_id),
                "root {} should not be a target of any edge", root.node_id
            );
        }
    }

    // ── DG-4: Canonical order is sorted ────────────────────────────────

    #[test]
    fn dg4_canonical_sorted(events in arb_events(20)) {
        let graph = DecisionGraph::from_decisions(&events);
        let canonical = graph.nodes_canonical();
        for window in canonical.windows(2) {
            let a_key = (window[0].timestamp_ms, window[0].pane_id, &window[0].rule_id);
            let b_key = (window[1].timestamp_ms, window[1].pane_id, &window[1].rule_id);
            prop_assert!(
                a_key <= b_key,
                "canonical order should be non-decreasing"
            );
        }
    }

    // ── DG-5: L1 equivalence ignores transient fields ──────────────────

    #[test]
    fn dg5_l1_ignores_transient(events in arb_events(10)) {
        let graph1 = DecisionGraph::from_decisions(&events);
        let mut events2 = events.clone();
        for e in &mut events2 {
            e.wall_clock_ms += 9999;
            e.replay_run_id = "different_run".into();
        }
        let graph2 = DecisionGraph::from_decisions(&events2);
        prop_assert!(graph1.l1_equivalent(&graph2));
    }

    // ── DG-6: L1 non-equivalence on output_hash change ─────────────────

    #[test]
    fn dg6_l1_nonequiv_output(events in arb_events(5)) {
        let graph1 = DecisionGraph::from_decisions(&events);
        let mut events2 = events.clone();
        events2[0].output_hash = "CHANGED".into();
        let graph2 = DecisionGraph::from_decisions(&events2);
        prop_assert!(!graph1.l1_equivalent(&graph2));
    }

    // ── DG-7: Causal chain of root is empty ────────────────────────────

    #[test]
    fn dg7_root_chain_empty(events in arb_events(15)) {
        let graph = DecisionGraph::from_decisions(&events);
        for root in graph.roots() {
            let chain = graph.causal_chain(root.node_id);
            prop_assert!(
                chain.is_empty(),
                "root {} should have empty causal chain", root.node_id
            );
        }
    }

    // ── DG-8: Effects are descendants (node_id > source) ───────────────

    #[test]
    fn dg8_effects_forward(events in arb_events(10)) {
        let graph = DecisionGraph::from_decisions(&events);
        for i in 0..events.len() as u64 {
            let fx = graph.effects(i);
            for node in &fx {
                prop_assert!(
                    node.node_id > i,
                    "effect {} should have id > source {}", node.node_id, i
                );
            }
        }
    }

    // ── DG-9: JSON roundtrip preserves structure ───────────────────────

    #[test]
    fn dg9_json_roundtrip(events in arb_events(10)) {
        let graph = DecisionGraph::from_decisions(&events);
        let json = graph.to_json();
        let restored = DecisionGraph::from_json(&json).unwrap();
        prop_assert_eq!(restored.node_count(), graph.node_count());
        prop_assert_eq!(restored.edge_count(), graph.edge_count());
    }

    // ── DG-10: JSON roundtrip preserves L1 ─────────────────────────────

    #[test]
    fn dg10_json_preserves_l1(events in arb_events(8)) {
        let graph = DecisionGraph::from_decisions(&events);
        let json = graph.to_json();
        let restored = DecisionGraph::from_json(&json).unwrap();
        prop_assert!(graph.l1_equivalent(&restored));
    }

    // ── DG-11: DecisionType serde roundtrip ────────────────────────────

    #[test]
    fn dg11_decision_type_serde(dt in arb_decision_type()) {
        let json = serde_json::to_string(&dt).unwrap();
        let restored: DecisionType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, dt);
    }

    // ── DG-12: EdgeType serde roundtrip ────────────────────────────────

    #[test]
    fn dg12_edge_type_serde(et in arb_edge_type()) {
        let json = serde_json::to_string(&et).unwrap();
        let restored: EdgeType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, et);
    }

    // ── DG-13: CausalEdge serde roundtrip ──────────────────────────────

    #[test]
    fn dg13_edge_serde(
        from_node in 0u64..100,
        to_node in 0u64..100,
        et in arb_edge_type(),
    ) {
        let edge = CausalEdge { from_node, to_node, edge_type: et };
        let json = serde_json::to_string(&edge).unwrap();
        let restored: CausalEdge = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, edge);
    }

    // ── DG-14: DecisionNode serde roundtrip ────────────────────────────

    #[test]
    fn dg14_node_serde(
        node_id in 0u64..100,
        dt in arb_decision_type(),
        ts in 0u64..10000,
        pane_id in 0u64..5,
    ) {
        let node = DecisionNode {
            node_id,
            decision_type: dt,
            rule_id: format!("r_{}", node_id),
            definition_hash: "def".into(),
            input_hash: "in".into(),
            output_hash: "out".into(),
            timestamp_ms: ts,
            pane_id,
            wall_clock_ms: ts * 2,
            replay_run_id: "run".into(),
        };
        let json = serde_json::to_string(&node).unwrap();
        let restored: DecisionNode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.node_id, node_id);
        prop_assert_eq!(restored.decision_type, dt);
    }

    // ── DG-15: All edges reference valid nodes ─────────────────────────

    #[test]
    fn dg15_edges_valid(events in arb_events(15)) {
        let graph = DecisionGraph::from_decisions(&events);
        for edge in graph.edges() {
            prop_assert!(
                graph.get_node(edge.from_node).is_some(),
                "edge from_node {} should exist", edge.from_node
            );
            prop_assert!(
                graph.get_node(edge.to_node).is_some(),
                "edge to_node {} should exist", edge.to_node
            );
        }
    }

    // ── DG-16: nodes_by_type partition covers all nodes ────────────────

    #[test]
    fn dg16_type_partition(events in arb_events(15)) {
        let graph = DecisionGraph::from_decisions(&events);
        let all_types = [
            DecisionType::PatternMatch,
            DecisionType::WorkflowStep,
            DecisionType::PolicyDecision,
            DecisionType::AlertFired,
            DecisionType::OverrideApplied,
            DecisionType::BarrierDecision,
            DecisionType::NoOp,
        ];
        let total: usize = all_types.iter().map(|t| graph.nodes_by_type(*t).len()).sum();
        prop_assert_eq!(total, graph.node_count());
    }

    // ── DG-17: TriggeredBy edges point forward ─────────────────────────

    #[test]
    fn dg17_triggered_forward(events in arb_events(15)) {
        let graph = DecisionGraph::from_decisions(&events);
        for edge in graph.edges() {
            if edge.edge_type == EdgeType::TriggeredBy || edge.edge_type == EdgeType::OverriddenBy {
                prop_assert!(
                    edge.from_node < edge.to_node,
                    "causal edge {}->{} should be forward", edge.from_node, edge.to_node
                );
            }
        }
    }

    // ── DG-18: Empty graph L1 equivalent to itself ─────────────────────

    #[test]
    fn dg18_empty_l1(_dummy in 0u8..1) {
        let graph = DecisionGraph::from_decisions(&[]);
        prop_assert!(graph.l1_equivalent(&graph));
    }

    // ── DG-19: Ancestors + self + descendants cover reachable set ──────

    #[test]
    fn dg19_coverage(events in arb_events(10)) {
        let graph = DecisionGraph::from_decisions(&events);
        if graph.node_count() > 0 {
            let mid = (graph.node_count() / 2) as u64;
            let ancestors: BTreeSet<u64> = graph.causal_chain(mid).iter().map(|n| n.node_id).collect();
            let descendants: BTreeSet<u64> = graph.effects(mid).iter().map(|n| n.node_id).collect();
            // Ancestors should all be < mid, descendants should all be > mid.
            for a in &ancestors {
                prop_assert!(*a < mid, "ancestor {} should be < mid {}", a, mid);
            }
            for d in &descendants {
                prop_assert!(*d > mid, "descendant {} should be > mid {}", d, mid);
            }
        }
    }

    // ── DG-20: Root count <= node count ────────────────────────────────

    #[test]
    fn dg20_root_bound(events in arb_events(15)) {
        let graph = DecisionGraph::from_decisions(&events);
        prop_assert!(graph.roots().len() <= graph.node_count());
    }
}
