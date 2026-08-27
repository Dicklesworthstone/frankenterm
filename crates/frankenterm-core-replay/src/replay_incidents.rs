//! Read-only robot/MCP incident surfaces for flight-recorder evidence.
//!
//! The surfaces in this module consume persisted source-set artifacts and
//! never collect live state, run proof commands, mutate panes, touch Beads, or
//! call Agent Mail. The replay transcript is structural: it exposes causal
//! ordering, redaction state, evidence references, hashes, and reason codes
//! without replaying raw pane text.

use frankenterm_core_replay_types::incident_dag::{
    IncidentDag, IncidentDagBuildConfig, IncidentExplanationGap, IncidentProofCoverage,
    build_incident_dag,
};
use frankenterm_core_replay_types::source_adapters::{
    SourceAdapterContext, SourceAdapterFailure, SourceAdapterSet, adapt_source_set,
    adapt_source_set_json,
};
use frankenterm_core_replay_types::swarm_causal_event::{
    CausalArtifactRef, CausalCorrelationKeys, CausalEventClass, CausalPayloadOmissionReason,
    CausalRedactionStatus, SwarmCausalEvent, SwarmCausalEventSource,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const INCIDENT_SURFACE_CONTRACT_ID_V1: &str = "ft.swarm.incident_surfaces.v1";
pub const INCIDENT_MCP_LIST_URI: &str = "wa://incidents";
pub const INCIDENT_MCP_SHOW_URI_TEMPLATE: &str = "wa://incidents/{incident_id}";
pub const INCIDENT_MCP_EXPLAIN_URI_TEMPLATE: &str = "wa://incidents/{incident_id}/explain";
pub const INCIDENT_MCP_REPLAY_URI_TEMPLATE: &str = "wa://incidents/{incident_id}/replay";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncidentSurfaceBuildOptions {
    pub incident_id: Option<String>,
    pub generated_at_ms: Option<u64>,
    pub first_ingest_sequence: u64,
    pub workspace_id: Option<String>,
    pub source_artifact: Option<String>,
    pub dag_config: IncidentDagBuildConfig,
}

impl Default for IncidentSurfaceBuildOptions {
    fn default() -> Self {
        Self {
            incident_id: None,
            generated_at_ms: None,
            first_ingest_sequence: 1,
            workspace_id: None,
            source_artifact: None,
            dag_config: IncidentDagBuildConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IncidentSurfaceFilter {
    pub bead: Option<String>,
    pub pane: Option<u64>,
    pub worker: Option<String>,
    pub build_id: Option<String>,
    pub source_kind: Option<SwarmCausalEventSource>,
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    pub causal_class: Option<CausalEventClass>,
    pub limit: usize,
}

impl Default for IncidentSurfaceFilter {
    fn default() -> Self {
        Self {
            bead: None,
            pane: None,
            worker: None,
            build_id: None,
            source_kind: None,
            since_ms: None,
            until_ms: None,
            causal_class: None,
            limit: 50,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentSurfaceErrorKind {
    NoIncidentFound,
    SourceUnavailable,
    RedactedContent,
    RetentionExpired,
    MalformedSourceSet,
    InvalidFilter,
    DagBuildFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentSurfaceErrorEnvelope {
    pub kind: IncidentSurfaceErrorKind,
    pub code: String,
    pub detail: String,
    pub event_ids: Vec<String>,
    pub recoverable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncidentSurfaceError {
    kind: IncidentSurfaceErrorKind,
    code: &'static str,
    detail: String,
}

impl IncidentSurfaceError {
    #[must_use]
    pub const fn kind(&self) -> IncidentSurfaceErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    fn new(kind: IncidentSurfaceErrorKind, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            kind,
            code,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for IncidentSurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for IncidentSurfaceError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentMcpResourceDescriptor {
    pub kind: String,
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri_template: Option<String>,
    pub read_only: bool,
    pub live_mutation_allowed: bool,
    pub bounded_output: bool,
    pub redaction_guaranteed: bool,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentListEntry {
    pub incident_id: String,
    pub source_set_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub generated_at_ms: u64,
    pub first_event_ms: Option<u64>,
    pub last_event_ms: Option<u64>,
    pub node_count: usize,
    pub edge_count: usize,
    pub root_event_ids: Vec<String>,
    pub root_cause_event_ids: Vec<String>,
    pub sources: Vec<SwarmCausalEventSource>,
    pub causal_classes: Vec<CausalEventClass>,
    pub bead_ids: Vec<String>,
    pub pane_ids: Vec<u64>,
    pub rch_build_ids: Vec<String>,
    pub rch_worker_ids: Vec<String>,
    pub proof_admissible: bool,
    pub outcome: IncidentOutcome,
    pub error_envelope_count: usize,
    pub source_artifact: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentOutcome {
    SourcePass,
    InfrastructureBlock,
    ContaminatedProofAttempt,
    SourceUnavailable,
    ProofIncomplete,
}

impl IncidentOutcome {
    #[must_use]
    pub const fn as_reason_code(self) -> &'static str {
        match self {
            Self::SourcePass => "incident.outcome.source_pass",
            Self::InfrastructureBlock => "incident.outcome.infrastructure_block",
            Self::ContaminatedProofAttempt => "incident.outcome.contaminated_proof_attempt",
            Self::SourceUnavailable => "incident.outcome.source_unavailable",
            Self::ProofIncomplete => "incident.outcome.proof_incomplete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentNodeSummary {
    pub event_id: String,
    pub source: SwarmCausalEventSource,
    pub event_class: CausalEventClass,
    pub occurred_at_ms: u64,
    pub ingested_at_ms: u64,
    pub ingest_sequence: u64,
    pub correlation: CausalCorrelationKeys,
    pub artifact_uris: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentEvidenceRef {
    pub event_id: String,
    pub source: SwarmCausalEventSource,
    pub kind: String,
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentReplayFrame {
    pub index: usize,
    pub event_id: String,
    pub occurred_at_ms: u64,
    pub ingested_at_ms: u64,
    pub ingest_sequence: u64,
    pub source: SwarmCausalEventSource,
    pub event_class: CausalEventClass,
    pub correlation: CausalCorrelationKeys,
    pub artifact_uris: Vec<String>,
    pub payload_hash_sha256: String,
    pub payload_bytes: usize,
    pub redaction_status: CausalRedactionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omission_reason: Option<CausalPayloadOmissionReason>,
    pub reason_codes: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentListPayload {
    pub schema_version: u32,
    pub contract_id: String,
    pub surface: String,
    pub generated_at_ms: u64,
    pub source: String,
    pub dry_run: bool,
    pub read_only: bool,
    pub raw_pane_content_stored: bool,
    pub live_mutation_allowed: bool,
    pub side_effects_executed: bool,
    pub filters: IncidentSurfaceFilter,
    pub incident_count: usize,
    pub incidents: Vec<IncidentListEntry>,
    pub error_envelopes: Vec<IncidentSurfaceErrorEnvelope>,
    pub mcp_resources: Vec<IncidentMcpResourceDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentShowPayload {
    pub schema_version: u32,
    pub contract_id: String,
    pub surface: String,
    pub generated_at_ms: u64,
    pub source: String,
    pub dry_run: bool,
    pub read_only: bool,
    pub raw_pane_content_stored: bool,
    pub live_mutation_allowed: bool,
    pub side_effects_executed: bool,
    pub incident: IncidentListEntry,
    pub dag: IncidentDag,
    pub evidence_refs: Vec<IncidentEvidenceRef>,
    pub root_cause_candidates: Vec<IncidentNodeSummary>,
    pub proof_coverage: IncidentProofCoverage,
    pub gaps: Vec<IncidentExplanationGap>,
    pub error_envelopes: Vec<IncidentSurfaceErrorEnvelope>,
    pub mcp_resources: Vec<IncidentMcpResourceDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentExplainPayload {
    pub schema_version: u32,
    pub contract_id: String,
    pub surface: String,
    pub generated_at_ms: u64,
    pub source: String,
    pub dry_run: bool,
    pub read_only: bool,
    pub raw_pane_content_stored: bool,
    pub live_mutation_allowed: bool,
    pub side_effects_executed: bool,
    pub incident_id: String,
    pub outcome: IncidentOutcome,
    pub reason_code: String,
    pub explanation: String,
    pub root_cause_candidates: Vec<IncidentNodeSummary>,
    pub proof_coverage: IncidentProofCoverage,
    pub gaps: Vec<IncidentExplanationGap>,
    pub error_envelopes: Vec<IncidentSurfaceErrorEnvelope>,
    pub mcp_resources: Vec<IncidentMcpResourceDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentReplayPayload {
    pub schema_version: u32,
    pub contract_id: String,
    pub surface: String,
    pub generated_at_ms: u64,
    pub source: String,
    pub dry_run: bool,
    pub read_only: bool,
    pub raw_pane_content_stored: bool,
    pub live_mutation_allowed: bool,
    pub side_effects_executed: bool,
    pub incident_id: String,
    pub deterministic_order: String,
    pub frame_count: usize,
    pub frames: Vec<IncidentReplayFrame>,
    pub error_envelopes: Vec<IncidentSurfaceErrorEnvelope>,
    pub mcp_resources: Vec<IncidentMcpResourceDescriptor>,
}

#[derive(Debug, Clone)]
pub struct IncidentSurfaceStore {
    incident: IncidentSurfaceIncident,
}

#[derive(Debug, Clone)]
struct IncidentSurfaceIncident {
    incident_id: String,
    source_set_id: String,
    workspace_id: Option<String>,
    generated_at_ms: u64,
    source_artifact: Option<String>,
    events: Vec<SwarmCausalEvent>,
    dag: IncidentDag,
    error_envelopes: Vec<IncidentSurfaceErrorEnvelope>,
}

impl IncidentSurfaceStore {
    pub fn from_source_set_path(
        path: &Path,
        mut options: IncidentSurfaceBuildOptions,
    ) -> Result<Self, IncidentSurfaceError> {
        let input = std::fs::read_to_string(path).map_err(|error| {
            IncidentSurfaceError::new(
                IncidentSurfaceErrorKind::SourceUnavailable,
                "incident.source_set_read_failed",
                format!("failed to read {}: {error}", path.display()),
            )
        })?;
        if options.source_artifact.is_none() {
            options.source_artifact = Some(path.display().to_string());
        }
        Self::from_source_set_json(&input, options)
    }

    pub fn from_source_set_json(
        input: &str,
        options: IncidentSurfaceBuildOptions,
    ) -> Result<Self, IncidentSurfaceError> {
        let parsed = serde_json::from_str::<SourceAdapterSet>(input);
        let (source_set_id, workspace_id, generated_at_ms, report) = match parsed {
            Ok(source_set) => {
                let generated_at_ms = options
                    .generated_at_ms
                    .unwrap_or(source_set.generated_at_ms);
                let workspace_id = options
                    .workspace_id
                    .clone()
                    .or_else(|| source_set.workspace_id.clone());
                let context = SourceAdapterContext {
                    workspace_id: workspace_id.clone(),
                    ingested_at_ms: generated_at_ms,
                    first_ingest_sequence: options.first_ingest_sequence,
                };
                (
                    source_set.source_set_id.clone(),
                    workspace_id,
                    generated_at_ms,
                    adapt_source_set(&source_set, context),
                )
            }
            Err(error) => {
                let generated_at_ms = options.generated_at_ms.unwrap_or(0);
                let workspace_id = options.workspace_id.clone();
                let context = SourceAdapterContext {
                    workspace_id: workspace_id.clone(),
                    ingested_at_ms: generated_at_ms,
                    first_ingest_sequence: options.first_ingest_sequence,
                };
                let mut report = adapt_source_set_json(input, context);
                report.failures.push(SourceAdapterFailure {
                    adapter:
                        frankenterm_core_replay_types::source_adapters::SourceAdapterKind::SourceSet,
                    source_id: None,
                    reason_code: "source_set.malformed_json".to_string(),
                    detail: error.to_string(),
                });
                (
                    "source-set-unavailable".to_string(),
                    workspace_id,
                    generated_at_ms,
                    report,
                )
            }
        };

        if report.events.is_empty() {
            return Err(IncidentSurfaceError::new(
                IncidentSurfaceErrorKind::NoIncidentFound,
                "incident.no_events",
                "source set produced no incident events",
            ));
        }

        let dag = build_incident_dag(&report.events, options.dag_config).map_err(|error| {
            IncidentSurfaceError::new(
                IncidentSurfaceErrorKind::DagBuildFailed,
                "incident.dag_build_failed",
                error.to_string(),
            )
        })?;
        let incident_id = options
            .incident_id
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| {
                if source_set_id.trim().is_empty() {
                    "source-set-unavailable".to_string()
                } else {
                    source_set_id.clone()
                }
            });
        let error_envelopes = error_envelopes(&report.events, &report.failures);

        Ok(Self {
            incident: IncidentSurfaceIncident {
                incident_id,
                source_set_id,
                workspace_id,
                generated_at_ms,
                source_artifact: options.source_artifact,
                events: report.events,
                dag,
                error_envelopes,
            },
        })
    }

    pub fn list_payload(
        &self,
        filters: IncidentSurfaceFilter,
    ) -> Result<IncidentListPayload, IncidentSurfaceError> {
        filters.validate()?;
        let mut incidents = Vec::new();
        if self.incident.matches_filters(&filters) && filters.limit > 0 {
            incidents.push(self.incident.list_entry());
        }
        Ok(IncidentListPayload {
            schema_version: 1,
            contract_id: INCIDENT_SURFACE_CONTRACT_ID_V1.to_string(),
            surface: "list".to_string(),
            generated_at_ms: self.incident.generated_at_ms,
            source: "robot.incidents.list".to_string(),
            dry_run: true,
            read_only: true,
            raw_pane_content_stored: false,
            live_mutation_allowed: false,
            side_effects_executed: false,
            filters,
            incident_count: incidents.len(),
            incidents,
            error_envelopes: self.incident.error_envelopes.clone(),
            mcp_resources: mcp_resources(Some(&self.incident.incident_id)),
        })
    }

    pub fn show_payload(
        &self,
        incident_id: &str,
    ) -> Result<IncidentShowPayload, IncidentSurfaceError> {
        let incident = self.require_incident(incident_id)?;
        Ok(IncidentShowPayload {
            schema_version: 1,
            contract_id: INCIDENT_SURFACE_CONTRACT_ID_V1.to_string(),
            surface: "show".to_string(),
            generated_at_ms: incident.generated_at_ms,
            source: "robot.incidents.show".to_string(),
            dry_run: true,
            read_only: true,
            raw_pane_content_stored: false,
            live_mutation_allowed: false,
            side_effects_executed: false,
            incident: incident.list_entry(),
            dag: incident.dag.clone(),
            evidence_refs: incident.evidence_refs(),
            root_cause_candidates: incident.root_cause_candidates(),
            proof_coverage: incident.dag.proof_coverage.clone(),
            gaps: incident.dag.unexplained_gaps.clone(),
            error_envelopes: incident.error_envelopes.clone(),
            mcp_resources: mcp_resources(Some(&incident.incident_id)),
        })
    }

    pub fn explain_payload(
        &self,
        incident_id: &str,
    ) -> Result<IncidentExplainPayload, IncidentSurfaceError> {
        let incident = self.require_incident(incident_id)?;
        let outcome = incident.outcome();
        Ok(IncidentExplainPayload {
            schema_version: 1,
            contract_id: INCIDENT_SURFACE_CONTRACT_ID_V1.to_string(),
            surface: "explain".to_string(),
            generated_at_ms: incident.generated_at_ms,
            source: "robot.incidents.explain".to_string(),
            dry_run: true,
            read_only: true,
            raw_pane_content_stored: false,
            live_mutation_allowed: false,
            side_effects_executed: false,
            incident_id: incident.incident_id.clone(),
            outcome,
            reason_code: outcome.as_reason_code().to_string(),
            explanation: explanation_for(outcome),
            root_cause_candidates: incident.root_cause_candidates(),
            proof_coverage: incident.dag.proof_coverage.clone(),
            gaps: incident.dag.unexplained_gaps.clone(),
            error_envelopes: incident.error_envelopes.clone(),
            mcp_resources: mcp_resources(Some(&incident.incident_id)),
        })
    }

    pub fn replay_payload(
        &self,
        incident_id: &str,
    ) -> Result<IncidentReplayPayload, IncidentSurfaceError> {
        let incident = self.require_incident(incident_id)?;
        let frames = incident.replay_frames();
        Ok(IncidentReplayPayload {
            schema_version: 1,
            contract_id: INCIDENT_SURFACE_CONTRACT_ID_V1.to_string(),
            surface: "replay".to_string(),
            generated_at_ms: incident.generated_at_ms,
            source: "robot.incidents.replay".to_string(),
            dry_run: true,
            read_only: true,
            raw_pane_content_stored: false,
            live_mutation_allowed: false,
            side_effects_executed: false,
            incident_id: incident.incident_id.clone(),
            deterministic_order: "occurred_at_ms,ingested_at_ms,ingest_sequence,event_id"
                .to_string(),
            frame_count: frames.len(),
            frames,
            error_envelopes: incident.error_envelopes.clone(),
            mcp_resources: mcp_resources(Some(&incident.incident_id)),
        })
    }

    fn require_incident(
        &self,
        incident_id: &str,
    ) -> Result<&IncidentSurfaceIncident, IncidentSurfaceError> {
        if self.incident.incident_id == incident_id {
            Ok(&self.incident)
        } else {
            Err(IncidentSurfaceError::new(
                IncidentSurfaceErrorKind::NoIncidentFound,
                "incident.no_incident_found",
                format!("incident {incident_id:?} was not found in source set"),
            ))
        }
    }
}

impl IncidentSurfaceFilter {
    fn validate(&self) -> Result<(), IncidentSurfaceError> {
        if self.limit == 0 {
            return Err(IncidentSurfaceError::new(
                IncidentSurfaceErrorKind::InvalidFilter,
                "incident.filter.limit_zero",
                "limit must be greater than zero",
            ));
        }
        if let (Some(since), Some(until)) = (self.since_ms, self.until_ms)
            && since > until
        {
            return Err(IncidentSurfaceError::new(
                IncidentSurfaceErrorKind::InvalidFilter,
                "incident.filter.invalid_time_range",
                "since_ms must be less than or equal to until_ms",
            ));
        }
        Ok(())
    }
}

impl IncidentSurfaceIncident {
    fn matches_filters(&self, filters: &IncidentSurfaceFilter) -> bool {
        filters.bead.as_deref().is_none_or(|bead| {
            self.events
                .iter()
                .any(|event| event.correlation.bead_id.as_deref() == Some(bead))
        }) && filters.pane.is_none_or(|pane| {
            self.events
                .iter()
                .any(|event| event.correlation.pane_id == Some(pane))
        }) && filters.worker.as_deref().is_none_or(|worker| {
            self.events
                .iter()
                .any(|event| event.correlation.rch_worker_id.as_deref() == Some(worker))
        }) && filters.build_id.as_deref().is_none_or(|build_id| {
            self.events
                .iter()
                .any(|event| event.correlation.rch_build_id.as_deref() == Some(build_id))
        }) && filters
            .source_kind
            .is_none_or(|source| self.events.iter().any(|event| event.source == source))
            && filters
                .causal_class
                .is_none_or(|class| self.events.iter().any(|event| event.event_class == class))
            && self.events.iter().any(|event| {
                filters
                    .since_ms
                    .is_none_or(|since| event.occurred_at_ms >= since)
                    && filters
                        .until_ms
                        .is_none_or(|until| event.occurred_at_ms <= until)
            })
    }

    fn list_entry(&self) -> IncidentListEntry {
        let mut event_times = self.events.iter().map(|event| event.occurred_at_ms);
        let first_event_ms = event_times.next();
        let (first_event_ms, last_event_ms) = if let Some(first) = first_event_ms {
            let mut min = first;
            let mut max = first;
            for occurred_at_ms in self.events.iter().map(|event| event.occurred_at_ms) {
                min = min.min(occurred_at_ms);
                max = max.max(occurred_at_ms);
            }
            (Some(min), Some(max))
        } else {
            (None, None)
        };

        IncidentListEntry {
            incident_id: self.incident_id.clone(),
            source_set_id: self.source_set_id.clone(),
            workspace_id: self.workspace_id.clone(),
            generated_at_ms: self.generated_at_ms,
            first_event_ms,
            last_event_ms,
            node_count: self.dag.node_count(),
            edge_count: self.dag.edge_count(),
            root_event_ids: self.dag.root_event_ids.clone(),
            root_cause_event_ids: self
                .dag
                .root_causes()
                .into_iter()
                .map(|node| node.event_id.clone())
                .collect(),
            sources: distinct_sources(&self.events),
            causal_classes: distinct_classes(&self.events),
            bead_ids: distinct_optional_strings(
                self.events
                    .iter()
                    .map(|event| event.correlation.bead_id.as_deref()),
            ),
            pane_ids: distinct_pane_ids(&self.events),
            rch_build_ids: distinct_optional_strings(
                self.events
                    .iter()
                    .map(|event| event.correlation.rch_build_id.as_deref()),
            ),
            rch_worker_ids: distinct_optional_strings(
                self.events
                    .iter()
                    .map(|event| event.correlation.rch_worker_id.as_deref()),
            ),
            proof_admissible: self.dag.proof_coverage.admissible,
            outcome: self.outcome(),
            error_envelope_count: self.error_envelopes.len(),
            source_artifact: self.source_artifact.clone(),
        }
    }

    fn outcome(&self) -> IncidentOutcome {
        if !self.dag.proof_coverage.dirty_tree_event_ids.is_empty() {
            IncidentOutcome::ContaminatedProofAttempt
        } else if !self.dag.proof_coverage.rch_blocking_event_ids.is_empty()
            || self
                .events
                .iter()
                .any(|event| event.event_class == CausalEventClass::InfrastructureFailure)
        {
            IncidentOutcome::InfrastructureBlock
        } else if !self
            .dag
            .proof_coverage
            .source_unavailable_event_ids
            .is_empty()
        {
            IncidentOutcome::SourceUnavailable
        } else if self.dag.proof_coverage.admissible {
            IncidentOutcome::SourcePass
        } else {
            IncidentOutcome::ProofIncomplete
        }
    }

    fn evidence_refs(&self) -> Vec<IncidentEvidenceRef> {
        let mut refs = Vec::new();
        for event in &self.events {
            for artifact in &event.artifacts {
                refs.push(evidence_ref(event, artifact));
            }
        }
        refs.sort_by_key(|item| (item.event_id.clone(), item.kind.clone(), item.uri.clone()));
        refs.dedup();
        refs
    }

    fn root_cause_candidates(&self) -> Vec<IncidentNodeSummary> {
        self.dag
            .root_causes()
            .into_iter()
            .map(IncidentNodeSummary::from)
            .collect()
    }

    fn replay_frames(&self) -> Vec<IncidentReplayFrame> {
        let mut events = self.events.clone();
        events.sort_by_key(|event| {
            (
                event.occurred_at_ms,
                event.ingested_at_ms,
                event.ingest_sequence,
                event.event_id.clone(),
            )
        });
        events
            .iter()
            .enumerate()
            .map(|(index, event)| IncidentReplayFrame {
                index,
                event_id: event.event_id.clone(),
                occurred_at_ms: event.occurred_at_ms,
                ingested_at_ms: event.ingested_at_ms,
                ingest_sequence: event.ingest_sequence,
                source: event.source,
                event_class: event.event_class,
                correlation: event.correlation.clone(),
                artifact_uris: artifact_uris(event),
                payload_hash_sha256: event.privacy.payload_hash_sha256.clone(),
                payload_bytes: event.privacy.payload_bytes,
                redaction_status: event.privacy.redaction_status,
                omission_reason: event.privacy.omission_reason,
                reason_codes: reason_codes(event),
                summary: event_summary(event),
            })
            .collect()
    }
}

impl From<&frankenterm_core_replay_types::incident_dag::IncidentDagNode> for IncidentNodeSummary {
    fn from(node: &frankenterm_core_replay_types::incident_dag::IncidentDagNode) -> Self {
        Self {
            event_id: node.event_id.clone(),
            source: node.source,
            event_class: node.event_class,
            occurred_at_ms: node.occurred_at_ms,
            ingested_at_ms: node.ingested_at_ms,
            ingest_sequence: node.ingest_sequence,
            correlation: node.correlation.clone(),
            artifact_uris: node.artifact_uris.clone(),
        }
    }
}

pub fn parse_source_kind(value: &str) -> Result<SwarmCausalEventSource, IncidentSurfaceError> {
    match normalize_filter_value(value).as_str() {
        "pane" => Ok(SwarmCausalEventSource::Pane),
        "robot" => Ok(SwarmCausalEventSource::Robot),
        "mcp" => Ok(SwarmCausalEventSource::Mcp),
        "workflow" => Ok(SwarmCausalEventSource::Workflow),
        "policy" => Ok(SwarmCausalEventSource::Policy),
        "beads" => Ok(SwarmCausalEventSource::Beads),
        "rch" => Ok(SwarmCausalEventSource::Rch),
        "agent_mail" => Ok(SwarmCausalEventSource::AgentMail),
        "git" => Ok(SwarmCausalEventSource::Git),
        "operator" => Ok(SwarmCausalEventSource::Operator),
        "runtime" => Ok(SwarmCausalEventSource::Runtime),
        "source_unavailable" => Ok(SwarmCausalEventSource::SourceUnavailable),
        _ => Err(IncidentSurfaceError::new(
            IncidentSurfaceErrorKind::InvalidFilter,
            "incident.filter.invalid_source_kind",
            format!("unknown incident source kind {value:?}"),
        )),
    }
}

pub fn parse_causal_class(value: &str) -> Result<CausalEventClass, IncidentSurfaceError> {
    match normalize_filter_value(value).as_str() {
        "source_pass" => Ok(CausalEventClass::SourcePass),
        "source_failure" => Ok(CausalEventClass::SourceFailure),
        "infrastructure_failure" => Ok(CausalEventClass::InfrastructureFailure),
        "dirty_tree_contamination" => Ok(CausalEventClass::DirtyTreeContamination),
        "communication_outage" => Ok(CausalEventClass::CommunicationOutage),
        "policy_denial" => Ok(CausalEventClass::PolicyDenial),
        "operator_cancellation" => Ok(CausalEventClass::OperatorCancellation),
        "evidence_unavailable" => Ok(CausalEventClass::EvidenceUnavailable),
        "informational" => Ok(CausalEventClass::Informational),
        _ => Err(IncidentSurfaceError::new(
            IncidentSurfaceErrorKind::InvalidFilter,
            "incident.filter.invalid_causal_class",
            format!("unknown incident causal class {value:?}"),
        )),
    }
}

#[must_use]
pub fn all_incident_mcp_resource_descriptors() -> Vec<IncidentMcpResourceDescriptor> {
    mcp_resources(None)
}

fn normalize_filter_value(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

fn mcp_resources(incident_id: Option<&str>) -> Vec<IncidentMcpResourceDescriptor> {
    let show_uri = incident_id.map_or_else(
        || INCIDENT_MCP_SHOW_URI_TEMPLATE.to_string(),
        |id| format!("wa://incidents/{id}"),
    );
    let explain_uri = incident_id.map_or_else(
        || INCIDENT_MCP_EXPLAIN_URI_TEMPLATE.to_string(),
        |id| format!("wa://incidents/{id}/explain"),
    );
    let replay_uri = incident_id.map_or_else(
        || INCIDENT_MCP_REPLAY_URI_TEMPLATE.to_string(),
        |id| format!("wa://incidents/{id}/replay"),
    );
    vec![
        mcp_resource("list", INCIDENT_MCP_LIST_URI, None),
        mcp_resource("show", &show_uri, Some(INCIDENT_MCP_SHOW_URI_TEMPLATE)),
        mcp_resource(
            "explain",
            &explain_uri,
            Some(INCIDENT_MCP_EXPLAIN_URI_TEMPLATE),
        ),
        mcp_resource(
            "replay",
            &replay_uri,
            Some(INCIDENT_MCP_REPLAY_URI_TEMPLATE),
        ),
    ]
}

fn mcp_resource(
    kind: &str,
    uri: &str,
    uri_template: Option<&str>,
) -> IncidentMcpResourceDescriptor {
    IncidentMcpResourceDescriptor {
        kind: kind.to_string(),
        uri: uri.to_string(),
        uri_template: uri_template.map(ToString::to_string),
        read_only: true,
        live_mutation_allowed: false,
        bounded_output: true,
        redaction_guaranteed: true,
        mime_type: "application/json".to_string(),
    }
}

fn error_envelopes(
    events: &[SwarmCausalEvent],
    failures: &[SourceAdapterFailure],
) -> Vec<IncidentSurfaceErrorEnvelope> {
    let mut envelopes = Vec::new();
    for failure in failures {
        envelopes.push(IncidentSurfaceErrorEnvelope {
            kind: if failure.reason_code == "source_set.malformed_json" {
                IncidentSurfaceErrorKind::MalformedSourceSet
            } else {
                IncidentSurfaceErrorKind::SourceUnavailable
            },
            code: failure.reason_code.clone(),
            detail: failure.detail.clone(),
            event_ids: Vec::new(),
            recoverable: true,
        });
    }
    let mut by_code = BTreeMap::<(IncidentSurfaceErrorKind, String), BTreeSet<String>>::new();
    for event in events {
        if event.source == SwarmCausalEventSource::SourceUnavailable {
            by_code
                .entry((
                    IncidentSurfaceErrorKind::SourceUnavailable,
                    payload_reason(event, "source_unavailable").to_string(),
                ))
                .or_default()
                .insert(event.event_id.clone());
        }
        if matches!(
            event.privacy.redaction_status,
            CausalRedactionStatus::Redacted
                | CausalRedactionStatus::Truncated
                | CausalRedactionStatus::HashOnly
        ) {
            by_code
                .entry((
                    IncidentSurfaceErrorKind::RedactedContent,
                    redaction_reason_code(event.privacy.redaction_status).to_string(),
                ))
                .or_default()
                .insert(event.event_id.clone());
        }
        if event.privacy.omission_reason == Some(CausalPayloadOmissionReason::ExpiredByRetention) {
            by_code
                .entry((
                    IncidentSurfaceErrorKind::RetentionExpired,
                    "incident.retention_expired".to_string(),
                ))
                .or_default()
                .insert(event.event_id.clone());
        }
    }
    envelopes.extend(by_code.into_iter().map(|((kind, code), event_ids)| {
        IncidentSurfaceErrorEnvelope {
            kind,
            code,
            detail: envelope_detail(kind),
            event_ids: event_ids.into_iter().collect(),
            recoverable: !matches!(kind, IncidentSurfaceErrorKind::RetentionExpired),
        }
    }));
    envelopes.sort_by_key(|item| (item.kind, item.code.clone(), item.event_ids.clone()));
    envelopes.dedup();
    envelopes
}

fn envelope_detail(kind: IncidentSurfaceErrorKind) -> String {
    match kind {
        IncidentSurfaceErrorKind::SourceUnavailable => {
            "source evidence was unavailable; replay remains structural".to_string()
        }
        IncidentSurfaceErrorKind::RedactedContent => {
            "raw content is omitted by redaction or size budget".to_string()
        }
        IncidentSurfaceErrorKind::RetentionExpired => {
            "raw content expired under retention policy".to_string()
        }
        IncidentSurfaceErrorKind::MalformedSourceSet => {
            "source-set artifact could not be parsed".to_string()
        }
        IncidentSurfaceErrorKind::NoIncidentFound
        | IncidentSurfaceErrorKind::InvalidFilter
        | IncidentSurfaceErrorKind::DagBuildFailed => "incident surface error".to_string(),
    }
}

fn redaction_reason_code(status: CausalRedactionStatus) -> &'static str {
    match status {
        CausalRedactionStatus::NotRequired => "incident.redaction.not_required",
        CausalRedactionStatus::Redacted => "incident.redaction.redacted",
        CausalRedactionStatus::Truncated => "incident.redaction.truncated",
        CausalRedactionStatus::HashOnly => "incident.redaction.hash_only",
        CausalRedactionStatus::Unavailable => "incident.redaction.unavailable",
    }
}

fn explanation_for(outcome: IncidentOutcome) -> String {
    match outcome {
        IncidentOutcome::SourcePass => {
            "remote RCH proof passed with no contaminating evidence in the incident DAG".to_string()
        }
        IncidentOutcome::InfrastructureBlock => {
            "RCH or infrastructure evidence blocked proof before an admissible source pass"
                .to_string()
        }
        IncidentOutcome::ContaminatedProofAttempt => {
            "dirty-tree evidence contaminated the proof attempt".to_string()
        }
        IncidentOutcome::SourceUnavailable => {
            "one or more evidence sources were unavailable; use fallback artifacts already present"
                .to_string()
        }
        IncidentOutcome::ProofIncomplete => {
            "the incident DAG lacks enough admissible remote proof coverage".to_string()
        }
    }
}

fn evidence_ref(event: &SwarmCausalEvent, artifact: &CausalArtifactRef) -> IncidentEvidenceRef {
    IncidentEvidenceRef {
        event_id: event.event_id.clone(),
        source: event.source,
        kind: artifact.kind.clone(),
        uri: artifact.uri.clone(),
        sha256: artifact.sha256.clone(),
        size_bytes: artifact.size_bytes,
    }
}

fn artifact_uris(event: &SwarmCausalEvent) -> Vec<String> {
    let mut uris = event
        .artifacts
        .iter()
        .map(|artifact| artifact.uri.clone())
        .collect::<Vec<_>>();
    uris.sort();
    uris.dedup();
    uris
}

fn reason_codes(event: &SwarmCausalEvent) -> Vec<String> {
    let mut codes = event
        .payload
        .get("reason_codes")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .take(16)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if codes.is_empty() {
        if let Some(reason) = event
            .payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
        {
            codes.push(reason.to_string());
        } else {
            codes.push(format!(
                "incident.event.{}.{}",
                source_label(event.source),
                class_label(event.event_class)
            ));
        }
    }
    codes.sort();
    codes.dedup();
    codes
}

fn event_summary(event: &SwarmCausalEvent) -> String {
    let adapter = event
        .payload
        .get("adapter")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| source_label(event.source));
    if let Some(reason) = event
        .payload
        .get("reason")
        .and_then(serde_json::Value::as_str)
    {
        format!(
            "{adapter} {} {} ({reason})",
            source_label(event.source),
            class_label(event.event_class)
        )
    } else {
        format!(
            "{adapter} {} {}",
            source_label(event.source),
            class_label(event.event_class)
        )
    }
}

fn payload_reason<'a>(event: &'a SwarmCausalEvent, fallback: &'a str) -> &'a str {
    event
        .payload
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(fallback)
}

fn distinct_sources(events: &[SwarmCausalEvent]) -> Vec<SwarmCausalEventSource> {
    let mut sources = events.iter().map(|event| event.source).collect::<Vec<_>>();
    sources.sort_by_key(|source| source_label(*source));
    sources.dedup();
    sources
}

fn distinct_classes(events: &[SwarmCausalEvent]) -> Vec<CausalEventClass> {
    let mut classes = events
        .iter()
        .map(|event| event.event_class)
        .collect::<Vec<_>>();
    classes.sort_by_key(|class| class_label(*class));
    classes.dedup();
    classes
}

fn distinct_pane_ids(events: &[SwarmCausalEvent]) -> Vec<u64> {
    let mut ids = events
        .iter()
        .filter_map(|event| event.correlation.pane_id)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn distinct_optional_strings<'a>(values: impl Iterator<Item = Option<&'a str>>) -> Vec<String> {
    let mut out = values
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    out.sort();
    out.dedup();
    out
}

const fn source_label(source: SwarmCausalEventSource) -> &'static str {
    match source {
        SwarmCausalEventSource::Pane => "pane",
        SwarmCausalEventSource::Robot => "robot",
        SwarmCausalEventSource::Mcp => "mcp",
        SwarmCausalEventSource::Workflow => "workflow",
        SwarmCausalEventSource::Policy => "policy",
        SwarmCausalEventSource::Beads => "beads",
        SwarmCausalEventSource::Rch => "rch",
        SwarmCausalEventSource::AgentMail => "agent_mail",
        SwarmCausalEventSource::Git => "git",
        SwarmCausalEventSource::Operator => "operator",
        SwarmCausalEventSource::Runtime => "runtime",
        SwarmCausalEventSource::SourceUnavailable => "source_unavailable",
    }
}

const fn class_label(class: CausalEventClass) -> &'static str {
    match class {
        CausalEventClass::SourcePass => "source_pass",
        CausalEventClass::SourceFailure => "source_failure",
        CausalEventClass::InfrastructureFailure => "infrastructure_failure",
        CausalEventClass::DirtyTreeContamination => "dirty_tree_contamination",
        CausalEventClass::CommunicationOutage => "communication_outage",
        CausalEventClass::PolicyDenial => "policy_denial",
        CausalEventClass::OperatorCancellation => "operator_cancellation",
        CausalEventClass::EvidenceUnavailable => "evidence_unavailable",
        CausalEventClass::Informational => "informational",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    const VALID_SOURCE_SET: &str =
        include_str!("../../../fixtures/flight-recorder/source-adapters/valid-source-set.json");
    const GOLDEN_SCENARIOS: &str =
        include_str!("../../../fixtures/flight-recorder/incident-dag/golden-scenarios.v1.json");
    const INCIDENT_CORPUS_MANIFEST: &str =
        include_str!("../../../fixtures/flight-recorder/incident-corpus/manifest.v1.json");

    #[derive(Debug, Deserialize)]
    struct GoldenFixture {
        scenarios: Vec<GoldenScenario>,
    }

    #[derive(Debug, Deserialize)]
    struct GoldenScenario {
        scenario_id: String,
        source_set: serde_json::Value,
    }

    #[derive(Debug, Deserialize)]
    struct IncidentCorpusManifest {
        contract_id: String,
        bead_id: String,
        scenarios: Vec<IncidentCorpusScenario>,
    }

    #[derive(Debug, Deserialize)]
    struct IncidentCorpusScenario {
        id: String,
        source_set: IncidentCorpusArtifact,
        golden_json: IncidentCorpusArtifact,
        golden_toon: IncidentCorpusArtifact,
        expected_outcome: IncidentOutcome,
        expected_proof_admissible: bool,
        material_remote_rch_metadata: bool,
    }

    #[derive(Debug, Deserialize)]
    struct IncidentCorpusArtifact {
        path: String,
        sha256: String,
    }

    #[derive(Debug, Deserialize)]
    struct IncidentGoldenArtifact {
        contract_id: String,
        bead_id: String,
        scenario_id: String,
        source_set_path: String,
        surface_contract_id: String,
        expected: IncidentGoldenExpected,
    }

    #[derive(Debug, Deserialize)]
    struct IncidentGoldenExpected {
        incident_id: String,
        outcome: IncidentOutcome,
        proof_admissible: bool,
        frame_count: usize,
        required_sources: Vec<SwarmCausalEventSource>,
        required_causal_classes: Vec<CausalEventClass>,
        required_inadmissible_reason_codes: Vec<String>,
        required_error_codes: Vec<String>,
        required_frame_reason_codes: Vec<String>,
        required_artifact_uris: Vec<String>,
        required_rch_build_ids: Vec<String>,
        required_rch_worker_ids: Vec<String>,
        required_source_set_substrings: Vec<String>,
        forbidden_substrings: Vec<String>,
    }

    fn store(input: &str) -> IncidentSurfaceStore {
        IncidentSurfaceStore::from_source_set_json(
            input,
            IncidentSurfaceBuildOptions {
                generated_at_ms: Some(1_778_000_100_000),
                ..Default::default()
            },
        )
        .unwrap()
    }

    fn fixture_path(path: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }

    fn fixture_string(path: &str) -> String {
        std::fs::read_to_string(fixture_path(path))
            .unwrap_or_else(|error| panic!("read fixture {path}: {error}"))
    }

    fn sha256_hex(input: &[u8]) -> String {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(input);
        hex::encode(hasher.finalize())
    }

    fn assert_contains_all<T>(actual: &[T], required: &[T], scenario_id: &str, field: &str)
    where
        T: std::fmt::Debug + PartialEq,
    {
        for item in required {
            assert!(
                actual.contains(item),
                "{scenario_id} missing {field} {item:?}; actual={actual:?}"
            );
        }
    }

    fn assert_rendered_does_not_contain(
        scenario_id: &str,
        forbidden: &[String],
        values: &[serde_json::Value],
    ) {
        let rendered = serde_json::to_string(values).unwrap();
        for needle in forbidden {
            assert!(
                !rendered.contains(needle),
                "{scenario_id} leaked forbidden substring {needle}: {rendered}"
            );
        }
    }

    #[test]
    fn list_show_explain_and_replay_expose_read_only_contract() {
        let store = store(VALID_SOURCE_SET);
        let list = store
            .list_payload(IncidentSurfaceFilter {
                bead: Some("ft-ogr3n.2".to_string()),
                pane: Some(42),
                worker: Some("vmi1227854".to_string()),
                build_id: Some("29871232832766246".to_string()),
                source_kind: Some(SwarmCausalEventSource::Rch),
                causal_class: Some(CausalEventClass::DirtyTreeContamination),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(list.incident_count, 1);
        assert_eq!(list.incidents[0].incident_id, "ft-ogr3n2-valid");
        assert_eq!(
            list.incidents[0].outcome,
            IncidentOutcome::ContaminatedProofAttempt
        );
        assert!(list.dry_run);
        assert!(list.read_only);
        assert!(!list.live_mutation_allowed);
        assert!(!list.side_effects_executed);

        let show = store.show_payload("ft-ogr3n2-valid").unwrap();
        assert_eq!(show.surface, "show");
        assert_ne!(
            show.evidence_refs,
            [] as [crate::replay_incidents::IncidentEvidenceRef; 0]
        );
        assert_ne!(
            show.root_cause_candidates,
            [] as [crate::replay_incidents::IncidentNodeSummary; 0]
        );
        assert_eq!(
            show.incident.outcome,
            IncidentOutcome::ContaminatedProofAttempt
        );

        let explain = store.explain_payload("ft-ogr3n2-valid").unwrap();
        assert_eq!(explain.outcome, IncidentOutcome::ContaminatedProofAttempt);
        assert_eq!(
            explain.reason_code,
            "incident.outcome.contaminated_proof_attempt"
        );

        let replay = store.replay_payload("ft-ogr3n2-valid").unwrap();
        assert_eq!(replay.frame_count, 5);
        assert_eq!(replay.frames[0].index, 0);
        assert_eq!(
            replay.frames[0].event_class,
            CausalEventClass::EvidenceUnavailable
        );
        assert!(
            replay
                .frames
                .iter()
                .all(|frame| !frame.summary.contains("ghp_"))
        );
        let rendered = serde_json::to_string(&replay).unwrap();
        assert!(!rendered.contains("ghp_examplefixturetoken"));
        assert!(!rendered.contains("API_TOKEN"));
    }

    #[test]
    fn golden_scenarios_classify_closeout_evidence() {
        let fixture: GoldenFixture = serde_json::from_str(GOLDEN_SCENARIOS).unwrap();
        let mut outcomes = BTreeMap::new();
        for scenario in fixture.scenarios {
            let input = serde_json::to_string(&scenario.source_set).unwrap();
            let store = store(&input);
            let list = store
                .list_payload(IncidentSurfaceFilter::default())
                .unwrap();
            outcomes.insert(scenario.scenario_id, list.incidents[0].outcome);
        }
        assert_eq!(
            outcomes["rch_mirror_failure_before_cargo"],
            IncidentOutcome::InfrastructureBlock
        );
        assert_eq!(
            outcomes["dirty_tree_proof_contamination"],
            IncidentOutcome::ContaminatedProofAttempt
        );
        assert_eq!(
            outcomes["agent_mail_outage_with_beads_fallback"],
            IncidentOutcome::SourceUnavailable
        );
        assert_eq!(
            outcomes["clean_remote_proof_pass"],
            IncidentOutcome::SourcePass
        );
    }

    #[test]
    fn incident_golden_corpus_matches_replay_surfaces() {
        let manifest: IncidentCorpusManifest =
            serde_json::from_str(INCIDENT_CORPUS_MANIFEST).unwrap();
        assert_eq!(
            manifest.contract_id,
            "ft.flight_recorder.incident_corpus.v1"
        );
        assert_eq!(manifest.bead_id, "ft-ogr3n.6");
        assert_eq!(manifest.scenarios.len(), 5);

        for scenario in manifest.scenarios {
            let source_set = fixture_string(&scenario.source_set.path);
            assert_eq!(
                sha256_hex(source_set.as_bytes()),
                scenario.source_set.sha256,
                "{} source-set hash drifted",
                scenario.id
            );
            let golden_json = fixture_string(&scenario.golden_json.path);
            assert_eq!(
                sha256_hex(golden_json.as_bytes()),
                scenario.golden_json.sha256,
                "{} golden JSON hash drifted",
                scenario.id
            );
            let golden_toon = fixture_string(&scenario.golden_toon.path);
            assert_eq!(
                sha256_hex(golden_toon.as_bytes()),
                scenario.golden_toon.sha256,
                "{} golden TOON hash drifted",
                scenario.id
            );

            let golden: IncidentGoldenArtifact = serde_json::from_str(&golden_json)
                .unwrap_or_else(|error| panic!("{} golden JSON: {error}", scenario.id));
            assert_eq!(golden.contract_id, "ft.flight_recorder.incident_golden.v1");
            assert_eq!(golden.bead_id, "ft-ogr3n.6");
            assert_eq!(golden.scenario_id, scenario.id);
            assert_eq!(golden.source_set_path, scenario.source_set.path);
            assert_eq!(golden.surface_contract_id, INCIDENT_SURFACE_CONTRACT_ID_V1);
            assert_eq!(golden.expected.outcome, scenario.expected_outcome);
            assert_eq!(
                golden.expected.proof_admissible,
                scenario.expected_proof_admissible
            );

            let store = IncidentSurfaceStore::from_source_set_json(
                &source_set,
                IncidentSurfaceBuildOptions {
                    source_artifact: Some(scenario.source_set.path.clone()),
                    ..Default::default()
                },
            )
            .unwrap_or_else(|error| panic!("{} store build: {error}", scenario.id));
            let list = store
                .list_payload(IncidentSurfaceFilter::default())
                .unwrap_or_else(|error| panic!("{} list payload: {error}", scenario.id));
            assert_eq!(list.incident_count, 1, "{}", scenario.id);
            let incident = &list.incidents[0];
            assert_eq!(incident.incident_id, golden.expected.incident_id);
            assert_eq!(incident.outcome, golden.expected.outcome);
            assert_eq!(incident.proof_admissible, golden.expected.proof_admissible);
            assert_eq!(list.contract_id, INCIDENT_SURFACE_CONTRACT_ID_V1);
            assert!(list.dry_run);
            assert!(list.read_only);
            assert!(!list.live_mutation_allowed);
            assert!(!list.side_effects_executed);
            assert_contains_all(
                &incident.sources,
                &golden.expected.required_sources,
                &scenario.id,
                "source",
            );
            assert_contains_all(
                &incident.causal_classes,
                &golden.expected.required_causal_classes,
                &scenario.id,
                "causal class",
            );
            assert_contains_all(
                &incident.rch_build_ids,
                &golden.expected.required_rch_build_ids,
                &scenario.id,
                "RCH build id",
            );
            assert_contains_all(
                &incident.rch_worker_ids,
                &golden.expected.required_rch_worker_ids,
                &scenario.id,
                "RCH worker id",
            );

            let show = store
                .show_payload(&golden.expected.incident_id)
                .unwrap_or_else(|error| panic!("{} show payload: {error}", scenario.id));
            let explain = store
                .explain_payload(&golden.expected.incident_id)
                .unwrap_or_else(|error| panic!("{} explain payload: {error}", scenario.id));
            let replay = store
                .replay_payload(&golden.expected.incident_id)
                .unwrap_or_else(|error| panic!("{} replay payload: {error}", scenario.id));
            assert_eq!(explain.outcome, golden.expected.outcome);
            assert_eq!(
                show.proof_coverage.admissible,
                golden.expected.proof_admissible
            );
            assert_eq!(replay.frame_count, golden.expected.frame_count);
            assert!(show.read_only);
            assert!(explain.read_only);
            assert!(replay.read_only);
            assert!(!show.live_mutation_allowed);
            assert!(!explain.live_mutation_allowed);
            assert!(!replay.live_mutation_allowed);
            assert!(!show.side_effects_executed);
            assert!(!explain.side_effects_executed);
            assert!(!replay.side_effects_executed);
            assert!(!show.raw_pane_content_stored);
            assert!(!explain.raw_pane_content_stored);
            assert!(!replay.raw_pane_content_stored);
            assert_eq!(
                replay.deterministic_order,
                "occurred_at_ms,ingested_at_ms,ingest_sequence,event_id"
            );
            assert_contains_all(
                &show.proof_coverage.inadmissible_reason_codes,
                &golden.expected.required_inadmissible_reason_codes,
                &scenario.id,
                "inadmissible reason",
            );
            let error_codes = list
                .error_envelopes
                .iter()
                .map(|envelope| envelope.code.clone())
                .collect::<Vec<_>>();
            assert_contains_all(
                &error_codes,
                &golden.expected.required_error_codes,
                &scenario.id,
                "error code",
            );
            let frame_reason_codes = replay
                .frames
                .iter()
                .flat_map(|frame| frame.reason_codes.clone())
                .collect::<Vec<_>>();
            assert_contains_all(
                &frame_reason_codes,
                &golden.expected.required_frame_reason_codes,
                &scenario.id,
                "frame reason code",
            );
            let artifact_uris = show
                .evidence_refs
                .iter()
                .map(|artifact| artifact.uri.clone())
                .collect::<Vec<_>>();
            assert_contains_all(
                &artifact_uris,
                &golden.expected.required_artifact_uris,
                &scenario.id,
                "artifact uri",
            );
            for required in &golden.expected.required_source_set_substrings {
                assert!(
                    source_set.contains(required),
                    "{} missing retained source-set substring {required}",
                    scenario.id
                );
            }
            if scenario.material_remote_rch_metadata {
                assert!(
                    incident
                        .rch_worker_ids
                        .iter()
                        .any(|worker| worker.starts_with("vmi")),
                    "{} missing retained remote worker metadata",
                    scenario.id
                );
                assert!(
                    incident
                        .rch_build_ids
                        .iter()
                        .any(|build| build.starts_with("j-")),
                    "{} missing retained RCH job/build metadata",
                    scenario.id
                );
            }
            assert!(golden_toon.contains("local_cargo_counted: false"));
            assert!(golden_toon.contains(&format!("scenario_id: {}", scenario.id)));
            assert_rendered_does_not_contain(
                &scenario.id,
                &golden.expected.forbidden_substrings,
                &[
                    serde_json::to_value(&list).unwrap(),
                    serde_json::to_value(&show).unwrap(),
                    serde_json::to_value(&explain).unwrap(),
                    serde_json::to_value(&replay).unwrap(),
                ],
            );
        }
    }

    #[test]
    fn error_envelopes_distinguish_redaction_source_and_filter_failures() {
        let store = store(VALID_SOURCE_SET);
        let list = store
            .list_payload(IncidentSurfaceFilter::default())
            .unwrap();
        assert!(list.error_envelopes.iter().any(|envelope| {
            envelope.kind == IncidentSurfaceErrorKind::RedactedContent
                && envelope.code == "incident.redaction.redacted"
        }));

        let err = store.show_payload("missing").unwrap_err();
        assert_eq!(err.kind(), IncidentSurfaceErrorKind::NoIncidentFound);
        assert_eq!(err.code(), "incident.no_incident_found");

        let err = store
            .list_payload(IncidentSurfaceFilter {
                limit: 0,
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(err.kind(), IncidentSurfaceErrorKind::InvalidFilter);
        assert_eq!(err.code(), "incident.filter.limit_zero");
    }

    #[test]
    fn malformed_source_set_remains_replayable_as_source_unavailable() {
        let store = store("{not-json}");
        let list = store
            .list_payload(IncidentSurfaceFilter::default())
            .unwrap();
        assert_eq!(list.incident_count, 1);
        assert_eq!(
            list.incidents[0].outcome,
            IncidentOutcome::SourceUnavailable
        );
        assert!(list.error_envelopes.iter().any(|envelope| {
            envelope.kind == IncidentSurfaceErrorKind::MalformedSourceSet
                || envelope.kind == IncidentSurfaceErrorKind::SourceUnavailable
        }));
    }

    #[test]
    fn parser_accepts_snake_and_dash_filter_values() {
        assert_eq!(
            parse_source_kind("agent-mail").unwrap(),
            SwarmCausalEventSource::AgentMail
        );
        assert_eq!(
            parse_causal_class("dirty-tree-contamination").unwrap(),
            CausalEventClass::DirtyTreeContamination
        );
        assert_eq!(
            parse_source_kind("nope").unwrap_err().kind(),
            IncidentSurfaceErrorKind::InvalidFilter
        );
    }
}
