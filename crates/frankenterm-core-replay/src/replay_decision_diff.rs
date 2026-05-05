//! Baseline-vs-candidate decision diff algorithm (ft-og6q6.5.2).
//!
//! Provides:
//! - [`DecisionDiff`] — Result of diffing two [`DecisionGraph`]s.
//! - [`Divergence`] — Single divergence with type, position, and root cause.
//! - [`RootCause`] — Machine-readable attribution for each divergence.
//! - [`DiffConfig`] — Configurable tolerance and equivalence settings.
//! - [`MissionCausalityDiff`] — Operator-facing causal summary for mission replay pairs.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use frankenterm_core_replay_types::replay_decision_graph::{
    DecisionGraph, DecisionNode, DecisionType,
};

// ============================================================================
// DivergenceType — kinds of difference
// ============================================================================

/// Type of divergence between baseline and candidate decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DivergenceType {
    /// Decision exists in candidate but not in baseline.
    Added,
    /// Decision exists in baseline but not in candidate.
    Removed,
    /// Same position but different output_hash.
    Modified,
    /// Same decision but shifted in time (within tolerance).
    Shifted,
}

// ============================================================================
// RootCause — attribution for divergence
// ============================================================================

/// Machine-readable root cause for a divergence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RootCause {
    /// Rule definition changed between baseline and candidate.
    RuleDefinitionChange {
        rule_id: String,
        baseline_hash: String,
        candidate_hash: String,
    },
    /// Input diverged from an upstream change.
    InputDivergence {
        upstream_rule_id: String,
        upstream_position: u64,
    },
    /// An override was applied in the candidate.
    OverrideApplied {
        rule_id: String,
        override_id: String,
    },
    /// Decision was added without a baseline counterpart.
    NewDecision { rule_id: String },
    /// Decision was removed without a candidate counterpart.
    DroppedDecision { rule_id: String },
    /// Timing shift (same logic, different scheduling).
    TimingShift {
        baseline_ms: u64,
        candidate_ms: u64,
        delta_ms: u64,
    },
    /// Unknown root cause.
    Unknown,
}

// ============================================================================
// EquivalenceLevel — strictness of equivalence checking
// ============================================================================

/// Equivalence level for diff comparison (from equivalence contract T0.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EquivalenceLevel {
    /// L0: Same event structure (types and counts match).
    L0,
    /// L1: Same decisions (output_hash matches, ignoring timing).
    L1,
    /// L2: Exact match (including timing).
    L2,
}

// ============================================================================
// Divergence — single difference between baseline and candidate
// ============================================================================

/// A single divergence between baseline and candidate graphs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Divergence {
    /// Position in the canonical ordering where divergence occurs.
    pub position: u64,
    /// Type of divergence.
    pub divergence_type: DivergenceType,
    /// Baseline node (if present).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_node: Option<DivergenceNode>,
    /// Candidate node (if present).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_node: Option<DivergenceNode>,
    /// Root cause attribution.
    pub root_cause: RootCause,
}

/// Lightweight reference to a node in a divergence (avoids cloning full node).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DivergenceNode {
    pub node_id: u64,
    pub rule_id: String,
    pub definition_hash: String,
    pub output_hash: String,
    pub timestamp_ms: u64,
    pub pane_id: u64,
}

impl DivergenceNode {
    fn from_decision_node(node: &DecisionNode) -> Self {
        Self {
            node_id: node.node_id,
            rule_id: node.rule_id.clone(),
            definition_hash: node.definition_hash.clone(),
            output_hash: node.output_hash.clone(),
            timestamp_ms: node.timestamp_ms,
            pane_id: node.pane_id,
        }
    }
}

// ============================================================================
// DiffSummary — aggregate counts
// ============================================================================

/// Aggregate summary of differences.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    /// Total baseline decisions.
    pub total_baseline: u64,
    /// Total candidate decisions.
    pub total_candidate: u64,
    /// Decisions that match exactly.
    pub unchanged: u64,
    /// Decisions added in candidate.
    pub added: u64,
    /// Decisions removed from baseline.
    pub removed: u64,
    /// Decisions with different output.
    pub modified: u64,
    /// Decisions with shifted timing.
    pub shifted: u64,
}

impl DiffSummary {
    /// Whether there are zero divergences.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0 && self.modified == 0 && self.shifted == 0
    }

    /// Total divergence count.
    #[must_use]
    pub fn total_divergences(&self) -> u64 {
        self.added + self.removed + self.modified + self.shifted
    }
}

// ============================================================================
// DiffConfig — configurable tolerance
// ============================================================================

/// Configuration for the diff algorithm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffConfig {
    /// Time tolerance for Shifted detection (default 100ms).
    pub time_tolerance_ms: u64,
    /// Whether to attribute root causes (can be disabled for speed).
    pub attribute_root_causes: bool,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            time_tolerance_ms: 100,
            attribute_root_causes: true,
        }
    }
}

// ============================================================================
// DecisionDiff — full diff result
// ============================================================================

/// Result of diffing two decision graphs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionDiff {
    /// All divergences found.
    pub divergences: Vec<Divergence>,
    /// Aggregate summary.
    pub summary: DiffSummary,
}

/// Key for matching decisions across graphs.
type MatchKey = (u64, u64, String); // (timestamp_ms, pane_id, rule_id)

/// Exact key including a stable ordinal for duplicate match keys.
type ExactKey = (u64, u64, String, usize); // (timestamp_ms, pane_id, rule_id, duplicate_ordinal)

/// Relaxed key ignoring timestamp (for shifted detection).
type RelaxedKey = (u64, String); // (pane_id, rule_id)

impl DecisionDiff {
    /// Diff two decision graphs.
    #[must_use]
    pub fn diff(baseline: &DecisionGraph, candidate: &DecisionGraph, config: &DiffConfig) -> Self {
        let base_nodes = baseline.nodes_canonical();
        let cand_nodes = candidate.nodes_canonical();

        let base_indexed = indexed_exact_nodes(&base_nodes);
        let cand_indexed = indexed_exact_nodes(&cand_nodes);
        let cand_exact: BTreeMap<ExactKey, &DecisionNode> = cand_indexed
            .iter()
            .map(|(key, node)| (key.clone(), *node))
            .collect();

        // Build relaxed map for shifted detection.
        let mut cand_relaxed: BTreeMap<RelaxedKey, Vec<(ExactKey, &DecisionNode)>> =
            BTreeMap::new();
        for (exact_key, node) in &cand_indexed {
            let key = (node.pane_id, node.rule_id.clone());
            cand_relaxed
                .entry(key)
                .or_default()
                .push((exact_key.clone(), *node));
        }

        let mut divergences = Vec::new();
        let mut unchanged = 0u64;
        let mut added = 0u64;
        let mut removed = 0u64;
        let mut modified = 0u64;
        let mut shifted = 0u64;
        let mut position = 0u64;

        // Track which candidate nodes we've matched.
        let mut matched_cand: std::collections::BTreeSet<ExactKey> =
            std::collections::BTreeSet::new();

        // Pass 1: Iterate baseline nodes, find matches in candidate.
        for (exact_key, node) in &base_indexed {
            if let Some(cand_node) = cand_exact.get(exact_key) {
                // Exact match on key.
                matched_cand.insert(exact_key.clone());
                if node.output_hash == cand_node.output_hash {
                    // Unchanged.
                    unchanged += 1;
                } else {
                    // Modified: same position, different output.
                    let root_cause = if config.attribute_root_causes {
                        attribute_modified(node, cand_node)
                    } else {
                        RootCause::Unknown
                    };
                    divergences.push(Divergence {
                        position,
                        divergence_type: DivergenceType::Modified,
                        baseline_node: Some(DivergenceNode::from_decision_node(node)),
                        candidate_node: Some(DivergenceNode::from_decision_node(cand_node)),
                        root_cause,
                    });
                    modified += 1;
                }
            } else {
                // Not exact match. Check for shifted.
                let relaxed_key = (node.pane_id, node.rule_id.clone());
                let found_shifted = if let Some(cand_list) = cand_relaxed.get(&relaxed_key) {
                    cand_list.iter().find(|(cand_key, cn)| {
                        let delta = cn.timestamp_ms.abs_diff(node.timestamp_ms);
                        delta <= config.time_tolerance_ms
                            && delta > 0
                            && !matched_cand.contains(cand_key)
                    })
                } else {
                    None
                };

                if let Some((shifted_key, shifted_node)) = found_shifted {
                    matched_cand.insert(shifted_key.clone());
                    let delta = shifted_node.timestamp_ms.abs_diff(node.timestamp_ms);
                    divergences.push(Divergence {
                        position,
                        divergence_type: DivergenceType::Shifted,
                        baseline_node: Some(DivergenceNode::from_decision_node(node)),
                        candidate_node: Some(DivergenceNode::from_decision_node(shifted_node)),
                        root_cause: RootCause::TimingShift {
                            baseline_ms: node.timestamp_ms,
                            candidate_ms: shifted_node.timestamp_ms,
                            delta_ms: delta,
                        },
                    });
                    shifted += 1;
                } else {
                    // Removed.
                    divergences.push(Divergence {
                        position,
                        divergence_type: DivergenceType::Removed,
                        baseline_node: Some(DivergenceNode::from_decision_node(node)),
                        candidate_node: None,
                        root_cause: RootCause::DroppedDecision {
                            rule_id: node.rule_id.clone(),
                        },
                    });
                    removed += 1;
                }
            }
            position += 1;
        }

        // Pass 2: Find added candidate nodes (not matched).
        for (exact_key, node) in &cand_indexed {
            if !matched_cand.contains(exact_key) {
                divergences.push(Divergence {
                    position,
                    divergence_type: DivergenceType::Added,
                    baseline_node: None,
                    candidate_node: Some(DivergenceNode::from_decision_node(node)),
                    root_cause: RootCause::NewDecision {
                        rule_id: node.rule_id.clone(),
                    },
                });
                added += 1;
                position += 1;
            }
        }

        let summary = DiffSummary {
            total_baseline: base_nodes.len() as u64,
            total_candidate: cand_nodes.len() as u64,
            unchanged,
            added,
            removed,
            modified,
            shifted,
        };

        Self {
            divergences,
            summary,
        }
    }

    /// Check equivalence at a given level.
    #[must_use]
    pub fn is_equivalent(&self, level: EquivalenceLevel) -> bool {
        match level {
            EquivalenceLevel::L0 => {
                // Same event structure: no added or removed.
                self.summary.added == 0 && self.summary.removed == 0
            }
            EquivalenceLevel::L1 => {
                // Same decisions: no added, removed, or modified.
                self.summary.added == 0 && self.summary.removed == 0 && self.summary.modified == 0
            }
            EquivalenceLevel::L2 => {
                // Exact match: no divergences at all.
                self.summary.is_empty()
            }
        }
    }

    /// Serialize to JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Build a mission-level causal explanation from this low-level diff.
    ///
    /// The returned structure is intentionally scrubbed: it cites stable node
    /// identifiers, rule IDs, hashes, decision kinds, and pane IDs, but omits
    /// wall-clock times, replay run IDs, paths, worker IDs, and raw inputs.
    #[must_use]
    pub fn to_mission_causality_diff(
        &self,
        baseline: &DecisionGraph,
        candidate: &DecisionGraph,
    ) -> MissionCausalityDiff {
        MissionCausalityDiff::from_decision_diff(self, baseline, candidate)
    }
}

// ============================================================================
// MissionCausalityDiff — operator-facing replay explanation
// ============================================================================

/// Top-level category for a mission replay divergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MissionCausalityCategory {
    /// The compared mission replays are equivalent at the configured level.
    Identical,
    /// A policy allow/deny/approval decision diverged.
    PolicyDecision,
    /// Resource pressure, admission, backpressure, or scheduling pressure diverged.
    ResourcePressure,
    /// Agent ownership, assignment, placement, or liveness diverged.
    AgentAssignment,
    /// Compensation, rollback, or recovery path diverged.
    CompensationPath,
    /// A rule definition changed.
    RuleDefinition,
    /// A stable input hash changed.
    InputDivergence,
    /// The same decision shifted in virtual time.
    Timing,
    /// The candidate introduced a decision.
    NewDecision,
    /// The candidate dropped a baseline decision.
    DroppedDecision,
    /// The diff could not classify the causal category.
    Unknown,
}

/// Stable, scrubbed summary of a decision node for mission replay diffs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionDecisionSummary {
    pub node_id: u64,
    pub decision_type: DecisionType,
    pub rule_id: String,
    pub pane_id: u64,
    pub input_hash: String,
    pub output_hash: String,
}

impl MissionDecisionSummary {
    fn from_node(node: &DecisionNode) -> Self {
        Self {
            node_id: node.node_id,
            decision_type: node.decision_type,
            rule_id: node.rule_id.clone(),
            pane_id: node.pane_id,
            input_hash: node.input_hash.clone(),
            output_hash: node.output_hash.clone(),
        }
    }
}

/// First causal difference between two mission replay graphs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionCausalDivergence {
    pub position: u64,
    pub category: MissionCausalityCategory,
    pub divergence_type: DivergenceType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<MissionDecisionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<MissionDecisionSummary>,
    pub root_cause: RootCause,
    pub explanation: String,
    pub evidence_refs: Vec<String>,
}

/// Causal chain around a divergence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionReasonChain {
    pub divergence: MissionCausalDivergence,
    pub upstream_chain: Vec<MissionDecisionSummary>,
    pub downstream_effects: Vec<MissionDecisionSummary>,
}

/// Aggregate mission replay causality summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionCausalitySummary {
    pub identical: bool,
    pub total_divergences: u64,
    pub first_category: MissionCausalityCategory,
    pub affected_decision_count: u64,
}

/// Artifact contract for chaos/recovery labs attaching replay pairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionReplayAttachmentContract {
    pub baseline_artifact_label: String,
    pub candidate_artifact_label: String,
    pub causality_diff_artifact_label: String,
    pub required_scrubbed_fields: Vec<String>,
    pub proof_command_hint: String,
}

impl Default for MissionReplayAttachmentContract {
    fn default() -> Self {
        Self {
            baseline_artifact_label: "baseline_replay_graph.json".into(),
            candidate_artifact_label: "candidate_replay_graph.json".into(),
            causality_diff_artifact_label: "mission_causality_diff.json".into(),
            required_scrubbed_fields: vec![
                "paths".into(),
                "tokens".into(),
                "pane_uuids".into(),
                "wall_clock_timestamps".into(),
                "worker_ids".into(),
                "replay_run_ids".into(),
            ],
            proof_command_hint:
                "rch exec -- bash -lc 'CARGO_TARGET_DIR=/tmp/ft-bsfb9-9-mission-diff cargo test -p frankenterm-core-replay --lib replay_decision_diff::tests::mission -- --nocapture'"
                    .into(),
        }
    }
}

/// Operator-facing causal diff for two mission replay graphs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionCausalityDiff {
    pub summary: MissionCausalitySummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_divergence: Option<MissionCausalDivergence>,
    pub reason_chains: Vec<MissionReasonChain>,
    pub attachment_contract: MissionReplayAttachmentContract,
}

impl MissionCausalityDiff {
    /// Diff two mission replay graphs and produce a scrubbed causal summary.
    #[must_use]
    pub fn from_graphs(
        baseline: &DecisionGraph,
        candidate: &DecisionGraph,
        config: &DiffConfig,
    ) -> Self {
        let diff = DecisionDiff::diff(baseline, candidate, config);
        Self::from_decision_diff(&diff, baseline, candidate)
    }

    /// Build a scrubbed causal summary from an existing decision diff.
    #[must_use]
    pub fn from_decision_diff(
        diff: &DecisionDiff,
        baseline: &DecisionGraph,
        candidate: &DecisionGraph,
    ) -> Self {
        let reason_chains: Vec<MissionReasonChain> = diff
            .divergences
            .iter()
            .map(|divergence| build_mission_reason_chain(divergence, baseline, candidate))
            .collect();
        let first_divergence = reason_chains.first().map(|chain| chain.divergence.clone());
        let first_category = first_divergence
            .as_ref()
            .map_or(MissionCausalityCategory::Identical, |divergence| {
                divergence.category
            });
        let summary = MissionCausalitySummary {
            identical: diff.summary.is_empty(),
            total_divergences: diff.summary.total_divergences(),
            first_category,
            affected_decision_count: affected_decision_count(&reason_chains),
        };

        Self {
            summary,
            first_divergence,
            reason_chains,
            attachment_contract: MissionReplayAttachmentContract::default(),
        }
    }

    /// Serialize the scrubbed causality diff to JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

fn build_mission_reason_chain(
    divergence: &Divergence,
    baseline: &DecisionGraph,
    candidate: &DecisionGraph,
) -> MissionReasonChain {
    let baseline_node = divergence
        .baseline_node
        .as_ref()
        .and_then(|node| baseline.get_node(node.node_id));
    let candidate_node = divergence
        .candidate_node
        .as_ref()
        .and_then(|node| candidate.get_node(node.node_id));
    let reference_graph = if candidate_node.is_some() {
        candidate
    } else {
        baseline
    };
    let reference_node = candidate_node.or(baseline_node);
    let category = classify_mission_category(divergence, baseline_node, candidate_node);
    let causal_divergence = MissionCausalDivergence {
        position: divergence.position,
        category,
        divergence_type: divergence.divergence_type,
        baseline: baseline_node.map(MissionDecisionSummary::from_node),
        candidate: candidate_node.map(MissionDecisionSummary::from_node),
        root_cause: divergence.root_cause.clone(),
        explanation: explain_mission_divergence(
            category,
            divergence,
            baseline_node,
            candidate_node,
        ),
        evidence_refs: mission_evidence_refs(divergence, baseline_node, candidate_node),
    };

    let upstream_chain = reference_node.map_or_else(Vec::new, |node| {
        reference_graph
            .causal_chain(node.node_id)
            .into_iter()
            .map(MissionDecisionSummary::from_node)
            .collect()
    });
    let downstream_effects = reference_node.map_or_else(Vec::new, |node| {
        reference_graph
            .effects(node.node_id)
            .into_iter()
            .map(MissionDecisionSummary::from_node)
            .collect()
    });

    MissionReasonChain {
        divergence: causal_divergence,
        upstream_chain,
        downstream_effects,
    }
}

fn classify_mission_category(
    divergence: &Divergence,
    baseline: Option<&DecisionNode>,
    candidate: Option<&DecisionNode>,
) -> MissionCausalityCategory {
    let text = mission_classification_text(divergence, baseline, candidate);
    if text.contains("resource")
        || text.contains("pressure")
        || text.contains("backpressure")
        || text.contains("admission")
    {
        MissionCausalityCategory::ResourcePressure
    } else if text.contains("agent")
        || text.contains("assignment")
        || text.contains("owner")
        || text.contains("placement")
        || text.contains("liveness")
    {
        MissionCausalityCategory::AgentAssignment
    } else if text.contains("compensat")
        || text.contains("rollback")
        || text.contains("recovery")
        || text.contains("repair")
    {
        MissionCausalityCategory::CompensationPath
    } else if baseline
        .or(candidate)
        .is_some_and(|node| node.decision_type == DecisionType::PolicyDecision)
        || text.contains("policy")
        || text.contains("approval")
    {
        MissionCausalityCategory::PolicyDecision
    } else {
        match divergence.root_cause {
            RootCause::RuleDefinitionChange { .. } => MissionCausalityCategory::RuleDefinition,
            RootCause::InputDivergence { .. } => MissionCausalityCategory::InputDivergence,
            RootCause::TimingShift { .. } => MissionCausalityCategory::Timing,
            RootCause::NewDecision { .. } => MissionCausalityCategory::NewDecision,
            RootCause::DroppedDecision { .. } => MissionCausalityCategory::DroppedDecision,
            RootCause::OverrideApplied { .. } | RootCause::Unknown => {
                MissionCausalityCategory::Unknown
            }
        }
    }
}

fn mission_classification_text(
    divergence: &Divergence,
    baseline: Option<&DecisionNode>,
    candidate: Option<&DecisionNode>,
) -> String {
    let mut parts = Vec::new();
    for node in [baseline, candidate].into_iter().flatten() {
        parts.push(node.rule_id.to_ascii_lowercase());
        parts.push(format!("{:?}", node.decision_type).to_ascii_lowercase());
    }
    match &divergence.root_cause {
        RootCause::RuleDefinitionChange { rule_id, .. }
        | RootCause::NewDecision { rule_id }
        | RootCause::DroppedDecision { rule_id } => parts.push(rule_id.to_ascii_lowercase()),
        RootCause::InputDivergence {
            upstream_rule_id, ..
        } => parts.push(upstream_rule_id.to_ascii_lowercase()),
        RootCause::OverrideApplied {
            rule_id,
            override_id,
        } => {
            parts.push(rule_id.to_ascii_lowercase());
            parts.push(override_id.to_ascii_lowercase());
        }
        RootCause::TimingShift { .. } | RootCause::Unknown => {}
    }
    parts.join(" ")
}

fn explain_mission_divergence(
    category: MissionCausalityCategory,
    divergence: &Divergence,
    baseline: Option<&DecisionNode>,
    candidate: Option<&DecisionNode>,
) -> String {
    let rule_id = candidate
        .or(baseline)
        .map_or("unknown", |node| node.rule_id.as_str());
    format!(
        "first {:?} divergence at canonical position {} classified as {:?} for rule {}",
        divergence.divergence_type, divergence.position, category, rule_id
    )
}

fn mission_evidence_refs(
    divergence: &Divergence,
    baseline: Option<&DecisionNode>,
    candidate: Option<&DecisionNode>,
) -> Vec<String> {
    let mut refs = vec![
        format!("divergence_position:{}", divergence.position),
        format!("divergence_type:{:?}", divergence.divergence_type),
        format!("root_cause:{}", root_cause_label(&divergence.root_cause)),
    ];
    if let Some(node) = baseline {
        refs.push(format!("baseline_node:{}", node.node_id));
    }
    if let Some(node) = candidate {
        refs.push(format!("candidate_node:{}", node.node_id));
    }
    refs
}

fn root_cause_label(root_cause: &RootCause) -> &'static str {
    match root_cause {
        RootCause::RuleDefinitionChange { .. } => "rule_definition_change",
        RootCause::InputDivergence { .. } => "input_divergence",
        RootCause::OverrideApplied { .. } => "override_applied",
        RootCause::NewDecision { .. } => "new_decision",
        RootCause::DroppedDecision { .. } => "dropped_decision",
        RootCause::TimingShift { .. } => "timing_shift",
        RootCause::Unknown => "unknown",
    }
}

fn affected_decision_count(reason_chains: &[MissionReasonChain]) -> u64 {
    let mut affected = BTreeSet::new();
    for chain in reason_chains {
        if let Some(node) = &chain.divergence.baseline {
            affected.insert(("baseline", node.node_id));
        }
        if let Some(node) = &chain.divergence.candidate {
            affected.insert(("candidate", node.node_id));
        }
        for node in &chain.upstream_chain {
            affected.insert(("upstream", node.node_id));
        }
        for node in &chain.downstream_effects {
            affected.insert(("downstream", node.node_id));
        }
    }
    affected.len() as u64
}

fn indexed_exact_nodes<'a>(nodes: &[&'a DecisionNode]) -> Vec<(ExactKey, &'a DecisionNode)> {
    let mut duplicate_ordinals: BTreeMap<MatchKey, usize> = BTreeMap::new();
    nodes
        .iter()
        .map(|node| {
            let match_key = (node.timestamp_ms, node.pane_id, node.rule_id.clone());
            let ordinal = duplicate_ordinals.entry(match_key.clone()).or_default();
            let exact_key = (match_key.0, match_key.1, match_key.2, *ordinal);
            *ordinal += 1;
            (exact_key, *node)
        })
        .collect()
}

/// Attribute root cause for a Modified divergence.
fn attribute_modified(baseline: &DecisionNode, candidate: &DecisionNode) -> RootCause {
    if baseline.definition_hash != candidate.definition_hash {
        RootCause::RuleDefinitionChange {
            rule_id: baseline.rule_id.clone(),
            baseline_hash: baseline.definition_hash.clone(),
            candidate_hash: candidate.definition_hash.clone(),
        }
    } else if baseline.input_hash != candidate.input_hash {
        RootCause::InputDivergence {
            upstream_rule_id: baseline.rule_id.clone(),
            upstream_position: baseline.node_id,
        }
    } else {
        RootCause::Unknown
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_core_replay_types::replay_decision_graph::{DecisionEvent, DecisionType};

    fn make_event(
        decision_type: DecisionType,
        rule_id: &str,
        timestamp_ms: u64,
        pane_id: u64,
        def_hash: &str,
        output_hash: &str,
    ) -> DecisionEvent {
        let input = format!("rule={rule_id};ts={timestamp_ms};pane={pane_id}");
        DecisionEvent::new(
            decision_type,
            pane_id,
            rule_id,
            def_hash,
            &input,
            serde_json::Value::String(output_hash.into()),
            None,
            Some(1.0),
            timestamp_ms,
        )
    }

    fn make_triggered_event(
        decision_type: DecisionType,
        rule_id: &str,
        timestamp_ms: u64,
        pane_id: u64,
        def_hash: &str,
        output_hash: &str,
        triggered_by: Option<u64>,
    ) -> DecisionEvent {
        let mut event = make_event(
            decision_type,
            rule_id,
            timestamp_ms,
            pane_id,
            def_hash,
            output_hash,
        );
        event.triggered_by = triggered_by;
        event
    }

    fn config() -> DiffConfig {
        DiffConfig::default()
    }

    // ── Identical graphs ───────────────────────────────────────────────

    #[test]
    fn diff_identical_empty() {
        let base = DecisionGraph::from_decisions(&[]);
        let cand = DecisionGraph::from_decisions(&[]);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert!(diff.divergences.is_empty());
        assert!(diff.summary.is_empty());
        assert!(diff.is_equivalent(EquivalenceLevel::L2));
    }

    #[test]
    fn diff_identical_nonempty() {
        let events = vec![
            make_event(DecisionType::PatternMatch, "r1", 100, 1, "def1", "out1"),
            make_event(DecisionType::WorkflowStep, "w1", 200, 1, "def2", "out2"),
        ];
        let base = DecisionGraph::from_decisions(&events);
        let cand = DecisionGraph::from_decisions(&events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert!(diff.divergences.is_empty());
        assert_eq!(diff.summary.unchanged, 2);
        assert!(diff.is_equivalent(EquivalenceLevel::L2));
    }

    // ── Added decision ─────────────────────────────────────────────────

    #[test]
    fn diff_detects_added() {
        let base_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out1",
        )];
        let cand_events = vec![
            make_event(DecisionType::PatternMatch, "r1", 100, 1, "def1", "out1"),
            make_event(DecisionType::AlertFired, "a1", 200, 1, "def_a", "out_a"),
        ];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert_eq!(diff.summary.added, 1);
        assert_eq!(diff.summary.unchanged, 1);
        assert!(!diff.is_equivalent(EquivalenceLevel::L0));
    }

    // ── Removed decision ───────────────────────────────────────────────

    #[test]
    fn diff_detects_removed() {
        let base_events = vec![
            make_event(DecisionType::PatternMatch, "r1", 100, 1, "def1", "out1"),
            make_event(DecisionType::AlertFired, "a1", 200, 1, "def_a", "out_a"),
        ];
        let cand_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out1",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert_eq!(diff.summary.removed, 1);
        assert!(!diff.is_equivalent(EquivalenceLevel::L0));
    }

    // ── Modified decision ──────────────────────────────────────────────

    #[test]
    fn diff_detects_modified() {
        let base_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out1",
        )];
        let cand_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out_DIFFERENT",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert_eq!(diff.summary.modified, 1);
        assert!(diff.is_equivalent(EquivalenceLevel::L0));
        assert!(!diff.is_equivalent(EquivalenceLevel::L1));
    }

    #[test]
    fn diff_preserve_duplicates_same_timestamp_pane_rule() {
        let base_events = vec![
            make_event(
                DecisionType::PatternMatch,
                "r1",
                100,
                1,
                "def1",
                "out_first",
            ),
            make_event(
                DecisionType::PatternMatch,
                "r1",
                100,
                1,
                "def1",
                "out_second",
            ),
        ];
        let cand_events = vec![
            make_event(
                DecisionType::PatternMatch,
                "r1",
                100,
                1,
                "def1",
                "out_first",
            ),
            make_event(
                DecisionType::PatternMatch,
                "r1",
                100,
                1,
                "def1",
                "out_second_changed",
            ),
        ];

        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());

        assert_eq!(diff.summary.total_baseline, 2);
        assert_eq!(diff.summary.total_candidate, 2);
        assert_eq!(diff.summary.unchanged, 1);
        assert_eq!(diff.summary.modified, 1);
        assert_eq!(diff.summary.added, 0);
        assert_eq!(diff.summary.removed, 0);
        assert_eq!(diff.divergences.len(), 1);
        assert_eq!(diff.divergences[0].position, 1);
        assert_ne!(
            diff.divergences[0]
                .baseline_node
                .as_ref()
                .expect("modified divergence has baseline")
                .output_hash,
            diff.divergences[0]
                .candidate_node
                .as_ref()
                .expect("modified divergence has candidate")
                .output_hash
        );
    }

    // ── Shifted decision ───────────────────────────────────────────────

    #[test]
    fn diff_detects_shifted() {
        let base_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out1",
        )];
        let cand_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            150,
            1,
            "def1",
            "out1",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert_eq!(diff.summary.shifted, 1);
        assert!(diff.is_equivalent(EquivalenceLevel::L1));
        assert!(!diff.is_equivalent(EquivalenceLevel::L2));
    }

    #[test]
    fn shifted_beyond_tolerance_is_removed_added() {
        let base_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out1",
        )];
        let cand_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            300,
            1,
            "def1",
            "out1",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        // 200ms shift > 100ms tolerance → not shifted, instead removed + added.
        assert_eq!(diff.summary.shifted, 0);
        assert_eq!(diff.summary.removed, 1);
        assert_eq!(diff.summary.added, 1);
    }

    // ── Root cause: rule definition change ──────────────────────────────

    #[test]
    fn root_cause_definition_change() {
        let base_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def_v1",
            "out1",
        )];
        let cand_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def_v2",
            "out2",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert_eq!(diff.divergences.len(), 1);
        let is_def_change = matches!(
            &diff.divergences[0].root_cause,
            RootCause::RuleDefinitionChange { rule_id, .. } if rule_id == "r1"
        );
        assert!(is_def_change);
    }

    // ── Root cause: input divergence ────────────────────────────────────

    #[test]
    fn root_cause_input_divergence() {
        let base_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out1",
        )];
        let mut cand_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out_different",
        )];
        // Same definition hash but different input hash → input divergence.
        cand_events[0].input_hash = "in_different".into();
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        let is_input_div = matches!(
            &diff.divergences[0].root_cause,
            RootCause::InputDivergence { .. }
        );
        assert!(is_input_div);
    }

    // ── is_equivalent levels ───────────────────────────────────────────

    #[test]
    fn l1_true_when_only_timing_differs() {
        let base_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out1",
        )];
        let cand_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            150,
            1,
            "def1",
            "out1",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert!(diff.is_equivalent(EquivalenceLevel::L1));
    }

    #[test]
    fn l0_true_when_structure_matches() {
        let base_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out1",
        )];
        let cand_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out_different",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert!(diff.is_equivalent(EquivalenceLevel::L0));
    }

    // ── DiffSummary counts ─────────────────────────────────────────────

    #[test]
    fn summary_counts_correct() {
        let base_events = vec![
            make_event(DecisionType::PatternMatch, "r1", 100, 1, "def1", "out1"),
            make_event(DecisionType::WorkflowStep, "w1", 200, 1, "def2", "out2"),
            make_event(DecisionType::AlertFired, "a1", 300, 1, "def3", "out3"),
        ];
        let cand_events = vec![
            make_event(DecisionType::PatternMatch, "r1", 100, 1, "def1", "out1"), // unchanged
            make_event(DecisionType::WorkflowStep, "w1", 200, 1, "def2", "out_mod"), // modified
            // a1 removed, new added:
            make_event(DecisionType::PolicyDecision, "p1", 400, 1, "def4", "out4"),
        ];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert_eq!(diff.summary.total_baseline, 3);
        assert_eq!(diff.summary.total_candidate, 3);
        assert_eq!(diff.summary.unchanged, 1);
        assert_eq!(diff.summary.modified, 1);
        assert_eq!(diff.summary.removed, 1);
        assert_eq!(diff.summary.added, 1);
    }

    // ── Config: no attribution ─────────────────────────────────────────

    #[test]
    fn no_attribution_config() {
        let base_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def_v1",
            "out1",
        )];
        let cand_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def_v2",
            "out2",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let cfg = DiffConfig {
            attribute_root_causes: false,
            ..DiffConfig::default()
        };
        let diff = DecisionDiff::diff(&base, &cand, &cfg);
        let is_unknown = matches!(&diff.divergences[0].root_cause, RootCause::Unknown);
        assert!(is_unknown);
    }

    // ── Config: custom tolerance ───────────────────────────────────────

    #[test]
    fn custom_tolerance() {
        let base_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out1",
        )];
        let cand_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            250,
            1,
            "def1",
            "out1",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        // Default tolerance (100ms): 150ms shift → removed + added.
        let diff_default = DecisionDiff::diff(&base, &cand, &config());
        assert_eq!(diff_default.summary.shifted, 0);
        // Wide tolerance (200ms): 150ms shift → shifted.
        let cfg = DiffConfig {
            time_tolerance_ms: 200,
            ..DiffConfig::default()
        };
        let diff_wide = DecisionDiff::diff(&base, &cand, &cfg);
        assert_eq!(diff_wide.summary.shifted, 1);
    }

    // ── JSON roundtrip ─────────────────────────────────────────────────

    #[test]
    fn diff_json_roundtrip() {
        let base_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out1",
        )];
        let cand_events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out2",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        let json = diff.to_json();
        let restored: DecisionDiff = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.summary.modified, 1);
    }

    // ── Mission causality diff ─────────────────────────────────────────

    #[test]
    fn mission_causality_identical_replay_has_attachment_contract() {
        let events = vec![
            make_event(
                DecisionType::PatternMatch,
                "usage.limit.detected",
                100,
                1,
                "usage_v1",
                "matched",
            ),
            make_triggered_event(
                DecisionType::PolicyDecision,
                "policy.approval_required",
                200,
                1,
                "policy_v1",
                "allow",
                Some(0),
            ),
        ];
        let base = DecisionGraph::from_decisions(&events);
        let cand = DecisionGraph::from_decisions(&events);
        let mission = MissionCausalityDiff::from_graphs(&base, &cand, &config());

        assert!(mission.summary.identical);
        assert_eq!(mission.summary.total_divergences, 0);
        assert_eq!(
            mission.summary.first_category,
            MissionCausalityCategory::Identical
        );
        assert!(mission.first_divergence.is_none());
        assert!(
            mission
                .attachment_contract
                .required_scrubbed_fields
                .contains(&"replay_run_ids".to_string())
        );
    }

    #[test]
    fn mission_causality_names_single_policy_decision_divergence() {
        let base_events = vec![
            make_event(
                DecisionType::PatternMatch,
                "usage.limit.detected",
                100,
                42,
                "usage_v1",
                "matched",
            ),
            make_triggered_event(
                DecisionType::PolicyDecision,
                "policy.approval_required",
                200,
                42,
                "policy_v1",
                "allow",
                Some(0),
            ),
            make_triggered_event(
                DecisionType::WorkflowStep,
                "workflow.continue",
                300,
                42,
                "workflow_v1",
                "continued",
                Some(1),
            ),
        ];
        let cand_events = vec![
            make_event(
                DecisionType::PatternMatch,
                "usage.limit.detected",
                100,
                42,
                "usage_v1",
                "matched",
            ),
            make_triggered_event(
                DecisionType::PolicyDecision,
                "policy.approval_required",
                200,
                42,
                "policy_v1",
                "require_approval",
                Some(0),
            ),
            make_triggered_event(
                DecisionType::WorkflowStep,
                "workflow.pause_for_approval",
                300,
                42,
                "workflow_v1",
                "paused",
                Some(1),
            ),
        ];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let mission = MissionCausalityDiff::from_graphs(&base, &cand, &config());

        let first = mission
            .first_divergence
            .as_ref()
            .expect("policy divergence is present");
        assert_eq!(first.position, 1);
        assert_eq!(first.category, MissionCausalityCategory::PolicyDecision);
        assert!(first.explanation.contains("canonical position 1"));
        assert!(
            first
                .evidence_refs
                .iter()
                .any(|evidence| evidence == "candidate_node:1")
        );
        let chain = &mission.reason_chains[0];
        assert!(
            chain
                .upstream_chain
                .iter()
                .any(|node| node.rule_id == "usage.limit.detected")
        );
        assert!(
            chain
                .downstream_effects
                .iter()
                .any(|node| node.rule_id == "workflow.pause_for_approval")
        );
    }

    #[test]
    fn mission_causality_classifies_resource_pressure_divergence() {
        let base_events = vec![make_event(
            DecisionType::PolicyDecision,
            "resource.pressure.admission",
            100,
            7,
            "resource_v1",
            "admit",
        )];
        let cand_events = vec![make_event(
            DecisionType::PolicyDecision,
            "resource.pressure.admission",
            100,
            7,
            "resource_v1",
            "degrade_capture_tier",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let mission = MissionCausalityDiff::from_graphs(&base, &cand, &config());

        assert_eq!(
            mission.summary.first_category,
            MissionCausalityCategory::ResourcePressure
        );
        assert_eq!(
            mission.reason_chains[0].divergence.category,
            MissionCausalityCategory::ResourcePressure
        );
    }

    #[test]
    fn mission_causality_classifies_agent_assignment_divergence() {
        let base_events = vec![make_event(
            DecisionType::WorkflowStep,
            "agent.assignment.rebalance",
            100,
            8,
            "assign_v1",
            "agent_a",
        )];
        let cand_events = vec![make_event(
            DecisionType::WorkflowStep,
            "agent.assignment.rebalance",
            100,
            8,
            "assign_v1",
            "agent_b",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let mission = MissionCausalityDiff::from_graphs(&base, &cand, &config());

        assert_eq!(
            mission.summary.first_category,
            MissionCausalityCategory::AgentAssignment
        );
    }

    #[test]
    fn mission_causality_classifies_compensation_path_divergence() {
        let base_events = vec![make_event(
            DecisionType::WorkflowStep,
            "compensation.rollback.step",
            100,
            9,
            "comp_v1",
            "skip",
        )];
        let cand_events = vec![make_event(
            DecisionType::WorkflowStep,
            "compensation.rollback.step",
            100,
            9,
            "comp_v1",
            "rollback",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let mission = MissionCausalityDiff::from_graphs(&base, &cand, &config());

        assert_eq!(
            mission.summary.first_category,
            MissionCausalityCategory::CompensationPath
        );
    }

    #[test]
    fn mission_causality_json_is_stable_and_scrubbed() {
        let base_events = vec![make_event(
            DecisionType::PolicyDecision,
            "policy.approval_required",
            100,
            3,
            "policy_v1",
            "allow",
        )];
        let cand_events = vec![make_event(
            DecisionType::PolicyDecision,
            "policy.approval_required",
            100,
            3,
            "policy_v1",
            "deny",
        )];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let json = MissionCausalityDiff::from_graphs(&base, &cand, &config()).to_json();

        assert!(json.contains("policy.approval_required"));
        assert!(json.contains("baseline_replay_graph.json"));
        assert!(json.contains("mission_causality_diff.json"));
        assert!(!json.contains("wall_clock_ms"));
        assert!(!json.contains("timestamp_ms"));
        assert!(!json.contains("\"replay_run_id\""));
    }

    // ── Empty divergence list for identical ─────────────────────────────

    #[test]
    fn empty_divergence_list() {
        let events = vec![make_event(
            DecisionType::PatternMatch,
            "r1",
            100,
            1,
            "def1",
            "out1",
        )];
        let base = DecisionGraph::from_decisions(&events);
        let cand = DecisionGraph::from_decisions(&events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert!(diff.divergences.is_empty());
    }

    // ── Multiple panes ─────────────────────────────────────────────────

    #[test]
    fn multi_pane_diff() {
        let base_events = vec![
            make_event(DecisionType::PatternMatch, "r1", 100, 1, "def1", "out1"),
            make_event(DecisionType::PatternMatch, "r1", 100, 2, "def1", "out1"),
        ];
        let cand_events = vec![
            make_event(DecisionType::PatternMatch, "r1", 100, 1, "def1", "out1"),
            make_event(DecisionType::PatternMatch, "r1", 100, 2, "def1", "out_mod"),
        ];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert_eq!(diff.summary.unchanged, 1);
        assert_eq!(diff.summary.modified, 1);
    }

    // ── Serde roundtrips on types ──────────────────────────────────────

    #[test]
    fn divergence_type_serde() {
        let dt = DivergenceType::Modified;
        let json = serde_json::to_string(&dt).unwrap();
        let restored: DivergenceType = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, dt);
    }

    #[test]
    fn root_cause_serde() {
        let rc = RootCause::RuleDefinitionChange {
            rule_id: "r1".into(),
            baseline_hash: "h1".into(),
            candidate_hash: "h2".into(),
        };
        let json = serde_json::to_string(&rc).unwrap();
        let restored: RootCause = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, rc);
    }

    #[test]
    fn equivalence_level_serde() {
        let el = EquivalenceLevel::L1;
        let json = serde_json::to_string(&el).unwrap();
        let restored: EquivalenceLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, el);
    }

    #[test]
    fn diff_config_serde() {
        let cfg = DiffConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: DiffConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.time_tolerance_ms, 100);
    }

    #[test]
    fn diff_summary_serde() {
        let summary = DiffSummary {
            total_baseline: 10,
            total_candidate: 12,
            unchanged: 8,
            added: 2,
            removed: 0,
            modified: 1,
            shifted: 1,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let restored: DiffSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, summary);
    }

    // ── Total divergences ──────────────────────────────────────────────

    #[test]
    fn total_divergences_method() {
        let summary = DiffSummary {
            total_baseline: 10,
            total_candidate: 10,
            unchanged: 5,
            added: 1,
            removed: 1,
            modified: 2,
            shifted: 1,
        };
        assert_eq!(summary.total_divergences(), 5);
    }

    // ── Divergence positions are sequential ────────────────────────────

    #[test]
    fn divergence_positions() {
        let base_events = vec![
            make_event(DecisionType::PatternMatch, "r1", 100, 1, "def_v1", "out1"),
            make_event(DecisionType::WorkflowStep, "w1", 200, 1, "def_v1", "out2"),
        ];
        let cand_events = vec![
            make_event(
                DecisionType::PatternMatch,
                "r1",
                100,
                1,
                "def_v2",
                "out_mod",
            ),
            make_event(
                DecisionType::WorkflowStep,
                "w1",
                200,
                1,
                "def_v2",
                "out_mod2",
            ),
        ];
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert_eq!(diff.divergences.len(), 2);
        assert_eq!(diff.divergences[0].position, 0);
        assert_eq!(diff.divergences[1].position, 1);
    }

    // ── Default config values ──────────────────────────────────────────

    #[test]
    fn default_config() {
        let cfg = DiffConfig::default();
        assert_eq!(cfg.time_tolerance_ms, 100);
        assert!(cfg.attribute_root_causes);
    }

    // ── Large diff ─────────────────────────────────────────────────────

    #[test]
    fn large_diff_completes() {
        let base_events: Vec<DecisionEvent> = (0..500)
            .map(|i| {
                make_event(
                    DecisionType::PatternMatch,
                    &format!("r_{}", i),
                    i * 10,
                    i % 5,
                    "def",
                    "out",
                )
            })
            .collect();
        let cand_events: Vec<DecisionEvent> = (0..500)
            .map(|i| {
                make_event(
                    DecisionType::PatternMatch,
                    &format!("r_{}", i),
                    i * 10,
                    i % 5,
                    "def",
                    "out_mod",
                )
            })
            .collect();
        let base = DecisionGraph::from_decisions(&base_events);
        let cand = DecisionGraph::from_decisions(&cand_events);
        let diff = DecisionDiff::diff(&base, &cand, &config());
        assert_eq!(diff.summary.modified, 500);
    }
}
