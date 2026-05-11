//! Explainability console for orchestration, policy, and connector decisions (ft-3681t.9.7).
//!
//! Provides operator-facing surfaces that show **why** actions were chosen, blocked,
//! retried, or rolled back. Aggregates decision traces from the PolicyEngine decision
//! log, audit chain, connector governor, and workflow engine into a queryable,
//! correlated view for incident triage and trust-building.
//!
//! # Architecture
//!
//! ```text
//! PolicyDecisionLog ─┐
//! AuditChain ────────┤
//! ConnectorGovernor ──┼──► ExplainabilityConsole ──► DecisionTrace[]
//! WorkflowEngine ────┤                                  │
//! ExplanationTemplates┘                                  ▼
//!                                                   TraceRenderer
//!                                                   (human / json)
//! ```
//!
//! # Key types
//!
//! - [`DecisionTrace`]: Complete causal chain from trigger to outcome.
//! - [`TraceQuery`]: Filter parameters for querying traces.
//! - [`TraceResult`]: Paginated result set with summary statistics.
//! - [`ExplainabilityConsole`]: Main entry point aggregating all decision sources.
//! - [`CausalLink`]: Edge in the causal graph connecting related decisions.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use crate::policy::{ActionKind, ActorKind, PolicySurface};
use crate::policy_decision_log::DecisionOutcome;

// ── Decision Trace ──────────────────────────────────────────────────────────

/// A complete decision trace showing why an action was taken or blocked.
///
/// Traces are the primary explainability artifact: each trace captures the
/// full causal chain from the triggering event through rule evaluation,
/// policy checks, and final outcome with human-readable reasoning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionTrace {
    /// Unique trace ID (monotonic within the console).
    pub trace_id: u64,
    /// Unix timestamp (ms) when the decision was made.
    pub timestamp_ms: u64,
    /// The action that was evaluated.
    pub action: ActionKind,
    /// The actor who requested the action.
    pub actor: ActorKind,
    /// Subsystem surface where the request originated.
    pub surface: PolicySurface,
    /// Target pane ID (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// The final outcome.
    pub outcome: DecisionOutcome,
    /// Rule that determined the outcome (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// Human-readable reason for the decision.
    pub reason: String,
    /// Number of rules evaluated before reaching this decision.
    pub rules_evaluated: u32,
    /// Explanation template ID (if one matched).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation_id: Option<String>,
    /// Structured context data (action-specific details).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub context: HashMap<String, String>,
    /// Causal links to related decisions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causal_links: Vec<CausalLink>,
    /// Correlation ID for grouping related traces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Source subsystem that produced this trace.
    pub source: TraceSource,
    /// Severity assessment of this decision.
    pub severity: TraceSeverity,
}

/// Where the trace originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceSource {
    /// PolicyEngine decision log.
    Policy,
    /// Audit chain event.
    Audit,
    /// Connector governor routing decision.
    Connector,
    /// Workflow engine step decision.
    Workflow,
    /// Command guard evaluation.
    CommandGuard,
    /// Rate limiter throttle.
    RateLimiter,
    /// Quarantine enforcement.
    Quarantine,
}

/// Severity assessment of a decision trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceSeverity {
    /// Normal operation, informational.
    Info,
    /// Decision warrants attention.
    Warning,
    /// Action was blocked or failed.
    Denied,
    /// Critical safety enforcement.
    Critical,
}

/// A causal link connecting two related decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalLink {
    /// The related trace ID.
    pub related_trace_id: u64,
    /// Nature of the relationship.
    pub relationship: CausalRelationship,
    /// Optional description of the causal connection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Nature of a causal relationship between traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalRelationship {
    /// This trace was triggered by the related trace.
    TriggeredBy,
    /// This trace triggered the related trace.
    Triggered,
    /// This trace is a retry of the related trace.
    RetryOf,
    /// This trace overrides/supersedes the related trace.
    Overrides,
    /// This trace is the rollback/compensation of the related trace.
    CompensationOf,
    /// Related by correlation (same operation or workflow).
    Correlated,
}

// ── Causal Graph Ledger ────────────────────────────────────────────────────

/// Node classes recorded by the per-session causal graph ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalNodeKind {
    /// Raw pane output, input, lifecycle, or state transition.
    PaneEvent,
    /// Pattern engine match derived from pane content.
    PatternMatch,
    /// Workflow trigger or workflow step.
    WorkflowTrigger,
    /// Mission dispatch, reassignment, or completion record.
    MissionDispatch,
    /// Policy allow/deny/require-approval decision.
    PolicyDecision,
    /// Human or delegated approval event.
    Approval,
    /// Storage write, retention, or migration event.
    StorageWrite,
    /// Recovery, rollback, retry, or compensation action.
    RecoveryAction,
    /// Explicit operator action.
    UserAction,
}

/// Evidence class for a causal edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalEvidenceKind {
    /// Directly observed in a first-party event or durable record.
    Observed,
    /// Derived from bounded inference and therefore uncertain.
    Inferred,
    /// Related by timestamp ordering within the same correlation scope.
    Temporal,
    /// Produced by policy decision wiring.
    Policy,
    /// Produced by mission dispatch wiring.
    Mission,
    /// Produced by storage write or persistence wiring.
    Storage,
    /// Produced by an explicit user/operator action.
    UserAction,
}

/// Direction used for traversal results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalTraversalDirection {
    /// Walk incoming edges toward root causes.
    Ancestors,
    /// Walk outgoing edges toward effects.
    Descendants,
}

/// A node in the bounded per-session causal graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalGraphNode {
    /// Stable node ID inside the session graph.
    pub id: String,
    /// Node class.
    pub kind: CausalNodeKind,
    /// Associated pane, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Human-readable short label.
    pub label: String,
    /// Context values after redaction.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub context: HashMap<String, String>,
}

impl CausalGraphNode {
    /// Build a node with empty context.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        kind: CausalNodeKind,
        timestamp_ms: u64,
        label: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            pane_id: None,
            timestamp_ms,
            label: label.into(),
            context: HashMap::new(),
        }
    }

    /// Attach a pane ID.
    #[must_use]
    pub fn with_pane_id(mut self, pane_id: u64) -> Self {
        self.pane_id = Some(pane_id);
        self
    }

    /// Attach one context key/value pair.
    #[must_use]
    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }
}

/// A directed edge in the bounded causal graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalGraphEdge {
    /// Cause node ID.
    pub from: String,
    /// Effect node ID.
    pub to: String,
    /// Evidence class.
    pub evidence: CausalEvidenceKind,
    /// Confidence in basis points, from 0 to 10_000.
    pub confidence_bps: u16,
    /// Source artifact or subsystem that produced the edge.
    pub source: String,
    /// Optional supporting evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl CausalGraphEdge {
    /// Build a directed causal edge.
    #[must_use]
    pub fn new(
        from: impl Into<String>,
        to: impl Into<String>,
        evidence: CausalEvidenceKind,
        confidence_bps: u16,
        source: impl Into<String>,
    ) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            evidence,
            confidence_bps,
            source: source.into(),
            description: None,
        }
    }

    /// Attach a short evidence description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Whether this edge should be surfaced as an uncertainty gap.
    #[must_use]
    pub const fn is_uncertain(&self) -> bool {
        self.confidence_bps < 10_000
            || matches!(
                self.evidence,
                CausalEvidenceKind::Inferred | CausalEvidenceKind::Temporal
            )
    }
}

/// Missing or uncertain causal evidence surfaced by graph queries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalUncertaintyGap {
    /// Optional start node for the gap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Optional end node for the gap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Machine-readable reason.
    pub reason: String,
    /// Human-readable evidence note.
    pub evidence: String,
}

/// Result of an ancestor or descendant graph walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalTraversalResult {
    /// Root node queried by the caller.
    pub root: String,
    /// Traversal direction.
    pub direction: CausalTraversalDirection,
    /// Nodes visited, including the root when present.
    pub nodes: Vec<CausalGraphNode>,
    /// Edges traversed in discovery order.
    pub edges: Vec<CausalGraphEdge>,
    /// Missing or uncertain links found during traversal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<CausalUncertaintyGap>,
    /// True when traversal stopped because the caller's limit was reached.
    pub truncated: bool,
}

/// Result of a shortest causal path query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalPathResult {
    /// Start node ID.
    pub from: String,
    /// End node ID.
    pub to: String,
    /// Whether a directed path was found.
    pub found: bool,
    /// Nodes on the path in order.
    pub nodes: Vec<CausalGraphNode>,
    /// Edges on the path in order.
    pub edges: Vec<CausalGraphEdge>,
    /// Missing or uncertain evidence along the path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<CausalUncertaintyGap>,
}

/// Causal graph ledger error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CausalGraphError {
    /// Edge references a node that is not retained.
    MissingNode { id: String },
    /// Self-edges are rejected because they do not add causal information.
    SelfEdge { id: String },
    /// Confidence must be in 0..=10_000 basis points.
    InvalidConfidence { confidence_bps: u16 },
}

impl fmt::Display for CausalGraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingNode { id } => write!(f, "causal graph node '{id}' is not retained"),
            Self::SelfEdge { id } => write!(f, "causal graph self-edge rejected for '{id}'"),
            Self::InvalidConfidence { confidence_bps } => {
                write!(f, "causal edge confidence {confidence_bps} exceeds 10000")
            }
        }
    }
}

impl std::error::Error for CausalGraphError {}

/// Bounded per-session graph for incident, mission, and policy causality.
pub struct CausalGraphLedger {
    nodes: HashMap<String, CausalGraphNode>,
    node_order: VecDeque<String>,
    edges: VecDeque<CausalGraphEdge>,
    max_nodes: usize,
    max_edges: usize,
    redaction_keys: HashSet<String>,
}

impl CausalGraphLedger {
    /// Create a bounded graph ledger.
    #[must_use]
    pub fn new(max_nodes: usize, max_edges: usize) -> Self {
        let mut redaction_keys = HashSet::new();
        for key in ["token", "secret", "password", "credential", "api_key"] {
            redaction_keys.insert(key.to_string());
        }
        Self {
            nodes: HashMap::new(),
            node_order: VecDeque::new(),
            edges: VecDeque::new(),
            max_nodes: max_nodes.max(1),
            max_edges: max_edges.max(1),
            redaction_keys,
        }
    }

    /// Add exact context keys that must be redacted on ingest.
    #[must_use]
    pub fn with_redaction_keys<I, K>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = K>,
        K: Into<String>,
    {
        for key in keys {
            self.redaction_keys.insert(key.into().to_lowercase());
        }
        self
    }

    /// Insert or replace a node after applying redaction controls.
    pub fn ingest_node(&mut self, mut node: CausalGraphNode) {
        self.redact_node(&mut node);
        if !self.nodes.contains_key(&node.id) {
            self.node_order.push_back(node.id.clone());
        }
        self.nodes.insert(node.id.clone(), node);
        self.enforce_retention();
    }

    /// Insert a directed edge after validating its invariants.
    pub fn link(&mut self, edge: CausalGraphEdge) -> Result<(), CausalGraphError> {
        if edge.from == edge.to {
            return Err(CausalGraphError::SelfEdge { id: edge.from });
        }
        if edge.confidence_bps > 10_000 {
            return Err(CausalGraphError::InvalidConfidence {
                confidence_bps: edge.confidence_bps,
            });
        }
        for id in [&edge.from, &edge.to] {
            if !self.nodes.contains_key(id) {
                return Err(CausalGraphError::MissingNode { id: id.clone() });
            }
        }
        self.edges.push_back(edge);
        self.enforce_retention();
        Ok(())
    }

    /// Query root causes for a node.
    #[must_use]
    pub fn ancestors(&self, root: &str, limit: usize) -> CausalTraversalResult {
        self.traverse(root, CausalTraversalDirection::Ancestors, limit)
    }

    /// Query effects caused by a node.
    #[must_use]
    pub fn descendants(&self, root: &str, limit: usize) -> CausalTraversalResult {
        self.traverse(root, CausalTraversalDirection::Descendants, limit)
    }

    /// Query the shortest directed path between two retained nodes.
    #[must_use]
    pub fn shortest_path(&self, from: &str, to: &str) -> CausalPathResult {
        let mut gaps = Vec::new();
        if !self.nodes.contains_key(from) {
            gaps.push(CausalUncertaintyGap {
                from: Some(from.to_string()),
                to: Some(to.to_string()),
                reason: "missing_start".to_string(),
                evidence: "start node is not retained in the causal graph".to_string(),
            });
        }
        if !self.nodes.contains_key(to) {
            gaps.push(CausalUncertaintyGap {
                from: Some(from.to_string()),
                to: Some(to.to_string()),
                reason: "missing_end".to_string(),
                evidence: "end node is not retained in the causal graph".to_string(),
            });
        }
        if !gaps.is_empty() {
            return CausalPathResult {
                from: from.to_string(),
                to: to.to_string(),
                found: false,
                nodes: Vec::new(),
                edges: Vec::new(),
                gaps,
            };
        }

        let mut queue = VecDeque::from([from.to_string()]);
        let mut seen = HashSet::from([from.to_string()]);
        let mut prev: HashMap<String, (String, usize)> = HashMap::new();

        while let Some(current) = queue.pop_front() {
            if current == to {
                break;
            }
            for (edge_idx, edge) in self.edges.iter().enumerate() {
                if edge.from != current || seen.contains(&edge.to) {
                    continue;
                }
                seen.insert(edge.to.clone());
                prev.insert(edge.to.clone(), (current.clone(), edge_idx));
                queue.push_back(edge.to.clone());
            }
        }

        if !seen.contains(to) {
            return CausalPathResult {
                from: from.to_string(),
                to: to.to_string(),
                found: false,
                nodes: Vec::new(),
                edges: Vec::new(),
                gaps: vec![CausalUncertaintyGap {
                    from: Some(from.to_string()),
                    to: Some(to.to_string()),
                    reason: "no_recorded_path".to_string(),
                    evidence: "no directed causal path is retained between these nodes".to_string(),
                }],
            };
        }

        let mut node_ids = vec![to.to_string()];
        let mut edge_indices = Vec::new();
        let mut cursor = to.to_string();
        while cursor != from {
            if let Some((previous, edge_idx)) = prev.get(&cursor) {
                edge_indices.push(*edge_idx);
                cursor = previous.clone();
                node_ids.push(cursor.clone());
            } else {
                break;
            }
        }
        node_ids.reverse();
        edge_indices.reverse();

        let nodes = node_ids
            .into_iter()
            .filter_map(|id| self.nodes.get(&id).cloned())
            .collect();
        let edges: Vec<CausalGraphEdge> = edge_indices
            .into_iter()
            .map(|idx| self.edges[idx].clone())
            .collect();
        let gaps = edges
            .iter()
            .filter(|edge| edge.is_uncertain())
            .map(Self::gap_for_uncertain_edge)
            .collect();

        CausalPathResult {
            from: from.to_string(),
            to: to.to_string(),
            found: true,
            nodes,
            edges,
            gaps,
        }
    }

    /// Return all retained uncertainty edges as gap records.
    #[must_use]
    pub fn suspicious_gaps(&self) -> Vec<CausalUncertaintyGap> {
        self.edges
            .iter()
            .filter(|edge| edge.is_uncertain())
            .map(Self::gap_for_uncertain_edge)
            .collect()
    }

    /// Number of retained nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of retained edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    fn traverse(
        &self,
        root: &str,
        direction: CausalTraversalDirection,
        limit: usize,
    ) -> CausalTraversalResult {
        let limit = limit.max(1);
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut gaps = Vec::new();
        let mut truncated = false;

        if let Some(root_node) = self.nodes.get(root) {
            nodes.push(root_node.clone());
        } else {
            gaps.push(CausalUncertaintyGap {
                from: Some(root.to_string()),
                to: None,
                reason: "missing_root".to_string(),
                evidence: "root node is not retained in the causal graph".to_string(),
            });
            return CausalTraversalResult {
                root: root.to_string(),
                direction,
                nodes,
                edges,
                gaps,
                truncated,
            };
        }

        let mut queue = VecDeque::from([root.to_string()]);
        let mut seen = HashSet::from([root.to_string()]);
        while let Some(current) = queue.pop_front() {
            for edge in self.edges_for(&current, direction) {
                let next = match direction {
                    CausalTraversalDirection::Ancestors => &edge.from,
                    CausalTraversalDirection::Descendants => &edge.to,
                };
                edges.push(edge.clone());
                if edge.is_uncertain() {
                    gaps.push(Self::gap_for_uncertain_edge(edge));
                }
                if seen.contains(next) {
                    continue;
                }
                if nodes.len() >= limit {
                    truncated = true;
                    continue;
                }
                if let Some(node) = self.nodes.get(next) {
                    seen.insert(next.clone());
                    nodes.push(node.clone());
                    queue.push_back(next.clone());
                } else {
                    gaps.push(CausalUncertaintyGap {
                        from: Some(edge.from.clone()),
                        to: Some(edge.to.clone()),
                        reason: "missing_link_endpoint".to_string(),
                        evidence: "edge endpoint is not retained in the causal graph".to_string(),
                    });
                }
            }
        }

        CausalTraversalResult {
            root: root.to_string(),
            direction,
            nodes,
            edges,
            gaps,
            truncated,
        }
    }

    fn edges_for(
        &self,
        node_id: &str,
        direction: CausalTraversalDirection,
    ) -> impl Iterator<Item = &CausalGraphEdge> {
        self.edges.iter().filter(move |edge| match direction {
            CausalTraversalDirection::Ancestors => edge.to == node_id,
            CausalTraversalDirection::Descendants => edge.from == node_id,
        })
    }

    fn enforce_retention(&mut self) {
        while self.node_order.len() > self.max_nodes {
            if let Some(evicted) = self.node_order.pop_front() {
                self.nodes.remove(&evicted);
                self.edges
                    .retain(|edge| edge.from != evicted && edge.to != evicted);
            }
        }
        while self.edges.len() > self.max_edges {
            self.edges.pop_front();
        }
    }

    fn redact_node(&self, node: &mut CausalGraphNode) {
        for (key, value) in &mut node.context {
            if self.should_redact_key(key) {
                *value = "[REDACTED]".to_string();
            }
        }
    }

    fn should_redact_key(&self, key: &str) -> bool {
        let key = key.to_lowercase();
        self.redaction_keys.contains(&key)
            || self
                .redaction_keys
                .iter()
                .any(|redacted| key.contains(redacted))
    }

    fn gap_for_uncertain_edge(edge: &CausalGraphEdge) -> CausalUncertaintyGap {
        CausalUncertaintyGap {
            from: Some(edge.from.clone()),
            to: Some(edge.to.clone()),
            reason: "uncertain_edge".to_string(),
            evidence: format!(
                "{:?} edge from {} at {} confidence bps",
                edge.evidence, edge.source, edge.confidence_bps
            ),
        }
    }
}

// ── Trace Query ─────────────────────────────────────────────────────────────

/// Filter parameters for querying decision traces.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceQuery {
    /// Filter by pane ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Filter by action kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<ActionKind>,
    /// Filter by actor kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<ActorKind>,
    /// Filter by outcome.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<DecisionOutcome>,
    /// Filter by source subsystem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<TraceSource>,
    /// Filter by minimum severity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_severity: Option<TraceSeverity>,
    /// Filter by correlation ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Filter by rule ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// Start of time range (epoch ms, inclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// End of time range (epoch ms, exclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    /// Maximum number of results to return.
    pub limit: usize,
    /// Offset for pagination.
    pub offset: usize,
}

impl TraceQuery {
    /// Create a query for all traces (up to limit).
    #[must_use]
    pub fn all(limit: usize) -> Self {
        Self {
            limit,
            ..Default::default()
        }
    }

    /// Create a query for a specific pane.
    #[must_use]
    pub fn for_pane(pane_id: u64, limit: usize) -> Self {
        Self {
            pane_id: Some(pane_id),
            limit,
            ..Default::default()
        }
    }

    /// Create a query for denied decisions only.
    #[must_use]
    pub fn denials(limit: usize) -> Self {
        Self {
            outcome: Some(DecisionOutcome::Deny),
            limit,
            ..Default::default()
        }
    }

    /// Create a query for a specific correlation ID.
    #[must_use]
    pub fn by_correlation(correlation_id: &str, limit: usize) -> Self {
        Self {
            correlation_id: Some(correlation_id.to_string()),
            limit,
            ..Default::default()
        }
    }

    /// Whether a trace matches this query's filters.
    #[must_use]
    pub fn matches(&self, trace: &DecisionTrace) -> bool {
        if let Some(pane_id) = self.pane_id {
            if trace.pane_id != Some(pane_id) {
                return false;
            }
        }
        if let Some(ref action) = self.action {
            if &trace.action != action {
                return false;
            }
        }
        if let Some(ref actor) = self.actor {
            if &trace.actor != actor {
                return false;
            }
        }
        if let Some(outcome) = self.outcome {
            if trace.outcome != outcome {
                return false;
            }
        }
        if let Some(source) = self.source {
            if trace.source != source {
                return false;
            }
        }
        if let Some(min_severity) = self.min_severity {
            if trace.severity < min_severity {
                return false;
            }
        }
        if let Some(ref corr) = self.correlation_id {
            if trace.correlation_id.as_ref() != Some(corr) {
                return false;
            }
        }
        if let Some(ref rule_id) = self.rule_id {
            if trace.rule_id.as_ref() != Some(rule_id) {
                return false;
            }
        }
        if let Some(since) = self.since_ms {
            if trace.timestamp_ms < since {
                return false;
            }
        }
        if let Some(until) = self.until_ms {
            if trace.timestamp_ms >= until {
                return false;
            }
        }
        true
    }
}

// ── Trace Result ────────────────────────────────────────────────────────────

/// Paginated result set from a trace query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceResult {
    /// Matching traces (paginated).
    pub traces: Vec<DecisionTrace>,
    /// Total number of matching traces (before pagination).
    pub total_count: usize,
    /// Summary statistics for the result set.
    pub summary: TraceSummary,
}

/// Summary statistics for a trace query result.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceSummary {
    /// Count by outcome.
    pub by_outcome: HashMap<String, usize>,
    /// Count by source.
    pub by_source: HashMap<String, usize>,
    /// Count by severity.
    pub by_severity: HashMap<String, usize>,
    /// Unique pane IDs involved.
    pub pane_ids: Vec<u64>,
    /// Unique rule IDs that triggered decisions.
    pub rule_ids: Vec<String>,
    /// Time range of results.
    pub earliest_ms: Option<u64>,
    pub latest_ms: Option<u64>,
}

// ── Explainability Console ──────────────────────────────────────────────────

/// Main entry point for the explainability system.
///
/// Collects decision traces from multiple subsystems and provides
/// queryable, correlated views for operators and automation clients.
pub struct ExplainabilityConsole {
    /// All collected traces, ordered by trace_id.
    traces: VecDeque<DecisionTrace>,
    /// Next trace ID to assign.
    next_trace_id: u64,
    /// Maximum traces to retain.
    capacity: usize,
    /// Index: correlation_id → trace indices.
    correlation_index: HashMap<String, Vec<usize>>,
    /// Index: pane_id → trace indices.
    pane_index: HashMap<u64, Vec<usize>>,
    /// Telemetry counters.
    telemetry: ConsoleTelemetry,
}

/// Telemetry counters for the explainability console.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConsoleTelemetry {
    /// Total traces ingested.
    pub traces_ingested: u64,
    /// Total traces evicted (capacity).
    pub traces_evicted: u64,
    /// Total queries executed.
    pub queries_executed: u64,
    /// Total traces matched across all queries.
    pub traces_matched: u64,
}

impl ExplainabilityConsole {
    /// Create a new console with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            traces: VecDeque::new(),
            next_trace_id: 1,
            capacity: capacity.max(1),
            correlation_index: HashMap::new(),
            pane_index: HashMap::new(),
            telemetry: ConsoleTelemetry::default(),
        }
    }

    /// Ingest a new decision trace from any subsystem.
    ///
    /// Assigns a trace_id, updates indices, and evicts oldest traces
    /// if capacity is exceeded. Returns the assigned trace_id.
    pub fn ingest(&mut self, mut trace: DecisionTrace) -> u64 {
        let trace_id = self.next_trace_id;
        self.next_trace_id += 1;
        trace.trace_id = trace_id;

        // Update indices
        let idx = self.traces.len();
        if let Some(ref corr) = trace.correlation_id {
            self.correlation_index
                .entry(corr.clone())
                .or_default()
                .push(idx);
        }
        if let Some(pane_id) = trace.pane_id {
            self.pane_index.entry(pane_id).or_default().push(idx);
        }

        self.traces.push_back(trace);
        self.telemetry.traces_ingested += 1;

        // Evict oldest if over capacity
        while self.traces.len() > self.capacity {
            self.traces.pop_front();
            self.telemetry.traces_evicted += 1;
            // Rebuild indices after removal (indices shifted)
            self.rebuild_indices();
        }

        trace_id
    }

    /// Ingest a trace built from a policy decision log entry.
    pub fn ingest_policy_decision(
        &mut self,
        action: ActionKind,
        actor: ActorKind,
        surface: PolicySurface,
        pane_id: Option<u64>,
        outcome: DecisionOutcome,
        rule_id: Option<String>,
        reason: String,
        rules_evaluated: u32,
        timestamp_ms: u64,
        correlation_id: Option<String>,
    ) -> u64 {
        let severity = match outcome {
            DecisionOutcome::Deny => TraceSeverity::Denied,
            DecisionOutcome::RequireApproval => TraceSeverity::Warning,
            DecisionOutcome::Allow => TraceSeverity::Info,
        };

        let trace = DecisionTrace {
            trace_id: 0, // assigned by ingest
            timestamp_ms,
            action,
            actor,
            surface,
            pane_id,
            outcome,
            rule_id,
            reason,
            rules_evaluated,
            explanation_id: None,
            context: HashMap::new(),
            causal_links: Vec::new(),
            correlation_id,
            source: TraceSource::Policy,
            severity,
        };

        self.ingest(trace)
    }

    /// Ingest a connector routing decision trace.
    pub fn ingest_connector_decision(
        &mut self,
        connector_id: &str,
        action: ActionKind,
        outcome: DecisionOutcome,
        reason: String,
        timestamp_ms: u64,
    ) -> u64 {
        let mut context = HashMap::new();
        context.insert("connector_id".to_string(), connector_id.to_string());

        let trace = DecisionTrace {
            trace_id: 0,
            timestamp_ms,
            action,
            actor: ActorKind::Robot,
            surface: PolicySurface::Connector,
            pane_id: None,
            outcome,
            rule_id: None,
            reason,
            rules_evaluated: 0,
            explanation_id: None,
            context,
            causal_links: Vec::new(),
            correlation_id: None,
            source: TraceSource::Connector,
            severity: if outcome == DecisionOutcome::Deny {
                TraceSeverity::Denied
            } else {
                TraceSeverity::Info
            },
        };

        self.ingest(trace)
    }

    /// Query traces with the given filter.
    pub fn query(&mut self, query: &TraceQuery) -> TraceResult {
        self.telemetry.queries_executed += 1;

        let matching: Vec<&DecisionTrace> =
            self.traces.iter().filter(|t| query.matches(t)).collect();

        let total_count = matching.len();
        self.telemetry.traces_matched += total_count as u64;

        // Build summary
        let summary = Self::build_summary(&matching);

        // Apply pagination
        let traces: Vec<DecisionTrace> = matching
            .into_iter()
            .skip(query.offset)
            .take(if query.limit == 0 {
                usize::MAX
            } else {
                query.limit
            })
            .cloned()
            .collect();

        TraceResult {
            traces,
            total_count,
            summary,
        }
    }

    /// Get a specific trace by ID.
    #[must_use]
    pub fn get_trace(&self, trace_id: u64) -> Option<&DecisionTrace> {
        self.traces.iter().find(|t| t.trace_id == trace_id)
    }

    /// Get all traces correlated to a given trace.
    #[must_use]
    pub fn get_correlated(&self, trace_id: u64) -> Vec<&DecisionTrace> {
        let trace = match self.get_trace(trace_id) {
            Some(t) => t,
            None => return Vec::new(),
        };

        let corr_id = match &trace.correlation_id {
            Some(c) => c,
            None => return Vec::new(),
        };

        self.traces
            .iter()
            .filter(|t| t.correlation_id.as_ref() == Some(corr_id) && t.trace_id != trace_id)
            .collect()
    }

    /// Link two traces with a causal relationship.
    pub fn link_traces(
        &mut self,
        from_id: u64,
        to_id: u64,
        relationship: CausalRelationship,
        description: Option<String>,
    ) -> bool {
        let from_idx = self.traces.iter().position(|t| t.trace_id == from_id);
        if let Some(idx) = from_idx {
            self.traces[idx].causal_links.push(CausalLink {
                related_trace_id: to_id,
                relationship,
                description,
            });
            return true;
        }
        false
    }

    /// Get the number of stored traces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.traces.len()
    }

    /// Whether the console has no traces.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.traces.is_empty()
    }

    /// Get the telemetry snapshot.
    #[must_use]
    pub fn telemetry(&self) -> &ConsoleTelemetry {
        &self.telemetry
    }

    /// Render a trace as a human-readable string.
    #[must_use]
    pub fn render_trace(trace: &DecisionTrace) -> String {
        let outcome_str = match trace.outcome {
            DecisionOutcome::Allow => "ALLOW",
            DecisionOutcome::Deny => "DENY",
            DecisionOutcome::RequireApproval => "REQUIRE_APPROVAL",
        };

        let rule_str = trace.rule_id.as_deref().unwrap_or("(no rule)");

        let mut lines = vec![
            format!(
                "[{}] #{} {} {:?} → {} (rule: {})",
                trace.timestamp_ms,
                trace.trace_id,
                format!("{:?}", trace.source).to_lowercase(),
                trace.action,
                outcome_str,
                rule_str,
            ),
            format!("  reason: {}", trace.reason),
        ];

        if let Some(pane_id) = trace.pane_id {
            lines.push(format!("  pane: {pane_id}"));
        }

        if !trace.context.is_empty() {
            for (k, v) in &trace.context {
                lines.push(format!("  {k}: {v}"));
            }
        }

        if !trace.causal_links.is_empty() {
            for link in &trace.causal_links {
                lines.push(format!(
                    "  → {:?} trace #{}{}",
                    link.relationship,
                    link.related_trace_id,
                    link.description
                        .as_ref()
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default(),
                ));
            }
        }

        lines.join("\n")
    }

    /// Build summary statistics from a set of traces.
    fn build_summary(traces: &[&DecisionTrace]) -> TraceSummary {
        let mut summary = TraceSummary::default();
        let mut pane_set = std::collections::HashSet::new();
        let mut rule_set = std::collections::HashSet::new();

        for trace in traces {
            // By outcome
            let outcome_key = format!("{:?}", trace.outcome).to_lowercase();
            *summary.by_outcome.entry(outcome_key).or_insert(0) += 1;

            // By source
            let source_key = format!("{:?}", trace.source).to_lowercase();
            *summary.by_source.entry(source_key).or_insert(0) += 1;

            // By severity
            let severity_key = format!("{:?}", trace.severity).to_lowercase();
            *summary.by_severity.entry(severity_key).or_insert(0) += 1;

            // Pane IDs
            if let Some(pane_id) = trace.pane_id {
                pane_set.insert(pane_id);
            }

            // Rule IDs
            if let Some(ref rule_id) = trace.rule_id {
                rule_set.insert(rule_id.clone());
            }

            // Time range
            match summary.earliest_ms {
                None => summary.earliest_ms = Some(trace.timestamp_ms),
                Some(e) if trace.timestamp_ms < e => summary.earliest_ms = Some(trace.timestamp_ms),
                _ => {}
            }
            match summary.latest_ms {
                None => summary.latest_ms = Some(trace.timestamp_ms),
                Some(l) if trace.timestamp_ms > l => summary.latest_ms = Some(trace.timestamp_ms),
                _ => {}
            }
        }

        summary.pane_ids = pane_set.into_iter().collect();
        summary.pane_ids.sort_unstable();
        summary.rule_ids = rule_set.into_iter().collect();
        summary.rule_ids.sort();

        summary
    }

    /// Rebuild all indices (after eviction).
    fn rebuild_indices(&mut self) {
        self.correlation_index.clear();
        self.pane_index.clear();

        for (idx, trace) in self.traces.iter().enumerate() {
            if let Some(ref corr) = trace.correlation_id {
                self.correlation_index
                    .entry(corr.clone())
                    .or_default()
                    .push(idx);
            }
            if let Some(pane_id) = trace.pane_id {
                self.pane_index.entry(pane_id).or_default().push(idx);
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_trace(
        action: ActionKind,
        outcome: DecisionOutcome,
        source: TraceSource,
        pane_id: Option<u64>,
        timestamp_ms: u64,
    ) -> DecisionTrace {
        DecisionTrace {
            trace_id: 0,
            timestamp_ms,
            action,
            actor: ActorKind::Robot,
            surface: PolicySurface::Robot,
            pane_id,
            outcome,
            rule_id: None,
            reason: "test reason".to_string(),
            rules_evaluated: 1,
            explanation_id: None,
            context: HashMap::new(),
            causal_links: Vec::new(),
            correlation_id: None,
            source,
            severity: TraceSeverity::Info,
        }
    }

    // -- Console basics --

    #[test]
    fn console_empty_initially() {
        let console = ExplainabilityConsole::new(100);
        assert!(console.is_empty());
        assert_eq!(console.len(), 0);
    }

    #[test]
    fn console_ingest_assigns_trace_id() {
        let mut console = ExplainabilityConsole::new(100);
        let trace = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            Some(1),
            1000,
        );
        let id = console.ingest(trace);
        assert_eq!(id, 1);
        assert_eq!(console.len(), 1);

        let trace2 = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            Some(2),
            1001,
        );
        let id2 = console.ingest(trace2);
        assert_eq!(id2, 2);
    }

    #[test]
    fn console_capacity_eviction() {
        let mut console = ExplainabilityConsole::new(3);
        for i in 0..5 {
            let trace = make_trace(
                ActionKind::SendText,
                DecisionOutcome::Allow,
                TraceSource::Policy,
                Some(i),
                1000 + i,
            );
            console.ingest(trace);
        }
        assert_eq!(console.len(), 3);
        // Oldest traces should be evicted
        assert!(console.get_trace(1).is_none());
        assert!(console.get_trace(2).is_none());
        assert!(console.get_trace(3).is_some());
    }

    #[test]
    fn console_fifo_eviction_preserves_retained_trace_order_ft_znj5k() {
        let mut console = ExplainabilityConsole::new(3);
        for i in 0..5 {
            console.ingest(make_trace(
                ActionKind::SendText,
                DecisionOutcome::Allow,
                TraceSource::Policy,
                Some(i),
                1000 + i,
            ));
        }

        let retained_trace_ids: Vec<u64> = console
            .query(&TraceQuery::all(10))
            .traces
            .into_iter()
            .map(|trace| trace.trace_id)
            .collect();
        assert_eq!(retained_trace_ids, vec![3, 4, 5]);
    }

    #[test]
    fn console_get_trace_by_id() {
        let mut console = ExplainabilityConsole::new(100);
        let trace = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            Some(42),
            1000,
        );
        let id = console.ingest(trace);
        let found = console.get_trace(id).unwrap();
        assert_eq!(found.pane_id, Some(42));
    }

    #[test]
    fn console_get_trace_not_found() {
        let console = ExplainabilityConsole::new(100);
        assert!(console.get_trace(999).is_none());
    }

    // -- Query tests --

    #[test]
    fn query_all() {
        let mut console = ExplainabilityConsole::new(100);
        for i in 0..5 {
            let trace = make_trace(
                ActionKind::SendText,
                DecisionOutcome::Allow,
                TraceSource::Policy,
                Some(i),
                1000 + i,
            );
            console.ingest(trace);
        }
        let result = console.query(&TraceQuery::all(10));
        assert_eq!(result.total_count, 5);
        assert_eq!(result.traces.len(), 5);
    }

    #[test]
    fn query_by_pane() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            Some(1),
            1000,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            Some(2),
            1001,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            Some(1),
            1002,
        ));

        let result = console.query(&TraceQuery::for_pane(1, 10));
        assert_eq!(result.total_count, 2);
    }

    #[test]
    fn query_denials_only() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1000,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            None,
            1001,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            None,
            1002,
        ));

        let result = console.query(&TraceQuery::denials(10));
        assert_eq!(result.total_count, 2);
        assert!(
            result
                .traces
                .iter()
                .all(|t| t.outcome == DecisionOutcome::Deny)
        );
    }

    #[test]
    fn query_by_source() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1000,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Connector,
            None,
            1001,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Workflow,
            None,
            1002,
        ));

        let query = TraceQuery {
            source: Some(TraceSource::Connector),
            limit: 10,
            ..Default::default()
        };
        let result = console.query(&query);
        assert_eq!(result.total_count, 1);
    }

    #[test]
    fn query_by_time_range() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1000,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            2000,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            3000,
        ));

        let query = TraceQuery {
            since_ms: Some(1500),
            until_ms: Some(2500),
            limit: 10,
            ..Default::default()
        };
        let result = console.query(&query);
        assert_eq!(result.total_count, 1);
        assert_eq!(result.traces[0].timestamp_ms, 2000);
    }

    #[test]
    fn query_by_severity() {
        let mut console = ExplainabilityConsole::new(100);

        let mut t1 = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1000,
        );
        t1.severity = TraceSeverity::Info;
        console.ingest(t1);

        let mut t2 = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            None,
            1001,
        );
        t2.severity = TraceSeverity::Denied;
        console.ingest(t2);

        let mut t3 = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            None,
            1002,
        );
        t3.severity = TraceSeverity::Critical;
        console.ingest(t3);

        let query = TraceQuery {
            min_severity: Some(TraceSeverity::Denied),
            limit: 10,
            ..Default::default()
        };
        let result = console.query(&query);
        assert_eq!(result.total_count, 2);
    }

    #[test]
    fn query_pagination() {
        let mut console = ExplainabilityConsole::new(100);
        for i in 0..10 {
            console.ingest(make_trace(
                ActionKind::SendText,
                DecisionOutcome::Allow,
                TraceSource::Policy,
                None,
                1000 + i,
            ));
        }

        let query = TraceQuery {
            limit: 3,
            offset: 2,
            ..Default::default()
        };
        let result = console.query(&query);
        assert_eq!(result.total_count, 10);
        assert_eq!(result.traces.len(), 3);
        assert_eq!(result.traces[0].trace_id, 3); // offset=2 skips first two
    }

    // -- Correlation tests --

    #[test]
    fn correlation_groups_traces() {
        let mut console = ExplainabilityConsole::new(100);

        let mut t1 = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1000,
        );
        t1.correlation_id = Some("op-123".to_string());
        let id1 = console.ingest(t1);

        let mut t2 = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Connector,
            None,
            1001,
        );
        t2.correlation_id = Some("op-123".to_string());
        console.ingest(t2);

        let mut t3 = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1002,
        );
        t3.correlation_id = Some("op-456".to_string());
        console.ingest(t3);

        let correlated = console.get_correlated(id1);
        assert_eq!(correlated.len(), 1);
        assert_eq!(correlated[0].source, TraceSource::Connector);
    }

    #[test]
    fn correlation_query() {
        let mut console = ExplainabilityConsole::new(100);

        let mut t1 = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1000,
        );
        t1.correlation_id = Some("op-123".to_string());
        console.ingest(t1);

        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1001,
        ));

        let result = console.query(&TraceQuery::by_correlation("op-123", 10));
        assert_eq!(result.total_count, 1);
    }

    // -- Causal link tests --

    #[test]
    fn link_traces_creates_causal_edge() {
        let mut console = ExplainabilityConsole::new(100);
        let id1 = console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            None,
            1000,
        ));
        let id2 = console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1001,
        ));

        let linked = console.link_traces(
            id2,
            id1,
            CausalRelationship::RetryOf,
            Some("retry after denial".into()),
        );
        assert!(linked);

        let trace = console.get_trace(id2).unwrap();
        assert_eq!(trace.causal_links.len(), 1);
        assert_eq!(trace.causal_links[0].related_trace_id, id1);
        assert_eq!(
            trace.causal_links[0].relationship,
            CausalRelationship::RetryOf
        );
    }

    #[test]
    fn link_nonexistent_trace_returns_false() {
        let mut console = ExplainabilityConsole::new(100);
        assert!(!console.link_traces(999, 1, CausalRelationship::TriggeredBy, None));
    }

    // -- Convenience ingest methods --

    #[test]
    fn ingest_policy_decision_works() {
        let mut console = ExplainabilityConsole::new(100);
        let id = console.ingest_policy_decision(
            ActionKind::SendText,
            ActorKind::Robot,
            PolicySurface::Robot,
            Some(42),
            DecisionOutcome::Deny,
            Some("safety.alt_screen".into()),
            "Alt screen active".into(),
            3,
            1000,
            None,
        );
        let trace = console.get_trace(id).unwrap();
        assert_eq!(trace.source, TraceSource::Policy);
        assert_eq!(trace.severity, TraceSeverity::Denied);
        assert_eq!(trace.pane_id, Some(42));
    }

    #[test]
    fn ingest_connector_decision_works() {
        let mut console = ExplainabilityConsole::new(100);
        let id = console.ingest_connector_decision(
            "fcp.github",
            ActionKind::SendText,
            DecisionOutcome::Allow,
            "Connector healthy".into(),
            2000,
        );
        let trace = console.get_trace(id).unwrap();
        assert_eq!(trace.source, TraceSource::Connector);
        assert_eq!(trace.context.get("connector_id").unwrap(), "fcp.github");
    }

    // -- Summary tests --

    #[test]
    fn summary_counts_by_outcome() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1000,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1001,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            None,
            1002,
        ));

        let result = console.query(&TraceQuery::all(10));
        assert_eq!(result.summary.by_outcome.get("allow"), Some(&2));
        assert_eq!(result.summary.by_outcome.get("deny"), Some(&1));
    }

    #[test]
    fn summary_tracks_pane_ids() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            Some(1),
            1000,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            Some(3),
            1001,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            Some(1),
            1002,
        ));

        let result = console.query(&TraceQuery::all(10));
        assert_eq!(result.summary.pane_ids, vec![1, 3]);
    }

    #[test]
    fn summary_time_range() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1000,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            3000,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            2000,
        ));

        let result = console.query(&TraceQuery::all(10));
        assert_eq!(result.summary.earliest_ms, Some(1000));
        assert_eq!(result.summary.latest_ms, Some(3000));
    }

    // -- Render tests --

    #[test]
    fn render_trace_contains_key_info() {
        let mut trace = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            Some(42),
            1000,
        );
        trace.trace_id = 7;
        trace.rule_id = Some("safety.alt_screen".into());
        trace.reason = "Alt screen is active".into();

        let rendered = ExplainabilityConsole::render_trace(&trace);
        assert!(rendered.contains("DENY"));
        assert!(rendered.contains("#7"));
        assert!(rendered.contains("safety.alt_screen"));
        assert!(rendered.contains("Alt screen is active"));
        assert!(rendered.contains("pane: 42"));
    }

    #[test]
    fn render_trace_shows_causal_links() {
        let mut trace = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1000,
        );
        trace.trace_id = 5;
        trace.causal_links.push(CausalLink {
            related_trace_id: 3,
            relationship: CausalRelationship::RetryOf,
            description: Some("retry after timeout".into()),
        });

        let rendered = ExplainabilityConsole::render_trace(&trace);
        assert!(rendered.contains("RetryOf"));
        assert!(rendered.contains("trace #3"));
        assert!(rendered.contains("retry after timeout"));
    }

    // -- Telemetry tests --

    #[test]
    fn telemetry_tracks_ingestion() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1000,
        ));
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1001,
        ));

        assert_eq!(console.telemetry().traces_ingested, 2);
    }

    #[test]
    fn telemetry_tracks_evictions() {
        let mut console = ExplainabilityConsole::new(2);
        for i in 0..5 {
            console.ingest(make_trace(
                ActionKind::SendText,
                DecisionOutcome::Allow,
                TraceSource::Policy,
                None,
                1000 + i,
            ));
        }
        assert_eq!(console.telemetry().traces_evicted, 3);
    }

    #[test]
    fn telemetry_tracks_queries() {
        let mut console = ExplainabilityConsole::new(100);
        console.ingest(make_trace(
            ActionKind::SendText,
            DecisionOutcome::Allow,
            TraceSource::Policy,
            None,
            1000,
        ));
        let _ = console.query(&TraceQuery::all(10));
        let _ = console.query(&TraceQuery::all(10));
        assert_eq!(console.telemetry().queries_executed, 2);
        assert_eq!(console.telemetry().traces_matched, 2);
    }

    // -- Serde roundtrip tests --

    #[test]
    fn decision_trace_serde_roundtrip() {
        let mut trace = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            Some(42),
            1000,
        );
        trace.trace_id = 1;
        trace.rule_id = Some("test.rule".into());
        trace.context.insert("key".into(), "value".into());

        let json = serde_json::to_string(&trace).unwrap();
        let trace2: DecisionTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(trace2.trace_id, 1);
        assert_eq!(trace2.outcome, DecisionOutcome::Deny);
        assert_eq!(trace2.context.get("key").unwrap(), "value");
    }

    #[test]
    fn trace_query_serde_roundtrip() {
        let query = TraceQuery::denials(50);
        let json = serde_json::to_string(&query).unwrap();
        let query2: TraceQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(query2.outcome, Some(DecisionOutcome::Deny));
        assert_eq!(query2.limit, 50);
    }

    #[test]
    fn trace_result_serde_roundtrip() {
        let result = TraceResult {
            traces: Vec::new(),
            total_count: 0,
            summary: TraceSummary::default(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let result2: TraceResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result2.total_count, 0);
    }

    #[test]
    fn trace_severity_ordering() {
        assert!(TraceSeverity::Info < TraceSeverity::Warning);
        assert!(TraceSeverity::Warning < TraceSeverity::Denied);
        assert!(TraceSeverity::Denied < TraceSeverity::Critical);
    }

    #[test]
    fn causal_relationship_serde() {
        let link = CausalLink {
            related_trace_id: 5,
            relationship: CausalRelationship::CompensationOf,
            description: None,
        };
        let json = serde_json::to_string(&link).unwrap();
        assert!(json.contains("compensation_of"));
        let link2: CausalLink = serde_json::from_str(&json).unwrap();
        assert_eq!(link2.relationship, CausalRelationship::CompensationOf);
    }

    #[test]
    fn causal_graph_ledger_queries_known_multi_pane_chain() {
        let mut ledger = CausalGraphLedger::new(32, 32);
        for node in [
            CausalGraphNode::new(
                "pane:1:event",
                CausalNodeKind::PaneEvent,
                1000,
                "pane 1 output",
            )
            .with_pane_id(1),
            CausalGraphNode::new(
                "pattern:rate_limit",
                CausalNodeKind::PatternMatch,
                1001,
                "rate-limit pattern",
            )
            .with_pane_id(1),
            CausalGraphNode::new(
                "policy:deny",
                CausalNodeKind::PolicyDecision,
                1002,
                "deny unsafe retry",
            )
            .with_pane_id(1),
            CausalGraphNode::new(
                "mission:handoff",
                CausalNodeKind::MissionDispatch,
                1003,
                "move task to pane 2",
            ),
            CausalGraphNode::new(
                "workflow:recovery",
                CausalNodeKind::WorkflowTrigger,
                1004,
                "run recovery workflow",
            )
            .with_pane_id(2),
            CausalGraphNode::new(
                "storage:audit",
                CausalNodeKind::StorageWrite,
                1005,
                "persist audit",
            ),
            CausalGraphNode::new(
                "pane:2:recovered",
                CausalNodeKind::RecoveryAction,
                1006,
                "pane 2 recovered",
            )
            .with_pane_id(2),
        ] {
            ledger.ingest_node(node);
        }

        for edge in [
            CausalGraphEdge::new(
                "pane:1:event",
                "pattern:rate_limit",
                CausalEvidenceKind::Observed,
                10_000,
                "pattern_engine",
            ),
            CausalGraphEdge::new(
                "pattern:rate_limit",
                "policy:deny",
                CausalEvidenceKind::Policy,
                10_000,
                "policy_gate",
            ),
            CausalGraphEdge::new(
                "policy:deny",
                "mission:handoff",
                CausalEvidenceKind::Mission,
                10_000,
                "mission_dispatch",
            ),
            CausalGraphEdge::new(
                "mission:handoff",
                "workflow:recovery",
                CausalEvidenceKind::Observed,
                10_000,
                "workflow_engine",
            ),
            CausalGraphEdge::new(
                "workflow:recovery",
                "storage:audit",
                CausalEvidenceKind::Storage,
                10_000,
                "storage",
            ),
            CausalGraphEdge::new(
                "storage:audit",
                "pane:2:recovered",
                CausalEvidenceKind::Observed,
                10_000,
                "recovery_driver",
            ),
        ] {
            ledger.link(edge).expect("valid causal edge");
        }

        let descendants = ledger.descendants("pane:1:event", 16);
        assert_eq!(descendants.nodes.len(), 7);
        assert!(descendants.gaps.is_empty());

        let ancestors = ledger.ancestors("pane:2:recovered", 16);
        let ancestor_ids: Vec<&str> = ancestors
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect();
        assert!(ancestor_ids.contains(&"pane:1:event"));
        assert!(ancestor_ids.contains(&"policy:deny"));

        let path = ledger.shortest_path("pane:1:event", "pane:2:recovered");
        assert!(path.found, "{path:#?}");
        let path_ids: Vec<&str> = path.nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(
            path_ids,
            vec![
                "pane:1:event",
                "pattern:rate_limit",
                "policy:deny",
                "mission:handoff",
                "workflow:recovery",
                "storage:audit",
                "pane:2:recovered",
            ]
        );
    }

    #[test]
    fn causal_graph_uncertainty_edges_report_gaps_without_fabrication() {
        let mut ledger = CausalGraphLedger::new(8, 8);
        ledger.ingest_node(CausalGraphNode::new(
            "pane:event",
            CausalNodeKind::PaneEvent,
            1,
            "pane event",
        ));
        ledger.ingest_node(CausalGraphNode::new(
            "workflow:maybe",
            CausalNodeKind::WorkflowTrigger,
            2,
            "maybe workflow",
        ));
        ledger
            .link(
                CausalGraphEdge::new(
                    "pane:event",
                    "workflow:maybe",
                    CausalEvidenceKind::Inferred,
                    6_500,
                    "temporal_correlator",
                )
                .with_description("same pane within 50ms"),
            )
            .expect("valid inferred edge");

        let descendants = ledger.descendants("pane:event", 8);
        assert_eq!(descendants.nodes.len(), 2);
        assert_eq!(descendants.gaps.len(), 1);
        assert_eq!(descendants.gaps[0].reason, "uncertain_edge");
        assert_eq!(ledger.suspicious_gaps().len(), 1);

        let missing = ledger.shortest_path("pane:event", "storage:missing");
        assert!(!missing.found);
        assert_eq!(missing.gaps[0].reason, "missing_end");
    }

    #[test]
    fn causal_graph_retention_redaction_and_cycle_handling() {
        let mut ledger = CausalGraphLedger::new(3, 8).with_redaction_keys(["session_cookie"]);
        ledger.ingest_node(
            CausalGraphNode::new("old", CausalNodeKind::PaneEvent, 1, "old")
                .with_context("api_token", "should not leak"),
        );
        ledger.ingest_node(CausalGraphNode::new("a", CausalNodeKind::PaneEvent, 2, "a"));
        ledger.ingest_node(CausalGraphNode::new(
            "b",
            CausalNodeKind::PolicyDecision,
            3,
            "b",
        ));
        ledger
            .link(CausalGraphEdge::new(
                "a",
                "b",
                CausalEvidenceKind::Observed,
                10_000,
                "test",
            ))
            .expect("valid edge");
        ledger
            .link(CausalGraphEdge::new(
                "b",
                "a",
                CausalEvidenceKind::Observed,
                10_000,
                "test",
            ))
            .expect("cycle allowed for partial-order traversal");
        ledger.ingest_node(
            CausalGraphNode::new("c", CausalNodeKind::UserAction, 4, "c")
                .with_context("session_cookie", "secret cookie"),
        );

        assert_eq!(ledger.node_count(), 3);
        assert_eq!(ledger.ancestors("old", 4).gaps[0].reason, "missing_root");
        assert_eq!(ledger.edge_count(), 2);

        let traversal = ledger.descendants("a", 8);
        let ids: Vec<&str> = traversal
            .nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert!(!traversal.truncated);

        let c = ledger.nodes.get("c").expect("retained node c");
        assert_eq!(c.context.get("session_cookie").unwrap(), "[REDACTED]");
    }

    #[test]
    fn causal_graph_fifo_edge_eviction_preserves_retained_order_ft_znj5k() {
        let mut ledger = CausalGraphLedger::new(8, 2);
        for id in ["a", "b", "c", "d"] {
            ledger.ingest_node(CausalGraphNode::new(id, CausalNodeKind::PaneEvent, 1, id));
        }
        for (from, to) in [("a", "b"), ("b", "c"), ("c", "d")] {
            ledger
                .link(CausalGraphEdge::new(
                    from,
                    to,
                    CausalEvidenceKind::Observed,
                    10_000,
                    "test",
                ))
                .expect("valid edge");
        }

        assert_eq!(ledger.edge_count(), 2);
        let retained_edges: Vec<(&str, &str)> = ledger
            .edges
            .iter()
            .map(|edge| (edge.from.as_str(), edge.to.as_str()))
            .collect();
        assert_eq!(retained_edges, vec![("b", "c"), ("c", "d")]);
    }

    #[test]
    fn causal_graph_rejects_invalid_edges() {
        let mut ledger = CausalGraphLedger::new(4, 4);
        ledger.ingest_node(CausalGraphNode::new("a", CausalNodeKind::PaneEvent, 1, "a"));
        assert_eq!(
            ledger.link(CausalGraphEdge::new(
                "a",
                "a",
                CausalEvidenceKind::Observed,
                10_000,
                "test"
            )),
            Err(CausalGraphError::SelfEdge { id: "a".into() })
        );
        assert_eq!(
            ledger.link(CausalGraphEdge::new(
                "a",
                "missing",
                CausalEvidenceKind::Observed,
                10_000,
                "test"
            )),
            Err(CausalGraphError::MissingNode {
                id: "missing".into()
            })
        );
        assert_eq!(
            ledger.link(CausalGraphEdge::new(
                "a",
                "missing",
                CausalEvidenceKind::Observed,
                10_001,
                "test"
            )),
            Err(CausalGraphError::InvalidConfidence {
                confidence_bps: 10_001
            })
        );
    }

    // -- Rule ID query test --

    #[test]
    fn query_by_rule_id() {
        let mut console = ExplainabilityConsole::new(100);
        let mut t1 = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            None,
            1000,
        );
        t1.rule_id = Some("safety.alt_screen".into());
        console.ingest(t1);

        let mut t2 = make_trace(
            ActionKind::SendText,
            DecisionOutcome::Deny,
            TraceSource::Policy,
            None,
            1001,
        );
        t2.rule_id = Some("safety.rate_limit".into());
        console.ingest(t2);

        let query = TraceQuery {
            rule_id: Some("safety.alt_screen".into()),
            limit: 10,
            ..Default::default()
        };
        let result = console.query(&query);
        assert_eq!(result.total_count, 1);
    }
}
