//! Actionable remediation guidance from diff findings (ft-og6q6.5.7).
//!
//! Provides:
//! - [`RemediationEngine`] — generates fix suggestions from divergences.
//! - [`Suggestion`] — a specific remediation action with rationale and effort estimate.
//! - [`SuggestionAction`] — the type of fix (UpdateRule, AddAnnotation, etc.).
//! - [`EffortEstimate`] — Low / Medium / High.

use serde::{Deserialize, Serialize};

use crate::replay_decision_diff::{Divergence, DivergenceType, RootCause};
use crate::replay_risk_scoring::{DivergenceSeverity, RiskScore, RiskScorer};

// ============================================================================
// SuggestionAction — types of remediation
// ============================================================================

/// Type of remediation action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SuggestionAction {
    /// Update the regression artifact annotations for this rule.
    UpdateRule,
    /// Add an expected-divergence annotation in the PR.
    AddAnnotation,
    /// Revert the change that caused the divergence.
    RevertChange,
    /// Investigate upstream dependencies that may have caused this.
    InvestigateUpstream,
    /// No action needed (informational only).
    NoActionNeeded,
}

// ============================================================================
// EffortEstimate — expected fix effort
// ============================================================================

/// Estimated effort to apply a suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EffortEstimate {
    /// Quick fix: add annotation, update config.
    Low,
    /// Moderate: investigate and fix rule definition.
    Medium,
    /// Significant: revert changes, investigate cascade.
    High,
}

// ============================================================================
// Suggestion — a specific remediation recommendation
// ============================================================================

/// A specific remediation recommendation for an operator or agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    /// The type of action to take.
    pub action: SuggestionAction,
    /// What to target (rule_id, file path, or config key).
    pub target: String,
    /// Why this action is recommended.
    pub rationale: String,
    /// Confidence that this suggestion will resolve the divergence (0.0-1.0).
    pub confidence: f64,
    /// Expected effort to apply.
    pub effort_estimate: EffortEstimate,
}

// ============================================================================
// RemediationEngine — generates suggestions from divergences
// ============================================================================

/// Generates actionable remediation suggestions from divergences.
pub struct RemediationEngine {
    scorer: RiskScorer,
}

impl RemediationEngine {
    /// Create an engine with default scoring.
    #[must_use]
    pub fn new() -> Self {
        Self {
            scorer: RiskScorer::new(),
        }
    }

    /// Create an engine with a custom scorer.
    #[must_use]
    pub fn with_scorer(scorer: RiskScorer) -> Self {
        Self { scorer }
    }

    /// Generate suggestions for a single divergence.
    #[must_use]
    pub fn suggest(&self, div: &Divergence) -> Vec<Suggestion> {
        let score = self.scorer.score(div, 0);

        // Info-severity divergences don't need remediation.
        if score.severity == DivergenceSeverity::Info {
            return vec![Suggestion {
                action: SuggestionAction::NoActionNeeded,
                target: extract_rule_id(div),
                rationale: "Info-level divergence; no action needed.".into(),
                confidence: 1.0,
                effort_estimate: EffortEstimate::Low,
            }];
        }

        let mut suggestions = Vec::new();

        match &div.root_cause {
            RootCause::RuleDefinitionChange {
                rule_id,
                baseline_hash,
                candidate_hash,
            } => {
                suggestions.push(Suggestion {
                    action: SuggestionAction::AddAnnotation,
                    target: rule_id.clone(),
                    rationale: format!(
                        "Rule '{}' definition changed ({} → {}). \
                         If intentional, add an expected-divergence annotation in the PR.",
                        rule_id, &baseline_hash[..baseline_hash.len().min(8)],
                        &candidate_hash[..candidate_hash.len().min(8)]
                    ),
                    confidence: 0.9,
                    effort_estimate: EffortEstimate::Low,
                });
                suggestions.push(Suggestion {
                    action: SuggestionAction::UpdateRule,
                    target: rule_id.clone(),
                    rationale: format!(
                        "Update regression artifact for rule '{}' to reflect the new definition.",
                        rule_id
                    ),
                    confidence: 0.8,
                    effort_estimate: EffortEstimate::Medium,
                });
            }

            RootCause::InputDivergence {
                upstream_rule_id,
                ..
            } => {
                suggestions.push(Suggestion {
                    action: SuggestionAction::InvestigateUpstream,
                    target: upstream_rule_id.clone(),
                    rationale: format!(
                        "Input diverged from upstream rule '{}'. \
                         Fix the upstream rule first; this divergence may resolve automatically.",
                        upstream_rule_id
                    ),
                    confidence: 0.7,
                    effort_estimate: EffortEstimate::Medium,
                });
            }

            RootCause::OverrideApplied {
                rule_id,
                override_id,
            } => {
                suggestions.push(Suggestion {
                    action: SuggestionAction::AddAnnotation,
                    target: rule_id.clone(),
                    rationale: format!(
                        "Override '{}' applied to rule '{}'. \
                         If the override is intentional, annotate this divergence.",
                        override_id, rule_id
                    ),
                    confidence: 0.85,
                    effort_estimate: EffortEstimate::Low,
                });
            }

            RootCause::NewDecision { rule_id } => {
                suggestions.push(Suggestion {
                    action: SuggestionAction::AddAnnotation,
                    target: rule_id.clone(),
                    rationale: format!(
                        "New decision from rule '{}' appeared. \
                         If expected, add an annotation. Otherwise investigate why a new rule fired.",
                        rule_id
                    ),
                    confidence: 0.75,
                    effort_estimate: EffortEstimate::Low,
                });
            }

            RootCause::DroppedDecision { rule_id } => {
                suggestions.push(Suggestion {
                    action: SuggestionAction::InvestigateUpstream,
                    target: rule_id.clone(),
                    rationale: format!(
                        "Rule '{}' no longer matches. Check pattern syntax and input data.",
                        rule_id
                    ),
                    confidence: 0.6,
                    effort_estimate: EffortEstimate::Medium,
                });
                if score.severity >= DivergenceSeverity::High {
                    suggestions.push(Suggestion {
                        action: SuggestionAction::RevertChange,
                        target: rule_id.clone(),
                        rationale: format!(
                            "High-severity: rule '{}' dropped. Consider reverting recent changes.",
                            rule_id
                        ),
                        confidence: 0.5,
                        effort_estimate: EffortEstimate::High,
                    });
                }
            }

            RootCause::TimingShift { delta_ms, .. } => {
                suggestions.push(Suggestion {
                    action: SuggestionAction::NoActionNeeded,
                    target: extract_rule_id(div),
                    rationale: format!(
                        "Timing shift of {}ms. Usually benign; no action needed unless strict L2 equivalence required.",
                        delta_ms
                    ),
                    confidence: 0.95,
                    effort_estimate: EffortEstimate::Low,
                });
            }

            RootCause::Unknown => {
                suggestions.push(build_unknown_suggestion(div, &score));
            }
        }

        suggestions
    }

    /// Generate suggestions for all divergences, identifying cascades.
    #[must_use]
    pub fn suggest_all(&self, divergences: &[Divergence]) -> Vec<(usize, Vec<Suggestion>)> {
        let mut results = Vec::new();

        // Identify potential cascading root causes: if the same upstream rule_id
        // appears in multiple InputDivergence root causes, consolidate.
        let mut cascade_roots: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();

        for (i, div) in divergences.iter().enumerate() {
            if let RootCause::InputDivergence {
                upstream_rule_id, ..
            } = &div.root_cause
            {
                cascade_roots
                    .entry(upstream_rule_id.clone())
                    .or_default()
                    .push(i);
            }
        }

        for (i, div) in divergences.iter().enumerate() {
            let mut suggestions = self.suggest(div);

            // Add cascade annotation if this is a root cause for others.
            let rule_id = extract_rule_id(div);
            if let Some(downstream) = cascade_roots.get(&rule_id) {
                if !downstream.is_empty() {
                    suggestions.push(Suggestion {
                        action: SuggestionAction::UpdateRule,
                        target: rule_id.clone(),
                        rationale: format!(
                            "Root cause: fixing rule '{}' will resolve {} downstream divergence(s).",
                            rule_id,
                            downstream.len()
                        ),
                        confidence: 0.8,
                        effort_estimate: EffortEstimate::Medium,
                    });
                }
            }

            results.push((i, suggestions));
        }

        results
    }
}

impl Default for RemediationEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn extract_rule_id(div: &Divergence) -> String {
    div.baseline_node
        .as_ref()
        .or(div.candidate_node.as_ref())
        .map(|n| n.rule_id.clone())
        .unwrap_or_else(|| "unknown".into())
}

fn build_unknown_suggestion(div: &Divergence, score: &RiskScore) -> Suggestion {
    let rule_id = extract_rule_id(div);
    match div.divergence_type {
        DivergenceType::Modified => Suggestion {
            action: SuggestionAction::InvestigateUpstream,
            target: rule_id,
            rationale: "Modified output with unknown root cause. Investigate inputs and rule definition.".into(),
            confidence: 0.5,
            effort_estimate: if score.severity >= DivergenceSeverity::High {
                EffortEstimate::High
            } else {
                EffortEstimate::Medium
            },
        },
        DivergenceType::Added => Suggestion {
            action: SuggestionAction::AddAnnotation,
            target: rule_id,
            rationale: "New decision appeared with unknown cause. Add annotation if intentional.".into(),
            confidence: 0.6,
            effort_estimate: EffortEstimate::Low,
        },
        DivergenceType::Removed => Suggestion {
            action: SuggestionAction::InvestigateUpstream,
            target: rule_id,
            rationale: "Decision dropped with unknown cause. Investigate rule matching and input data.".into(),
            confidence: 0.5,
            effort_estimate: EffortEstimate::Medium,
        },
        DivergenceType::Shifted => Suggestion {
            action: SuggestionAction::NoActionNeeded,
            target: rule_id,
            rationale: "Timing shift; usually benign.".into(),
            confidence: 0.95,
            effort_estimate: EffortEstimate::Low,
        },
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_decision_diff::{DivergenceNode, DivergenceType, RootCause};

    fn make_div_node(rule_id: &str, def: &str, out: &str) -> DivergenceNode {
        DivergenceNode {
            node_id: 0,
            rule_id: rule_id.into(),
            definition_hash: def.into(),
            output_hash: out.into(),
            timestamp_ms: 100,
            pane_id: 1,
        }
    }

    fn make_divergence(
        dt: DivergenceType,
        root_cause: RootCause,
        base_rule: &str,
        cand_rule: &str,
    ) -> Divergence {
        Divergence {
            position: 0,
            divergence_type: dt,
            baseline_node: Some(make_div_node(base_rule, "d1", "o1")),
            candidate_node: Some(make_div_node(cand_rule, "d2", "o2")),
            root_cause,
        }
    }

    // ── Rule definition change → UpdateRule + AddAnnotation ───────────

    #[test]
    fn rule_def_change_suggestions() {
        let engine = RemediationEngine::new();
        let div = make_divergence(
            DivergenceType::Modified,
            RootCause::RuleDefinitionChange {
                rule_id: "pol_auth".into(),
                baseline_hash: "hash_v1".into(),
                candidate_hash: "hash_v2".into(),
            },
            "pol_auth",
            "pol_auth",
        );
        let suggestions = engine.suggest(&div);
        assert!(suggestions.len() >= 2);
        let actions: Vec<_> = suggestions.iter().map(|s| s.action).collect();
        assert!(actions.contains(&SuggestionAction::AddAnnotation));
        assert!(actions.contains(&SuggestionAction::UpdateRule));
    }

    // ── Input divergence → InvestigateUpstream ────────────────────────

    #[test]
    fn input_divergence_suggestions() {
        let engine = RemediationEngine::new();
        let div = make_divergence(
            DivergenceType::Modified,
            RootCause::InputDivergence {
                upstream_rule_id: "rule_upstream".into(),
                upstream_position: 0,
            },
            "rule_downstream",
            "rule_downstream",
        );
        let suggestions = engine.suggest(&div);
        assert!(!suggestions.is_empty());
        assert_eq!(suggestions[0].action, SuggestionAction::InvestigateUpstream);
        assert!(suggestions[0].target.contains("rule_upstream"));
    }

    // ── Override applied → AddAnnotation ──────────────────────────────

    #[test]
    fn override_suggestions() {
        let engine = RemediationEngine::new();
        let div = make_divergence(
            DivergenceType::Modified,
            RootCause::OverrideApplied {
                rule_id: "rule_x".into(),
                override_id: "ovr_1".into(),
            },
            "rule_x",
            "rule_x",
        );
        let suggestions = engine.suggest(&div);
        assert!(!suggestions.is_empty());
        assert_eq!(suggestions[0].action, SuggestionAction::AddAnnotation);
    }

    // ── New decision → AddAnnotation ──────────────────────────────────

    #[test]
    fn new_decision_suggestions() {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Added,
            baseline_node: None,
            candidate_node: Some(make_div_node("new_rule", "d", "o")),
            root_cause: RootCause::NewDecision {
                rule_id: "new_rule".into(),
            },
        };
        let suggestions = engine.suggest(&div);
        assert!(!suggestions.is_empty());
        assert_eq!(suggestions[0].action, SuggestionAction::AddAnnotation);
    }

    // ── Dropped decision → InvestigateUpstream ────────────────────────

    #[test]
    fn dropped_decision_suggestions() {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Removed,
            baseline_node: Some(make_div_node("pol_auth", "d", "o")),
            candidate_node: None,
            root_cause: RootCause::DroppedDecision {
                rule_id: "pol_auth".into(),
            },
        };
        let suggestions = engine.suggest(&div);
        assert!(!suggestions.is_empty());
        assert_eq!(suggestions[0].action, SuggestionAction::InvestigateUpstream);
    }

    // ── High-severity dropped → also suggests RevertChange ────────────

    #[test]
    fn high_severity_dropped_revert() {
        let engine = RemediationEngine::new();
        // Policy rule dropped → Critical severity → should suggest revert.
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Removed,
            baseline_node: Some(make_div_node("pol_auth", "d", "o")),
            candidate_node: None,
            root_cause: RootCause::DroppedDecision {
                rule_id: "pol_auth".into(),
            },
        };
        let suggestions = engine.suggest(&div);
        let has_revert = suggestions.iter().any(|s| s.action == SuggestionAction::RevertChange);
        assert!(has_revert, "high-severity dropped should suggest revert");
    }

    // ── Timing shift → NoActionNeeded ─────────────────────────────────

    #[test]
    fn timing_shift_no_action() {
        let engine = RemediationEngine::new();
        let div = make_divergence(
            DivergenceType::Shifted,
            RootCause::TimingShift {
                baseline_ms: 100,
                candidate_ms: 150,
                delta_ms: 50,
            },
            "rule_a",
            "rule_a",
        );
        let suggestions = engine.suggest(&div);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].action, SuggestionAction::NoActionNeeded);
    }

    // ── Info severity → NoActionNeeded ────────────────────────────────

    #[test]
    fn info_severity_no_action() {
        let engine = RemediationEngine::new();
        // Shifted is always Info.
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Shifted,
            baseline_node: Some(make_div_node("rule_a", "d", "o")),
            candidate_node: Some(make_div_node("rule_a", "d", "o")),
            root_cause: RootCause::TimingShift {
                baseline_ms: 100,
                candidate_ms: 110,
                delta_ms: 10,
            },
        };
        let suggestions = engine.suggest(&div);
        assert_eq!(suggestions[0].action, SuggestionAction::NoActionNeeded);
    }

    // ── Unknown root cause → fallback suggestion ──────────────────────

    #[test]
    fn unknown_root_cause() {
        let engine = RemediationEngine::new();
        let div = make_divergence(
            DivergenceType::Modified,
            RootCause::Unknown,
            "rule_x",
            "rule_x",
        );
        let suggestions = engine.suggest(&div);
        assert!(!suggestions.is_empty());
        assert_eq!(suggestions[0].action, SuggestionAction::InvestigateUpstream);
    }

    // ── Confidence is bounded [0, 1] ──────────────────────────────────

    #[test]
    fn confidence_bounded() {
        let engine = RemediationEngine::new();
        let divergences = vec![
            make_divergence(
                DivergenceType::Modified,
                RootCause::RuleDefinitionChange {
                    rule_id: "r".into(),
                    baseline_hash: "h1".into(),
                    candidate_hash: "h2".into(),
                },
                "r",
                "r",
            ),
            make_divergence(
                DivergenceType::Modified,
                RootCause::Unknown,
                "r2",
                "r2",
            ),
        ];
        for div in &divergences {
            for s in engine.suggest(div) {
                assert!(
                    s.confidence >= 0.0 && s.confidence <= 1.0,
                    "confidence {} out of range",
                    s.confidence
                );
            }
        }
    }

    // ── Effort estimate is reasonable ─────────────────────────────────

    #[test]
    fn effort_estimates() {
        let engine = RemediationEngine::new();
        // AddAnnotation should be Low effort.
        let div = make_divergence(
            DivergenceType::Modified,
            RootCause::OverrideApplied {
                rule_id: "r".into(),
                override_id: "o".into(),
            },
            "r",
            "r",
        );
        let suggestions = engine.suggest(&div);
        assert_eq!(suggestions[0].effort_estimate, EffortEstimate::Low);
    }

    // ── Multiple suggestions for complex divergence ───────────────────

    #[test]
    fn multiple_suggestions() {
        let engine = RemediationEngine::new();
        let div = make_divergence(
            DivergenceType::Modified,
            RootCause::RuleDefinitionChange {
                rule_id: "pol_rate_limit".into(),
                baseline_hash: "h1".into(),
                candidate_hash: "h2".into(),
            },
            "pol_rate_limit",
            "pol_rate_limit",
        );
        let suggestions = engine.suggest(&div);
        assert!(suggestions.len() >= 2, "should have multiple suggestions");
    }

    // ── Target references correct rule_id ─────────────────────────────

    #[test]
    fn target_references_rule() {
        let engine = RemediationEngine::new();
        let div = make_divergence(
            DivergenceType::Modified,
            RootCause::RuleDefinitionChange {
                rule_id: "my_special_rule".into(),
                baseline_hash: "h1".into(),
                candidate_hash: "h2".into(),
            },
            "my_special_rule",
            "my_special_rule",
        );
        let suggestions = engine.suggest(&div);
        for s in &suggestions {
            assert!(
                s.target.contains("my_special_rule"),
                "target should reference rule_id"
            );
        }
    }

    // ── Cascading root cause identification ───────────────────────────

    #[test]
    fn cascading_identification() {
        let engine = RemediationEngine::new();
        let divergences = vec![
            make_divergence(
                DivergenceType::Modified,
                RootCause::RuleDefinitionChange {
                    rule_id: "root_rule".into(),
                    baseline_hash: "h1".into(),
                    candidate_hash: "h2".into(),
                },
                "root_rule",
                "root_rule",
            ),
            make_divergence(
                DivergenceType::Modified,
                RootCause::InputDivergence {
                    upstream_rule_id: "root_rule".into(),
                    upstream_position: 0,
                },
                "downstream_a",
                "downstream_a",
            ),
            make_divergence(
                DivergenceType::Modified,
                RootCause::InputDivergence {
                    upstream_rule_id: "root_rule".into(),
                    upstream_position: 1,
                },
                "downstream_b",
                "downstream_b",
            ),
        ];

        let all = engine.suggest_all(&divergences);
        // First divergence (root_rule) should have a cascade suggestion.
        let (_, root_suggestions) = &all[0];
        let has_cascade = root_suggestions
            .iter()
            .any(|s| s.rationale.contains("downstream"));
        assert!(has_cascade, "root cause should mention downstream impacts");
    }

    // ── suggest_all covers all divergences ────────────────────────────

    #[test]
    fn suggest_all_coverage() {
        let engine = RemediationEngine::new();
        let divergences = vec![
            make_divergence(
                DivergenceType::Modified,
                RootCause::Unknown,
                "r1",
                "r1",
            ),
            make_divergence(
                DivergenceType::Added,
                RootCause::NewDecision { rule_id: "r2".into() },
                "r2",
                "r2",
            ),
        ];
        let results = engine.suggest_all(&divergences);
        assert_eq!(results.len(), divergences.len());
        for (i, suggestions) in &results {
            assert!(!suggestions.is_empty(), "divergence {} has no suggestions", i);
        }
    }

    // ── SuggestionAction serde roundtrip ──────────────────────────────

    #[test]
    fn action_serde() {
        for action in &[
            SuggestionAction::UpdateRule,
            SuggestionAction::AddAnnotation,
            SuggestionAction::RevertChange,
            SuggestionAction::InvestigateUpstream,
            SuggestionAction::NoActionNeeded,
        ] {
            let json = serde_json::to_string(action).unwrap();
            let restored: SuggestionAction = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, *action);
        }
    }

    // ── EffortEstimate serde roundtrip ────────────────────────────────

    #[test]
    fn effort_serde() {
        for effort in &[EffortEstimate::Low, EffortEstimate::Medium, EffortEstimate::High] {
            let json = serde_json::to_string(effort).unwrap();
            let restored: EffortEstimate = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, *effort);
        }
    }

    // ── EffortEstimate ordering ───────────────────────────────────────

    #[test]
    fn effort_ordering() {
        assert!(EffortEstimate::Low < EffortEstimate::Medium);
        assert!(EffortEstimate::Medium < EffortEstimate::High);
    }

    // ── Suggestion serde roundtrip ────────────────────────────────────

    #[test]
    fn suggestion_serde() {
        let s = Suggestion {
            action: SuggestionAction::UpdateRule,
            target: "rule_x".into(),
            rationale: "test".into(),
            confidence: 0.8,
            effort_estimate: EffortEstimate::Medium,
        };
        let json = serde_json::to_string(&s).unwrap();
        let restored: Suggestion = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.action, s.action);
        assert_eq!(restored.target, "rule_x");
    }

    // ── Default engine ────────────────────────────────────────────────

    #[test]
    fn default_engine() {
        let engine = RemediationEngine::default();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Shifted,
            baseline_node: Some(make_div_node("r", "d", "o")),
            candidate_node: Some(make_div_node("r", "d", "o")),
            root_cause: RootCause::TimingShift {
                baseline_ms: 100,
                candidate_ms: 110,
                delta_ms: 10,
            },
        };
        let suggestions = engine.suggest(&div);
        assert!(!suggestions.is_empty());
    }

    // ── Added with Unknown → AddAnnotation ────────────────────────────

    #[test]
    fn added_unknown_annotation() {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Added,
            baseline_node: None,
            candidate_node: Some(make_div_node("new_r", "d", "o")),
            root_cause: RootCause::Unknown,
        };
        let suggestions = engine.suggest(&div);
        assert_eq!(suggestions[0].action, SuggestionAction::AddAnnotation);
    }

    // ── Removed with Unknown → InvestigateUpstream ────────────────────

    #[test]
    fn removed_unknown_investigate() {
        let engine = RemediationEngine::new();
        let div = Divergence {
            position: 0,
            divergence_type: DivergenceType::Removed,
            baseline_node: Some(make_div_node("old_r", "d", "o")),
            candidate_node: None,
            root_cause: RootCause::Unknown,
        };
        let suggestions = engine.suggest(&div);
        assert_eq!(suggestions[0].action, SuggestionAction::InvestigateUpstream);
    }

    // ── Policy flip scenario ──────────────────────────────────────────

    #[test]
    fn policy_flip_scenario() {
        let engine = RemediationEngine::new();
        let div = make_divergence(
            DivergenceType::Modified,
            RootCause::RuleDefinitionChange {
                rule_id: "pol_auth".into(),
                baseline_hash: "allow_hash".into(),
                candidate_hash: "deny_hash".into(),
            },
            "pol_auth",
            "pol_auth",
        );
        let suggestions = engine.suggest(&div);
        // Should have AddAnnotation + UpdateRule at minimum.
        let actions: Vec<_> = suggestions.iter().map(|s| s.action).collect();
        assert!(actions.contains(&SuggestionAction::AddAnnotation));
        assert!(actions.contains(&SuggestionAction::UpdateRule));
        // Rationale should mention the hash change.
        assert!(suggestions[0].rationale.contains("allow_ha"));
    }

    // ── Empty divergences → empty results ─────────────────────────────

    #[test]
    fn empty_divergences() {
        let engine = RemediationEngine::new();
        let results = engine.suggest_all(&[]);
        assert!(results.is_empty());
    }

    // ── Rationale is non-empty ────────────────────────────────────────

    #[test]
    fn rationale_nonempty() {
        let engine = RemediationEngine::new();
        let test_cases = vec![
            make_divergence(
                DivergenceType::Modified,
                RootCause::RuleDefinitionChange {
                    rule_id: "r".into(),
                    baseline_hash: "h1".into(),
                    candidate_hash: "h2".into(),
                },
                "r",
                "r",
            ),
            make_divergence(
                DivergenceType::Modified,
                RootCause::InputDivergence {
                    upstream_rule_id: "u".into(),
                    upstream_position: 0,
                },
                "r",
                "r",
            ),
            make_divergence(
                DivergenceType::Modified,
                RootCause::Unknown,
                "r",
                "r",
            ),
        ];
        for div in &test_cases {
            for s in engine.suggest(div) {
                assert!(!s.rationale.is_empty(), "rationale should not be empty");
            }
        }
    }
}
