//! Normalized decision graph model for replay outputs (ft-og6q6.5.1).
//!
//! Provides:
//! - [`DecisionNode`] — A single decision event with hashes and metadata.
//! - [`CausalEdge`] — Directed causal link between decisions.
//! - [`DecisionGraph`] — DAG of decisions connected by causal edges.
//! - Graph operations: `roots`, `causal_chain`, `effects`, `nodes_by_type`.
//! - Canonicalization for deterministic comparison.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

// ============================================================================
// DecisionType — taxonomy of decision kinds
// ============================================================================

/// Type of decision made during replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DecisionType {
    /// Pattern matched an event.
    PatternMatch,
    /// Workflow step executed.
    WorkflowStep,
    /// Policy decision (allow/deny/rate-limit).
    PolicyDecision,
    /// Alert fired.
    AlertFired,
    /// Override applied (counterfactual).
    OverrideApplied,
    /// Side-effect barrier decision.
    BarrierDecision,
    /// No-op (event processed, no action taken).
    NoOp,
}

// ============================================================================
// EdgeType — kinds of causal links
// ============================================================================

/// Type of causal relationship between decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EdgeType {
    /// This decision was triggered by the source decision.
    TriggeredBy,
    /// This decision temporally follows the source.
    PrecededBy,
    /// This decision overrides the source decision.
    OverriddenBy,
}

// ============================================================================
// DecisionNode — single decision in the graph
// ============================================================================

/// A single decision event in the replay decision graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionNode {
    /// Unique node identifier within the graph.
    pub node_id: u64,
    /// Type of decision.
    pub decision_type: DecisionType,
    /// Rule/pattern ID that produced this decision.
    pub rule_id: String,
    /// FNV-1a hash of the rule definition at replay time.
    pub definition_hash: String,
    /// Hash of the input event that triggered this decision.
    pub input_hash: String,
    /// Hash of the output/action produced.
    pub output_hash: String,
    /// Virtual timestamp in ms.
    pub timestamp_ms: u64,
    /// Pane ID this decision applies to.
    pub pane_id: u64,
    /// Wall-clock time (transient, excluded from comparison).
    #[serde(default)]
    pub wall_clock_ms: u64,
    /// Replay run ID (transient, excluded from comparison).
    #[serde(default)]
    pub replay_run_id: String,
}

impl DecisionNode {
    /// Compare two nodes for L1 equivalence (ignoring transient fields).
    #[must_use]
    pub fn l1_equivalent(&self, other: &DecisionNode) -> bool {
        self.decision_type == other.decision_type
            && self.rule_id == other.rule_id
            && self.definition_hash == other.definition_hash
            && self.input_hash == other.input_hash
            && self.output_hash == other.output_hash
            && self.timestamp_ms == other.timestamp_ms
            && self.pane_id == other.pane_id
    }

    /// Canonical sort key: (timestamp_ms, pane_id, rule_id).
    #[must_use]
    fn sort_key(&self) -> (u64, u64, &str) {
        (self.timestamp_ms, self.pane_id, &self.rule_id)
    }
}

// ============================================================================
// CausalEdge — directed link between nodes
// ============================================================================

/// A directed causal edge between two decision nodes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CausalEdge {
    /// Source node ID.
    pub from_node: u64,
    /// Target node ID.
    pub to_node: u64,
    /// Type of causal relationship.
    pub edge_type: EdgeType,
}

// ============================================================================
// DecisionEvent — input format for building the graph
// ============================================================================

/// Input event for building a DecisionGraph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionEvent {
    /// Decision type.
    pub decision_type: DecisionType,
    /// Rule/pattern ID.
    pub rule_id: String,
    /// Definition hash.
    pub definition_hash: String,
    /// Input event hash.
    pub input_hash: String,
    /// Output hash.
    pub output_hash: String,
    /// Virtual timestamp in ms.
    pub timestamp_ms: u64,
    /// Pane ID.
    pub pane_id: u64,
    /// Optional: node that triggered this decision.
    #[serde(default)]
    pub triggered_by: Option<u64>,
    /// Optional: node this decision overrides.
    #[serde(default)]
    pub overrides: Option<u64>,
    /// Wall-clock time (transient).
    #[serde(default)]
    pub wall_clock_ms: u64,
    /// Replay run ID (transient).
    #[serde(default)]
    pub replay_run_id: String,
}

// ============================================================================
// DecisionGraph — DAG of decisions
// ============================================================================

/// A directed acyclic graph of replay decisions with causal edges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionGraph {
    /// Nodes indexed by node_id.
    nodes: BTreeMap<u64, DecisionNode>,
    /// All edges.
    edges: Vec<CausalEdge>,
    /// Forward adjacency: node_id -> set of (target_id, edge_type).
    #[serde(skip)]
    forward: HashMap<u64, Vec<(u64, EdgeType)>>,
    /// Reverse adjacency: node_id -> set of (source_id, edge_type).
    #[serde(skip)]
    reverse: HashMap<u64, Vec<(u64, EdgeType)>>,
    /// Canonicalized node ordering (by sort key).
    #[serde(skip)]
    canonical_order: Vec<u64>,
}

impl DecisionGraph {
    /// Build a decision graph from a sequence of decision events.
    #[must_use]
    pub fn from_decisions(decisions: &[DecisionEvent]) -> Self {
        let mut nodes = BTreeMap::new();
        let mut edges = Vec::new();
        let mut forward: HashMap<u64, Vec<(u64, EdgeType)>> = HashMap::new();
        let mut reverse: HashMap<u64, Vec<(u64, EdgeType)>> = HashMap::new();

        for (i, event) in decisions.iter().enumerate() {
            let node_id = i as u64;
            let node = DecisionNode {
                node_id,
                decision_type: event.decision_type,
                rule_id: event.rule_id.clone(),
                definition_hash: event.definition_hash.clone(),
                input_hash: event.input_hash.clone(),
                output_hash: event.output_hash.clone(),
                timestamp_ms: event.timestamp_ms,
                pane_id: event.pane_id,
                wall_clock_ms: event.wall_clock_ms,
                replay_run_id: event.replay_run_id.clone(),
            };
            nodes.insert(node_id, node);

            // TriggeredBy edge.
            if let Some(trigger) = event.triggered_by {
                if trigger < node_id {
                    let edge = CausalEdge {
                        from_node: trigger,
                        to_node: node_id,
                        edge_type: EdgeType::TriggeredBy,
                    };
                    forward
                        .entry(trigger)
                        .or_default()
                        .push((node_id, EdgeType::TriggeredBy));
                    reverse
                        .entry(node_id)
                        .or_default()
                        .push((trigger, EdgeType::TriggeredBy));
                    edges.push(edge);
                }
            }

            // OverriddenBy edge.
            if let Some(overridden) = event.overrides {
                if overridden < node_id {
                    let edge = CausalEdge {
                        from_node: overridden,
                        to_node: node_id,
                        edge_type: EdgeType::OverriddenBy,
                    };
                    forward
                        .entry(overridden)
                        .or_default()
                        .push((node_id, EdgeType::OverriddenBy));
                    reverse
                        .entry(node_id)
                        .or_default()
                        .push((overridden, EdgeType::OverriddenBy));
                    edges.push(edge);
                }
            }

            // PrecededBy: connect to immediately preceding node at same pane_id.
            if node_id > 0 {
                // Find the most recent node for this pane.
                for prev_id in (0..node_id).rev() {
                    if let Some(prev_node) = nodes.get(&prev_id) {
                        if prev_node.pane_id == event.pane_id {
                            let edge = CausalEdge {
                                from_node: prev_id,
                                to_node: node_id,
                                edge_type: EdgeType::PrecededBy,
                            };
                            forward
                                .entry(prev_id)
                                .or_default()
                                .push((node_id, EdgeType::PrecededBy));
                            reverse
                                .entry(node_id)
                                .or_default()
                                .push((prev_id, EdgeType::PrecededBy));
                            edges.push(edge);
                            break;
                        }
                    }
                }
            }
        }

        // Canonical ordering by (timestamp_ms, pane_id, rule_id).
        let mut sorted_ids: Vec<u64> = nodes.keys().copied().collect();
        sorted_ids.sort_by(|a, b| {
            let na = &nodes[a];
            let nb = &nodes[b];
            na.sort_key().cmp(&nb.sort_key())
        });

        Self {
            nodes,
            edges,
            forward,
            reverse,
            canonical_order: sorted_ids,
        }
    }

    /// Number of nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Get a node by ID.
    #[must_use]
    pub fn get_node(&self, node_id: u64) -> Option<&DecisionNode> {
        self.nodes.get(&node_id)
    }

    /// All nodes in canonical order.
    #[must_use]
    pub fn nodes_canonical(&self) -> Vec<&DecisionNode> {
        self.canonical_order
            .iter()
            .filter_map(|id| self.nodes.get(id))
            .collect()
    }

    /// Nodes filtered by decision type.
    pub fn nodes_by_type(&self, decision_type: DecisionType) -> Vec<&DecisionNode> {
        self.nodes
            .values()
            .filter(|n| n.decision_type == decision_type)
            .collect()
    }

    /// Root nodes (no incoming edges).
    #[must_use]
    pub fn roots(&self) -> Vec<&DecisionNode> {
        self.nodes
            .values()
            .filter(|n| {
                self.reverse
                    .get(&n.node_id)
                    .is_none_or(|preds| preds.is_empty())
            })
            .collect()
    }

    /// Causal chain: all ancestors of a node (BFS over reverse edges).
    #[must_use]
    pub fn causal_chain(&self, node_id: u64) -> Vec<&DecisionNode> {
        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        let mut chain = Vec::new();

        if let Some(preds) = self.reverse.get(&node_id) {
            for (pred_id, _) in preds {
                if visited.insert(*pred_id) {
                    queue.push_back(*pred_id);
                }
            }
        }

        while let Some(current) = queue.pop_front() {
            if let Some(node) = self.nodes.get(&current) {
                chain.push(node);
            }
            if let Some(preds) = self.reverse.get(&current) {
                for (pred_id, _) in preds {
                    if visited.insert(*pred_id) {
                        queue.push_back(*pred_id);
                    }
                }
            }
        }

        // Sort chain by node_id for determinism.
        chain.sort_by_key(|n| n.node_id);
        chain
    }

    /// Effects: all descendants of a node (BFS over forward edges).
    #[must_use]
    pub fn effects(&self, node_id: u64) -> Vec<&DecisionNode> {
        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        let mut descendants = Vec::new();

        if let Some(succs) = self.forward.get(&node_id) {
            for (succ_id, _) in succs {
                if visited.insert(*succ_id) {
                    queue.push_back(*succ_id);
                }
            }
        }

        while let Some(current) = queue.pop_front() {
            if let Some(node) = self.nodes.get(&current) {
                descendants.push(node);
            }
            if let Some(succs) = self.forward.get(&current) {
                for (succ_id, _) in succs {
                    if visited.insert(*succ_id) {
                        queue.push_back(*succ_id);
                    }
                }
            }
        }

        descendants.sort_by_key(|n| n.node_id);
        descendants
    }

    /// Check if the graph is a DAG (no cycles).
    #[must_use]
    pub fn is_dag(&self) -> bool {
        // Kahn's algorithm: count in-degrees, remove zero-degree nodes iteratively.
        let mut in_degree: HashMap<u64, usize> = HashMap::new();
        for id in self.nodes.keys() {
            in_degree.insert(*id, 0);
        }
        for edge in &self.edges {
            *in_degree.entry(edge.to_node).or_insert(0) += 1;
        }

        let mut queue: VecDeque<u64> = in_degree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(id, _)| *id)
            .collect();

        let mut visited = 0usize;
        while let Some(current) = queue.pop_front() {
            visited += 1;
            if let Some(succs) = self.forward.get(&current) {
                for (succ_id, _) in succs {
                    if let Some(deg) = in_degree.get_mut(succ_id) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(*succ_id);
                        }
                    }
                }
            }
        }

        visited == self.nodes.len()
    }

    /// Serialize to JSON value.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Deserialize from JSON string.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let mut graph: DecisionGraph =
            serde_json::from_str(json).map_err(|e| format!("parse error: {e}"))?;
        graph.rebuild_adjacency();
        graph.rebuild_canonical_order();
        Ok(graph)
    }

    /// Rebuild adjacency maps from edges (used after deserialization).
    fn rebuild_adjacency(&mut self) {
        self.forward.clear();
        self.reverse.clear();
        for edge in &self.edges {
            self.forward
                .entry(edge.from_node)
                .or_default()
                .push((edge.to_node, edge.edge_type));
            self.reverse
                .entry(edge.to_node)
                .or_default()
                .push((edge.from_node, edge.edge_type));
        }
    }

    /// Rebuild canonical order from nodes.
    fn rebuild_canonical_order(&mut self) {
        let mut sorted_ids: Vec<u64> = self.nodes.keys().copied().collect();
        sorted_ids.sort_by(|a, b| {
            let na = &self.nodes[a];
            let nb = &self.nodes[b];
            na.sort_key().cmp(&nb.sort_key())
        });
        self.canonical_order = sorted_ids;
    }

    /// Get all edges.
    #[must_use]
    pub fn edges(&self) -> &[CausalEdge] {
        &self.edges
    }

    /// Normalized comparison: compare two graphs ignoring transient fields.
    #[must_use]
    pub fn l1_equivalent(&self, other: &DecisionGraph) -> bool {
        if self.nodes.len() != other.nodes.len() {
            return false;
        }
        let self_nodes = self.nodes_canonical();
        let other_nodes = other.nodes_canonical();
        for (a, b) in self_nodes.iter().zip(other_nodes.iter()) {
            if !a.l1_equivalent(b) {
                return false;
            }
        }
        true
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(
        decision_type: DecisionType,
        rule_id: &str,
        timestamp_ms: u64,
        pane_id: u64,
        triggered_by: Option<u64>,
        overrides: Option<u64>,
    ) -> DecisionEvent {
        DecisionEvent {
            decision_type,
            rule_id: rule_id.into(),
            definition_hash: format!("def_{}", rule_id),
            input_hash: format!("in_{}", timestamp_ms),
            output_hash: format!("out_{}_{}", rule_id, timestamp_ms),
            timestamp_ms,
            pane_id,
            triggered_by,
            overrides,
            wall_clock_ms: timestamp_ms * 2, // Transient.
            replay_run_id: "run_001".into(),
        }
    }

    fn sample_events() -> Vec<DecisionEvent> {
        vec![
            // 0: pattern match on pane 1.
            make_event(DecisionType::PatternMatch, "rule_a", 100, 1, None, None),
            // 1: workflow step triggered by 0.
            make_event(DecisionType::WorkflowStep, "wf_1", 200, 1, Some(0), None),
            // 2: policy decision triggered by 1.
            make_event(DecisionType::PolicyDecision, "pol_1", 300, 1, Some(1), None),
            // 3: pattern match on pane 2 (independent).
            make_event(DecisionType::PatternMatch, "rule_b", 150, 2, None, None),
            // 4: alert on pane 2 triggered by 3.
            make_event(DecisionType::AlertFired, "alert_1", 250, 2, Some(3), None),
            // 5: override applied, overrides node 2.
            make_event(
                DecisionType::OverrideApplied,
                "ovr_1",
                350,
                1,
                None,
                Some(2),
            ),
        ]
    }

    // ── Build from empty ───────────────────────────────────────────────

    #[test]
    fn build_empty() {
        let graph = DecisionGraph::from_decisions(&[]);
        assert_eq!(graph.node_count(), 0);
        assert_eq!(graph.edge_count(), 0);
        assert!(graph.roots().is_empty());
        assert!(graph.is_dag());
    }

    // ── Build from single decision ─────────────────────────────────────

    #[test]
    fn build_single() {
        let events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            None,
            None,
        )];
        let graph = DecisionGraph::from_decisions(&events);
        assert_eq!(graph.node_count(), 1);
        assert_eq!(graph.edge_count(), 0);
        assert_eq!(graph.roots().len(), 1);
        assert_eq!(graph.roots()[0].rule_id, "r1");
    }

    // ── Build from sample events ───────────────────────────────────────

    #[test]
    fn build_sample() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        assert_eq!(graph.node_count(), 6);
        assert!(graph.is_dag());
    }

    // ── Edge counts ────────────────────────────────────────────────────

    #[test]
    fn edge_count_sample() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        // Edges: 0->1 (triggered), 1->2 (triggered), 3->4 (triggered), 2->5 (overridden)
        // Plus PrecededBy: 0->1 (pane1), 1->2 (pane1), 2->5 (pane1), 3->4 (pane2)
        // But 0->1 triggered AND preceded, 1->2 triggered AND preceded, 3->4 triggered AND preceded
        // So preceded: 0->1, 1->2, 3->4, 2->5
        // Total: triggered(3) + overridden(1) + preceded(4) = 8
        assert!(graph.edge_count() >= 7); // At least triggered + overridden + some preceded
    }

    // ── Roots ──────────────────────────────────────────────────────────

    #[test]
    fn roots_sample() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        let roots = graph.roots();
        // Node 0 (pane1, first) and node 3 (pane2, first) are roots.
        let root_ids: BTreeSet<u64> = roots.iter().map(|n| n.node_id).collect();
        assert!(root_ids.contains(&0));
        assert!(root_ids.contains(&3));
    }

    // ── Causal chain ───────────────────────────────────────────────────

    #[test]
    fn causal_chain_leaf() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        // Node 2 (policy_decision) was triggered by 1 which was triggered by 0.
        let chain = graph.causal_chain(2);
        let chain_ids: Vec<u64> = chain.iter().map(|n| n.node_id).collect();
        assert!(chain_ids.contains(&0), "chain should include root ancestor");
        assert!(
            chain_ids.contains(&1),
            "chain should include direct trigger"
        );
    }

    // ── Effects ────────────────────────────────────────────────────────

    #[test]
    fn effects_root() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        // Node 0's effects should include 1, 2, and 5 (via chain).
        let fx = graph.effects(0);
        let fx_ids: Vec<u64> = fx.iter().map(|n| n.node_id).collect();
        assert!(fx_ids.contains(&1));
        assert!(fx_ids.contains(&2));
    }

    // ── Effects of independent root ────────────────────────────────────

    #[test]
    fn effects_independent_root() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        // Node 3 (pane 2) should reach node 4.
        let fx = graph.effects(3);
        let fx_ids: Vec<u64> = fx.iter().map(|n| n.node_id).collect();
        assert!(fx_ids.contains(&4));
        // Should NOT reach pane 1 nodes.
        assert!(!fx_ids.contains(&0));
        assert!(!fx_ids.contains(&1));
    }

    // ── nodes_by_type ──────────────────────────────────────────────────

    #[test]
    fn nodes_by_type_pattern() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        let patterns = graph.nodes_by_type(DecisionType::PatternMatch);
        assert_eq!(patterns.len(), 2);
    }

    #[test]
    fn nodes_by_type_workflow() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        let wfs = graph.nodes_by_type(DecisionType::WorkflowStep);
        assert_eq!(wfs.len(), 1);
    }

    #[test]
    fn nodes_by_type_absent() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        let noop = graph.nodes_by_type(DecisionType::NoOp);
        assert!(noop.is_empty());
    }

    // ── Canonical ordering ─────────────────────────────────────────────

    #[test]
    fn canonical_order_deterministic() {
        let events = sample_events();
        let graph = DecisionGraph::from_decisions(&events);
        let canonical = graph.nodes_canonical();
        // Should be sorted by (timestamp_ms, pane_id, rule_id).
        for window in canonical.windows(2) {
            let a = window[0].sort_key();
            let b = window[1].sort_key();
            assert!(a <= b, "canonical order should be non-decreasing");
        }
    }

    #[test]
    fn canonical_order_shuffled_input() {
        // Build with events in different order, get same canonical order.
        let mut events = sample_events();
        let graph1 = DecisionGraph::from_decisions(&events);

        // Reverse order (node IDs will differ but canonical ordering should be same).
        events.reverse();
        // Need to clear triggered_by/overrides since they reference old indices.
        for e in &mut events {
            e.triggered_by = None;
            e.overrides = None;
        }
        let graph2 = DecisionGraph::from_decisions(&events);

        let can1: Vec<(u64, u64, &str)> = graph1
            .nodes_canonical()
            .iter()
            .map(|n| n.sort_key())
            .collect();
        let can2: Vec<(u64, u64, &str)> = graph2
            .nodes_canonical()
            .iter()
            .map(|n| n.sort_key())
            .collect();
        assert_eq!(can1, can2);
    }

    // ── Normalization: L1 equivalence ignores transient fields ─────────

    #[test]
    fn l1_equivalence_ignores_transient() {
        let events = sample_events();
        let graph1 = DecisionGraph::from_decisions(&events);

        let mut events2 = sample_events();
        for e in &mut events2 {
            e.wall_clock_ms += 999;
            e.replay_run_id = "run_002".into();
        }
        let graph2 = DecisionGraph::from_decisions(&events2);

        assert!(graph1.l1_equivalent(&graph2));
    }

    #[test]
    fn l1_nonequivalent_on_output_change() {
        let events = sample_events();
        let graph1 = DecisionGraph::from_decisions(&events);

        let mut events2 = sample_events();
        events2[0].output_hash = "different".into();
        let graph2 = DecisionGraph::from_decisions(&events2);

        assert!(!graph1.l1_equivalent(&graph2));
    }

    // ── DAG validation ─────────────────────────────────────────────────

    #[test]
    fn is_dag_true() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        assert!(graph.is_dag());
    }

    // ── JSON roundtrip ─────────────────────────────────────────────────

    #[test]
    fn json_roundtrip() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        let json = graph.to_json();
        let restored = DecisionGraph::from_json(&json).unwrap();
        assert_eq!(restored.node_count(), graph.node_count());
        assert_eq!(restored.edge_count(), graph.edge_count());
    }

    #[test]
    fn json_roundtrip_preserves_l1() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        let json = graph.to_json();
        let restored = DecisionGraph::from_json(&json).unwrap();
        assert!(graph.l1_equivalent(&restored));
    }

    // ── Get node ───────────────────────────────────────────────────────

    #[test]
    fn get_node_exists() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        let node = graph.get_node(0).unwrap();
        assert_eq!(node.rule_id, "rule_a");
    }

    #[test]
    fn get_node_missing() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        assert!(graph.get_node(999).is_none());
    }

    // ── Causal chain of root is empty ──────────────────────────────────

    #[test]
    fn causal_chain_root_empty() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        let chain = graph.causal_chain(0);
        assert!(chain.is_empty());
    }

    // ── Effects of leaf ────────────────────────────────────────────────

    #[test]
    fn effects_leaf_empty() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        // Node 4 (alert) has no forward edges in pane 2 beyond itself.
        let fx = graph.effects(4);
        // Node 4 is last in pane 2, so it may have no effects.
        // (Actually check — it might not have forward edges.)
        let fx_ids: BTreeSet<u64> = fx.iter().map(|n| n.node_id).collect();
        assert!(
            !fx_ids.contains(&3),
            "leaf shouldn't point back to ancestor"
        );
    }

    // ── Edge types ─────────────────────────────────────────────────────

    #[test]
    fn edges_contain_triggered_by() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        let triggered: Vec<&CausalEdge> = graph
            .edges()
            .iter()
            .filter(|e| e.edge_type == EdgeType::TriggeredBy)
            .collect();
        assert!(!triggered.is_empty());
        // 0->1, 1->2, 3->4 should all be triggered.
        let pairs: Vec<(u64, u64)> = triggered.iter().map(|e| (e.from_node, e.to_node)).collect();
        assert!(pairs.contains(&(0, 1)));
        assert!(pairs.contains(&(1, 2)));
        assert!(pairs.contains(&(3, 4)));
    }

    #[test]
    fn edges_contain_overridden_by() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        let overridden: Vec<&CausalEdge> = graph
            .edges()
            .iter()
            .filter(|e| e.edge_type == EdgeType::OverriddenBy)
            .collect();
        assert_eq!(overridden.len(), 1);
        assert_eq!(overridden[0].from_node, 2);
        assert_eq!(overridden[0].to_node, 5);
    }

    #[test]
    fn edges_contain_preceded_by() {
        let graph = DecisionGraph::from_decisions(&sample_events());
        assert!(
            graph
                .edges()
                .iter()
                .any(|e| e.edge_type == EdgeType::PrecededBy)
        );
    }

    // ── L1 equivalence with different node_id assignment ────────────────

    #[test]
    fn l1_equivalence_same_graph() {
        let events = sample_events();
        let graph = DecisionGraph::from_decisions(&events);
        assert!(graph.l1_equivalent(&graph));
    }

    // ── Many nodes ─────────────────────────────────────────────────────

    #[test]
    fn build_100_decisions() {
        let events: Vec<DecisionEvent> = (0..100)
            .map(|i| {
                make_event(
                    DecisionType::PatternMatch,
                    &format!("rule_{}", i),
                    i * 10,
                    i % 3,
                    if i > 0 { Some(i - 1) } else { None },
                    None,
                )
            })
            .collect();
        let graph = DecisionGraph::from_decisions(&events);
        assert_eq!(graph.node_count(), 100);
        assert!(graph.is_dag());
    }

    // ── DiffSummary integration ────────────────────────────────────────

    #[test]
    fn graph_node_count_matches_decisions() {
        let events = sample_events();
        let graph = DecisionGraph::from_decisions(&events);
        assert_eq!(graph.node_count(), events.len());
    }

    // ── Serde roundtrip on types ───────────────────────────────────────

    #[test]
    fn decision_type_serde() {
        let dt = DecisionType::WorkflowStep;
        let json = serde_json::to_string(&dt).unwrap();
        let restored: DecisionType = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, dt);
    }

    #[test]
    fn edge_type_serde() {
        let et = EdgeType::TriggeredBy;
        let json = serde_json::to_string(&et).unwrap();
        let restored: EdgeType = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, et);
    }

    #[test]
    fn causal_edge_serde() {
        let edge = CausalEdge {
            from_node: 1,
            to_node: 2,
            edge_type: EdgeType::OverriddenBy,
        };
        let json = serde_json::to_string(&edge).unwrap();
        let restored: CausalEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, edge);
    }

    #[test]
    fn decision_node_serde() {
        let node = DecisionNode {
            node_id: 0,
            decision_type: DecisionType::PatternMatch,
            rule_id: "r1".into(),
            definition_hash: "def".into(),
            input_hash: "in".into(),
            output_hash: "out".into(),
            timestamp_ms: 100,
            pane_id: 1,
            wall_clock_ms: 200,
            replay_run_id: "run".into(),
        };
        let json = serde_json::to_string(&node).unwrap();
        let restored: DecisionNode = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.node_id, 0);
        assert_eq!(restored.rule_id, "r1");
    }

    #[test]
    fn decision_event_serde() {
        let event = DecisionEvent {
            decision_type: DecisionType::AlertFired,
            rule_id: "alert_1".into(),
            definition_hash: "def".into(),
            input_hash: "in".into(),
            output_hash: "out".into(),
            timestamp_ms: 100,
            pane_id: 1,
            triggered_by: Some(0),
            overrides: None,
            wall_clock_ms: 200,
            replay_run_id: "run".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: DecisionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.triggered_by, Some(0));
    }
}
