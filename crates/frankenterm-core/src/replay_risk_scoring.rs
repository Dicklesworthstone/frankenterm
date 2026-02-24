//! Severity taxonomy and risk scoring for replay divergences (ft-og6q6.5.3).
//!
//! Provides:
//! - [`DivergenceSeverity`] — Info/Low/Medium/High/Critical classification.
//! - [`RiskScore`] — Per-divergence risk with severity, impact, confidence.
//! - [`RiskScorer`] — Classifies divergences and computes aggregate risk.
//! - [`AggregateRisk`] — Summary with recommendation (Pass/Review/Block).
//! - [`SeverityConfig`] — Configurable severity rules via TOML.

use serde::{Deserialize, Serialize};

use crate::replay_decision_diff::{Divergence, DivergenceType, RootCause};
use crate::replay_decision_graph::DecisionType;

// ============================================================================
// DivergenceSeverity — classification levels
// ============================================================================

/// Severity level for a divergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DivergenceSeverity {
    /// Timing shift only — no behavioral impact.
    Info,
    /// Low-confidence or cosmetic difference.
    Low,
    /// Detection output changed (matched text, severity).
    Medium,
    /// Workflow decision changed (different step result).
    High,
    /// Policy decision changed (allow vs deny).
    Critical,
}

impl DivergenceSeverity {
    /// Numeric score for aggregation.
    #[must_use]
    pub fn score(&self) -> u32 {
        match self {
            Self::Info => 1,
            Self::Low => 2,
            Self::Medium => 5,
            Self::High => 10,
            Self::Critical => 25,
        }
    }
}

// ============================================================================
// Recommendation — aggregate outcome
// ============================================================================

/// Aggregate risk recommendation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Recommendation {
    /// All divergences are Info/Low — safe to proceed.
    Pass,
    /// Medium divergences present — human review needed.
    Review,
    /// High/Critical divergences present — do not merge.
    Block,
}

// ============================================================================
// RiskScore — per-divergence risk
// ============================================================================

/// Risk assessment for a single divergence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskScore {
    /// Severity classification.
    pub severity: DivergenceSeverity,
    /// Number of downstream decisions potentially affected.
    pub impact_radius: u64,
    /// Confidence in the classification (0.0–1.0).
    pub confidence: f64,
    /// Human-readable explanation.
    pub explanation: String,
}

// ============================================================================
// AggregateRisk — summary across all divergences
// ============================================================================

/// Aggregate risk across all divergences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateRisk {
    /// Highest individual severity.
    pub max_severity: DivergenceSeverity,
    /// Sum of all individual severity scores.
    pub total_risk_score: u64,
    /// Count of Critical divergences.
    pub critical_count: u64,
    /// Count of High divergences.
    pub high_count: u64,
    /// Count of Medium divergences.
    pub medium_count: u64,
    /// Count of Low divergences.
    pub low_count: u64,
    /// Count of Info divergences.
    pub info_count: u64,
    /// Overall recommendation.
    pub recommendation: Recommendation,
}

impl AggregateRisk {
    /// Compute from individual risk scores.
    #[must_use]
    pub fn from_scores(scores: &[RiskScore]) -> Self {
        if scores.is_empty() {
            return Self {
                max_severity: DivergenceSeverity::Info,
                total_risk_score: 0,
                critical_count: 0,
                high_count: 0,
                medium_count: 0,
                low_count: 0,
                info_count: 0,
                recommendation: Recommendation::Pass,
            };
        }

        let mut max_severity = DivergenceSeverity::Info;
        let mut total_risk_score = 0u64;
        let mut critical_count = 0u64;
        let mut high_count = 0u64;
        let mut medium_count = 0u64;
        let mut low_count = 0u64;
        let mut info_count = 0u64;

        for score in scores {
            if score.severity > max_severity {
                max_severity = score.severity;
            }
            total_risk_score += score.severity.score() as u64;
            match score.severity {
                DivergenceSeverity::Critical => critical_count += 1,
                DivergenceSeverity::High => high_count += 1,
                DivergenceSeverity::Medium => medium_count += 1,
                DivergenceSeverity::Low => low_count += 1,
                DivergenceSeverity::Info => info_count += 1,
            }
        }

        let recommendation = if critical_count > 0 || high_count > 0 {
            Recommendation::Block
        } else if medium_count > 0 {
            Recommendation::Review
        } else {
            Recommendation::Pass
        };

        Self {
            max_severity,
            total_risk_score,
            critical_count,
            high_count,
            medium_count,
            low_count,
            info_count,
            recommendation,
        }
    }
}

// ============================================================================
// SeverityRule — custom override rule
// ============================================================================

/// A custom severity override rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeverityRule {
    /// Decision type to match (if None, matches any).
    #[serde(default)]
    pub decision_type: Option<DecisionType>,
    /// Rule ID pattern (supports * wildcards, if None matches any).
    #[serde(default)]
    pub rule_id_pattern: Option<String>,
    /// Override severity.
    pub severity: DivergenceSeverity,
}

impl SeverityRule {
    /// Check if this rule matches a divergence.
    #[must_use]
    pub fn matches(&self, decision_type: Option<DecisionType>, rule_id: &str) -> bool {
        let type_matches = match self.decision_type {
            Some(dt) => decision_type == Some(dt),
            None => true,
        };
        let pattern_matches = match &self.rule_id_pattern {
            Some(pattern) => wildcard_matches(pattern, rule_id),
            None => true,
        };
        type_matches && pattern_matches
    }
}

/// Simple wildcard matching (same as replay_counterfactual).
fn wildcard_matches(pattern: &str, target: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let starts = pattern.starts_with('*');
    let ends = pattern.ends_with('*');
    match (starts, ends) {
        (true, true) => {
            let inner = &pattern[1..pattern.len() - 1];
            target.contains(inner)
        }
        (true, false) => {
            let suffix = &pattern[1..];
            target.ends_with(suffix)
        }
        (false, true) => {
            let prefix = &pattern[..pattern.len() - 1];
            target.starts_with(prefix)
        }
        (false, false) => target == pattern,
    }
}

// ============================================================================
// SeverityConfig — configurable severity rules
// ============================================================================

/// Configuration for severity classification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SeverityConfig {
    /// Custom severity rules (checked in order, first match wins).
    #[serde(default)]
    pub rules: Vec<SeverityRule>,
}

impl SeverityConfig {
    /// Load from TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self, String> {
        toml::from_str(toml_str).map_err(|e| format!("severity config parse error: {e}"))
    }

    /// Find matching custom severity, or None.
    #[must_use]
    pub fn lookup(&self, decision_type: Option<DecisionType>, rule_id: &str) -> Option<DivergenceSeverity> {
        for rule in &self.rules {
            if rule.matches(decision_type, rule_id) {
                return Some(rule.severity);
            }
        }
        None
    }
}

// ============================================================================
// RiskScorer — classifies divergences
// ============================================================================

/// Classifies divergences and computes risk scores.
pub struct RiskScorer {
    config: SeverityConfig,
}

impl RiskScorer {
    /// Create a scorer with default severity rules.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: SeverityConfig::default(),
        }
    }

    /// Create a scorer with custom configuration.
    #[must_use]
    pub fn with_config(config: SeverityConfig) -> Self {
        Self { config }
    }

    /// Score a single divergence.
    #[must_use]
    pub fn score(&self, divergence: &Divergence, impact_radius: u64) -> RiskScore {
        // Extract decision type and rule_id from the divergence nodes.
        let rule_id = divergence
            .baseline_node
            .as_ref()
            .or(divergence.candidate_node.as_ref())
            .map(|n| n.rule_id.as_str())
            .unwrap_or("");

        // Check custom rules first.
        let decision_type = self.infer_decision_type(divergence);
        if let Some(override_severity) = self.config.lookup(decision_type, rule_id) {
            return RiskScore {
                severity: override_severity,
                impact_radius,
                confidence: 1.0,
                explanation: format!(
                    "Custom rule override: {} classified as {:?}",
                    rule_id, override_severity
                ),
            };
        }

        // Default classification.
        let (severity, confidence, explanation) = self.classify_default(divergence);

        RiskScore {
            severity,
            impact_radius,
            confidence,
            explanation,
        }
    }

    /// Score multiple divergences and compute aggregate.
    #[must_use]
    pub fn aggregate(&self, divergences: &[Divergence]) -> AggregateRisk {
        let scores: Vec<RiskScore> = divergences
            .iter()
            .map(|d| self.score(d, 0))
            .collect();
        AggregateRisk::from_scores(&scores)
    }

    /// Infer decision type from divergence nodes.
    fn infer_decision_type(&self, divergence: &Divergence) -> Option<DecisionType> {
        // We can't directly infer DecisionType from DivergenceNode since it
        // doesn't carry that field. Use the divergence type + root cause as proxy.
        match &divergence.root_cause {
            RootCause::RuleDefinitionChange { rule_id, .. } => {
                Self::guess_type_from_rule_id(rule_id)
            }
            RootCause::InputDivergence { upstream_rule_id, .. } => {
                Self::guess_type_from_rule_id(upstream_rule_id)
            }
            RootCause::OverrideApplied { .. } => Some(DecisionType::OverrideApplied),
            _ => None,
        }
    }

    /// Heuristic: guess decision type from rule_id prefix.
    fn guess_type_from_rule_id(rule_id: &str) -> Option<DecisionType> {
        if rule_id.starts_with("pol_") || rule_id.starts_with("policy_") || rule_id.starts_with("rate_limit_") {
            Some(DecisionType::PolicyDecision)
        } else if rule_id.starts_with("wf_") || rule_id.starts_with("workflow_") {
            Some(DecisionType::WorkflowStep)
        } else if rule_id.starts_with("alert_") {
            Some(DecisionType::AlertFired)
        } else if rule_id.starts_with("rule_") || rule_id.starts_with("pattern_") {
            Some(DecisionType::PatternMatch)
        } else {
            None
        }
    }

    /// Default severity classification.
    fn classify_default(&self, divergence: &Divergence) -> (DivergenceSeverity, f64, String) {
        match divergence.divergence_type {
            DivergenceType::Shifted => (
                DivergenceSeverity::Info,
                0.95,
                "Timing shift only — no behavioral impact".into(),
            ),
            DivergenceType::Added => {
                let rule_id = divergence
                    .candidate_node
                    .as_ref()
                    .map(|n| n.rule_id.as_str())
                    .unwrap_or("");
                if let Some(dt) = Self::guess_type_from_rule_id(rule_id) {
                    match dt {
                        DecisionType::PolicyDecision => (
                            DivergenceSeverity::Critical,
                            0.9,
                            format!("New policy decision added: {}", rule_id),
                        ),
                        DecisionType::WorkflowStep => (
                            DivergenceSeverity::High,
                            0.8,
                            format!("New workflow step added: {}", rule_id),
                        ),
                        _ => (
                            DivergenceSeverity::Low,
                            0.7,
                            format!("New decision added: {}", rule_id),
                        ),
                    }
                } else {
                    (
                        DivergenceSeverity::Low,
                        0.5,
                        format!("New decision added: {}", rule_id),
                    )
                }
            }
            DivergenceType::Removed => {
                let rule_id = divergence
                    .baseline_node
                    .as_ref()
                    .map(|n| n.rule_id.as_str())
                    .unwrap_or("");
                if let Some(dt) = Self::guess_type_from_rule_id(rule_id) {
                    match dt {
                        DecisionType::PolicyDecision => (
                            DivergenceSeverity::Critical,
                            0.9,
                            format!("Policy decision removed: {}", rule_id),
                        ),
                        DecisionType::WorkflowStep => (
                            DivergenceSeverity::High,
                            0.8,
                            format!("Workflow step removed: {}", rule_id),
                        ),
                        _ => (
                            DivergenceSeverity::Low,
                            0.7,
                            format!("Decision removed: {}", rule_id),
                        ),
                    }
                } else {
                    (
                        DivergenceSeverity::Low,
                        0.5,
                        format!("Decision removed: {}", rule_id),
                    )
                }
            }
            DivergenceType::Modified => {
                // Classify based on root cause.
                match &divergence.root_cause {
                    RootCause::RuleDefinitionChange { rule_id, .. } => {
                        if let Some(dt) = Self::guess_type_from_rule_id(rule_id) {
                            match dt {
                                DecisionType::PolicyDecision => (
                                    DivergenceSeverity::Critical,
                                    0.95,
                                    format!("Policy rule definition changed: {}", rule_id),
                                ),
                                DecisionType::WorkflowStep => (
                                    DivergenceSeverity::High,
                                    0.9,
                                    format!("Workflow rule definition changed: {}", rule_id),
                                ),
                                _ => (
                                    DivergenceSeverity::Medium,
                                    0.85,
                                    format!("Detection rule definition changed: {}", rule_id),
                                ),
                            }
                        } else {
                            (
                                DivergenceSeverity::Medium,
                                0.7,
                                format!("Rule definition changed: {}", rule_id),
                            )
                        }
                    }
                    RootCause::InputDivergence { upstream_rule_id, .. } => (
                        DivergenceSeverity::Medium,
                        0.6,
                        format!("Input diverged from upstream: {}", upstream_rule_id),
                    ),
                    _ => (
                        DivergenceSeverity::Medium,
                        0.5,
                        "Modified decision with unknown root cause".into(),
                    ),
                }
            }
        }
    }
}

impl Default for RiskScorer {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_decision_diff::{DivergenceNode, DivergenceType, RootCause};

    fn make_divergence(
        dtype: DivergenceType,
        rule_id: &str,
        root_cause: RootCause,
    ) -> Divergence {
        let node = DivergenceNode {
            node_id: 0,
            rule_id: rule_id.into(),
            definition_hash: "def".into(),
            output_hash: "out".into(),
            timestamp_ms: 100,
            pane_id: 1,
        };
        Divergence {
            position: 0,
            divergence_type: dtype,
            baseline_node: Some(node.clone()),
            candidate_node: Some(node),
            root_cause,
        }
    }

    fn make_added(rule_id: &str) -> Divergence {
        Divergence {
            position: 0,
            divergence_type: DivergenceType::Added,
            baseline_node: None,
            candidate_node: Some(DivergenceNode {
                node_id: 0,
                rule_id: rule_id.into(),
                definition_hash: "def".into(),
                output_hash: "out".into(),
                timestamp_ms: 100,
                pane_id: 1,
            }),
            root_cause: RootCause::NewDecision { rule_id: rule_id.into() },
        }
    }

    fn make_removed(rule_id: &str) -> Divergence {
        Divergence {
            position: 0,
            divergence_type: DivergenceType::Removed,
            baseline_node: Some(DivergenceNode {
                node_id: 0,
                rule_id: rule_id.into(),
                definition_hash: "def".into(),
                output_hash: "out".into(),
                timestamp_ms: 100,
                pane_id: 1,
            }),
            candidate_node: None,
            root_cause: RootCause::DroppedDecision { rule_id: rule_id.into() },
        }
    }

    // ── Shifted → Info ─────────────────────────────────────────────────

    #[test]
    fn shifted_is_info() {
        let div = make_divergence(
            DivergenceType::Shifted,
            "rule_a",
            RootCause::TimingShift { baseline_ms: 100, candidate_ms: 150, delta_ms: 50 },
        );
        let scorer = RiskScorer::new();
        let score = scorer.score(&div, 0);
        assert_eq!(score.severity, DivergenceSeverity::Info);
    }

    // ── Added detection → Low ──────────────────────────────────────────

    #[test]
    fn added_detection_is_low() {
        let div = make_added("rule_a");
        let scorer = RiskScorer::new();
        let score = scorer.score(&div, 0);
        assert_eq!(score.severity, DivergenceSeverity::Low);
    }

    // ── Added policy → Critical ────────────────────────────────────────

    #[test]
    fn added_policy_is_critical() {
        let div = make_added("pol_rate_limit");
        let scorer = RiskScorer::new();
        let score = scorer.score(&div, 0);
        assert_eq!(score.severity, DivergenceSeverity::Critical);
    }

    // ── Added workflow → High ──────────────────────────────────────────

    #[test]
    fn added_workflow_is_high() {
        let div = make_added("wf_deploy");
        let scorer = RiskScorer::new();
        let score = scorer.score(&div, 0);
        assert_eq!(score.severity, DivergenceSeverity::High);
    }

    // ── Removed policy → Critical ──────────────────────────────────────

    #[test]
    fn removed_policy_is_critical() {
        let div = make_removed("policy_auth");
        let scorer = RiskScorer::new();
        let score = scorer.score(&div, 0);
        assert_eq!(score.severity, DivergenceSeverity::Critical);
    }

    // ── Modified detection with definition change → Medium ─────────────

    #[test]
    fn modified_detection_is_medium() {
        let div = make_divergence(
            DivergenceType::Modified,
            "rule_a",
            RootCause::RuleDefinitionChange {
                rule_id: "rule_a".into(),
                baseline_hash: "h1".into(),
                candidate_hash: "h2".into(),
            },
        );
        let scorer = RiskScorer::new();
        let score = scorer.score(&div, 0);
        assert_eq!(score.severity, DivergenceSeverity::Medium);
    }

    // ── Modified workflow → High ───────────────────────────────────────

    #[test]
    fn modified_workflow_is_high() {
        let div = make_divergence(
            DivergenceType::Modified,
            "wf_deploy",
            RootCause::RuleDefinitionChange {
                rule_id: "wf_deploy".into(),
                baseline_hash: "h1".into(),
                candidate_hash: "h2".into(),
            },
        );
        let scorer = RiskScorer::new();
        let score = scorer.score(&div, 0);
        assert_eq!(score.severity, DivergenceSeverity::High);
    }

    // ── Modified policy → Critical ─────────────────────────────────────

    #[test]
    fn modified_policy_is_critical() {
        let div = make_divergence(
            DivergenceType::Modified,
            "pol_auth",
            RootCause::RuleDefinitionChange {
                rule_id: "pol_auth".into(),
                baseline_hash: "h1".into(),
                candidate_hash: "h2".into(),
            },
        );
        let scorer = RiskScorer::new();
        let score = scorer.score(&div, 0);
        assert_eq!(score.severity, DivergenceSeverity::Critical);
    }

    // ── Impact radius ──────────────────────────────────────────────────

    #[test]
    fn impact_radius_propagated() {
        let div = make_divergence(
            DivergenceType::Modified,
            "rule_a",
            RootCause::Unknown,
        );
        let scorer = RiskScorer::new();
        let score = scorer.score(&div, 42);
        assert_eq!(score.impact_radius, 42);
    }

    // ── Aggregate: all Info → Pass ─────────────────────────────────────

    #[test]
    fn aggregate_all_info_pass() {
        let divs = vec![
            make_divergence(DivergenceType::Shifted, "r1", RootCause::TimingShift { baseline_ms: 100, candidate_ms: 150, delta_ms: 50 }),
            make_divergence(DivergenceType::Shifted, "r2", RootCause::TimingShift { baseline_ms: 200, candidate_ms: 250, delta_ms: 50 }),
        ];
        let scorer = RiskScorer::new();
        let agg = scorer.aggregate(&divs);
        assert_eq!(agg.recommendation, Recommendation::Pass);
        assert_eq!(agg.max_severity, DivergenceSeverity::Info);
    }

    // ── Aggregate: Critical → Block ────────────────────────────────────

    #[test]
    fn aggregate_critical_block() {
        let divs = vec![
            make_divergence(DivergenceType::Shifted, "r1", RootCause::TimingShift { baseline_ms: 100, candidate_ms: 150, delta_ms: 50 }),
            make_divergence(DivergenceType::Modified, "pol_auth", RootCause::RuleDefinitionChange {
                rule_id: "pol_auth".into(),
                baseline_hash: "h1".into(),
                candidate_hash: "h2".into(),
            }),
        ];
        let scorer = RiskScorer::new();
        let agg = scorer.aggregate(&divs);
        assert_eq!(agg.recommendation, Recommendation::Block);
        assert_eq!(agg.critical_count, 1);
    }

    // ── Aggregate: Medium → Review ─────────────────────────────────────

    #[test]
    fn aggregate_medium_review() {
        let divs = vec![
            make_divergence(DivergenceType::Modified, "rule_a", RootCause::RuleDefinitionChange {
                rule_id: "rule_a".into(),
                baseline_hash: "h1".into(),
                candidate_hash: "h2".into(),
            }),
        ];
        let scorer = RiskScorer::new();
        let agg = scorer.aggregate(&divs);
        assert_eq!(agg.recommendation, Recommendation::Review);
    }

    // ── Aggregate: empty → Pass ────────────────────────────────────────

    #[test]
    fn aggregate_empty_pass() {
        let scorer = RiskScorer::new();
        let agg = scorer.aggregate(&[]);
        assert_eq!(agg.recommendation, Recommendation::Pass);
        assert_eq!(agg.total_risk_score, 0);
    }

    // ── Aggregate counts ───────────────────────────────────────────────

    #[test]
    fn aggregate_counts() {
        let divs = vec![
            make_divergence(DivergenceType::Shifted, "r1", RootCause::TimingShift { baseline_ms: 100, candidate_ms: 150, delta_ms: 50 }),
            make_added("rule_a"),
            make_divergence(DivergenceType::Modified, "rule_b", RootCause::Unknown),
        ];
        let scorer = RiskScorer::new();
        let agg = scorer.aggregate(&divs);
        assert_eq!(agg.info_count, 1);
        assert_eq!(agg.low_count, 1);
        assert_eq!(agg.medium_count, 1);
    }

    // ── Custom severity rule ───────────────────────────────────────────

    #[test]
    fn custom_rule_override() {
        let config = SeverityConfig {
            rules: vec![SeverityRule {
                decision_type: None,
                rule_id_pattern: Some("rate_limit_*".into()),
                severity: DivergenceSeverity::Critical,
            }],
        };
        let scorer = RiskScorer::with_config(config);
        let div = make_divergence(
            DivergenceType::Modified,
            "rate_limit_api",
            RootCause::RuleDefinitionChange {
                rule_id: "rate_limit_api".into(),
                baseline_hash: "h1".into(),
                candidate_hash: "h2".into(),
            },
        );
        let score = scorer.score(&div, 0);
        assert_eq!(score.severity, DivergenceSeverity::Critical);
    }

    // ── Custom rule: first match wins ──────────────────────────────────

    #[test]
    fn custom_rule_first_match() {
        let config = SeverityConfig {
            rules: vec![
                SeverityRule {
                    decision_type: None,
                    rule_id_pattern: Some("rate_limit_api".into()),
                    severity: DivergenceSeverity::Low,
                },
                SeverityRule {
                    decision_type: None,
                    rule_id_pattern: Some("rate_limit_*".into()),
                    severity: DivergenceSeverity::Critical,
                },
            ],
        };
        let scorer = RiskScorer::with_config(config);
        let div = make_divergence(
            DivergenceType::Modified,
            "rate_limit_api",
            RootCause::RuleDefinitionChange {
                rule_id: "rate_limit_api".into(),
                baseline_hash: "h1".into(),
                candidate_hash: "h2".into(),
            },
        );
        let score = scorer.score(&div, 0);
        assert_eq!(score.severity, DivergenceSeverity::Low); // First rule wins.
    }

    // ── Severity ordering ──────────────────────────────────────────────

    #[test]
    fn severity_ordering() {
        assert!(DivergenceSeverity::Info < DivergenceSeverity::Low);
        assert!(DivergenceSeverity::Low < DivergenceSeverity::Medium);
        assert!(DivergenceSeverity::Medium < DivergenceSeverity::High);
        assert!(DivergenceSeverity::High < DivergenceSeverity::Critical);
    }

    // ── Severity scores ────────────────────────────────────────────────

    #[test]
    fn severity_scores() {
        assert_eq!(DivergenceSeverity::Info.score(), 1);
        assert_eq!(DivergenceSeverity::Low.score(), 2);
        assert_eq!(DivergenceSeverity::Medium.score(), 5);
        assert_eq!(DivergenceSeverity::High.score(), 10);
        assert_eq!(DivergenceSeverity::Critical.score(), 25);
    }

    // ── Recommendation ordering ────────────────────────────────────────

    #[test]
    fn recommendation_ordering() {
        assert!(Recommendation::Pass < Recommendation::Review);
        assert!(Recommendation::Review < Recommendation::Block);
    }

    // ── Total risk score ───────────────────────────────────────────────

    #[test]
    fn total_risk_score() {
        let scores = vec![
            RiskScore { severity: DivergenceSeverity::Info, impact_radius: 0, confidence: 1.0, explanation: String::new() },
            RiskScore { severity: DivergenceSeverity::Critical, impact_radius: 0, confidence: 1.0, explanation: String::new() },
        ];
        let agg = AggregateRisk::from_scores(&scores);
        assert_eq!(agg.total_risk_score, 1 + 25); // Info(1) + Critical(25)
    }

    // ── SeverityConfig from TOML ───────────────────────────────────────

    #[test]
    fn config_from_toml() {
        let toml = r#"
[[rules]]
rule_id_pattern = "rate_limit_*"
severity = "Critical"
"#;
        let config = SeverityConfig::from_toml(toml).unwrap();
        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].severity, DivergenceSeverity::Critical);
    }

    // ── SeverityConfig lookup ──────────────────────────────────────────

    #[test]
    fn config_lookup() {
        let config = SeverityConfig {
            rules: vec![SeverityRule {
                decision_type: Some(DecisionType::PolicyDecision),
                rule_id_pattern: None,
                severity: DivergenceSeverity::Critical,
            }],
        };
        assert_eq!(
            config.lookup(Some(DecisionType::PolicyDecision), "any_rule"),
            Some(DivergenceSeverity::Critical)
        );
        assert_eq!(
            config.lookup(Some(DecisionType::PatternMatch), "any_rule"),
            None
        );
    }

    // ── Serde roundtrips ───────────────────────────────────────────────

    #[test]
    fn severity_serde() {
        let s = DivergenceSeverity::High;
        let json = serde_json::to_string(&s).unwrap();
        let restored: DivergenceSeverity = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, s);
    }

    #[test]
    fn recommendation_serde() {
        let r = Recommendation::Review;
        let json = serde_json::to_string(&r).unwrap();
        let restored: Recommendation = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, r);
    }

    #[test]
    fn risk_score_serde() {
        let rs = RiskScore {
            severity: DivergenceSeverity::Medium,
            impact_radius: 5,
            confidence: 0.8,
            explanation: "test".into(),
        };
        let json = serde_json::to_string(&rs).unwrap();
        let restored: RiskScore = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.severity, DivergenceSeverity::Medium);
    }

    #[test]
    fn aggregate_risk_serde() {
        let agg = AggregateRisk::from_scores(&[]);
        let json = serde_json::to_string(&agg).unwrap();
        let restored: AggregateRisk = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.recommendation, Recommendation::Pass);
    }

    // ── Input divergence root cause → Medium ───────────────────────────

    #[test]
    fn input_divergence_is_medium() {
        let div = make_divergence(
            DivergenceType::Modified,
            "rule_a",
            RootCause::InputDivergence {
                upstream_rule_id: "rule_upstream".into(),
                upstream_position: 0,
            },
        );
        let scorer = RiskScorer::new();
        let score = scorer.score(&div, 0);
        assert_eq!(score.severity, DivergenceSeverity::Medium);
    }

    // ── Confidence is in [0, 1] ────────────────────────────────────────

    #[test]
    fn confidence_bounded() {
        let divs = vec![
            make_divergence(DivergenceType::Shifted, "r1", RootCause::TimingShift { baseline_ms: 100, candidate_ms: 150, delta_ms: 50 }),
            make_added("rule_a"),
            make_removed("wf_deploy"),
            make_divergence(DivergenceType::Modified, "pol_auth", RootCause::RuleDefinitionChange {
                rule_id: "pol_auth".into(),
                baseline_hash: "h1".into(),
                candidate_hash: "h2".into(),
            }),
        ];
        let scorer = RiskScorer::new();
        for div in &divs {
            let score = scorer.score(div, 0);
            assert!(score.confidence >= 0.0 && score.confidence <= 1.0);
        }
    }
}
