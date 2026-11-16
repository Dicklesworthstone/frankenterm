//! Deterministic incident DAG builder for swarm flight-recorder events.
//!
//! This module consumes already-normalized
//! [`crate::swarm_causal_event::SwarmCausalEvent`] values and derives a
//! byte-stable incident graph. Explicit event links stay distinguishable from
//! conservative inferred links so proof review can tell facts from hints.

use crate::swarm_causal_event::{
    CausalCorrelationKeys, CausalEventClass, CausalIncidentBudget, CausalPrivacyAudit,
    SwarmCausalEvent, SwarmCausalEventSource, default_incident_budget,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Schema identifier for incident DAG exports.
pub const INCIDENT_DAG_CONTRACT_ID_V1: &str = "ft.swarm.incident_dag.v1";

/// Default proximity window for weak temporal links.
pub const DEFAULT_TEMPORAL_LINK_WINDOW_MS: u64 = 5 * 60 * 1000;

/// Configuration for deterministic incident DAG construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IncidentDagBuildConfig {
    pub budget: CausalIncidentBudget,
    pub temporal_link_window_ms: u64,
    pub include_temporal_proximity_edges: bool,
}

impl Default for IncidentDagBuildConfig {
    fn default() -> Self {
        Self {
            budget: default_incident_budget(),
            temporal_link_window_ms: DEFAULT_TEMPORAL_LINK_WINDOW_MS,
            include_temporal_proximity_edges: true,
        }
    }
}

/// Directed edge kind in a flight-recorder incident DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentDagEdgeKind {
    ExplicitParent,
    ExplicitCause,
    ExplicitRoot,
    SameBead,
    SameRchBuild,
    SamePane,
    SameThread,
    SameGitCommit,
    SameCommand,
    SameArtifact,
    TemporalProximity,
}

/// How strongly an edge is supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentDagEdgeStrength {
    ExplicitFact,
    StrongInference,
    WeakInference,
}

/// A deterministic causal edge between two event nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentDagEdge {
    pub from_event_id: String,
    pub to_event_id: String,
    pub kind: IncidentDagEdgeKind,
    pub strength: IncidentDagEdgeStrength,
    pub confidence_millis: u16,
    pub evidence_key: String,
    pub explanation: String,
}

/// Node summary for one causal event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentDagNode {
    pub event_id: String,
    pub source: SwarmCausalEventSource,
    pub event_class: CausalEventClass,
    pub occurred_at_ms: u64,
    pub ingested_at_ms: u64,
    pub ingest_sequence: u64,
    pub correlation: CausalCorrelationKeys,
    pub privacy: CausalPrivacyAudit,
    pub artifact_uris: Vec<String>,
}

/// Machine-readable gap or suppressed edge explanation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentExplanationGap {
    pub kind: IncidentExplanationGapKind,
    pub event_id: Option<String>,
    pub related_event_ids: Vec<String>,
    pub reason_code: String,
    pub detail: String,
}

/// Gap categories reported by DAG construction and queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentExplanationGapKind {
    MissingLinkedEvent,
    CycleSuppressed,
    IsolatedEvent,
}

/// Proof-admissibility coverage summary derived from structural event data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentProofCoverage {
    pub total_events: usize,
    pub proof_event_count: usize,
    pub admissible: bool,
    pub rch_remote_pass_event_ids: Vec<String>,
    pub rch_blocking_event_ids: Vec<String>,
    pub dirty_tree_event_ids: Vec<String>,
    pub communication_outage_event_ids: Vec<String>,
    pub source_unavailable_event_ids: Vec<String>,
    pub policy_denial_event_ids: Vec<String>,
    pub operator_cancellation_event_ids: Vec<String>,
    pub inadmissible_reason_codes: Vec<String>,
}

/// Deterministic incident graph and bounded query summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentDag {
    pub schema_version: u32,
    pub contract_id: String,
    pub config: IncidentDagBuildConfig,
    pub nodes: Vec<IncidentDagNode>,
    pub edges: Vec<IncidentDagEdge>,
    pub root_event_ids: Vec<String>,
    pub proof_coverage: IncidentProofCoverage,
    pub unexplained_gaps: Vec<IncidentExplanationGap>,
}

/// Build failure for incident DAG construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncidentDagError {
    TooManyEvents { actual: usize, max: usize },
    IncidentPayloadTooLarge { actual: usize, max: usize },
    InvalidEvent { event_id: String, detail: String },
    DuplicateEventId { event_id: String },
}

impl std::fmt::Display for IncidentDagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyEvents { actual, max } => {
                write!(f, "incident has {actual} events, max is {max}")
            }
            Self::IncidentPayloadTooLarge { actual, max } => {
                write!(f, "incident payload total is {actual} bytes, max is {max}")
            }
            Self::InvalidEvent { event_id, detail } => {
                write!(f, "event {event_id:?} is invalid: {detail}")
            }
            Self::DuplicateEventId { event_id } => {
                write!(f, "contradictory duplicate event_id {event_id:?}")
            }
        }
    }
}

impl std::error::Error for IncidentDagError {}

/// Build a deterministic incident DAG from causal events.
pub fn build_incident_dag(
    events: &[SwarmCausalEvent],
    config: IncidentDagBuildConfig,
) -> Result<IncidentDag, IncidentDagError> {
    IncidentDag::from_events(events, config)
}

impl IncidentDag {
    /// Build a deterministic incident DAG from causal events.
    pub fn from_events(
        events: &[SwarmCausalEvent],
        config: IncidentDagBuildConfig,
    ) -> Result<Self, IncidentDagError> {
        let ordered_events = validate_and_order_events(events, config)?;
        let nodes: Vec<IncidentDagNode> = ordered_events
            .iter()
            .map(IncidentDagNode::from_event)
            .collect();
        let mut gaps = Vec::new();
        let edge_map = derive_edges(&ordered_events, config, &mut gaps);
        let edges = sorted_edges(edge_map);
        gaps.extend(isolated_event_gaps(&ordered_events, &edges));
        gaps.sort_by_key(gap_sort_key);
        gaps.dedup();
        let root_event_ids = root_event_ids(&ordered_events, &edges);
        let proof_coverage = proof_coverage(&ordered_events);
        Ok(Self {
            schema_version: 1,
            contract_id: INCIDENT_DAG_CONTRACT_ID_V1.to_string(),
            config,
            nodes,
            edges,
            root_event_ids,
            proof_coverage,
            unexplained_gaps: gaps,
        })
    }

    /// Number of nodes in the graph.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges in the graph.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Return one node by event id.
    #[must_use]
    pub fn node(&self, event_id: &str) -> Option<&IncidentDagNode> {
        self.nodes.iter().find(|node| node.event_id == event_id)
    }

    /// Root-cause candidates, preferring failure roots over informational roots.
    #[must_use]
    pub fn root_causes(&self) -> Vec<&IncidentDagNode> {
        let roots: Vec<&IncidentDagNode> = self
            .root_event_ids
            .iter()
            .filter_map(|event_id| self.node(event_id))
            .collect();
        let failure_roots: Vec<&IncidentDagNode> = roots
            .iter()
            .copied()
            .filter(|node| is_failure_class(node.event_class))
            .collect();
        if failure_roots.is_empty() {
            roots
        } else {
            failure_roots
        }
    }

    /// Bounded downstream effects from one event.
    #[must_use]
    pub fn downstream_effects(&self, event_id: &str, limit: usize) -> Vec<&IncidentDagNode> {
        traverse_graph(event_id, limit, &self.edges, EdgeDirection::Forward)
            .into_iter()
            .filter_map(|node_id| self.node(&node_id))
            .collect()
    }

    /// Bounded causal chain leading into one event.
    #[must_use]
    pub fn causal_chain(&self, event_id: &str, limit: usize) -> Vec<&IncidentDagNode> {
        traverse_graph(event_id, limit, &self.edges, EdgeDirection::Reverse)
            .into_iter()
            .filter_map(|node_id| self.node(&node_id))
            .collect()
    }

    /// Check that the stored edges are acyclic.
    #[must_use]
    pub fn is_dag(&self) -> bool {
        is_acyclic(
            &self.edges,
            self.nodes.iter().map(|node| node.event_id.clone()),
        )
    }

    /// Render stable JSON for golden comparisons.
    #[must_use]
    pub fn to_canonical_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Payload-free proof coverage summary.
    #[must_use]
    pub const fn proof_coverage(&self) -> &IncidentProofCoverage {
        &self.proof_coverage
    }

    /// Machine-readable unexplained gaps.
    #[must_use]
    pub fn unexplained_gaps(&self) -> &[IncidentExplanationGap] {
        &self.unexplained_gaps
    }
}

impl IncidentDagNode {
    fn from_event(event: &SwarmCausalEvent) -> Self {
        let mut artifact_uris: Vec<String> = event
            .artifacts
            .iter()
            .map(|artifact| artifact.uri.clone())
            .collect();
        artifact_uris.sort();
        artifact_uris.dedup();
        Self {
            event_id: event.event_id.clone(),
            source: event.source,
            event_class: event.event_class,
            occurred_at_ms: event.occurred_at_ms,
            ingested_at_ms: event.ingested_at_ms,
            ingest_sequence: event.ingest_sequence,
            correlation: event.correlation.clone(),
            privacy: event.privacy_audit(),
            artifact_uris,
        }
    }
}

fn validate_and_order_events(
    events: &[SwarmCausalEvent],
    config: IncidentDagBuildConfig,
) -> Result<Vec<SwarmCausalEvent>, IncidentDagError> {
    if events.len() > config.budget.max_events {
        return Err(IncidentDagError::TooManyEvents {
            actual: events.len(),
            max: config.budget.max_events,
        });
    }
    let mut total_payload_bytes = 0usize;
    let mut unique = BTreeMap::new();
    for event in events {
        event
            .validate()
            .map_err(|error| IncidentDagError::InvalidEvent {
                event_id: event.event_id.clone(),
                detail: error.to_string(),
            })?;
        total_payload_bytes = total_payload_bytes.saturating_add(event.privacy.payload_bytes);
        if let Some(previous) = unique.insert(event.event_id.clone(), event.clone())
            && previous != *event
        {
            return Err(IncidentDagError::DuplicateEventId {
                event_id: event.event_id.clone(),
            });
        }
    }
    if total_payload_bytes > config.budget.max_total_payload_bytes {
        return Err(IncidentDagError::IncidentPayloadTooLarge {
            actual: total_payload_bytes,
            max: config.budget.max_total_payload_bytes,
        });
    }
    let mut ordered_events: Vec<SwarmCausalEvent> = unique.into_values().collect();
    ordered_events.sort_by(|left, right| event_sort_key(left).cmp(&event_sort_key(right)));
    Ok(ordered_events)
}

fn derive_edges(
    events: &[SwarmCausalEvent],
    config: IncidentDagBuildConfig,
    gaps: &mut Vec<IncidentExplanationGap>,
) -> BTreeMap<(String, String), IncidentDagEdge> {
    let event_ids: BTreeSet<&str> = events.iter().map(|event| event.event_id.as_str()).collect();
    let mut edges = BTreeMap::new();
    for event in events {
        add_explicit_links(event, &event_ids, &mut edges, gaps);
    }
    add_correlation_edges(events, &mut edges, gaps);
    if config.include_temporal_proximity_edges {
        add_temporal_edges(events, config.temporal_link_window_ms, &mut edges, gaps);
    }
    edges
}

fn add_explicit_links(
    event: &SwarmCausalEvent,
    event_ids: &BTreeSet<&str>,
    edges: &mut BTreeMap<(String, String), IncidentDagEdge>,
    gaps: &mut Vec<IncidentExplanationGap>,
) {
    for parent_id in &event.links.parent_event_ids {
        add_explicit_edge_or_gap(
            parent_id,
            event,
            IncidentDagEdgeKind::ExplicitParent,
            "explicit links.parent_event_ids",
            event_ids,
            edges,
            gaps,
        );
    }
    for cause_id in &event.links.caused_by_event_ids {
        add_explicit_edge_or_gap(
            cause_id,
            event,
            IncidentDagEdgeKind::ExplicitCause,
            "explicit links.caused_by_event_ids",
            event_ids,
            edges,
            gaps,
        );
    }
    if let Some(root_id) = &event.links.root_event_id
        && root_id != &event.event_id
    {
        add_explicit_edge_or_gap(
            root_id,
            event,
            IncidentDagEdgeKind::ExplicitRoot,
            "explicit links.root_event_id",
            event_ids,
            edges,
            gaps,
        );
    }
}

fn add_explicit_edge_or_gap(
    from_event_id: &str,
    event: &SwarmCausalEvent,
    kind: IncidentDagEdgeKind,
    evidence_key: &str,
    event_ids: &BTreeSet<&str>,
    edges: &mut BTreeMap<(String, String), IncidentDagEdge>,
    gaps: &mut Vec<IncidentExplanationGap>,
) {
    if !event_ids.contains(from_event_id) {
        gaps.push(IncidentExplanationGap {
            kind: IncidentExplanationGapKind::MissingLinkedEvent,
            event_id: Some(event.event_id.clone()),
            related_event_ids: vec![from_event_id.to_string()],
            reason_code: "incident_dag.missing_explicit_link_target".to_string(),
            detail: format!("{evidence_key} references missing event {from_event_id}"),
        });
        return;
    }
    insert_edge(
        edges,
        IncidentDagEdge {
            from_event_id: from_event_id.to_string(),
            to_event_id: event.event_id.clone(),
            kind,
            strength: IncidentDagEdgeStrength::ExplicitFact,
            confidence_millis: 1000,
            evidence_key: evidence_key.to_string(),
            explanation: "event carried an explicit causal link".to_string(),
        },
        gaps,
    );
}

fn add_correlation_edges(
    events: &[SwarmCausalEvent],
    edges: &mut BTreeMap<(String, String), IncidentDagEdge>,
    gaps: &mut Vec<IncidentExplanationGap>,
) {
    let mut bead = BTreeMap::<String, Vec<usize>>::new();
    let mut rch = BTreeMap::<String, Vec<usize>>::new();
    let mut pane = BTreeMap::<String, Vec<usize>>::new();
    let mut thread = BTreeMap::<String, Vec<usize>>::new();
    let mut git = BTreeMap::<String, Vec<usize>>::new();
    let mut command = BTreeMap::<String, Vec<usize>>::new();
    let mut artifact = BTreeMap::<String, Vec<usize>>::new();

    for (index, event) in events.iter().enumerate() {
        push_optional_key(&mut bead, event.correlation.bead_id.as_deref(), index);
        push_optional_key(&mut rch, event.correlation.rch_build_id.as_deref(), index);
        if let Some(pane_id) = event.correlation.pane_id {
            pane.entry(pane_id.to_string()).or_default().push(index);
        }
        push_optional_key(&mut thread, event.correlation.thread_id.as_deref(), index);
        push_optional_key(&mut git, event.correlation.git_commit.as_deref(), index);
        push_optional_key(&mut command, event.correlation.command_id.as_deref(), index);
        for item in &event.artifacts {
            artifact.entry(item.uri.clone()).or_default().push(index);
        }
    }

    add_group_chains(
        events,
        &bead,
        IncidentDagEdgeKind::SameBead,
        "bead_id",
        850,
        edges,
        gaps,
    );
    add_group_chains(
        events,
        &rch,
        IncidentDagEdgeKind::SameRchBuild,
        "rch_build_id",
        900,
        edges,
        gaps,
    );
    add_group_chains(
        events,
        &pane,
        IncidentDagEdgeKind::SamePane,
        "pane_id",
        650,
        edges,
        gaps,
    );
    add_group_chains(
        events,
        &thread,
        IncidentDagEdgeKind::SameThread,
        "thread_id",
        800,
        edges,
        gaps,
    );
    add_group_chains(
        events,
        &git,
        IncidentDagEdgeKind::SameGitCommit,
        "git_commit",
        750,
        edges,
        gaps,
    );
    add_group_chains(
        events,
        &command,
        IncidentDagEdgeKind::SameCommand,
        "command_id",
        750,
        edges,
        gaps,
    );
    add_group_chains(
        events,
        &artifact,
        IncidentDagEdgeKind::SameArtifact,
        "artifact_uri",
        700,
        edges,
        gaps,
    );
}

fn push_optional_key(index: &mut BTreeMap<String, Vec<usize>>, value: Option<&str>, event: usize) {
    if let Some(value) = value.filter(|item| !item.trim().is_empty()) {
        index.entry(value.to_string()).or_default().push(event);
    }
}

fn add_group_chains(
    events: &[SwarmCausalEvent],
    index: &BTreeMap<String, Vec<usize>>,
    kind: IncidentDagEdgeKind,
    key_label: &str,
    confidence_millis: u16,
    edges: &mut BTreeMap<(String, String), IncidentDagEdge>,
    gaps: &mut Vec<IncidentExplanationGap>,
) {
    for (key, indices) in index {
        if indices.len() < 2 {
            continue;
        }
        for pair in indices.windows(2) {
            let from = &events[pair[0]];
            let to = &events[pair[1]];
            insert_edge(
                edges,
                IncidentDagEdge {
                    from_event_id: from.event_id.clone(),
                    to_event_id: to.event_id.clone(),
                    kind,
                    strength: IncidentDagEdgeStrength::StrongInference,
                    confidence_millis,
                    evidence_key: format!("{key_label}:{}", bounded_key(key)),
                    explanation: format!("events share {key_label} correlation"),
                },
                gaps,
            );
        }
    }
}

fn add_temporal_edges(
    events: &[SwarmCausalEvent],
    temporal_window_ms: u64,
    edges: &mut BTreeMap<(String, String), IncidentDagEdge>,
    gaps: &mut Vec<IncidentExplanationGap>,
) {
    if temporal_window_ms == 0 {
        return;
    }
    for pair in events.windows(2) {
        let from = &pair[0];
        let to = &pair[1];
        let delta = to.ingested_at_ms.saturating_sub(from.ingested_at_ms);
        if delta <= temporal_window_ms {
            insert_edge(
                edges,
                IncidentDagEdge {
                    from_event_id: from.event_id.clone(),
                    to_event_id: to.event_id.clone(),
                    kind: IncidentDagEdgeKind::TemporalProximity,
                    strength: IncidentDagEdgeStrength::WeakInference,
                    confidence_millis: 250,
                    evidence_key: format!("ingest_delta_ms:{delta}"),
                    explanation: "adjacent ingest sequence fell within temporal window".to_string(),
                },
                gaps,
            );
        }
    }
}

fn insert_edge(
    edges: &mut BTreeMap<(String, String), IncidentDagEdge>,
    edge: IncidentDagEdge,
    gaps: &mut Vec<IncidentExplanationGap>,
) {
    if edge.from_event_id == edge.to_event_id {
        return;
    }
    let key = (edge.from_event_id.clone(), edge.to_event_id.clone());
    if let Some(existing) = edges.get(&key)
        && edge_rank(&edge) <= edge_rank(existing)
    {
        return;
    }
    if !edges.contains_key(&key) && path_exists(&edge.to_event_id, &edge.from_event_id, edges) {
        gaps.push(IncidentExplanationGap {
            kind: IncidentExplanationGapKind::CycleSuppressed,
            event_id: Some(edge.to_event_id.clone()),
            related_event_ids: vec![edge.from_event_id.clone()],
            reason_code: "incident_dag.edge_would_create_cycle".to_string(),
            detail: format!(
                "suppressed {:?} edge {} -> {} to preserve DAG",
                edge.kind, edge.from_event_id, edge.to_event_id
            ),
        });
        return;
    }
    edges.insert(key, edge);
}

fn edge_rank(edge: &IncidentDagEdge) -> (u8, u16) {
    let strength = match edge.strength {
        IncidentDagEdgeStrength::ExplicitFact => 3,
        IncidentDagEdgeStrength::StrongInference => 2,
        IncidentDagEdgeStrength::WeakInference => 1,
    };
    (strength, edge.confidence_millis)
}

fn path_exists(
    start: &str,
    target: &str,
    edges: &BTreeMap<(String, String), IncidentDagEdge>,
) -> bool {
    let mut queue = VecDeque::from([start.to_string()]);
    let mut seen = BTreeSet::new();
    while let Some(current) = queue.pop_front() {
        if current == target {
            return true;
        }
        if !seen.insert(current.clone()) {
            continue;
        }
        for edge in edges.values().filter(|edge| edge.from_event_id == current) {
            queue.push_back(edge.to_event_id.clone());
        }
    }
    false
}

fn sorted_edges(edges: BTreeMap<(String, String), IncidentDagEdge>) -> Vec<IncidentDagEdge> {
    let mut edges: Vec<IncidentDagEdge> = edges.into_values().collect();
    edges.sort_by(|left, right| edge_sort_key(left).cmp(&edge_sort_key(right)));
    edges
}

fn root_event_ids(events: &[SwarmCausalEvent], edges: &[IncidentDagEdge]) -> Vec<String> {
    let incoming: BTreeSet<&str> = edges.iter().map(|edge| edge.to_event_id.as_str()).collect();
    events
        .iter()
        .filter(|event| !incoming.contains(event.event_id.as_str()))
        .map(|event| event.event_id.clone())
        .collect()
}

fn proof_coverage(events: &[SwarmCausalEvent]) -> IncidentProofCoverage {
    let mut coverage = IncidentProofCoverage {
        total_events: events.len(),
        ..Default::default()
    };
    for event in events {
        if event.source == SwarmCausalEventSource::Rch {
            coverage.proof_event_count += 1;
            if is_remote_rch_pass(event) {
                coverage
                    .rch_remote_pass_event_ids
                    .push(event.event_id.clone());
            } else if event.event_class != CausalEventClass::SourcePass {
                coverage.rch_blocking_event_ids.push(event.event_id.clone());
            }
        }
        match event.event_class {
            CausalEventClass::DirtyTreeContamination => {
                coverage.dirty_tree_event_ids.push(event.event_id.clone());
            }
            CausalEventClass::CommunicationOutage => {
                coverage
                    .communication_outage_event_ids
                    .push(event.event_id.clone());
            }
            CausalEventClass::PolicyDenial => {
                coverage
                    .policy_denial_event_ids
                    .push(event.event_id.clone());
            }
            CausalEventClass::OperatorCancellation => {
                coverage
                    .operator_cancellation_event_ids
                    .push(event.event_id.clone());
            }
            CausalEventClass::SourceFailure
            | CausalEventClass::InfrastructureFailure
            | CausalEventClass::EvidenceUnavailable
            | CausalEventClass::SourcePass
            | CausalEventClass::Informational => {}
        }
        if event.source == SwarmCausalEventSource::SourceUnavailable {
            coverage
                .source_unavailable_event_ids
                .push(event.event_id.clone());
        }
    }
    let mut reasons = Vec::new();
    if coverage.rch_remote_pass_event_ids.is_empty() {
        reasons.push("proof.no_remote_rch_pass".to_string());
    }
    if !coverage.rch_blocking_event_ids.is_empty() {
        reasons.push("proof.rch_blocking_event".to_string());
    }
    if !coverage.dirty_tree_event_ids.is_empty() {
        reasons.push("proof.dirty_tree_contamination".to_string());
    }
    if !coverage.source_unavailable_event_ids.is_empty() {
        reasons.push("proof.source_unavailable".to_string());
    }
    if !coverage.policy_denial_event_ids.is_empty() {
        reasons.push("proof.policy_denial".to_string());
    }
    if !coverage.operator_cancellation_event_ids.is_empty() {
        reasons.push("proof.operator_cancellation".to_string());
    }
    reasons.sort();
    reasons.dedup();
    coverage.admissible = reasons.is_empty();
    coverage.inadmissible_reason_codes = reasons;
    coverage
}

fn is_remote_rch_pass(event: &SwarmCausalEvent) -> bool {
    event.source == SwarmCausalEventSource::Rch
        && event.event_class == CausalEventClass::SourcePass
        && event
            .payload
            .get("location")
            .and_then(Value::as_str)
            .is_some_and(|location| location == "remote")
        && event.payload.get("exit_code").and_then(Value::as_i64) == Some(0)
}

fn isolated_event_gaps(
    events: &[SwarmCausalEvent],
    edges: &[IncidentDagEdge],
) -> Vec<IncidentExplanationGap> {
    let linked: BTreeSet<&str> = edges
        .iter()
        .flat_map(|edge| [edge.from_event_id.as_str(), edge.to_event_id.as_str()])
        .collect();
    events
        .iter()
        .filter(|event| !linked.contains(event.event_id.as_str()))
        .map(|event| IncidentExplanationGap {
            kind: IncidentExplanationGapKind::IsolatedEvent,
            event_id: Some(event.event_id.clone()),
            related_event_ids: Vec::new(),
            reason_code: "incident_dag.isolated_event".to_string(),
            detail: "event had no explicit, correlation, or temporal link".to_string(),
        })
        .collect()
}

fn traverse_graph(
    event_id: &str,
    limit: usize,
    edges: &[IncidentDagEdge],
    direction: EdgeDirection,
) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    let mut adjacency = BTreeMap::<&str, Vec<&str>>::new();
    for edge in edges {
        match direction {
            EdgeDirection::Forward => adjacency
                .entry(edge.from_event_id.as_str())
                .or_default()
                .push(edge.to_event_id.as_str()),
            EdgeDirection::Reverse => adjacency
                .entry(edge.to_event_id.as_str())
                .or_default()
                .push(edge.from_event_id.as_str()),
        }
    }
    for targets in adjacency.values_mut() {
        targets.sort();
        targets.dedup();
    }
    let mut out = Vec::new();
    let mut queue = VecDeque::from([event_id]);
    let mut seen = BTreeSet::from([event_id]);
    while let Some(current) = queue.pop_front() {
        if let Some(next) = adjacency.get(current) {
            for candidate in next {
                if seen.insert(candidate) && out.len() < limit {
                    out.push((*candidate).to_string());
                    queue.push_back(candidate);
                }
            }
        }
    }
    out
}

fn is_acyclic(edges: &[IncidentDagEdge], node_ids: impl Iterator<Item = String>) -> bool {
    let mut in_degree = BTreeMap::<String, usize>::new();
    let mut forward = BTreeMap::<String, Vec<String>>::new();
    for node_id in node_ids {
        in_degree.insert(node_id, 0);
    }
    for edge in edges {
        *in_degree.entry(edge.to_event_id.clone()).or_insert(0) += 1;
        forward
            .entry(edge.from_event_id.clone())
            .or_default()
            .push(edge.to_event_id.clone());
    }
    let mut queue: VecDeque<String> = in_degree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(node_id, _)| node_id.clone())
        .collect();
    let mut visited = 0usize;
    while let Some(current) = queue.pop_front() {
        visited += 1;
        if let Some(children) = forward.get(&current) {
            for child in children {
                if let Some(degree) = in_degree.get_mut(child) {
                    *degree = degree.saturating_sub(1);
                    if *degree == 0 {
                        queue.push_back(child.clone());
                    }
                }
            }
        }
    }
    visited == in_degree.len()
}

fn is_failure_class(event_class: CausalEventClass) -> bool {
    matches!(
        event_class,
        CausalEventClass::SourceFailure
            | CausalEventClass::InfrastructureFailure
            | CausalEventClass::DirtyTreeContamination
            | CausalEventClass::CommunicationOutage
            | CausalEventClass::PolicyDenial
            | CausalEventClass::OperatorCancellation
            | CausalEventClass::EvidenceUnavailable
    )
}

fn event_sort_key(event: &SwarmCausalEvent) -> (u64, u64, u64, &str) {
    (
        event.ingest_sequence,
        event.ingested_at_ms,
        event.occurred_at_ms,
        event.event_id.as_str(),
    )
}

fn edge_sort_key(edge: &IncidentDagEdge) -> (&str, &str, IncidentDagEdgeKind) {
    (
        edge.from_event_id.as_str(),
        edge.to_event_id.as_str(),
        edge.kind,
    )
}

fn gap_sort_key(gap: &IncidentExplanationGap) -> (String, String, String) {
    (
        gap.event_id.clone().unwrap_or_default(),
        gap.reason_code.clone(),
        gap.related_event_ids.join(","),
    )
}

fn bounded_key(key: &str) -> String {
    const MAX_KEY_BYTES: usize = 96;
    if key.len() <= MAX_KEY_BYTES {
        key.to_string()
    } else {
        format!("{}...", &key[..safe_prefix_boundary(key, MAX_KEY_BYTES)])
    }
}

fn safe_prefix_boundary(value: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

enum EdgeDirection {
    Forward,
    Reverse,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_adapters::{SourceAdapterContext, SourceAdapterSet, adapt_source_set};
    use crate::swarm_causal_event::{
        CausalLinks, CausalPayloadSensitivity, CausalRedactionStatus, CausalRetentionClass,
    };
    use serde::Deserialize;
    use serde_json::json;

    const GOLDEN_SCENARIOS: &str =
        include_str!("../../../fixtures/flight-recorder/incident-dag/golden-scenarios.v1.json");

    fn base_correlation(source: SwarmCausalEventSource) -> CausalCorrelationKeys {
        let mut correlation = CausalCorrelationKeys {
            workspace_id: Some("frankenterm".to_string()),
            ..Default::default()
        };
        match source {
            SwarmCausalEventSource::Pane => correlation.pane_id = Some(7),
            SwarmCausalEventSource::Beads => correlation.bead_id = Some("ft-ogr3n.3".to_string()),
            SwarmCausalEventSource::Rch => {
                correlation.rch_build_id = Some("29871232832766299".to_string());
                correlation.rch_worker_id = Some("vmi1149989".to_string());
            }
            SwarmCausalEventSource::AgentMail => {
                correlation.thread_id = Some("ft-ogr3n.3".to_string());
            }
            SwarmCausalEventSource::Git => {
                correlation.git_commit = Some("abcdef1234567890".to_string());
                correlation.git_branch = Some("main".to_string());
            }
            SwarmCausalEventSource::Robot
            | SwarmCausalEventSource::Mcp
            | SwarmCausalEventSource::Workflow
            | SwarmCausalEventSource::Policy
            | SwarmCausalEventSource::Operator
            | SwarmCausalEventSource::Runtime
            | SwarmCausalEventSource::SourceUnavailable => {}
        }
        correlation
    }

    fn event(
        event_id: &str,
        source: SwarmCausalEventSource,
        event_class: CausalEventClass,
        ingest_sequence: u64,
        links: CausalLinks,
    ) -> SwarmCausalEvent {
        SwarmCausalEvent::new(
            event_id,
            source,
            event_class,
            1_778_000_000_000 + ingest_sequence.saturating_mul(100),
            1_778_000_010_000 + ingest_sequence.saturating_mul(10),
            ingest_sequence,
            base_correlation(source),
            links,
            CausalPayloadSensitivity::Structural,
            CausalRedactionStatus::NotRequired,
            CausalRetentionClass::Proof,
            Vec::new(),
            json!({
                "event": event_id,
                "location": if source == SwarmCausalEventSource::Rch { "remote" } else { "n/a" },
                "exit_code": if source == SwarmCausalEventSource::Rch { 0 } else { -1 },
            }),
        )
        .unwrap()
    }

    #[test]
    fn same_inputs_produce_byte_stable_dag_output() {
        let events = vec![
            event(
                "event-rch-pass",
                SwarmCausalEventSource::Rch,
                CausalEventClass::SourcePass,
                2,
                CausalLinks::default(),
            ),
            event(
                "event-bead-close",
                SwarmCausalEventSource::Beads,
                CausalEventClass::SourcePass,
                1,
                CausalLinks::default(),
            ),
        ];
        let reversed = events.iter().cloned().rev().collect::<Vec<_>>();
        let first = build_incident_dag(&events, IncidentDagBuildConfig::default()).unwrap();
        let second = build_incident_dag(&reversed, IncidentDagBuildConfig::default()).unwrap();
        assert_eq!(first.to_canonical_json(), second.to_canonical_json());
        assert!(first.is_dag());
    }

    #[test]
    fn explicit_links_take_precedence_over_inferred_links() {
        let parent = event(
            "event-parent",
            SwarmCausalEventSource::Beads,
            CausalEventClass::SourcePass,
            1,
            CausalLinks::default(),
        );
        let child = event(
            "event-child",
            SwarmCausalEventSource::Beads,
            CausalEventClass::SourcePass,
            2,
            CausalLinks {
                parent_event_ids: vec!["event-parent".to_string()],
                ..Default::default()
            },
        );
        let dag = build_incident_dag(&[child, parent], IncidentDagBuildConfig::default()).unwrap();
        let edge = dag
            .edges
            .iter()
            .find(|edge| edge.from_event_id == "event-parent" && edge.to_event_id == "event-child")
            .unwrap();
        assert_eq!(edge.kind, IncidentDagEdgeKind::ExplicitParent);
        assert_eq!(edge.strength, IncidentDagEdgeStrength::ExplicitFact);
        assert_eq!(edge.confidence_millis, 1000);
    }

    #[test]
    fn clock_skew_uses_ingest_order_for_partial_ordering() {
        let mut first = event(
            "event-first-ingested",
            SwarmCausalEventSource::Pane,
            CausalEventClass::SourcePass,
            1,
            CausalLinks::default(),
        );
        first.occurred_at_ms = first.ingested_at_ms.saturating_sub(1);
        let mut second = event(
            "event-second-ingested",
            SwarmCausalEventSource::Pane,
            CausalEventClass::EvidenceUnavailable,
            2,
            CausalLinks::default(),
        );
        second.occurred_at_ms = first.occurred_at_ms.saturating_sub(5_000);
        let dag = build_incident_dag(&[second, first], IncidentDagBuildConfig::default()).unwrap();
        assert!(dag.edges.iter().any(|edge| {
            edge.kind == IncidentDagEdgeKind::SamePane
                && edge.from_event_id == "event-first-ingested"
                && edge.to_event_id == "event-second-ingested"
        }));
    }

    #[test]
    fn missing_explicit_link_is_reported_without_dropping_event() {
        let child = event(
            "event-child",
            SwarmCausalEventSource::Beads,
            CausalEventClass::SourcePass,
            2,
            CausalLinks {
                caused_by_event_ids: vec!["event-missing".to_string()],
                ..Default::default()
            },
        );
        let dag = build_incident_dag(&[child], IncidentDagBuildConfig::default()).unwrap();
        assert_eq!(dag.node_count(), 1);
        assert!(dag.unexplained_gaps.iter().any(|gap| {
            gap.kind == IncidentExplanationGapKind::MissingLinkedEvent
                && gap.related_event_ids == ["event-missing"]
        }));
    }

    #[test]
    fn contradictory_duplicate_event_id_fails_closed() {
        let left = event(
            "event-dup",
            SwarmCausalEventSource::Beads,
            CausalEventClass::SourcePass,
            1,
            CausalLinks::default(),
        );
        let right = event(
            "event-dup",
            SwarmCausalEventSource::Beads,
            CausalEventClass::EvidenceUnavailable,
            2,
            CausalLinks::default(),
        );
        let err = build_incident_dag(&[left, right], IncidentDagBuildConfig::default())
            .expect_err("duplicate event id should fail closed");
        assert_eq!(
            err,
            IncidentDagError::DuplicateEventId {
                event_id: "event-dup".to_string()
            }
        );
    }

    #[test]
    fn proof_coverage_explains_admissible_and_blocked_proof() {
        let rch = event(
            "event-rch-pass",
            SwarmCausalEventSource::Rch,
            CausalEventClass::SourcePass,
            1,
            CausalLinks::default(),
        );
        let clean = build_incident_dag(
            std::slice::from_ref(&rch),
            IncidentDagBuildConfig::default(),
        )
        .unwrap();
        assert!(clean.proof_coverage.admissible);
        assert_eq!(
            clean.proof_coverage.rch_remote_pass_event_ids,
            ["event-rch-pass"]
        );

        let dirty = event(
            "event-dirty-git",
            SwarmCausalEventSource::Git,
            CausalEventClass::DirtyTreeContamination,
            2,
            CausalLinks::default(),
        );
        let blocked = build_incident_dag(&[rch, dirty], IncidentDagBuildConfig::default()).unwrap();
        assert!(!blocked.proof_coverage.admissible);
        assert!(
            blocked
                .proof_coverage
                .inadmissible_reason_codes
                .contains(&"proof.dirty_tree_contamination".to_string())
        );
    }

    #[test]
    fn root_and_effect_queries_are_bounded() {
        let root = event(
            "event-root",
            SwarmCausalEventSource::Rch,
            CausalEventClass::InfrastructureFailure,
            1,
            CausalLinks::default(),
        );
        let child = event(
            "event-child",
            SwarmCausalEventSource::Beads,
            CausalEventClass::EvidenceUnavailable,
            2,
            CausalLinks {
                caused_by_event_ids: vec!["event-root".to_string()],
                ..Default::default()
            },
        );
        let dag = build_incident_dag(&[root, child], IncidentDagBuildConfig::default()).unwrap();
        assert_eq!(dag.root_causes()[0].event_id, "event-root");
        assert_eq!(
            dag.downstream_effects("event-root", 1)[0].event_id,
            "event-child"
        );
        assert_eq!(dag.downstream_effects("event-root", 0).len(), 0);
        assert_eq!(dag.causal_chain("event-child", 4)[0].event_id, "event-root");
    }

    #[derive(Debug, Deserialize)]
    struct GoldenFixture {
        contract_id: String,
        scenarios: Vec<GoldenScenario>,
    }

    #[derive(Debug, Deserialize)]
    struct GoldenScenario {
        scenario_id: String,
        source_set: SourceAdapterSet,
        expected: GoldenExpected,
    }

    #[derive(Debug, Deserialize)]
    struct GoldenExpected {
        admissible: bool,
        min_nodes: usize,
        required_classes: Vec<CausalEventClass>,
        required_edge_kinds: Vec<IncidentDagEdgeKind>,
    }

    #[test]
    fn golden_fixture_covers_required_incident_classes() {
        let fixture: GoldenFixture = serde_json::from_str(GOLDEN_SCENARIOS).unwrap();
        assert_eq!(fixture.contract_id, "ft.swarm.incident_dag.fixture.v1");
        assert_eq!(fixture.scenarios.len(), 4);
        let mut seen = BTreeSet::new();
        for scenario in fixture.scenarios {
            seen.insert(scenario.scenario_id.clone());
            let report = adapt_source_set(
                &scenario.source_set,
                SourceAdapterContext {
                    workspace_id: None,
                    ingested_at_ms: 0,
                    first_ingest_sequence: 100,
                },
            );
            assert_eq!(report.failures, Vec::new(), "{}", scenario.scenario_id);
            let dag = build_incident_dag(&report.events, IncidentDagBuildConfig::default())
                .unwrap_or_else(|error| panic!("{}: {error}", scenario.scenario_id));
            assert!(dag.node_count() >= scenario.expected.min_nodes);
            assert_eq!(
                dag.proof_coverage.admissible, scenario.expected.admissible,
                "{}",
                scenario.scenario_id
            );
            for required in scenario.expected.required_classes {
                assert!(
                    dag.nodes.iter().any(|node| node.event_class == required),
                    "{} missing class {:?}",
                    scenario.scenario_id,
                    required
                );
            }
            for required in scenario.expected.required_edge_kinds {
                assert!(
                    dag.edges.iter().any(|edge| edge.kind == required),
                    "{} missing edge {:?}",
                    scenario.scenario_id,
                    required
                );
            }
        }
        assert!(seen.contains("rch_mirror_failure_before_cargo"));
        assert!(seen.contains("dirty_tree_proof_contamination"));
        assert!(seen.contains("agent_mail_outage_with_beads_fallback"));
        assert!(seen.contains("clean_remote_proof_pass"));
    }
}
