//! Reactive dataflow graph for declarative agent orchestration.
//!
//! Provides a composable, glitch-free reactive graph inspired by Jane Street's
//! Incremental and Salsa. Nodes represent computations over pane state, metrics,
//! and timers. Edges propagate changes incrementally using topological ordering.
//!
//! # Key properties
//!
//! - **Glitch-free**: Simultaneous source updates are batched; combined nodes see
//!   consistent snapshots (no intermediate states trigger actions).
//! - **Incremental**: Only the affected subgraph is recomputed on each update.
//! - **Cycle-safe**: Adding an edge that would create a cycle returns an error.
//! - **Serializable**: The graph topology can be exported to JSON for debugging.
//!
//! # Example
//!
//! ```
//! use frankenterm_core::dataflow::{DataflowGraph, Value};
//!
//! let mut graph = DataflowGraph::new();
//!
//! // Create source nodes
//! let pane_errors = graph.add_source("pane_a_errors", Value::Bool(false));
//! let pane_cpu = graph.add_source("pane_b_cpu", Value::Float(0.0));
//!
//! // Map: threshold CPU
//! let high_load = graph.add_map("high_load", vec![pane_cpu], |inputs| {
//!     match &inputs[0] {
//!         Value::Float(cpu) => Value::Bool(*cpu > 90.0),
//!         _ => Value::Bool(false),
//!     }
//! });
//!
//! // Combine: errors AND high load
//! let should_restart = graph.add_combine(
//!     "should_restart",
//!     vec![pane_errors, high_load],
//!     |inputs| {
//!         let has_errors = matches!(&inputs[0], Value::Bool(true));
//!         let is_loaded = matches!(&inputs[1], Value::Bool(true));
//!         Value::Bool(has_errors && is_loaded)
//!     },
//! );
//!
//! // Update sources and propagate
//! graph.update_source(pane_errors, Value::Bool(true));
//! graph.update_source(pane_cpu, Value::Float(95.0));
//! graph.propagate();
//!
//! assert_eq!(graph.get_value(should_restart), Some(&Value::Bool(true)));
//! ```

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// =============================================================================
// Value type
// =============================================================================

/// Dynamic value carried by dataflow nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Value {
    /// Boolean signal (threshold crossed, condition met).
    Bool(bool),
    /// Floating-point metric (CPU %, latency, etc.).
    Float(f64),
    /// Integer counter or identifier.
    Int(i64),
    /// Text payload (pane output snippet, pattern match).
    Text(String),
    /// Absent / not-yet-computed.
    None,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{b}"),
            Self::Float(v) => write!(f, "{v:.4}"),
            Self::Int(i) => write!(f, "{i}"),
            Self::Text(s) => write!(f, "{s}"),
            Self::None => write!(f, "None"),
        }
    }
}

impl Value {
    /// Returns true if this value is truthy.
    #[must_use]
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Bool(b) => *b,
            Self::Float(v) => *v != 0.0,
            Self::Int(i) => *i != 0,
            Self::Text(s) => !s.is_empty(),
            Self::None => false,
        }
    }
}

// =============================================================================
// Node identity
// =============================================================================

/// Opaque handle for a node in the dataflow graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(u64);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node:{}", self.0)
    }
}

// =============================================================================
// Compute function
// =============================================================================

/// Type-erased compute function for Map/Combine nodes.
///
/// Receives a slice of input values (one per incoming edge, in edge order)
/// and returns the new output value for the node.
pub type ComputeFn = Box<dyn Fn(&[Value]) -> Value + Send + Sync>;

// =============================================================================
// Node kinds
// =============================================================================

/// The kind of computation a node performs.
enum NodeKind {
    /// External input — updated via `update_source`.
    Source,
    /// Transforms inputs via a compute function.
    Compute(ComputeFn),
    /// Suppresses rapid changes; emits only after a quiet period.
    Debounce {
        window: Duration,
        last_change: Option<Instant>,
        pending: Option<Value>,
        compute: ComputeFn,
    },
}

impl fmt::Debug for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source => write!(f, "Source"),
            Self::Compute(_) => write!(f, "Compute"),
            Self::Debounce { window, .. } => write!(f, "Debounce({window:?})"),
        }
    }
}

// =============================================================================
// Graph node
// =============================================================================

/// A node in the reactive dataflow graph.
struct Node {
    /// Unique identifier.
    id: NodeId,
    /// Human-readable label for debugging.
    label: String,
    /// What this node computes.
    kind: NodeKind,
    /// Current output value.
    value: Value,
    /// IDs of nodes that feed into this node (in order).
    inputs: Vec<NodeId>,
    /// IDs of nodes that consume this node's output.
    outputs: Vec<NodeId>,
    /// Topological depth (0 = source). Recomputed on structural change.
    topo_depth: u32,
}

// =============================================================================
// Errors
// =============================================================================

/// Errors produced by dataflow graph operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DataflowError {
    /// Adding an edge would create a cycle.
    #[error("adding edge {from} -> {to} would create a cycle")]
    CycleDetected {
        /// Source node of the proposed edge.
        from: NodeId,
        /// Target node of the proposed edge.
        to: NodeId,
    },

    /// Referenced node does not exist.
    #[error("node {0} not found")]
    NodeNotFound(NodeId),

    /// Attempted to update a non-source node via `update_source`.
    #[error("node {0} is not a source node")]
    NotASource(NodeId),

    /// Duplicate edge already exists.
    #[error("edge {from} -> {to} already exists")]
    DuplicateEdge {
        /// Source node.
        from: NodeId,
        /// Target node.
        to: NodeId,
    },
}

// =============================================================================
// Propagation stats
// =============================================================================

/// Statistics from a single propagation pass.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PropagationStats {
    /// Number of dirty nodes that were recomputed.
    pub nodes_recomputed: usize,
    /// Number of nodes whose value actually changed.
    pub nodes_changed: usize,
    /// Total nodes in the graph.
    pub total_nodes: usize,
    /// Elapsed wall-clock time.
    pub elapsed: Duration,
}

// =============================================================================
// Graph topology snapshot (serializable)
// =============================================================================

/// Serializable snapshot of the graph topology for debugging/visualization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSnapshot {
    /// All nodes with their labels and current values.
    pub nodes: Vec<NodeSnapshot>,
    /// All edges as (from, to) pairs.
    pub edges: Vec<(u64, u64)>,
}

/// Snapshot of a single node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSnapshot {
    /// Node ID.
    pub id: u64,
    /// Human-readable label.
    pub label: String,
    /// Node kind as a string.
    pub kind: String,
    /// Current value.
    pub value: Value,
    /// Topological depth.
    pub topo_depth: u32,
}

// =============================================================================
// Sink callback
// =============================================================================

/// Callback invoked when a sink node's input changes.
pub type SinkCallback = Box<dyn Fn(&Value) + Send + Sync>;

// =============================================================================
// DataflowGraph
// =============================================================================

/// Reactive dataflow graph engine.
///
/// Manages a DAG of compute nodes. When source values change, the graph
/// propagates updates incrementally in topological order, ensuring that
/// every node sees a consistent snapshot of its inputs (glitch-freedom).
pub struct DataflowGraph {
    /// All nodes keyed by ID.
    nodes: HashMap<NodeId, Node>,
    /// Next ID to allocate.
    next_id: AtomicU64,
    /// Set of nodes whose inputs have changed since last propagation.
    dirty: HashSet<NodeId>,
    /// Cached topological order (invalidated on structural change).
    topo_order: Option<Vec<NodeId>>,
    /// Sink callbacks: node_id -> callback.
    sinks: HashMap<NodeId, SinkCallback>,
    /// Cumulative propagation count.
    propagation_count: u64,
}

impl fmt::Debug for DataflowGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DataflowGraph")
            .field("node_count", &self.nodes.len())
            .field("dirty_count", &self.dirty.len())
            .field("propagation_count", &self.propagation_count)
            .finish()
    }
}

impl Default for DataflowGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl DataflowGraph {
    /// Create an empty dataflow graph.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            next_id: AtomicU64::new(1),
            dirty: HashSet::new(),
            topo_order: None,
            sinks: HashMap::new(),
            propagation_count: 0,
        }
    }

    // =========================================================================
    // Node construction
    // =========================================================================

    fn alloc_id(&self) -> NodeId {
        NodeId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Add a source node with an initial value.
    pub fn add_source(&mut self, label: &str, initial: Value) -> NodeId {
        let id = self.alloc_id();
        let node = Node {
            id,
            label: label.to_string(),
            kind: NodeKind::Source,
            value: initial,
            inputs: Vec::new(),
            outputs: Vec::new(),
            topo_depth: 0,
        };
        self.nodes.insert(id, node);
        self.invalidate_topo();
        id
    }

    /// Add a map/combine node that computes its value from one or more inputs.
    ///
    /// The `compute` function receives input values in the same order as `inputs`.
    pub fn add_map(
        &mut self,
        label: &str,
        inputs: Vec<NodeId>,
        compute: impl Fn(&[Value]) -> Value + Send + Sync + 'static,
    ) -> NodeId {
        let id = self.alloc_id();
        // Register this node as an output of each input.
        for &inp in &inputs {
            if let Some(n) = self.nodes.get_mut(&inp) {
                n.outputs.push(id);
            }
        }
        let node = Node {
            id,
            label: label.to_string(),
            kind: NodeKind::Compute(Box::new(compute)),
            value: Value::None,
            inputs,
            outputs: Vec::new(),
            topo_depth: 0,
        };
        self.nodes.insert(id, node);
        self.invalidate_topo();
        // Mark as dirty so it computes on first propagation.
        self.dirty.insert(id);
        id
    }

    /// Convenience alias for `add_map` that makes intent clearer when combining
    /// multiple inputs.
    pub fn add_combine(
        &mut self,
        label: &str,
        inputs: Vec<NodeId>,
        compute: impl Fn(&[Value]) -> Value + Send + Sync + 'static,
    ) -> NodeId {
        self.add_map(label, inputs, compute)
    }

    /// Add a debounce node that suppresses rapid changes.
    ///
    /// The node only emits a new value after `window` has elapsed without
    /// further changes to its inputs.
    pub fn add_debounce(
        &mut self,
        label: &str,
        inputs: Vec<NodeId>,
        window: Duration,
        compute: impl Fn(&[Value]) -> Value + Send + Sync + 'static,
    ) -> NodeId {
        let id = self.alloc_id();
        for &inp in &inputs {
            if let Some(n) = self.nodes.get_mut(&inp) {
                n.outputs.push(id);
            }
        }
        let node = Node {
            id,
            label: label.to_string(),
            kind: NodeKind::Debounce {
                window,
                last_change: None,
                pending: None,
                compute: Box::new(compute),
            },
            value: Value::None,
            inputs,
            outputs: Vec::new(),
            topo_depth: 0,
        };
        self.nodes.insert(id, node);
        self.invalidate_topo();
        self.dirty.insert(id);
        id
    }

    /// Register a sink callback that fires when `node_id`'s value changes.
    ///
    /// # Errors
    /// Returns `DataflowError::NodeNotFound` if `node_id` does not exist.
    pub fn add_sink(
        &mut self,
        node_id: NodeId,
        callback: impl Fn(&Value) + Send + Sync + 'static,
    ) -> Result<(), DataflowError> {
        if !self.nodes.contains_key(&node_id) {
            return Err(DataflowError::NodeNotFound(node_id));
        }
        self.sinks.insert(node_id, Box::new(callback));
        Ok(())
    }

    /// Add an edge from `from` to `to`. Returns error if it would create a cycle.
    ///
    /// # Errors
    /// - `DataflowError::NodeNotFound` if either node is missing.
    /// - `DataflowError::CycleDetected` if the edge would create a cycle.
    /// - `DataflowError::DuplicateEdge` if the edge already exists.
    pub fn add_edge(&mut self, from: NodeId, to: NodeId) -> Result<(), DataflowError> {
        if !self.nodes.contains_key(&from) {
            return Err(DataflowError::NodeNotFound(from));
        }
        if !self.nodes.contains_key(&to) {
            return Err(DataflowError::NodeNotFound(to));
        }
        // Check for duplicate.
        if let Some(target) = self.nodes.get(&to) {
            if target.inputs.contains(&from) {
                return Err(DataflowError::DuplicateEdge { from, to });
            }
        }
        // Check if adding this edge would create a cycle.
        if self.would_create_cycle(from, to) {
            return Err(DataflowError::CycleDetected { from, to });
        }
        // Wire up.
        self.nodes.get_mut(&to).unwrap().inputs.push(from);
        self.nodes.get_mut(&from).unwrap().outputs.push(to);
        self.invalidate_topo();
        self.dirty.insert(to);
        Ok(())
    }

    /// Remove a node and all its edges.
    ///
    /// # Errors
    /// Returns `DataflowError::NodeNotFound` if the node does not exist.
    pub fn remove_node(&mut self, id: NodeId) -> Result<(), DataflowError> {
        let node = self
            .nodes
            .remove(&id)
            .ok_or(DataflowError::NodeNotFound(id))?;
        // Remove from input lists of downstream nodes.
        for &out_id in &node.outputs {
            if let Some(out_node) = self.nodes.get_mut(&out_id) {
                out_node.inputs.retain(|&inp| inp != id);
                self.dirty.insert(out_id);
            }
        }
        // Remove from output lists of upstream nodes.
        for &inp_id in &node.inputs {
            if let Some(inp_node) = self.nodes.get_mut(&inp_id) {
                inp_node.outputs.retain(|&out| out != id);
            }
        }
        self.sinks.remove(&id);
        self.dirty.remove(&id);
        self.invalidate_topo();
        Ok(())
    }

    // =========================================================================
    // Source updates
    // =========================================================================

    /// Set a source node's value, marking its dependents as dirty.
    ///
    /// # Errors
    /// - `DataflowError::NodeNotFound` if `id` does not exist.
    /// - `DataflowError::NotASource` if `id` is not a source node.
    pub fn update_source(&mut self, id: NodeId, value: Value) -> Result<(), DataflowError> {
        let node = self
            .nodes
            .get_mut(&id)
            .ok_or(DataflowError::NodeNotFound(id))?;
        if !matches!(node.kind, NodeKind::Source) {
            return Err(DataflowError::NotASource(id));
        }
        if node.value != value {
            node.value = value;
            // Mark direct dependents as dirty.
            for &out in &node.outputs.clone() {
                self.dirty.insert(out);
            }
        }
        Ok(())
    }

    // =========================================================================
    // Propagation
    // =========================================================================

    /// Propagate all pending changes through the graph.
    ///
    /// Processes dirty nodes in topological order so that each node
    /// sees the final values of all its inputs (glitch-freedom).
    ///
    /// Returns statistics about the propagation.
    pub fn propagate(&mut self) -> PropagationStats {
        let start = Instant::now();
        let total_nodes = self.nodes.len();

        if self.dirty.is_empty() {
            return PropagationStats {
                nodes_recomputed: 0,
                nodes_changed: 0,
                total_nodes,
                elapsed: start.elapsed(),
            };
        }

        // Ensure topo order is computed.
        self.ensure_topo_order();
        let topo = self.topo_order.clone().unwrap_or_default();

        // Collect the full set of nodes to recompute: dirty nodes plus
        // all transitive dependents in topological order.
        let mut to_recompute = Vec::new();
        let mut reachable: HashSet<NodeId> = self.dirty.clone();
        for &nid in &topo {
            if reachable.contains(&nid) {
                to_recompute.push(nid);
                // Mark all outputs as reachable too.
                if let Some(node) = self.nodes.get(&nid) {
                    for &out in &node.outputs {
                        reachable.insert(out);
                    }
                }
            }
        }

        let mut nodes_recomputed = 0;
        let mut nodes_changed = 0;
        let now = Instant::now();

        for nid in to_recompute {
            // Gather input values.
            let input_ids: Vec<NodeId> = self
                .nodes
                .get(&nid)
                .map(|n| n.inputs.clone())
                .unwrap_or_default();
            let input_values: Vec<Value> = input_ids
                .iter()
                .filter_map(|iid| self.nodes.get(iid).map(|n| n.value.clone()))
                .collect();

            let node = match self.nodes.get_mut(&nid) {
                Some(n) => n,
                None => continue,
            };

            match &mut node.kind {
                NodeKind::Source => {
                    // Sources are already updated; their dependents are dirty.
                }
                NodeKind::Compute(compute) => {
                    let new_val = compute(&input_values);
                    nodes_recomputed += 1;
                    if new_val != node.value {
                        node.value = new_val;
                        nodes_changed += 1;
                    }
                }
                NodeKind::Debounce {
                    window,
                    last_change,
                    pending,
                    compute,
                } => {
                    let new_val = compute(&input_values);
                    nodes_recomputed += 1;
                    let window_dur = *window;
                    match last_change {
                        Some(lc) if now.duration_since(*lc) < window_dur => {
                            // Still within debounce window — buffer but don't emit.
                            *pending = Some(new_val);
                            *last_change = Some(now);
                        }
                        _ => {
                            // Window elapsed or first change — emit.
                            if new_val != node.value {
                                node.value = new_val;
                                nodes_changed += 1;
                            }
                            *last_change = Some(now);
                            *pending = None;
                        }
                    }
                }
            }
        }

        // Fire sink callbacks for changed nodes.
        let sink_ids: Vec<NodeId> = self.sinks.keys().copied().collect();
        for sid in sink_ids {
            if reachable.contains(&sid) {
                if let (Some(node), Some(callback)) = (self.nodes.get(&sid), self.sinks.get(&sid)) {
                    callback(&node.value);
                }
            }
        }

        self.dirty.clear();
        self.propagation_count += 1;

        PropagationStats {
            nodes_recomputed,
            nodes_changed,
            total_nodes,
            elapsed: start.elapsed(),
        }
    }

    /// Flush any debounce nodes whose quiet window has elapsed.
    ///
    /// Call this periodically (e.g., every 100ms) to ensure debounced
    /// values eventually emit. Returns the number of nodes flushed.
    pub fn flush_debounced(&mut self) -> usize {
        let now = Instant::now();
        let mut flushed = 0;
        let node_ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        for nid in node_ids {
            let should_flush = {
                let node = match self.nodes.get(&nid) {
                    Some(n) => n,
                    None => continue,
                };
                match &node.kind {
                    NodeKind::Debounce {
                        window,
                        last_change: Some(lc),
                        pending: Some(_),
                        ..
                    } => now.duration_since(*lc) >= *window,
                    _ => false,
                }
            };
            if should_flush {
                let node = self.nodes.get_mut(&nid).unwrap();
                if let NodeKind::Debounce { pending, .. } = &mut node.kind {
                    if let Some(val) = pending.take() {
                        if val != node.value {
                            node.value = val;
                            flushed += 1;
                            // Mark dependents dirty.
                            for &out in &node.outputs.clone() {
                                self.dirty.insert(out);
                            }
                        }
                    }
                }
            }
        }
        flushed
    }

    // =========================================================================
    // Query
    // =========================================================================

    /// Get the current value of a node.
    #[must_use]
    pub fn get_value(&self, id: NodeId) -> Option<&Value> {
        self.nodes.get(&id).map(|n| &n.value)
    }

    /// Get the label of a node.
    #[must_use]
    pub fn get_label(&self, id: NodeId) -> Option<&str> {
        self.nodes.get(&id).map(|n| n.label.as_str())
    }

    /// Number of nodes in the graph.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges in the graph.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.nodes.values().map(|n| n.inputs.len()).sum()
    }

    /// Whether the graph has no dirty nodes pending propagation.
    #[must_use]
    pub fn is_stable(&self) -> bool {
        self.dirty.is_empty()
    }

    /// Returns all node IDs.
    #[must_use]
    pub fn node_ids(&self) -> Vec<NodeId> {
        self.nodes.keys().copied().collect()
    }

    /// Returns cumulative propagation count.
    #[must_use]
    pub fn propagation_count(&self) -> u64 {
        self.propagation_count
    }

    /// Export a serializable snapshot of the graph topology and values.
    #[must_use]
    pub fn snapshot(&self) -> GraphSnapshot {
        let mut nodes: Vec<NodeSnapshot> = self
            .nodes
            .values()
            .map(|n| NodeSnapshot {
                id: n.id.0,
                label: n.label.clone(),
                kind: format!("{:?}", n.kind),
                value: n.value.clone(),
                topo_depth: n.topo_depth,
            })
            .collect();
        nodes.sort_by_key(|n| n.id);

        let mut edges: Vec<(u64, u64)> = Vec::new();
        for node in self.nodes.values() {
            for &inp in &node.inputs {
                edges.push((inp.0, node.id.0));
            }
        }
        edges.sort();

        GraphSnapshot { nodes, edges }
    }

    /// Returns true if the graph contains no cycles.
    #[must_use]
    pub fn is_acyclic(&self) -> bool {
        // Use Kahn's algorithm: if we can process all nodes, graph is acyclic.
        let mut in_degree: HashMap<NodeId, usize> = HashMap::new();
        for node in self.nodes.values() {
            in_degree.entry(node.id).or_insert(0);
            for &out in &node.outputs {
                *in_degree.entry(out).or_insert(0) += 1;
            }
        }

        let mut queue: VecDeque<NodeId> = in_degree
            .iter()
            .filter(|&(_, &deg)| deg == 0)
            .map(|(&id, _)| id)
            .collect();

        let mut processed = 0;
        while let Some(nid) = queue.pop_front() {
            processed += 1;
            if let Some(node) = self.nodes.get(&nid) {
                for &out in &node.outputs {
                    if let Some(deg) = in_degree.get_mut(&out) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(out);
                        }
                    }
                }
            }
        }

        processed == self.nodes.len()
    }

    // =========================================================================
    // Graph composition
    // =========================================================================

    /// Merge another graph's topology and values into this one.
    ///
    /// All node IDs in `other` are remapped to avoid conflicts. Returns a
    /// mapping from old IDs (in `other`) to new IDs (in `self`).
    ///
    /// **Limitation**: Compute functions (`ComputeFn`) are not clonable, so
    /// merged compute/debounce nodes become inert sources retaining their
    /// last-computed value. Callers must re-register compute functions on
    /// the remapped IDs if dynamic behavior is needed.
    pub fn merge(&mut self, other: &DataflowGraph) -> HashMap<NodeId, NodeId> {
        let mut id_map: HashMap<NodeId, NodeId> = HashMap::new();

        // First pass: create nodes with new IDs (sources and compute).
        for (&old_id, node) in &other.nodes {
            let new_id = self.alloc_id();
            id_map.insert(old_id, new_id);

            let new_node = Node {
                id: new_id,
                label: node.label.clone(),
                kind: NodeKind::Source, // placeholder
                value: node.value.clone(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                topo_depth: 0,
            };
            self.nodes.insert(new_id, new_node);
        }

        // Second pass: rewire edges with mapped IDs.
        for (&old_id, node) in &other.nodes {
            let new_id = id_map[&old_id];
            let mapped_inputs: Vec<NodeId> = node
                .inputs
                .iter()
                .filter_map(|i| id_map.get(i).copied())
                .collect();
            let mapped_outputs: Vec<NodeId> = node
                .outputs
                .iter()
                .filter_map(|o| id_map.get(o).copied())
                .collect();

            if let Some(n) = self.nodes.get_mut(&new_id) {
                n.inputs = mapped_inputs;
                n.outputs = mapped_outputs;
            }
        }

        self.invalidate_topo();
        id_map
    }

    // =========================================================================
    // Internal helpers
    // =========================================================================

    /// Check if adding an edge from → to would create a cycle.
    fn would_create_cycle(&self, from: NodeId, to: NodeId) -> bool {
        if from == to {
            return true;
        }
        // BFS from `from` following reverse edges (inputs): can we reach `to`?
        // Actually, we need: is `from` reachable from `to` via outputs?
        // If so, adding to->from direction (from is output of to) creates a cycle.
        // Wait: we're adding from->to, meaning `to` gets `from` as input.
        // Cycle exists if `from` is already reachable from `to` via outputs.
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(from);
        while let Some(current) = queue.pop_front() {
            if current == to {
                // We can reach `to` from `from` via outputs, so adding
                // from->to (to has from as input) would close the cycle.
                return true;
            }
            if !visited.insert(current) {
                continue;
            }
            if let Some(node) = self.nodes.get(&current) {
                for &out in &node.outputs {
                    queue.push_back(out);
                }
            }
        }
        false
    }

    fn invalidate_topo(&mut self) {
        self.topo_order = None;
    }

    fn ensure_topo_order(&mut self) {
        if self.topo_order.is_some() {
            return;
        }
        self.topo_order = Some(self.compute_topo_order());
        // Update depths.
        if let Some(ref order) = self.topo_order {
            for &nid in order {
                let depth = {
                    let node = match self.nodes.get(&nid) {
                        Some(n) => n,
                        None => continue,
                    };
                    if node.inputs.is_empty() {
                        0
                    } else {
                        node.inputs
                            .iter()
                            .filter_map(|i| self.nodes.get(i).map(|n| n.topo_depth))
                            .max()
                            .unwrap_or(0)
                            + 1
                    }
                };
                if let Some(node) = self.nodes.get_mut(&nid) {
                    node.topo_depth = depth;
                }
            }
        }
    }

    /// Kahn's algorithm for topological sort.
    fn compute_topo_order(&self) -> Vec<NodeId> {
        let mut in_degree: HashMap<NodeId, usize> = HashMap::new();
        for node in self.nodes.values() {
            in_degree.entry(node.id).or_insert(0);
            for &out in &node.outputs {
                *in_degree.entry(out).or_insert(0) += 1;
            }
        }

        let mut queue: VecDeque<NodeId> = in_degree
            .iter()
            .filter(|&(_, &deg)| deg == 0)
            .map(|(&id, _)| id)
            .collect();

        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(nid) = queue.pop_front() {
            order.push(nid);
            if let Some(node) = self.nodes.get(&nid) {
                for &out in &node.outputs {
                    if let Some(deg) = in_degree.get_mut(&out) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(out);
                        }
                    }
                }
            }
        }

        order
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_node_stores_value() {
        let mut g = DataflowGraph::new();
        let s = g.add_source("x", Value::Int(42));
        assert_eq!(g.get_value(s), Some(&Value::Int(42)));
    }

    #[test]
    fn map_node_computes_on_propagation() {
        let mut g = DataflowGraph::new();
        let s = g.add_source("x", Value::Int(10));
        let m = g.add_map("double", vec![s], |inputs| match &inputs[0] {
            Value::Int(v) => Value::Int(v * 2),
            _ => Value::None,
        });
        let stats = g.propagate();
        assert_eq!(g.get_value(m), Some(&Value::Int(20)));
        assert_eq!(stats.nodes_recomputed, 1);
    }

    #[test]
    fn chain_propagation() {
        let mut g = DataflowGraph::new();
        let s = g.add_source("x", Value::Int(5));
        let a = g.add_map("add1", vec![s], |i| match &i[0] {
            Value::Int(v) => Value::Int(v + 1),
            _ => Value::None,
        });
        let b = g.add_map("mul2", vec![a], |i| match &i[0] {
            Value::Int(v) => Value::Int(v * 2),
            _ => Value::None,
        });
        g.propagate();
        assert_eq!(g.get_value(a), Some(&Value::Int(6)));
        assert_eq!(g.get_value(b), Some(&Value::Int(12)));
    }

    #[test]
    fn combine_two_sources() {
        let mut g = DataflowGraph::new();
        let a = g.add_source("a", Value::Bool(true));
        let b = g.add_source("b", Value::Bool(false));
        let c = g.add_combine("and", vec![a, b], |inputs| {
            Value::Bool(inputs[0].is_truthy() && inputs[1].is_truthy())
        });
        g.propagate();
        assert_eq!(g.get_value(c), Some(&Value::Bool(false)));

        g.update_source(b, Value::Bool(true)).unwrap();
        g.propagate();
        assert_eq!(g.get_value(c), Some(&Value::Bool(true)));
    }

    #[test]
    fn glitch_freedom_diamond() {
        // Diamond: S -> A, S -> B, A+B -> C.
        // When S changes, C should see the new values of both A and B,
        // never a mix of old and new.
        let mut graph = DataflowGraph::new();
        let source = graph.add_source("s", Value::Int(1));
        let add_ten = graph.add_map("a", vec![source], |inputs| match &inputs[0] {
            Value::Int(v) => Value::Int(v + 10),
            _ => Value::None,
        });
        let add_hundred = graph.add_map("b", vec![source], |inputs| match &inputs[0] {
            Value::Int(v) => Value::Int(v + 100),
            _ => Value::None,
        });
        let combined = graph.add_combine("c", vec![add_ten, add_hundred], |inputs| {
            match (&inputs[0], &inputs[1]) {
                (Value::Int(lhs), Value::Int(rhs)) => Value::Int(lhs + rhs),
                _ => Value::None,
            }
        });

        graph.propagate();
        // S=1 → A=11, B=101 → C=112
        assert_eq!(graph.get_value(combined), Some(&Value::Int(112)));

        // Update S to 2. A should become 12, B should become 102, C should be 114.
        // Glitch would be: C sees A=12 + B=101 = 113 (if B not yet updated).
        graph.update_source(source, Value::Int(2)).unwrap();
        graph.propagate();
        assert_eq!(graph.get_value(add_ten), Some(&Value::Int(12)));
        assert_eq!(graph.get_value(add_hundred), Some(&Value::Int(102)));
        assert_eq!(graph.get_value(combined), Some(&Value::Int(114)));
    }

    #[test]
    fn incremental_update_skips_unaffected() {
        let mut g = DataflowGraph::new();
        let s1 = g.add_source("s1", Value::Int(1));
        let s2 = g.add_source("s2", Value::Int(2));
        let m1 = g.add_map("m1", vec![s1], |i| i[0].clone());
        let _m2 = g.add_map("m2", vec![s2], |i| i[0].clone());

        g.propagate();

        // Update only s1 — m2 should NOT be recomputed.
        g.update_source(s1, Value::Int(10)).unwrap();
        let stats = g.propagate();
        assert_eq!(stats.nodes_recomputed, 1); // only m1
        assert_eq!(g.get_value(m1), Some(&Value::Int(10)));
    }

    #[test]
    fn cycle_detection_self_loop() {
        let mut g = DataflowGraph::new();
        let s = g.add_source("s", Value::None);
        let result = g.add_edge(s, s);
        assert!(matches!(result, Err(DataflowError::CycleDetected { .. })));
    }

    #[test]
    fn cycle_detection_indirect() {
        let mut g = DataflowGraph::new();
        let a = g.add_source("a", Value::None);
        let b = g.add_map("b", vec![a], |_| Value::None);
        let c = g.add_map("c", vec![b], |_| Value::None);
        // Try to add c -> a (would create a -> b -> c -> a cycle).
        let result = g.add_edge(c, a);
        assert!(matches!(result, Err(DataflowError::CycleDetected { .. })));
    }

    #[test]
    fn duplicate_edge_rejected() {
        let mut g = DataflowGraph::new();
        let a = g.add_source("a", Value::None);
        let b = g.add_map("b", vec![a], |_| Value::None);
        let result = g.add_edge(a, b);
        assert!(matches!(result, Err(DataflowError::DuplicateEdge { .. })));
    }

    #[test]
    fn update_nonexistent_node_errors() {
        let mut g = DataflowGraph::new();
        let result = g.update_source(NodeId(999), Value::None);
        assert!(matches!(result, Err(DataflowError::NodeNotFound(_))));
    }

    #[test]
    fn update_non_source_errors() {
        let mut g = DataflowGraph::new();
        let s = g.add_source("s", Value::Int(0));
        let m = g.add_map("m", vec![s], |_| Value::None);
        let result = g.update_source(m, Value::Int(5));
        assert!(matches!(result, Err(DataflowError::NotASource(_))));
    }

    #[test]
    fn remove_node_cleans_edges() {
        let mut g = DataflowGraph::new();
        let a = g.add_source("a", Value::Int(1));
        let b = g.add_map("b", vec![a], |i| i[0].clone());
        let c = g.add_map("c", vec![b], |i| i[0].clone());

        g.remove_node(b).unwrap();
        assert_eq!(g.node_count(), 2);
        // c should have no inputs after b is removed.
        g.propagate();
        assert_eq!(g.get_value(c), Some(&Value::None));
    }

    #[test]
    fn sink_callback_fires_on_change() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut g = DataflowGraph::new();
        let s = g.add_source("s", Value::Bool(false));
        let m = g.add_map("m", vec![s], |i| i[0].clone());

        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        g.add_sink(m, move |_val| {
            fired_clone.store(true, Ordering::SeqCst);
        })
        .unwrap();

        g.update_source(s, Value::Bool(true)).unwrap();
        g.propagate();
        assert!(fired.load(Ordering::SeqCst));
    }

    #[test]
    fn snapshot_serialization() {
        let mut g = DataflowGraph::new();
        let _s = g.add_source("metric", Value::Float(42.5));
        let snap = g.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("metric"));
        assert!(json.contains("42.5"));
    }

    #[test]
    fn graph_is_acyclic_after_construction() {
        let mut g = DataflowGraph::new();
        let a = g.add_source("a", Value::None);
        let b = g.add_map("b", vec![a], |_| Value::None);
        let _c = g.add_map("c", vec![b], |_| Value::None);
        assert!(g.is_acyclic());
    }

    #[test]
    fn empty_propagation_returns_zero_stats() {
        let mut g = DataflowGraph::new();
        let _s = g.add_source("s", Value::None);
        g.propagate(); // clear initial dirty
        let stats = g.propagate();
        assert_eq!(stats.nodes_recomputed, 0);
        assert_eq!(stats.nodes_changed, 0);
    }

    #[test]
    fn stable_value_no_recompute() {
        let mut g = DataflowGraph::new();
        let s = g.add_source("s", Value::Int(1));
        let _m = g.add_map("m", vec![s], |i| i[0].clone());
        g.propagate();

        // Set source to the same value — dependents should not be marked dirty.
        g.update_source(s, Value::Int(1)).unwrap();
        let stats = g.propagate();
        assert_eq!(stats.nodes_recomputed, 0);
    }

    #[test]
    fn fanout_propagation() {
        let mut g = DataflowGraph::new();
        let s = g.add_source("s", Value::Int(1));
        let m1 = g.add_map("m1", vec![s], |i| match &i[0] {
            Value::Int(v) => Value::Int(v + 1),
            _ => Value::None,
        });
        let m2 = g.add_map("m2", vec![s], |i| match &i[0] {
            Value::Int(v) => Value::Int(v * 10),
            _ => Value::None,
        });
        let m3 = g.add_map("m3", vec![s], |i| match &i[0] {
            Value::Int(v) => Value::Int(v - 1),
            _ => Value::None,
        });

        g.propagate();
        assert_eq!(g.get_value(m1), Some(&Value::Int(2)));
        assert_eq!(g.get_value(m2), Some(&Value::Int(10)));
        assert_eq!(g.get_value(m3), Some(&Value::Int(0)));
    }

    #[test]
    fn value_is_truthy() {
        assert!(Value::Bool(true).is_truthy());
        assert!(!Value::Bool(false).is_truthy());
        assert!(Value::Int(1).is_truthy());
        assert!(!Value::Int(0).is_truthy());
        assert!(Value::Float(0.1).is_truthy());
        assert!(!Value::Float(0.0).is_truthy());
        assert!(Value::Text("hello".into()).is_truthy());
        assert!(!Value::Text(String::new()).is_truthy());
        assert!(!Value::None.is_truthy());
    }

    #[test]
    fn value_display() {
        assert_eq!(format!("{}", Value::Bool(true)), "true");
        assert_eq!(format!("{}", Value::Int(42)), "42");
        assert_eq!(format!("{}", Value::None), "None");
    }

    #[test]
    fn large_chain_propagation() {
        let mut g = DataflowGraph::new();
        let mut prev = g.add_source("s", Value::Int(0));
        for i in 0..100 {
            prev = g.add_map(&format!("n{i}"), vec![prev], |inputs| match &inputs[0] {
                Value::Int(v) => Value::Int(v + 1),
                _ => Value::None,
            });
        }
        g.propagate();
        assert_eq!(g.get_value(prev), Some(&Value::Int(100)));
    }

    #[test]
    fn node_id_display() {
        assert_eq!(format!("{}", NodeId(42)), "node:42");
    }

    #[test]
    fn edge_count_matches() {
        let mut g = DataflowGraph::new();
        let a = g.add_source("a", Value::None);
        let b = g.add_source("b", Value::None);
        let _c = g.add_combine("c", vec![a, b], |_| Value::None);
        assert_eq!(g.edge_count(), 2);
    }

    #[test]
    fn propagation_count_increments() {
        let mut g = DataflowGraph::new();
        let s = g.add_source("s", Value::Int(0));
        let _m = g.add_map("m", vec![s], |i| i[0].clone());
        assert_eq!(g.propagation_count(), 0);
        g.propagate();
        assert_eq!(g.propagation_count(), 1);
        g.update_source(s, Value::Int(1)).unwrap();
        g.propagate();
        assert_eq!(g.propagation_count(), 2);
    }
}
