//! Deterministic incident DAG construction for swarm flight-recorder events.
//!
//! This module consumes already-normalized
//! [`crate::swarm_causal_event::SwarmCausalEvent`] values and links them into a
//! bounded, byte-stable incident graph. It is intentionally read-only: it does
//! not inspect live panes, call Beads/RCH/Agent Mail, or mutate git state.

use crate::swarm_causal_event::{
    CausalCorrelationKeys, CausalEventClass, CausalIncidentBudget, CausalPrivacyAudit,
    DEFAULT_MAX_INCIDENT_EVENTS, DEFAULT_MAX_INCIDENT_PAYLOAD_BYTES, SwarmCausalEvent,
    SwarmCausalEventSource, default_incident_budget,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const INCIDENT_CAUSALITY_CONTRACT_ID_V1: &str = "ft.swarm.incident_dag.v1";
pub const DEFAULT_TEMPORAL_WINDOW_MS: u64 = 5 * 60 * 1000;

/// Tunables for deterministic incident DAG construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentGraphConfig {
    pub temporal_window_ms: u64,
    pub budget: CausalIncidentBudget,
}

impl Default for IncidentGraphConfig {
    fn default() -> Self {
        Self {
            temporal_window_ms: DEFAULT_TEMPORAL_WINDOW_MS,
            budget: default_incident_budget(),
        }
    }
}

/// Failure building a bounded incident DAG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncidentCausalityError {
    DuplicateEventId {
        event_id: String,
    },
    EventValidationFailed {
        event_id: String,
        reason: String,
    },
    EventBudgetExceeded {
        actual: usize,
        max: usize,
    },
    PayloadBudgetExceeded {
        actual: usize,
        max: usize,
    },
}

impl std::fmt::Display for IncidentCausalityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateEventId { event_id } => {
                write!(f, "duplicate causal event id {event_id:?}")
            }
            Self::EventValidationFailed { event_id, reason } => {
                write!(f, "causal event {event_id:?} failed validation: {reason}")
            }
            Self::EventBudgetExceeded { actual, max } => {
                write!(f, "incident has {actual} events, max is {max}")
            }
            Self::PayloadBudgetExceeded { actual, max } => {
                write!(f, "incident payload total is {actual} bytes, max is {max}")
            }
        }
    }
}

impl std::error::Error for IncidentCausalityError {}

/// Node wrapper used by incident replay/export surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentNode {
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

/// Causal edge type between normalized flight-recorder events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentEdgeKind {
    ExplicitParent,
    ExplicitCause,
    ExplicitRoot,
    SameBead,
    SameRchBuild,
    SamePane,
    SameThread,
    SameGitCommit,
    SharedArtifact,
    TemporalWindow,
}

impl IncidentEdgeKind {
    const fn confidence(self) -> IncidentEdgeConfidence {
        match self {
            Self::ExplicitParent | Self::ExplicitCause | Self::ExplicitRoot => {
                IncidentEdgeConfidence::Explicit
            }
            Self::SameBead | Self::SameRchBuild => IncidentEdgeConfidence::High,
            Self::SamePane | Self::SameThread | Self::SameGitCommit => {
                IncidentEdgeConfidence::Medium
            }
            Self::SharedArtifact | Self::TemporalWindow => IncidentEdgeConfidence::Low,
        }
    }

    const fn is_explicit(self) -> bool {
        matches!(
            self,
            Self::ExplicitParent | Self::ExplicitCause | Self::ExplicitRoot
        )
    }
}

/// Coarse confidence class for explicit and inferred links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentEdgeConfidence {
    Explicit,
    High,
    Medium,
    Low,
}

/// Directed causal link in the incident DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentEdge {
    pub from_event_id: String,
    pub to_event_id: String,
    pub kind: IncidentEdgeKind,
    pub confidence: IncidentEdgeConfidence,
    pub explanation: String,
}

/// Machine-readable unresolved evidence or contradiction in the incident DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentGap {
    pub event_id: String,
    pub source: SwarmCausalEventSource,
    pub event_class: CausalEventClass,
    pub reason_code: String,
    pub explanation: String,
}

/// Aggregate proof state extracted from incident events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentProofCoverage {
    pub source_pass_count: usize,
    pub source_failure_count: usize,
    pub infrastructure_failure_count: usize,
    pub dirty_tree_contamination_count: usize,
    pub communication_outage_count: usize,
    pub policy_denial_count: usize,
    pub operator_cancellation_count: usize,
    pub evidence_unavailable_count: usize,
    pub informational_count: usize,
    pub has_admissible_remote_rch_pass: bool,
    pub has_failed_or_local_rch: bool,
    pub contaminated_by_dirty_tree: bool,
    pub missing_evidence: bool,
}

/// Bounded deterministic DAG for a flight-recorder incident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentDag {
    pub schema_version: String,
    pub incident_id: String,
    pub nodes: Vec<IncidentNode>,
    pub edges: Vec<IncidentEdge>,
    pub root_event_ids: Vec<String>,
    pub proof_coverage: IncidentProofCoverage,
    pub gaps: Vec<IncidentGap>,
}

impl IncidentDag {
    pub fn from_events(
        events: &[SwarmCausalEvent],
        config: IncidentGraphConfig,
    ) -> Result<Self, IncidentCausalityError> {
        validate_budget(events, config.budget)?;
        let ordered = canonical_events(events)?;
        let event_ids = ordered
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut builder = IncidentDagBuilder::new(&ordered);

        for event in &ordered {
            builder.add_explicit_edges(event, &event_ids);
        }
        builder.add_key_edges(&ordered);
        builder.add_temporal_edges(&ordered, config.temporal_window_ms);
        Ok(builder.finish())
    }

    pub fn root_causes(&self) -> Vec<&IncidentNode> {
        self.root_event_ids
            .iter()
            .filter_map(|event_id| self.node(event_id))
            .filter(|node| {
                !matches!(
                    node.event_class,
                    CausalEventClass::SourcePass | CausalEventClass::Informational
                )
            })
            .collect()
    }

    pub fn downstream_effects(&self, event_id: &str) -> Vec<&IncidentNode> {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::new();
        let adjacency = edge_adjacency(&self.edges);
        if let Some(children) = adjacency.get(event_id) {
            for child in children {
                if seen.insert(child.clone()) {
                    queue.push_back(child.clone());
                }
            }
        }
        let mut out = Vec::new();
        while let Some(current) = queue.pop_front() {
            if let Some(node) = self.node(&current) {
                out.push(node);
            }
            if let Some(children) = adjacency.get(current.as_str()) {
                for child in children {
                    if seen.insert(child.clone()) {
                        queue.push_back(child.clone());
                    }
                }
            }
        }
        out.sort_by_key(|node| (node.ingest_sequence, node.occurred_at_ms, node.event_id.as_str()));
        out
    }

    pub fn node(&self, event_id: &str) -> Option<&IncidentNode> {
        self.nodes.iter().find(|node| node.event_id == event_id)
    }

    pub fn is_acyclic(&self) -> bool {
        let mut incoming = self
            .nodes
            .iter()
            .map(|node| (node.event_id.clone(), 0usize))
            .collect::<BTreeMap<_, _>>();
        let adjacency = edge_adjacency(&self.edges);
        for edge in &self.edges {
            *incoming.entry(edge.to_event_id.clone()).or_default() += 1;
        }
        let mut queue = incoming
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(event_id, _)| event_id.clone())
            .collect::<VecDeque<_>>();
        let mut visited = 0usize;
        while let Some(event_id) = queue.pop_front() {
            visited = visited.saturating_add(1);
            if let Some(children) = adjacency.get(event_id.as_str()) {
                for child in children {
                    if let Some(count) = incoming.get_mut(child) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            queue.push_back(child.clone());
                        }
                    }
                }
            }
        }
        visited == self.nodes.len()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

pub fn build_incident_dag(
    events: &[SwarmCausalEvent],
) -> Result<IncidentDag, IncidentCausalityError> {
    IncidentDag::from_events(events, IncidentGraphConfig::default())
}

struct IncidentDagBuilder {
    nodes: Vec<IncidentNode>,
    node_sources: BTreeMap<String, (SwarmCausalEventSource, CausalEventClass)>,
    edges: BTreeMap<(String, String), IncidentEdge>,
    adjacency: BTreeMap<String, BTreeSet<String>>,
    gaps: Vec<IncidentGap>,
}

impl IncidentDagBuilder {
    fn new(ordered: &[&SwarmCausalEvent]) -> Self {
        let nodes = ordered.iter().map(|event| node_from_event(event)).collect();
        let node_sources = ordered
            .iter()
            .map(|event| {
                (
                    event.event_id.clone(),
                    (event.source, event.event_class),
                )
            })
            .collect();
        let gaps = ordered.iter().flat_map(|event| event_gaps(event)).collect();
        Self {
            nodes,
            node_sources,
            edges: BTreeMap::new(),
            adjacency: BTreeMap::new(),
            gaps,
        }
    }

    fn add_explicit_edges(&mut self, event: &SwarmCausalEvent, event_ids: &BTreeSet<&str>) {
        for parent_id in &event.links.parent_event_ids {
            self.add_explicit_or_gap(
                parent_id,
                &event.event_id,
                event,
                IncidentEdgeKind::ExplicitParent,
                "event lists this parent_event_id explicitly",
                event_ids,
            );
        }
        for cause_id in &event.links.caused_by_event_ids {
            self.add_explicit_or_gap(
                cause_id,
                &event.event_id,
                event,
                IncidentEdgeKind::ExplicitCause,
                "event lists this caused_by_event_id explicitly",
                event_ids,
            );
        }
        if let Some(root_id) = &event.links.root_event_id {
            self.add_explicit_or_gap(
                root_id,
                &event.event_id,
                event,
                IncidentEdgeKind::ExplicitRoot,
                "event lists this root_event_id explicitly",
                event_ids,
            );
        }
    }

    fn add_explicit_or_gap(
        &mut self,
        from_event_id: &str,
        to_event_id: &str,
        event: &SwarmCausalEvent,
        kind: IncidentEdgeKind,
        explanation: &str,
        event_ids: &BTreeSet<&str>,
    ) {
        if !event_ids.contains(from_event_id) {
            self.gaps.push(IncidentGap {
                event_id: event.event_id.clone(),
                source: event.source,
                event_class: event.event_class,
                reason_code: "causality.missing_explicit_link_target".to_string(),
                explanation: format!(
                    "{kind:?} references absent event id {from_event_id:?}"
                ),
            });
            return;
        }
        self.add_edge(from_event_id, to_event_id, kind, explanation.to_string());
    }

    fn add_key_edges(&mut self, ordered: &[&SwarmCausalEvent]) {
        let mut groups = CausalKeyGroups::default();
        for event in ordered {
            groups.insert_event(event);
        }
        for (value, event_ids) in groups.beads {
            self.add_group_edges(event_ids, IncidentEdgeKind::SameBead, format!("same bead_id {value}"));
        }
        for (value, event_ids) in groups.rch_builds {
            self.add_group_edges(
                event_ids,
                IncidentEdgeKind::SameRchBuild,
                format!("same rch_build_id {value}"),
            );
        }
        for (value, event_ids) in groups.panes {
            self.add_group_edges(event_ids, IncidentEdgeKind::SamePane, format!("same pane_id {value}"));
        }
        for (value, event_ids) in groups.threads {
            self.add_group_edges(
                event_ids,
                IncidentEdgeKind::SameThread,
                format!("same thread_id {value}"),
            );
        }
        for (value, event_ids) in groups.git_commits {
            self.add_group_edges(
                event_ids,
                IncidentEdgeKind::SameGitCommit,
                format!("same git_commit {value}"),
            );
        }
        for (value, event_ids) in groups.artifacts {
            self.add_group_edges(
                event_ids,
                IncidentEdgeKind::SharedArtifact,
                format!("shared artifact uri {value}"),
            );
        }
    }

    fn add_group_edges(
        &mut self,
        event_ids: Vec<String>,
        kind: IncidentEdgeKind,
        explanation: String,
    ) {
        for pair in event_ids.windows(2) {
            self.add_edge(&pair[0], &pair[1], kind, explanation.clone());
        }
    }

    fn add_temporal_edges(&mut self, ordered: &[&SwarmCausalEvent], temporal_window_ms: u64) {
        if temporal_window_ms == 0 {
            return;
        }
        for pair in ordered.windows(2) {
            let before = pair[0];
            let after = pair[1];
            let delta = before.occurred_at_ms.abs_diff(after.occurred_at_ms);
            if delta <= temporal_window_ms {
                self.add_edge(
                    &before.event_id,
                    &after.event_id,
                    IncidentEdgeKind::TemporalWindow,
                    format!(
                        "adjacent ingest sequence within {temporal_window_ms}ms source-time window"
                    ),
                );
            }
        }
    }

    fn add_edge(
        &mut self,
        from_event_id: &str,
        to_event_id: &str,
        kind: IncidentEdgeKind,
        explanation: String,
    ) {
        if from_event_id == to_event_id {
            self.push_cycle_gap(from_event_id, to_event_id, kind);
            return;
        }
        let key = (from_event_id.to_string(), to_event_id.to_string());
        if self.edges.contains_key(&key) {
            return;
        }
        if path_exists(&self.adjacency, to_event_id, from_event_id) {
            self.push_cycle_gap(from_event_id, to_event_id, kind);
            return;
        }
        let edge = IncidentEdge {
            from_event_id: from_event_id.to_string(),
            to_event_id: to_event_id.to_string(),
            kind,
            confidence: kind.confidence(),
            explanation,
        };
        self.edges.insert(key, edge);
        self.adjacency
            .entry(from_event_id.to_string())
            .or_default()
            .insert(to_event_id.to_string());
    }

    fn push_cycle_gap(&mut self, from_event_id: &str, to_event_id: &str, kind: IncidentEdgeKind) {
        let (source, event_class) = self
            .node_sources
            .get(to_event_id)
            .copied()
            .unwrap_or((SwarmCausalEventSource::SourceUnavailable, CausalEventClass::SourceFailure));
        self.gaps.push(IncidentGap {
            event_id: to_event_id.to_string(),
            source,
            event_class,
            reason_code: "causality.edge_would_cycle".to_string(),
            explanation: format!(
                "{kind:?} from {from_event_id:?} to {to_event_id:?} would violate DAG ordering"
            ),
        });
    }

    fn finish(mut self) -> IncidentDag {
        let edges = self.edges.into_values().collect::<Vec<_>>();
        self.gaps
            .sort_by_key(|gap| (gap.event_id.clone(), gap.reason_code.clone()));
        let root_event_ids = root_event_ids(&self.nodes, &edges);
        let proof_coverage = proof_coverage(&self.nodes);
        IncidentDag {
            schema_version: INCIDENT_CAUSALITY_CONTRACT_ID_V1.to_string(),
            incident_id: incident_id_for(&self.nodes),
            nodes: self.nodes,
            edges,
            root_event_ids,
            proof_coverage,
            gaps: self.gaps,
        }
    }
}

#[derive(Default)]
struct CausalKeyGroups {
    beads: BTreeMap<String, Vec<String>>,
    rch_builds: BTreeMap<String, Vec<String>>,
    panes: BTreeMap<String, Vec<String>>,
    threads: BTreeMap<String, Vec<String>>,
    git_commits: BTreeMap<String, Vec<String>>,
    artifacts: BTreeMap<String, Vec<String>>,
}

impl CausalKeyGroups {
    fn insert_event(&mut self, event: &SwarmCausalEvent) {
        if let Some(value) = present(event.correlation.bead_id.as_ref()) {
            self.beads
                .entry(value.to_string())
                .or_default()
                .push(event.event_id.clone());
        }
        if let Some(value) = present(event.correlation.rch_build_id.as_ref()) {
            self.rch_builds
                .entry(value.to_string())
                .or_default()
                .push(event.event_id.clone());
        }
        if let Some(value) = event.correlation.pane_id {
            self.panes
                .entry(value.to_string())
                .or_default()
                .push(event.event_id.clone());
        }
        if let Some(value) = present(event.correlation.thread_id.as_ref()) {
            self.threads
                .entry(value.to_string())
                .or_default()
                .push(event.event_id.clone());
        }
        if let Some(value) = present(event.correlation.git_commit.as_ref()) {
            self.git_commits
                .entry(value.to_string())
                .or_default()
                .push(event.event_id.clone());
        }
        for artifact in &event.artifacts {
            if let Some(value) = present(Some(&artifact.uri)) {
                self.artifacts
                    .entry(value.to_string())
                    .or_default()
                    .push(event.event_id.clone());
            }
        }
    }
}

fn canonical_events(
    events: &[SwarmCausalEvent],
) -> Result<Vec<&SwarmCausalEvent>, IncidentCausalityError> {
    let mut seen = BTreeSet::new();
    for event in events {
        if !seen.insert(event.event_id.clone()) {
            return Err(IncidentCausalityError::DuplicateEventId {
                event_id: event.event_id.clone(),
            });
        }
        event
            .validate()
            .map_err(|error| IncidentCausalityError::EventValidationFailed {
                event_id: event.event_id.clone(),
                reason: error.to_string(),
            })?;
    }
    let mut ordered = events.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        (
            left.ingest_sequence,
            left.occurred_at_ms,
            left.event_id.as_str(),
        )
            .cmp(&(
                right.ingest_sequence,
                right.occurred_at_ms,
                right.event_id.as_str(),
            ))
    });
    Ok(ordered)
}

fn validate_budget(
    events: &[SwarmCausalEvent],
    budget: CausalIncidentBudget,
) -> Result<(), IncidentCausalityError> {
    let max_events = if budget.max_events == 0 {
        DEFAULT_MAX_INCIDENT_EVENTS
    } else {
        budget.max_events
    };
    if events.len() > max_events {
        return Err(IncidentCausalityError::EventBudgetExceeded {
            actual: events.len(),
            max: max_events,
        });
    }
    let max_total_payload_bytes = if budget.max_total_payload_bytes == 0 {
        DEFAULT_MAX_INCIDENT_PAYLOAD_BYTES
    } else {
        budget.max_total_payload_bytes
    };
    let total_payload_bytes = events
        .iter()
        .map(|event| event.privacy.payload_bytes)
        .try_fold(0usize, usize::checked_add)
        .unwrap_or(usize::MAX);
    if total_payload_bytes > max_total_payload_bytes {
        return Err(IncidentCausalityError::PayloadBudgetExceeded {
            actual: total_payload_bytes,
            max: max_total_payload_bytes,
        });
    }
    Ok(())
}

fn node_from_event(event: &SwarmCausalEvent) -> IncidentNode {
    IncidentNode {
        event_id: event.event_id.clone(),
        source: event.source,
        event_class: event.event_class,
        occurred_at_ms: event.occurred_at_ms,
        ingested_at_ms: event.ingested_at_ms,
        ingest_sequence: event.ingest_sequence,
        correlation: event.correlation.clone(),
        privacy: event.privacy_audit(),
        artifact_uris: event
            .artifacts
            .iter()
            .map(|artifact| artifact.uri.clone())
            .collect(),
    }
}

fn event_gaps(event: &SwarmCausalEvent) -> Vec<IncidentGap> {
    let mut gaps = Vec::new();
    if matches!(
        event.event_class,
        CausalEventClass::SourceFailure
            | CausalEventClass::InfrastructureFailure
            | CausalEventClass::DirtyTreeContamination
            | CausalEventClass::CommunicationOutage
            | CausalEventClass::PolicyDenial
            | CausalEventClass::OperatorCancellation
            | CausalEventClass::EvidenceUnavailable
    ) {
        gaps.push(IncidentGap {
            event_id: event.event_id.clone(),
            source: event.source,
            event_class: event.event_class,
            reason_code: class_reason_code(event.event_class).to_string(),
            explanation: class_explanation(event.event_class).to_string(),
        });
    }
    if event.privacy.omission_reason.is_some() {
        gaps.push(IncidentGap {
            event_id: event.event_id.clone(),
            source: event.source,
            event_class: event.event_class,
            reason_code: "causality.payload_omitted".to_string(),
            explanation: "event payload was redacted, truncated, hash-only, or unavailable"
                .to_string(),
        });
    }
    gaps
}

fn root_event_ids(nodes: &[IncidentNode], edges: &[IncidentEdge]) -> Vec<String> {
    let incoming = edges
        .iter()
        .map(|edge| edge.to_event_id.as_str())
        .collect::<BTreeSet<_>>();
    nodes
        .iter()
        .filter(|node| !incoming.contains(node.event_id.as_str()))
        .map(|node| node.event_id.clone())
        .collect()
}

fn proof_coverage(nodes: &[IncidentNode]) -> IncidentProofCoverage {
    let mut coverage = IncidentProofCoverage::default();
    for node in nodes {
        match node.event_class {
            CausalEventClass::SourcePass => coverage.source_pass_count += 1,
            CausalEventClass::SourceFailure => coverage.source_failure_count += 1,
            CausalEventClass::InfrastructureFailure => coverage.infrastructure_failure_count += 1,
            CausalEventClass::DirtyTreeContamination => {
                coverage.dirty_tree_contamination_count += 1;
                coverage.contaminated_by_dirty_tree = true;
            }
            CausalEventClass::CommunicationOutage => coverage.communication_outage_count += 1,
            CausalEventClass::PolicyDenial => coverage.policy_denial_count += 1,
            CausalEventClass::OperatorCancellation => coverage.operator_cancellation_count += 1,
            CausalEventClass::EvidenceUnavailable => {
                coverage.evidence_unavailable_count += 1;
                coverage.missing_evidence = true;
            }
            CausalEventClass::Informational => coverage.informational_count += 1,
        }
        if node.source == SwarmCausalEventSource::SourceUnavailable {
            coverage.missing_evidence = true;
        }
        if node.source == SwarmCausalEventSource::Rch {
            if node.event_class == CausalEventClass::SourcePass
                && node.correlation.rch_worker_id.is_some()
            {
                coverage.has_admissible_remote_rch_pass = true;
            }
            if node.event_class == CausalEventClass::InfrastructureFailure {
                coverage.has_failed_or_local_rch = true;
            }
        }
    }
    coverage
}

fn edge_adjacency(edges: &[IncidentEdge]) -> BTreeMap<&str, Vec<String>> {
    let mut adjacency = BTreeMap::<&str, Vec<String>>::new();
    for edge in edges {
        adjacency
            .entry(edge.from_event_id.as_str())
            .or_default()
            .push(edge.to_event_id.clone());
    }
    adjacency
}

fn path_exists(
    adjacency: &BTreeMap<String, BTreeSet<String>>,
    start_event_id: &str,
    goal_event_id: &str,
) -> bool {
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([start_event_id.to_string()]);
    while let Some(current) = queue.pop_front() {
        if current == goal_event_id {
            return true;
        }
        if !seen.insert(current.clone()) {
            continue;
        }
        if let Some(children) = adjacency.get(current.as_str()) {
            for child in children {
                queue.push_back(child.clone());
            }
        }
    }
    false
}

fn present(value: Option<&String>) -> Option<&str> {
    value
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn class_reason_code(event_class: CausalEventClass) -> &'static str {
    match event_class {
        CausalEventClass::SourcePass => "causality.source_pass",
        CausalEventClass::SourceFailure => "causality.source_failure",
        CausalEventClass::InfrastructureFailure => "causality.infrastructure_failure",
        CausalEventClass::DirtyTreeContamination => "causality.dirty_tree_contamination",
        CausalEventClass::CommunicationOutage => "causality.communication_outage",
        CausalEventClass::PolicyDenial => "causality.policy_denial",
        CausalEventClass::OperatorCancellation => "causality.operator_cancellation",
        CausalEventClass::EvidenceUnavailable => "causality.evidence_unavailable",
        CausalEventClass::Informational => "causality.informational",
    }
}

fn class_explanation(event_class: CausalEventClass) -> &'static str {
    match event_class {
        CausalEventClass::SourcePass => "source provided usable evidence",
        CausalEventClass::SourceFailure => "source failed while collecting evidence",
        CausalEventClass::InfrastructureFailure => "proof or collection infrastructure failed",
        CausalEventClass::DirtyTreeContamination => "shared worktree contamination affected proof",
        CausalEventClass::CommunicationOutage => "coordination source was unavailable",
        CausalEventClass::PolicyDenial => "policy gate denied or required approval",
        CausalEventClass::OperatorCancellation => "operator cancelled the action",
        CausalEventClass::EvidenceUnavailable => "expected evidence is unavailable",
        CausalEventClass::Informational => "informational event",
    }
}

fn incident_id_for(nodes: &[IncidentNode]) -> String {
    let mut hasher = Sha256::new();
    for node in nodes {
        hasher.update(node.event_id.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    format!("incident-{}", hex_prefix(&digest, 12))
}

fn hex_prefix(bytes: &[u8], len: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(len);
    for &byte in bytes {
        if out.len() >= len {
            break;
        }
        out.push(HEX[(byte >> 4) as usize] as char);
        if out.len() >= len {
            break;
        }
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm_causal_event::{
        CausalArtifactRef, CausalLinks, CausalPayloadSensitivity, CausalRedactionStatus,
        CausalRetentionClass,
    };
    use serde_json::json;

    fn event(
        event_id: &str,
        source: SwarmCausalEventSource,
        event_class: CausalEventClass,
        occurred_at_ms: u64,
        ingest_sequence: u64,
        correlation: CausalCorrelationKeys,
        links: CausalLinks,
    ) -> SwarmCausalEvent {
        let payload = if source == SwarmCausalEventSource::SourceUnavailable {
            json!({"reason": "test.unavailable"})
        } else {
            json!({"summary": event_id})
        };
        SwarmCausalEvent::new(
            event_id,
            source,
            event_class,
            occurred_at_ms,
            occurred_at_ms.max(10_000),
            ingest_sequence,
            correlation,
            links,
            CausalPayloadSensitivity::Structural,
            if source == SwarmCausalEventSource::SourceUnavailable {
                CausalRedactionStatus::Unavailable
            } else {
                CausalRedactionStatus::NotRequired
            },
            CausalRetentionClass::Proof,
            Vec::new(),
            payload,
        )
        .unwrap()
    }

    fn bead_event(event_id: &str, bead_id: &str, sequence: u64) -> SwarmCausalEvent {
        event(
            event_id,
            SwarmCausalEventSource::Beads,
            CausalEventClass::SourcePass,
            1_000 + sequence,
            sequence,
            CausalCorrelationKeys {
                bead_id: Some(bead_id.to_string()),
                ..Default::default()
            },
            CausalLinks::default(),
        )
    }

    fn rch_event(event_id: &str, class: CausalEventClass, sequence: u64) -> SwarmCausalEvent {
        event(
            event_id,
            SwarmCausalEventSource::Rch,
            class,
            2_000 + sequence,
            sequence,
            CausalCorrelationKeys {
                rch_build_id: Some("29871232832766299".to_string()),
                rch_worker_id: Some("vmi1149989".to_string()),
                ..Default::default()
            },
            CausalLinks::default(),
        )
    }

    #[test]
    fn deterministic_sorting_uses_ingest_sequence_before_source_time() {
        let mut late_source_time = bead_event("late-source-time", "ft-ogr3n.3", 1);
        late_source_time.occurred_at_ms = 50_000;
        let mut early_source_time = bead_event("early-source-time", "ft-ogr3n.3", 2);
        early_source_time.occurred_at_ms = 10;
        let first = build_incident_dag(&[early_source_time.clone(), late_source_time.clone()])
            .unwrap();
        let second = build_incident_dag(&[late_source_time, early_source_time]).unwrap();

        assert_eq!(first.incident_id, second.incident_id);
        assert_eq!(
            first.nodes.iter().map(|node| node.event_id.as_str()).collect::<Vec<_>>(),
            vec!["late-source-time", "early-source-time"]
        );
        assert_eq!(first, second);
        assert!(first.is_acyclic());
    }

    #[test]
    fn explicit_edges_take_precedence_over_inferred_keys() {
        let parent = bead_event("parent", "ft-ogr3n.3", 1);
        let child = event(
            "child",
            SwarmCausalEventSource::Beads,
            CausalEventClass::SourcePass,
            1_002,
            2,
            CausalCorrelationKeys {
                bead_id: Some("ft-ogr3n.3".to_string()),
                ..Default::default()
            },
            CausalLinks {
                parent_event_ids: vec!["parent".to_string()],
                ..Default::default()
            },
        );

        let dag = build_incident_dag(&[child, parent]).unwrap();
        assert_eq!(dag.edges.len(), 1);
        assert_eq!(dag.edges[0].kind, IncidentEdgeKind::ExplicitParent);
        assert_eq!(dag.edges[0].confidence, IncidentEdgeConfidence::Explicit);
    }

    #[test]
    fn clock_skew_keeps_ingest_order_for_temporal_links() {
        let first = event(
            "first-ingested",
            SwarmCausalEventSource::Operator,
            CausalEventClass::Informational,
            10_000,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
        );
        let second = event(
            "second-ingested-with-older-clock",
            SwarmCausalEventSource::Operator,
            CausalEventClass::Informational,
            9_500,
            2,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
        );

        let dag = build_incident_dag(&[second, first]).unwrap();
        assert_eq!(dag.edges.len(), 1);
        assert_eq!(dag.edges[0].from_event_id, "first-ingested");
        assert_eq!(dag.edges[0].to_event_id, "second-ingested-with-older-clock");
        assert_eq!(dag.edges[0].kind, IncidentEdgeKind::TemporalWindow);
    }

    #[test]
    fn source_failures_and_outages_are_separate_proof_classes() {
        let rch_fail = rch_event("rch-preflight", CausalEventClass::InfrastructureFailure, 1);
        let mail_outage = event(
            "mail-outage",
            SwarmCausalEventSource::SourceUnavailable,
            CausalEventClass::CommunicationOutage,
            2_000,
            2,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
        );
        let dirty_git = event(
            "dirty-git",
            SwarmCausalEventSource::Git,
            CausalEventClass::DirtyTreeContamination,
            2_100,
            3,
            CausalCorrelationKeys {
                git_commit: Some("ea967abe4".to_string()),
                ..Default::default()
            },
            CausalLinks::default(),
        );

        let dag = build_incident_dag(&[dirty_git, mail_outage, rch_fail]).unwrap();
        assert_eq!(dag.proof_coverage.infrastructure_failure_count, 1);
        assert_eq!(dag.proof_coverage.communication_outage_count, 1);
        assert_eq!(dag.proof_coverage.dirty_tree_contamination_count, 1);
        assert!(dag.proof_coverage.has_failed_or_local_rch);
        assert!(dag.proof_coverage.contaminated_by_dirty_tree);
        assert!(dag.proof_coverage.missing_evidence);
        assert!(
            dag.gaps
                .iter()
                .any(|gap| gap.reason_code == "causality.communication_outage")
        );
    }

    #[test]
    fn conservative_key_edges_link_adjacent_events_only() {
        let first = bead_event("first", "ft-ogr3n.3", 1);
        let second = bead_event("second", "ft-ogr3n.3", 2);
        let third = bead_event("third", "ft-ogr3n.3", 3);

        let dag = build_incident_dag(&[third, first, second]).unwrap();
        let same_bead_edges = dag
            .edges
            .iter()
            .filter(|edge| edge.kind == IncidentEdgeKind::SameBead)
            .collect::<Vec<_>>();
        assert_eq!(same_bead_edges.len(), 2);
        assert_eq!(same_bead_edges[0].from_event_id, "first");
        assert_eq!(same_bead_edges[0].to_event_id, "second");
        assert_eq!(same_bead_edges[1].from_event_id, "second");
        assert_eq!(same_bead_edges[1].to_event_id, "third");
    }

    #[test]
    fn contradictory_explicit_edges_become_gaps_not_cycles() {
        let first = event(
            "first",
            SwarmCausalEventSource::Operator,
            CausalEventClass::Informational,
            1_000,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks {
                parent_event_ids: vec!["second".to_string()],
                ..Default::default()
            },
        );
        let second = event(
            "second",
            SwarmCausalEventSource::Operator,
            CausalEventClass::Informational,
            1_001,
            2,
            CausalCorrelationKeys::default(),
            CausalLinks {
                parent_event_ids: vec!["first".to_string()],
                ..Default::default()
            },
        );

        let dag = build_incident_dag(&[first, second]).unwrap();
        assert!(dag.is_acyclic());
        assert_eq!(
            dag.gaps
                .iter()
                .filter(|gap| gap.reason_code == "causality.edge_would_cycle")
                .count(),
            1
        );
    }

    #[test]
    fn orphan_events_are_roots_and_downstream_queries_are_bounded() {
        let root = event(
            "orphan-root",
            SwarmCausalEventSource::Operator,
            CausalEventClass::OperatorCancellation,
            1_000,
            1,
            CausalCorrelationKeys::default(),
            CausalLinks::default(),
        );
        let child = event(
            "child",
            SwarmCausalEventSource::Rch,
            CausalEventClass::InfrastructureFailure,
            1_001,
            2,
            CausalCorrelationKeys {
                rch_build_id: Some("29871232832766299".to_string()),
                ..Default::default()
            },
            CausalLinks {
                caused_by_event_ids: vec!["orphan-root".to_string()],
                ..Default::default()
            },
        );

        let dag = build_incident_dag(&[child, root]).unwrap();
        assert_eq!(dag.root_event_ids, vec!["orphan-root".to_string()]);
        assert_eq!(dag.root_causes()[0].event_id, "orphan-root");
        assert_eq!(dag.downstream_effects("orphan-root")[0].event_id, "child");
    }

    #[test]
    fn duplicate_ids_and_budget_overruns_fail_closed() {
        let event = bead_event("dup", "ft-ogr3n.3", 1);
        let err = build_incident_dag(&[event.clone(), event]).unwrap_err();
        assert!(matches!(
            err,
            IncidentCausalityError::DuplicateEventId { .. }
        ));

        let events = vec![bead_event("one", "ft-ogr3n.3", 1)];
        let err = IncidentDag::from_events(
            &events,
            IncidentGraphConfig {
                temporal_window_ms: DEFAULT_TEMPORAL_WINDOW_MS,
                budget: CausalIncidentBudget {
                    max_total_payload_bytes: 1,
                    max_events: 1,
                },
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            IncidentCausalityError::PayloadBudgetExceeded { .. }
        ));
    }

    #[test]
    fn shared_artifacts_and_remote_rch_pass_cover_proof_queries() {
        let mut beads = bead_event("beads-closeout", "ft-ogr3n.3", 1);
        beads.artifacts = vec![CausalArtifactRef {
            kind: "jsonl".to_string(),
            uri: "tests/e2e/logs/ft-ogr3n3.jsonl".to_string(),
            sha256: None,
            size_bytes: None,
        }];
        let mut rch = rch_event("rch-pass", CausalEventClass::SourcePass, 2);
        rch.artifacts.clone_from(&beads.artifacts);

        let dag = build_incident_dag(&[rch, beads]).unwrap();
        assert!(dag.proof_coverage.has_admissible_remote_rch_pass);
        assert!(
            dag.edges
                .iter()
                .any(|edge| edge.kind == IncidentEdgeKind::SharedArtifact)
        );
        assert!(dag.to_json().contains(INCIDENT_CAUSALITY_CONTRACT_ID_V1));
    }
}
