#![allow(clippy::module_name_repetitions)]

//! Redacted incident autopsy compiler substrate.
//!
//! The compiler is deliberately pure: callers provide already-collected
//! evidence from storage, causal graph, replay, policy, and pane excerpts. The
//! output is a deterministic manifest, a human report, a compact TOON-like
//! manifest, source hashes, and a decision log explaining every inclusion or
//! exclusion.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::explainability_console::{CausalGraphEdge, CausalGraphNode};
use crate::redactor::Redactor;

/// Default compiler version embedded in incident manifests.
pub const INCIDENT_AUTOPSY_COMPILER_VERSION: &str = "incident_autopsy.v1";

/// Default redaction policy version embedded in incident manifests.
pub const INCIDENT_AUTOPSY_REDACTION_POLICY_VERSION: &str = "frankenterm.redactor.v1";

/// Incident time window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentWindow {
    /// Inclusive start timestamp in epoch milliseconds.
    pub start_ms: u64,
    /// Inclusive end timestamp in epoch milliseconds.
    pub end_ms: u64,
}

/// Source artifact class in an incident autopsy bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentArtifactKind {
    /// Causal graph ledger.
    CausalGraph,
    /// Pattern hit summary.
    PatternHits,
    /// Mission or transaction log.
    MissionLog,
    /// Policy denial or approval record.
    PolicyDenials,
    /// Storage health record.
    StorageHealth,
    /// Pane text excerpt.
    PaneExcerpt,
    /// Snapshot divergence report.
    SnapshotDivergence,
    /// Workflow state or step log.
    WorkflowState,
    /// Replay command or transcript.
    ReplayInstructions,
    /// Other caller-supplied artifact.
    Other,
}

/// Caller-supplied source artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentSourceArtifact {
    /// Stable artifact id.
    pub id: String,
    /// Artifact kind.
    pub kind: IncidentArtifactKind,
    /// Human-readable label.
    pub label: String,
    /// Raw artifact content. The compiler redacts this before output.
    pub content: String,
    /// Whether this artifact should be included in the bundle.
    pub include: bool,
    /// Reason for exclusion when `include` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusion_reason: Option<String>,
}

impl IncidentSourceArtifact {
    /// Build an included source artifact.
    #[must_use]
    pub fn included(
        id: impl Into<String>,
        kind: IncidentArtifactKind,
        label: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            label: label.into(),
            content: content.into(),
            include: true,
            exclusion_reason: None,
        }
    }

    /// Build an excluded source artifact with a reason.
    #[must_use]
    pub fn excluded(
        id: impl Into<String>,
        kind: IncidentArtifactKind,
        label: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            label: label.into(),
            content: String::new(),
            include: false,
            exclusion_reason: Some(reason.into()),
        }
    }
}

/// Evidence citation attached to timeline entries and hypotheses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentEvidenceCitation {
    /// Artifact id that contains the evidence.
    pub artifact_id: String,
    /// Evidence id inside the artifact, for example node id or row id.
    pub evidence_id: String,
    /// Redacted human label.
    pub label: String,
}

impl IncidentEvidenceCitation {
    /// Build a citation.
    #[must_use]
    pub fn new(
        artifact_id: impl Into<String>,
        evidence_id: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            artifact_id: artifact_id.into(),
            evidence_id: evidence_id.into(),
            label: label.into(),
        }
    }
}

/// Pane excerpt included in the incident bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentPaneExcerpt {
    /// Pane id.
    pub pane_id: u64,
    /// Excerpt label.
    pub label: String,
    /// Raw excerpt text. The compiler redacts this before output.
    pub text: String,
    /// Supporting citations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<IncidentEvidenceCitation>,
}

/// Timeline event included in the incident bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentTimelineEntry {
    /// Event timestamp in epoch milliseconds.
    pub timestamp_ms: u64,
    /// Redacted event label.
    pub label: String,
    /// Supporting citations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<IncidentEvidenceCitation>,
}

/// Root-cause confidence class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentHypothesisClass {
    /// Claim has at least one concrete citation.
    EvidenceCited,
    /// Claim is bounded inference and not proven.
    Inferred,
    /// There is insufficient evidence to make a claim.
    Unknown,
}

/// Candidate root-cause hypothesis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentRootCauseHypothesis {
    /// Stable hypothesis id.
    pub id: String,
    /// Redacted summary.
    pub summary: String,
    /// Confidence class.
    pub class: IncidentHypothesisClass,
    /// Score in basis points, 0 to 10_000.
    pub score_bps: u16,
    /// Evidence citations. Required for `EvidenceCited` claims.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<IncidentEvidenceCitation>,
    /// Notes added by normalization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl IncidentRootCauseHypothesis {
    /// Build an evidence-cited hypothesis.
    #[must_use]
    pub fn evidence_cited(
        id: impl Into<String>,
        summary: impl Into<String>,
        score_bps: u16,
        citations: Vec<IncidentEvidenceCitation>,
    ) -> Self {
        Self {
            id: id.into(),
            summary: summary.into(),
            class: IncidentHypothesisClass::EvidenceCited,
            score_bps,
            citations,
            notes: Vec::new(),
        }
    }

    /// Build an inferred hypothesis.
    #[must_use]
    pub fn inferred(id: impl Into<String>, summary: impl Into<String>, score_bps: u16) -> Self {
        Self {
            id: id.into(),
            summary: summary.into(),
            class: IncidentHypothesisClass::Inferred,
            score_bps,
            citations: Vec::new(),
            notes: Vec::new(),
        }
    }
}

/// Compiler input. Every text field is redacted before output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentAutopsyInput {
    /// Stable session id.
    pub session_id: String,
    /// Generation timestamp in epoch milliseconds.
    pub generated_at_ms: u64,
    /// Optional incident window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<IncidentWindow>,
    /// Source artifacts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<IncidentSourceArtifact>,
    /// Pane excerpts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pane_excerpts: Vec<IncidentPaneExcerpt>,
    /// Timeline entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timeline: Vec<IncidentTimelineEntry>,
    /// Causal graph nodes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causal_nodes: Vec<CausalGraphNode>,
    /// Causal graph edges.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causal_edges: Vec<CausalGraphEdge>,
    /// Candidate root-cause hypotheses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hypotheses: Vec<IncidentRootCauseHypothesis>,
    /// Replay commands or next-step commands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replay_commands: Vec<String>,
}

impl IncidentAutopsyInput {
    /// Create an empty incident input.
    #[must_use]
    pub fn new(session_id: impl Into<String>, generated_at_ms: u64) -> Self {
        Self {
            session_id: session_id.into(),
            generated_at_ms,
            window: None,
            artifacts: Vec::new(),
            pane_excerpts: Vec::new(),
            timeline: Vec::new(),
            causal_nodes: Vec::new(),
            causal_edges: Vec::new(),
            hypotheses: Vec::new(),
            replay_commands: Vec::new(),
        }
    }
}

/// Included artifact after redaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentIncludedArtifact {
    /// Artifact id.
    pub id: String,
    /// Artifact kind.
    pub kind: IncidentArtifactKind,
    /// Redacted label.
    pub label: String,
    /// SHA-256 hash of redacted content.
    pub sha256: String,
    /// Redacted excerpt retained in the bundle.
    pub redacted_excerpt: String,
}

/// Excluded artifact record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentExcludedArtifact {
    /// Artifact id.
    pub id: String,
    /// Artifact kind.
    pub kind: IncidentArtifactKind,
    /// Redacted label.
    pub label: String,
    /// Exclusion reason.
    pub reason: String,
}

/// Source hash entry in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentSourceHash {
    /// Artifact id.
    pub artifact_id: String,
    /// Artifact kind.
    pub kind: IncidentArtifactKind,
    /// SHA-256 hash of redacted content.
    pub sha256: String,
}

/// One compiler decision log entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentCompileLog {
    /// Optional artifact id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    /// Machine-readable action.
    pub action: String,
    /// Redacted reason.
    pub reason: String,
}

/// Deterministic machine-readable manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentAutopsyManifest {
    /// Stable schema version.
    pub schema_version: String,
    /// Compiler version.
    pub compiler_version: String,
    /// Redaction policy version.
    pub redaction_policy_version: String,
    /// Session id.
    pub session_id: String,
    /// Generation timestamp in epoch milliseconds.
    pub generated_at_ms: u64,
    /// Optional incident window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<IncidentWindow>,
    /// Hash stable across generation timestamps.
    pub reproducible_hash: String,
    /// Included artifact count.
    pub included_artifacts: usize,
    /// Excluded artifact count.
    pub excluded_artifacts: usize,
    /// Pane excerpt count.
    pub pane_excerpt_count: usize,
    /// Timeline entry count.
    pub timeline_entry_count: usize,
    /// Causal graph node count.
    pub causal_node_count: usize,
    /// Causal graph edge count.
    pub causal_edge_count: usize,
    /// Source hashes keyed by artifact id.
    pub source_hashes: BTreeMap<String, IncidentSourceHash>,
    /// Root-cause hypotheses after normalization.
    pub hypotheses: Vec<IncidentRootCauseHypothesis>,
    /// Missing or excluded evidence notes.
    pub omissions: Vec<String>,
    /// Replay commands.
    pub replay_commands: Vec<String>,
}

/// Complete incident autopsy output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentAutopsyBundle {
    /// Machine-readable manifest.
    pub manifest: IncidentAutopsyManifest,
    /// Pretty JSON manifest.
    pub manifest_json: String,
    /// Compact TOON-like manifest for robot surfaces.
    pub manifest_toon: String,
    /// Human-readable Markdown report.
    pub report_markdown: String,
    /// Included artifacts after redaction.
    pub included_artifacts: Vec<IncidentIncludedArtifact>,
    /// Excluded artifacts.
    pub excluded_artifacts: Vec<IncidentExcludedArtifact>,
    /// Compile decision log.
    pub compile_logs: Vec<IncidentCompileLog>,
}

/// Compiler configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentAutopsyCompilerConfig {
    /// Compiler version.
    pub compiler_version: String,
    /// Redaction policy version.
    pub redaction_policy_version: String,
    /// Maximum artifact excerpt characters retained in the bundle.
    pub max_artifact_excerpt_chars: usize,
    /// Maximum pane excerpt characters retained in the report.
    pub max_pane_excerpt_chars: usize,
    /// Maximum causal nodes rendered in the report.
    pub max_causal_nodes_in_report: usize,
}

impl Default for IncidentAutopsyCompilerConfig {
    fn default() -> Self {
        Self {
            compiler_version: INCIDENT_AUTOPSY_COMPILER_VERSION.to_string(),
            redaction_policy_version: INCIDENT_AUTOPSY_REDACTION_POLICY_VERSION.to_string(),
            max_artifact_excerpt_chars: 640,
            max_pane_excerpt_chars: 480,
            max_causal_nodes_in_report: 16,
        }
    }
}

/// Redacted incident autopsy compiler.
#[derive(Debug, Clone)]
pub struct IncidentAutopsyCompiler {
    config: IncidentAutopsyCompilerConfig,
    redactor: Redactor,
}

impl Default for IncidentAutopsyCompiler {
    fn default() -> Self {
        Self::new(IncidentAutopsyCompilerConfig::default())
    }
}

impl IncidentAutopsyCompiler {
    /// Create a compiler.
    #[must_use]
    pub fn new(config: IncidentAutopsyCompilerConfig) -> Self {
        Self {
            config,
            redactor: Redactor::new(),
        }
    }

    /// Compile a redacted incident autopsy bundle.
    #[must_use]
    pub fn compile(&self, input: IncidentAutopsyInput) -> IncidentAutopsyBundle {
        let mut logs = Vec::new();
        let mut included = Vec::new();
        let mut excluded = Vec::new();
        let mut source_hashes = BTreeMap::new();

        for artifact in input.artifacts {
            let label = self.redactor.redact(&artifact.label);
            if artifact.include {
                let redacted_content = self.redactor.redact(&artifact.content);
                let sha256 = sha256_text(&redacted_content);
                let excerpt =
                    truncate_chars(&redacted_content, self.config.max_artifact_excerpt_chars);
                source_hashes.insert(
                    artifact.id.clone(),
                    IncidentSourceHash {
                        artifact_id: artifact.id.clone(),
                        kind: artifact.kind,
                        sha256: sha256.clone(),
                    },
                );
                included.push(IncidentIncludedArtifact {
                    id: artifact.id.clone(),
                    kind: artifact.kind,
                    label,
                    sha256,
                    redacted_excerpt: excerpt,
                });
                logs.push(IncidentCompileLog {
                    artifact_id: Some(artifact.id),
                    action: "included".to_string(),
                    reason: "artifact included after redaction and hashing".to_string(),
                });
            } else {
                let reason = artifact
                    .exclusion_reason
                    .unwrap_or_else(|| "artifact marked excluded without reason".to_string());
                excluded.push(IncidentExcludedArtifact {
                    id: artifact.id.clone(),
                    kind: artifact.kind,
                    label,
                    reason: self.redactor.redact(&reason),
                });
                logs.push(IncidentCompileLog {
                    artifact_id: Some(artifact.id),
                    action: "excluded".to_string(),
                    reason: self.redactor.redact(&reason),
                });
            }
        }

        included.sort_by(|left, right| left.id.cmp(&right.id));
        excluded.sort_by(|left, right| left.id.cmp(&right.id));

        let pane_excerpts = self.redact_pane_excerpts(input.pane_excerpts);
        let timeline = self.redact_timeline(input.timeline);
        let causal_nodes = self.redact_causal_nodes(input.causal_nodes);
        let causal_edges = self.redact_causal_edges(input.causal_edges);
        let hypotheses = self.normalize_hypotheses(input.hypotheses, &source_hashes, &mut logs);
        let replay_commands = input
            .replay_commands
            .into_iter()
            .map(|command| self.redactor.redact(&command))
            .collect::<Vec<_>>();
        let omissions = omissions_for(
            &included,
            &excluded,
            &pane_excerpts,
            &timeline,
            &causal_nodes,
            &hypotheses,
        );

        let manifest_seed = StableIncidentAutopsySeed {
            compiler_version: self.config.compiler_version.clone(),
            redaction_policy_version: self.config.redaction_policy_version.clone(),
            session_id: input.session_id.clone(),
            window: input.window.clone(),
            source_hashes: source_hashes.clone(),
            included_artifact_ids: included
                .iter()
                .map(|artifact| artifact.id.clone())
                .collect(),
            excluded_artifacts: excluded.clone(),
            pane_excerpt_count: pane_excerpts.len(),
            timeline_entry_count: timeline.len(),
            causal_node_ids: causal_nodes.iter().map(|node| node.id.clone()).collect(),
            causal_edge_count: causal_edges.len(),
            hypotheses: hypotheses.clone(),
            replay_commands: replay_commands.clone(),
            omissions: omissions.clone(),
        };
        let reproducible_hash = sha256_text(&canonical_json(&manifest_seed));

        let manifest = IncidentAutopsyManifest {
            schema_version: "frankenterm.incident_autopsy.v1".to_string(),
            compiler_version: self.config.compiler_version.clone(),
            redaction_policy_version: self.config.redaction_policy_version.clone(),
            session_id: input.session_id,
            generated_at_ms: input.generated_at_ms,
            window: input.window,
            reproducible_hash,
            included_artifacts: included.len(),
            excluded_artifacts: excluded.len(),
            pane_excerpt_count: pane_excerpts.len(),
            timeline_entry_count: timeline.len(),
            causal_node_count: causal_nodes.len(),
            causal_edge_count: causal_edges.len(),
            source_hashes,
            hypotheses,
            omissions,
            replay_commands,
        };

        let manifest_json = pretty_json(&manifest);
        let manifest_toon = manifest_to_toon(&manifest);
        let report_markdown = self.render_report(
            &manifest,
            &included,
            &excluded,
            &pane_excerpts,
            &timeline,
            &causal_nodes,
            &causal_edges,
        );

        IncidentAutopsyBundle {
            manifest,
            manifest_json,
            manifest_toon,
            report_markdown,
            included_artifacts: included,
            excluded_artifacts: excluded,
            compile_logs: logs,
        }
    }

    fn redact_pane_excerpts(
        &self,
        mut excerpts: Vec<IncidentPaneExcerpt>,
    ) -> Vec<IncidentPaneExcerpt> {
        for excerpt in &mut excerpts {
            excerpt.label = self.redactor.redact(&excerpt.label);
            excerpt.text = truncate_chars(
                &self.redactor.redact(&excerpt.text),
                self.config.max_pane_excerpt_chars,
            );
            redact_citations(&self.redactor, &mut excerpt.citations);
        }
        excerpts.sort_by_key(|excerpt| (excerpt.pane_id, excerpt.label.clone()));
        excerpts
    }

    fn redact_timeline(
        &self,
        mut timeline: Vec<IncidentTimelineEntry>,
    ) -> Vec<IncidentTimelineEntry> {
        for entry in &mut timeline {
            entry.label = self.redactor.redact(&entry.label);
            redact_citations(&self.redactor, &mut entry.citations);
        }
        timeline.sort_by_key(|entry| (entry.timestamp_ms, entry.label.clone()));
        timeline
    }

    fn redact_causal_nodes(&self, mut nodes: Vec<CausalGraphNode>) -> Vec<CausalGraphNode> {
        for node in &mut nodes {
            node.label = self.redactor.redact(&node.label);
            for value in node.context.values_mut() {
                *value = self.redactor.redact(value);
            }
        }
        nodes.sort_by(|left, right| left.id.cmp(&right.id));
        nodes
    }

    fn redact_causal_edges(&self, mut edges: Vec<CausalGraphEdge>) -> Vec<CausalGraphEdge> {
        for edge in &mut edges {
            edge.source = self.redactor.redact(&edge.source);
            if let Some(description) = &mut edge.description {
                *description = self.redactor.redact(description);
            }
        }
        edges.sort_by(|left, right| {
            left.from
                .cmp(&right.from)
                .then_with(|| left.to.cmp(&right.to))
                .then_with(|| left.source.cmp(&right.source))
        });
        edges
    }

    fn normalize_hypotheses(
        &self,
        mut hypotheses: Vec<IncidentRootCauseHypothesis>,
        source_hashes: &BTreeMap<String, IncidentSourceHash>,
        logs: &mut Vec<IncidentCompileLog>,
    ) -> Vec<IncidentRootCauseHypothesis> {
        if hypotheses.is_empty() {
            return vec![IncidentRootCauseHypothesis {
                id: "unknown-root-cause".to_string(),
                summary: "No root cause was proven from the supplied evidence.".to_string(),
                class: IncidentHypothesisClass::Unknown,
                score_bps: 0,
                citations: Vec::new(),
                notes: vec![
                    "compiler inserted unknown hypothesis because input had none".to_string(),
                ],
            }];
        }

        for hypothesis in &mut hypotheses {
            hypothesis.summary = self.redactor.redact(&hypothesis.summary);
            hypothesis.score_bps = hypothesis.score_bps.min(10_000);
            redact_citations(&self.redactor, &mut hypothesis.citations);
            if hypothesis.class == IncidentHypothesisClass::EvidenceCited
                && hypothesis.citations.is_empty()
            {
                hypothesis.class = IncidentHypothesisClass::Inferred;
                hypothesis.notes.push(
                    "downgraded from evidence_cited because no citations were supplied".to_string(),
                );
                logs.push(IncidentCompileLog {
                    artifact_id: None,
                    action: "hypothesis_downgraded".to_string(),
                    reason: format!("hypothesis {} had no citations", hypothesis.id),
                });
            }

            let mut missing_artifacts = BTreeSet::new();
            for citation in &hypothesis.citations {
                if !source_hashes.contains_key(&citation.artifact_id) {
                    missing_artifacts.insert(citation.artifact_id.clone());
                }
            }
            if !missing_artifacts.is_empty() {
                hypothesis.class = IncidentHypothesisClass::Inferred;
                hypothesis.notes.push(format!(
                    "citation artifact(s) not included: {}",
                    missing_artifacts.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
        }

        hypotheses.sort_by(|left, right| {
            right
                .score_bps
                .cmp(&left.score_bps)
                .then_with(|| left.id.cmp(&right.id))
        });
        hypotheses
    }

    fn render_report(
        &self,
        manifest: &IncidentAutopsyManifest,
        included: &[IncidentIncludedArtifact],
        excluded: &[IncidentExcludedArtifact],
        pane_excerpts: &[IncidentPaneExcerpt],
        timeline: &[IncidentTimelineEntry],
        causal_nodes: &[CausalGraphNode],
        causal_edges: &[CausalGraphEdge],
    ) -> String {
        let mut report = String::new();
        report.push_str("# Swarm Incident Autopsy\n\n");
        report.push_str(&format!(
            "**Session:** `{}`\n",
            escape_md(&manifest.session_id)
        ));
        report.push_str(&format!(
            "**Manifest hash:** `{}`\n",
            manifest.reproducible_hash
        ));
        report.push_str(&format!(
            "**Redaction policy:** `{}`\n\n",
            escape_md(&manifest.redaction_policy_version)
        ));

        report.push_str("## Summary\n\n");
        report.push_str(&format!(
            "- Included artifacts: {}\n- Excluded artifacts: {}\n- Timeline entries: {}\n- Causal graph: {} nodes / {} edges\n- Pane excerpts: {}\n\n",
            manifest.included_artifacts,
            manifest.excluded_artifacts,
            manifest.timeline_entry_count,
            manifest.causal_node_count,
            manifest.causal_edge_count,
            manifest.pane_excerpt_count
        ));

        report.push_str("## Root Cause Hypotheses\n\n");
        for hypothesis in &manifest.hypotheses {
            report.push_str(&format!(
                "- `{}` [{:?}, {} bps]: {}\n",
                escape_md(&hypothesis.id),
                hypothesis.class,
                hypothesis.score_bps,
                escape_md(&hypothesis.summary)
            ));
            if hypothesis.citations.is_empty() {
                report.push_str("  Evidence: inferred_or_unknown\n");
            } else {
                report.push_str(&format!(
                    "  Evidence: {}\n",
                    hypothesis
                        .citations
                        .iter()
                        .map(format_citation)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            for note in &hypothesis.notes {
                report.push_str(&format!("  Note: {}\n", escape_md(note)));
            }
        }
        report.push('\n');

        report.push_str("## Timeline\n\n");
        if timeline.is_empty() {
            report.push_str("No timeline evidence supplied.\n\n");
        } else {
            for entry in timeline {
                report.push_str(&format!(
                    "- `{}` {}\n",
                    entry.timestamp_ms,
                    escape_md(&entry.label)
                ));
            }
            report.push('\n');
        }

        report.push_str("## Evidence Artifacts\n\n");
        if included.is_empty() {
            report.push_str("No artifacts included.\n\n");
        } else {
            report.push_str("| Artifact | Kind | Hash | Label |\n");
            report.push_str("|----------|------|------|-------|\n");
            for artifact in included {
                report.push_str(&format!(
                    "| `{}` | {:?} | `{}` | {} |\n",
                    escape_table(&artifact.id),
                    artifact.kind,
                    artifact.sha256,
                    escape_table(&artifact.label)
                ));
            }
            report.push('\n');
        }

        report.push_str("## Pane Excerpts\n\n");
        if pane_excerpts.is_empty() {
            report.push_str("No pane excerpts supplied.\n\n");
        } else {
            for excerpt in pane_excerpts {
                report.push_str(&format!(
                    "### Pane {} - {}\n\n```text\n{}\n```\n\n",
                    excerpt.pane_id,
                    escape_md(&excerpt.label),
                    excerpt.text
                ));
            }
        }

        report.push_str("## Causal Graph\n\n");
        if causal_nodes.is_empty() && causal_edges.is_empty() {
            report.push_str("No causal graph evidence supplied.\n\n");
        } else {
            for node in causal_nodes
                .iter()
                .take(self.config.max_causal_nodes_in_report)
            {
                report.push_str(&format!(
                    "- `{}` {:?}: {}\n",
                    escape_md(&node.id),
                    node.kind,
                    escape_md(&node.label)
                ));
            }
            if causal_nodes.len() > self.config.max_causal_nodes_in_report {
                report.push_str(&format!(
                    "- omitted {} additional causal node(s) from human report\n",
                    causal_nodes.len() - self.config.max_causal_nodes_in_report
                ));
            }
            report.push('\n');
        }

        report.push_str("## Omissions\n\n");
        if manifest.omissions.is_empty() && excluded.is_empty() {
            report.push_str("No omissions recorded.\n\n");
        } else {
            for omission in &manifest.omissions {
                report.push_str(&format!("- {}\n", escape_md(omission)));
            }
            for artifact in excluded {
                report.push_str(&format!(
                    "- excluded `{}`: {}\n",
                    escape_md(&artifact.id),
                    escape_md(&artifact.reason)
                ));
            }
            report.push('\n');
        }

        report.push_str("## Replay\n\n");
        if manifest.replay_commands.is_empty() {
            report.push_str("No replay commands supplied.\n");
        } else {
            for command in &manifest.replay_commands {
                report.push_str(&format!("- `{}`\n", escape_md(command)));
            }
        }

        report
    }
}

#[derive(Debug, Serialize)]
struct StableIncidentAutopsySeed {
    compiler_version: String,
    redaction_policy_version: String,
    session_id: String,
    window: Option<IncidentWindow>,
    source_hashes: BTreeMap<String, IncidentSourceHash>,
    included_artifact_ids: Vec<String>,
    excluded_artifacts: Vec<IncidentExcludedArtifact>,
    pane_excerpt_count: usize,
    timeline_entry_count: usize,
    causal_node_ids: Vec<String>,
    causal_edge_count: usize,
    hypotheses: Vec<IncidentRootCauseHypothesis>,
    replay_commands: Vec<String>,
    omissions: Vec<String>,
}

fn redact_citations(redactor: &Redactor, citations: &mut [IncidentEvidenceCitation]) {
    for citation in citations {
        citation.artifact_id = redactor.redact(&citation.artifact_id);
        citation.evidence_id = redactor.redact(&citation.evidence_id);
        citation.label = redactor.redact(&citation.label);
    }
}

fn omissions_for(
    included: &[IncidentIncludedArtifact],
    excluded: &[IncidentExcludedArtifact],
    pane_excerpts: &[IncidentPaneExcerpt],
    timeline: &[IncidentTimelineEntry],
    causal_nodes: &[CausalGraphNode],
    hypotheses: &[IncidentRootCauseHypothesis],
) -> Vec<String> {
    let mut omissions = Vec::new();
    if included.is_empty() {
        omissions.push("no source artifacts were included".to_string());
    }
    if !excluded.is_empty() {
        omissions.push(format!(
            "{} source artifact(s) were excluded",
            excluded.len()
        ));
    }
    if pane_excerpts.is_empty() {
        omissions.push("no pane excerpts were supplied".to_string());
    }
    if timeline.is_empty() {
        omissions.push("no timeline entries were supplied".to_string());
    }
    if causal_nodes.is_empty() {
        omissions.push("no causal graph nodes were supplied".to_string());
    }
    if hypotheses
        .iter()
        .all(|hypothesis| hypothesis.class != IncidentHypothesisClass::EvidenceCited)
    {
        omissions.push("no evidence-cited root-cause hypothesis was proven".to_string());
    }
    omissions
}

fn manifest_to_toon(manifest: &IncidentAutopsyManifest) -> String {
    let mut out = String::new();
    out.push_str("incident_autopsy{");
    out.push_str(&format!(
        "session_id:\"{}\",hash:\"{}\",included:{},excluded:{},timeline:{},panes:{},causal_nodes:{},causal_edges:{}",
        escape_toon(&manifest.session_id),
        manifest.reproducible_hash,
        manifest.included_artifacts,
        manifest.excluded_artifacts,
        manifest.timeline_entry_count,
        manifest.pane_excerpt_count,
        manifest.causal_node_count,
        manifest.causal_edge_count
    ));
    out.push_str(",hypotheses:[");
    for (idx, hypothesis) in manifest.hypotheses.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{id:\"{}\",class:{:?},score:{},citations:{}}}",
            escape_toon(&hypothesis.id),
            hypothesis.class,
            hypothesis.score_bps,
            hypothesis.citations.len()
        ));
    }
    out.push_str("]}");
    out
}

fn format_citation(citation: &IncidentEvidenceCitation) -> String {
    format!(
        "`{}#{}` {}",
        escape_md(&citation.artifact_id),
        escape_md(&citation.evidence_id),
        escape_md(&citation.label)
    )
}

fn canonical_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|err| format!("serde_error:{err}"))
}

fn pretty_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|err| {
        format!(
            "{{\"error\":\"failed to serialize incident autopsy manifest\",\"detail\":\"{}\"}}",
            escape_json_string(&err.to_string())
        )
    })
}

fn sha256_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out = text.chars().take(max_chars).collect::<String>();
    out.push_str("\n[truncated]");
    out
}

fn escape_md(text: &str) -> String {
    text.replace('`', "\\`")
}

fn escape_table(text: &str) -> String {
    escape_md(text).replace('|', "\\|")
}

fn escape_toon(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape_json_string(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explainability_console::{CausalGraphEdge, CausalGraphNode, CausalNodeKind};
    use crate::recording::RecorderEventPayload;
    use crate::redactor::REDACTED_MARKER;
    use crate::test_fixtures::synthetic_swarm::{SyntheticSwarmScale, synthetic_swarm_scenario};

    fn long_secret() -> &'static str {
        "sk-proj-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
    }

    fn cited_hypothesis() -> IncidentRootCauseHypothesis {
        IncidentRootCauseHypothesis::evidence_cited(
            "h-rate-limit",
            "Pattern and policy evidence show the workflow stalled on a rate limit.",
            9_100,
            vec![IncidentEvidenceCitation::new(
                "patterns",
                "rate_limit",
                "rate limit hit",
            )],
        )
    }

    fn input_with_secret(generated_at_ms: u64) -> IncidentAutopsyInput {
        let mut input = IncidentAutopsyInput::new("session-a", generated_at_ms);
        input.artifacts.push(IncidentSourceArtifact::included(
            "patterns",
            IncidentArtifactKind::PatternHits,
            "pattern hits",
            format!("rate_limit matched token {}", long_secret()),
        ));
        input.pane_excerpts.push(IncidentPaneExcerpt {
            pane_id: 7,
            label: "agent output".to_string(),
            text: format!("retrying with {}", long_secret()),
            citations: vec![IncidentEvidenceCitation::new(
                "patterns",
                "line-9",
                "pane excerpt",
            )],
        });
        input.timeline.push(IncidentTimelineEntry {
            timestamp_ms: 10,
            label: "rate limit detected".to_string(),
            citations: vec![IncidentEvidenceCitation::new(
                "patterns",
                "rate_limit",
                "pattern hit",
            )],
        });
        input.hypotheses.push(cited_hypothesis());
        input
            .replay_commands
            .push("ft snapshot diff before after -f json".to_string());
        input
    }

    fn synthetic_swarm_autopsy_input(generated_at_ms: u64) -> IncidentAutopsyInput {
        let scenario = synthetic_swarm_scenario(SyntheticSwarmScale::Fleet10);
        let mut input =
            IncidentAutopsyInput::new(scenario.manifest.scenario_id.clone(), generated_at_ms);
        let first_event = scenario
            .events
            .first()
            .expect("synthetic scenario has events");
        let last_event = scenario
            .events
            .last()
            .expect("synthetic scenario has events");
        input.window = Some(IncidentWindow {
            start_ms: first_event.occurred_at_ms,
            end_ms: last_event.occurred_at_ms,
        });

        input.artifacts.push(IncidentSourceArtifact::included(
            "synthetic-manifest",
            IncidentArtifactKind::Other,
            "synthetic swarm manifest",
            serde_json::to_string(&scenario.manifest).expect("manifest serializes"),
        ));
        input.artifacts.push(IncidentSourceArtifact::included(
            "pattern-hits",
            IncidentArtifactKind::PatternHits,
            "expected synthetic pattern hits",
            scenario
                .manifest
                .expected_pattern_hits
                .iter()
                .map(|hit| format!("{}>={}", hit.rule_id, hit.min_count))
                .collect::<Vec<_>>()
                .join("\n"),
        ));

        let first_pane_id = scenario
            .pane_scripts
            .first()
            .expect("synthetic scenario has panes")
            .pane_id;
        let pane_text = scenario
            .events
            .iter()
            .filter(|event| event.pane_id == first_pane_id)
            .filter_map(|event| match &event.payload {
                RecorderEventPayload::IngressText { text, .. }
                | RecorderEventPayload::EgressOutput { text, .. } => Some(text.clone()),
                RecorderEventPayload::ControlMarker { details, .. } => Some(details.to_string()),
                RecorderEventPayload::LifecycleMarker { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        input.pane_excerpts.push(IncidentPaneExcerpt {
            pane_id: first_pane_id,
            label: "synthetic first pane transcript".to_string(),
            text: pane_text,
            citations: vec![IncidentEvidenceCitation::new(
                "synthetic-manifest",
                format!("pane-{first_pane_id}"),
                "synthetic pane script",
            )],
        });

        for event in scenario.events.iter().take(6) {
            input.timeline.push(IncidentTimelineEntry {
                timestamp_ms: event.occurred_at_ms,
                label: format!("{:?} {}", event.source, event.event_id),
                citations: vec![IncidentEvidenceCitation::new(
                    "synthetic-manifest",
                    event.event_id.clone(),
                    "recorder event",
                )],
            });
        }

        input.causal_nodes.push(
            CausalGraphNode::new(
                "synthetic-signal",
                CausalNodeKind::PatternMatch,
                first_event.occurred_at_ms + 200,
                "synthetic signal detected",
            )
            .with_pane_id(first_pane_id),
        );
        input.causal_nodes.push(
            CausalGraphNode::new(
                "synthetic-policy",
                CausalNodeKind::PolicyDecision,
                first_event.occurred_at_ms + 300,
                "synthetic observe-only decision",
            )
            .with_pane_id(first_pane_id),
        );
        input.causal_edges.push(
            CausalGraphEdge::new(
                "synthetic-signal",
                "synthetic-policy",
                crate::explainability_console::CausalEvidenceKind::Policy,
                10_000,
                "synthetic fixture causality",
            )
            .with_description("signal event directly precedes policy marker"),
        );
        input
            .hypotheses
            .push(IncidentRootCauseHypothesis::evidence_cited(
                "h-synthetic-pattern",
                "Synthetic fixture recovered from a known pattern-triggered incident.",
                8_800,
                vec![IncidentEvidenceCitation::new(
                    "pattern-hits",
                    "expected-rules",
                    "expected synthetic pattern hits",
                )],
            ));
        input.replay_commands.push(format!(
            "cargo test -p frankenterm-core --lib synthetic_swarm -- --nocapture # {}",
            scenario.manifest.scenario_id
        ));
        input
    }

    #[test]
    fn compiler_redacts_secrets_from_report_and_manifests() {
        let bundle = IncidentAutopsyCompiler::default().compile(input_with_secret(100));

        assert!(!bundle.report_markdown.contains(long_secret()));
        assert!(!bundle.manifest_json.contains(long_secret()));
        assert!(!bundle.manifest_toon.contains(long_secret()));
        assert!(bundle.report_markdown.contains(REDACTED_MARKER));
        assert!(
            bundle.included_artifacts[0]
                .redacted_excerpt
                .contains(REDACTED_MARKER)
        );
    }

    #[test]
    fn evidence_cited_hypothesis_without_citations_is_downgraded() {
        let mut input = IncidentAutopsyInput::new("session-b", 100);
        input.artifacts.push(IncidentSourceArtifact::included(
            "events",
            IncidentArtifactKind::PatternHits,
            "events",
            "event data",
        ));
        input
            .hypotheses
            .push(IncidentRootCauseHypothesis::evidence_cited(
                "h-missing",
                "Claim without evidence",
                7_000,
                Vec::new(),
            ));

        let bundle = IncidentAutopsyCompiler::default().compile(input);
        let hypothesis = &bundle.manifest.hypotheses[0];

        assert_eq!(hypothesis.class, IncidentHypothesisClass::Inferred);
        assert!(
            hypothesis
                .notes
                .iter()
                .any(|note| note.contains("downgraded"))
        );
        assert!(
            bundle
                .compile_logs
                .iter()
                .any(|log| log.action == "hypothesis_downgraded")
        );
    }

    #[test]
    fn included_and_excluded_artifacts_are_logged_and_counted() {
        let mut input = IncidentAutopsyInput::new("session-c", 100);
        input.artifacts.push(IncidentSourceArtifact::included(
            "policy",
            IncidentArtifactKind::PolicyDenials,
            "policy denial",
            "deny send_text",
        ));
        input.artifacts.push(IncidentSourceArtifact::excluded(
            "storage",
            IncidentArtifactKind::StorageHealth,
            "storage health",
            "outside requested incident window",
        ));
        input.hypotheses.push(cited_hypothesis());

        let bundle = IncidentAutopsyCompiler::default().compile(input);

        assert_eq!(bundle.manifest.included_artifacts, 1);
        assert_eq!(bundle.manifest.excluded_artifacts, 1);
        assert_eq!(bundle.compile_logs.len(), 2);
        assert!(
            bundle
                .manifest
                .omissions
                .iter()
                .any(|omission| omission.contains("excluded"))
        );
    }

    #[test]
    fn reproducible_hash_ignores_generation_timestamp() {
        let first = IncidentAutopsyCompiler::default().compile(input_with_secret(100));
        let second = IncidentAutopsyCompiler::default().compile(input_with_secret(200));

        assert_eq!(
            first.manifest.reproducible_hash,
            second.manifest.reproducible_hash
        );
        assert_ne!(
            first.manifest.generated_at_ms,
            second.manifest.generated_at_ms
        );
    }

    #[test]
    fn missing_data_adds_unknown_hypothesis_and_omissions() {
        let input = IncidentAutopsyInput::new("session-d", 100);
        let bundle = IncidentAutopsyCompiler::default().compile(input);

        assert_eq!(bundle.manifest.hypotheses.len(), 1);
        assert_eq!(
            bundle.manifest.hypotheses[0].class,
            IncidentHypothesisClass::Unknown
        );
        assert!(
            bundle
                .manifest
                .omissions
                .iter()
                .any(|omission| omission.contains("no source artifacts"))
        );
        assert!(
            bundle
                .report_markdown
                .contains("No timeline evidence supplied.")
        );
    }

    #[test]
    fn causal_context_is_redacted_before_rendering() {
        let mut input = IncidentAutopsyInput::new("session-e", 100);
        input.artifacts.push(IncidentSourceArtifact::included(
            "graph",
            IncidentArtifactKind::CausalGraph,
            "graph",
            "graph content",
        ));
        input.causal_nodes.push(
            CausalGraphNode::new("node-a", CausalNodeKind::PolicyDecision, 10, long_secret())
                .with_context("token", long_secret()),
        );
        input.causal_nodes.push(CausalGraphNode::new(
            "node-b",
            CausalNodeKind::RecoveryAction,
            11,
            "retry",
        ));
        input.causal_edges.push(
            CausalGraphEdge::new(
                "node-a",
                "node-b",
                crate::explainability_console::CausalEvidenceKind::Policy,
                10_000,
                long_secret(),
            )
            .with_description(long_secret()),
        );
        input.hypotheses.push(cited_hypothesis());

        let bundle = IncidentAutopsyCompiler::default().compile(input);

        assert!(!bundle.report_markdown.contains(long_secret()));
        assert!(bundle.report_markdown.contains(REDACTED_MARKER));
        assert_eq!(bundle.manifest.causal_node_count, 2);
        assert_eq!(bundle.manifest.causal_edge_count, 1);
    }

    #[test]
    fn synthetic_swarm_replay_autopsy_has_stable_hash_and_sections() {
        let compiler = IncidentAutopsyCompiler::default();
        let first = compiler.compile(synthetic_swarm_autopsy_input(100));
        let second = compiler.compile(synthetic_swarm_autopsy_input(200));

        assert_eq!(
            first.manifest.reproducible_hash,
            second.manifest.reproducible_hash
        );
        assert_ne!(
            first.manifest.generated_at_ms,
            second.manifest.generated_at_ms
        );
        assert_eq!(first.manifest.included_artifacts, 2);
        assert_eq!(first.manifest.pane_excerpt_count, 1);
        assert!(first.manifest.causal_node_count >= 2);
        assert!(first.manifest.causal_edge_count >= 1);
        assert!(
            first
                .manifest
                .hypotheses
                .iter()
                .any(|hypothesis| hypothesis.class == IncidentHypothesisClass::EvidenceCited)
        );
        for section in [
            "# Swarm Incident Autopsy",
            "## Root Cause Hypotheses",
            "## Timeline",
            "## Evidence Artifacts",
            "## Pane Excerpts",
            "## Causal Graph",
            "## Replay",
        ] {
            assert!(
                first.report_markdown.contains(section),
                "missing report section {section:?}"
            );
        }
        assert!(first.manifest_toon.contains("incident_autopsy{"));
        assert!(first.manifest_json.contains("\"source_hashes\""));
    }
}
