//! Baseline-vs-candidate decision diff algorithm (ft-og6q6.5.2).
//!
//! Provides:
//! - [`DecisionDiff`] — Result of diffing two [`DecisionGraph`]s.
//! - [`Divergence`] — Single divergence with type, position, and root cause.
//! - [`RootCause`] — Machine-readable attribution for each divergence.
//! - [`DiffConfig`] — Configurable tolerance and equivalence settings.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::replay_decision_graph::{DecisionGraph, DecisionNode};

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

/// Relaxed key ignoring timestamp (for shifted detection).
type RelaxedKey = (u64, String); // (pane_id, rule_id)

impl DecisionDiff {
    /// Diff two decision graphs.
    #[must_use]
    pub fn diff(baseline: &DecisionGraph, candidate: &DecisionGraph, config: &DiffConfig) -> Self {
        let base_nodes = baseline.nodes_canonical();
        let cand_nodes = candidate.nodes_canonical();

        // Build match maps: exact key -> node.
        let mut base_exact: BTreeMap<MatchKey, &DecisionNode> = BTreeMap::new();
        for node in &base_nodes {
            let key = (node.timestamp_ms, node.pane_id, node.rule_id.clone());
            base_exact.insert(key, node);
        }

        let mut cand_exact: BTreeMap<MatchKey, &DecisionNode> = BTreeMap::new();
        for node in &cand_nodes {
            let key = (node.timestamp_ms, node.pane_id, node.rule_id.clone());
            cand_exact.insert(key, node);
        }

        // Build relaxed maps for shifted detection.
        let mut base_relaxed: BTreeMap<RelaxedKey, Vec<&DecisionNode>> = BTreeMap::new();
        for node in &base_nodes {
            let key = (node.pane_id, node.rule_id.clone());
            base_relaxed.entry(key).or_default().push(node);
        }

        let mut cand_relaxed: BTreeMap<RelaxedKey, Vec<&DecisionNode>> = BTreeMap::new();
        for node in &cand_nodes {
            let key = (node.pane_id, node.rule_id.clone());
            cand_relaxed.entry(key).or_default().push(node);
        }

        let mut divergences = Vec::new();
        let mut unchanged = 0u64;
        let mut added = 0u64;
        let mut removed = 0u64;
        let mut modified = 0u64;
        let mut shifted = 0u64;
        let mut position = 0u64;

        // Track which candidate nodes we've matched.
        let mut matched_cand: std::collections::BTreeSet<MatchKey> =
            std::collections::BTreeSet::new();

        // Pass 1: Iterate baseline nodes, find matches in candidate.
        for node in &base_nodes {
            let exact_key = (node.timestamp_ms, node.pane_id, node.rule_id.clone());

            if let Some(cand_node) = cand_exact.get(&exact_key) {
                // Exact match on key.
                matched_cand.insert(exact_key);
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
                    cand_list.iter().find(|cn| {
                        let delta = cn.timestamp_ms.abs_diff(node.timestamp_ms);
                        delta <= config.time_tolerance_ms
                            && delta > 0
                            && !matched_cand.contains(&(
                                cn.timestamp_ms,
                                cn.pane_id,
                                cn.rule_id.clone(),
                            ))
                    })
                } else {
                    None
                };

                if let Some(shifted_node) = found_shifted {
                    let shifted_key = (
                        shifted_node.timestamp_ms,
                        shifted_node.pane_id,
                        shifted_node.rule_id.clone(),
                    );
                    matched_cand.insert(shifted_key);
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
        for node in &cand_nodes {
            let exact_key = (node.timestamp_ms, node.pane_id, node.rule_id.clone());
            if !matched_cand.contains(&exact_key) {
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
    use crate::replay_decision_graph::{DecisionEvent, DecisionType};

    fn make_event(
        decision_type: DecisionType,
        rule_id: &str,
        timestamp_ms: u64,
        pane_id: u64,
        def_hash: &str,
        output_hash: &str,
    ) -> DecisionEvent {
        DecisionEvent {
            decision_type,
            rule_id: rule_id.into(),
            definition_hash: def_hash.into(),
            input_hash: format!("in_{}", timestamp_ms),
            output_hash: output_hash.into(),
            timestamp_ms,
            pane_id,
            triggered_by: None,
            overrides: None,
            input_summary: String::new(),
            parent_event_id: None,
            confidence: None,
            wall_clock_ms: 0,
            replay_run_id: String::new(),
        }
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
