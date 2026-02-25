//! Regression budgets and CI gate evaluator for decision-diff reports (ft-og6q6.5.5).
//!
//! Provides:
//! - [`RegressionBudget`] — configurable thresholds for acceptable divergences.
//! - [`GateEvaluator`] — evaluates a report against a budget, producing [`GateResult`].
//! - [`ExpectedDivergenceAnnotation`] — marks known-intentional divergences for exclusion.
//! - [`Violation`] / [`Warning`] — structured reasons for gate failure/warning.

use serde::{Deserialize, Serialize};

use crate::replay_report::JsonReport;

// ============================================================================
// RegressionBudget — divergence thresholds
// ============================================================================

/// Configurable divergence thresholds for CI pass/fail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionBudget {
    /// Maximum Critical divergences allowed (default 0).
    #[serde(default)]
    pub max_critical: u32,
    /// Maximum High divergences allowed (default 0).
    #[serde(default)]
    pub max_high: u32,
    /// Maximum Medium divergences allowed (default 5).
    #[serde(default = "default_max_medium")]
    pub max_medium: u32,
    /// Maximum percentage of artifacts that may be skipped (default 10.0).
    #[serde(default = "default_skip_budget")]
    pub skip_budget_percent: f64,
    /// Maximum replay wall-clock time in ms (default 1_800_000 = 30 min).
    #[serde(default = "default_time_budget")]
    pub time_budget_ms: u64,
}

fn default_max_medium() -> u32 {
    5
}
fn default_skip_budget() -> f64 {
    10.0
}
fn default_time_budget() -> u64 {
    1_800_000
}

impl Default for RegressionBudget {
    fn default() -> Self {
        Self {
            max_critical: 0,
            max_high: 0,
            max_medium: default_max_medium(),
            skip_budget_percent: default_skip_budget(),
            time_budget_ms: default_time_budget(),
        }
    }
}

impl RegressionBudget {
    /// Parse a budget from TOML.
    pub fn from_toml(s: &str) -> Result<Self, String> {
        toml::from_str(s).map_err(|e| format!("budget TOML parse error: {}", e))
    }

    /// Serialize to TOML.
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string(self).map_err(|e| format!("budget TOML serialize error: {}", e))
    }
}

// ============================================================================
// ExpectedDivergenceAnnotation — known-intentional exclusions
// ============================================================================

/// Annotation marking a divergence as expected/intentional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedDivergenceAnnotation {
    /// Position in the divergence list this annotation covers.
    pub position: u64,
    /// Human-readable reason this divergence is expected.
    pub reason: String,
    /// PR reference (required for validity).
    pub pr_reference: String,
    /// Hash of the definition change (for traceability).
    #[serde(default)]
    pub definition_change_hash: String,
}

impl ExpectedDivergenceAnnotation {
    /// Validate the annotation. Returns error message if invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.pr_reference.is_empty() {
            return Err(format!(
                "annotation at position {} missing pr_reference",
                self.position
            ));
        }
        if self.reason.is_empty() {
            return Err(format!(
                "annotation at position {} missing reason",
                self.position
            ));
        }
        Ok(())
    }
}

// ============================================================================
// Violation / Warning — gate evaluation details
// ============================================================================

/// A specific budget violation causing gate failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    /// Which budget dimension was exceeded.
    pub budget_dimension: String,
    /// The configured limit.
    pub limit: String,
    /// The actual value.
    pub actual: String,
    /// How much the budget was exceeded by.
    pub excess: String,
    /// Rule IDs of divergences that contributed.
    pub contributing_rule_ids: Vec<String>,
}

/// A budget warning (approaching threshold, etc.).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Warning {
    /// Description of the warning condition.
    pub message: String,
    /// Which budget dimension is at risk.
    pub budget_dimension: String,
    /// Current usage as percentage of budget.
    pub usage_percent: f64,
}

// ============================================================================
// GateResult — evaluation outcome
// ============================================================================

/// CI gate evaluation result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GateResult {
    /// All budgets satisfied.
    Pass,
    /// One or more budget violations.
    Fail(Vec<Violation>),
    /// No violations but some budgets approaching limits.
    Warn(Vec<Warning>),
}

impl GateResult {
    /// Whether the gate passes (Pass or Warn).
    #[must_use]
    pub fn is_pass(&self) -> bool {
        matches!(self, GateResult::Pass | GateResult::Warn(_))
    }

    /// Whether the gate failed.
    #[must_use]
    pub fn is_fail(&self) -> bool {
        matches!(self, GateResult::Fail(_))
    }

    /// Get violations (empty if Pass/Warn).
    #[must_use]
    pub fn violations(&self) -> &[Violation] {
        match self {
            GateResult::Fail(v) => v,
            _ => &[],
        }
    }

    /// Get warnings (empty if Pass/Fail).
    #[must_use]
    pub fn warnings(&self) -> &[Warning] {
        match self {
            GateResult::Warn(w) => w,
            _ => &[],
        }
    }
}

// ============================================================================
// EvaluationContext — enriched evaluation input
// ============================================================================

/// Optional context for gate evaluation beyond the report itself.
#[derive(Debug, Clone, Default)]
pub struct EvaluationContext {
    /// Expected divergence annotations.
    pub annotations: Vec<ExpectedDivergenceAnnotation>,
    /// Total artifacts in the replay matrix (for skip budget).
    pub total_artifacts: u64,
    /// Number of artifacts that were skipped/errored.
    pub skipped_artifacts: u64,
    /// Actual replay wall-clock time in ms.
    pub replay_duration_ms: u64,
}

// ============================================================================
// GateEvaluator — evaluates reports against budgets
// ============================================================================

/// Warning threshold as fraction of budget (80%).
const WARN_THRESHOLD: f64 = 0.8;

/// Evaluates decision-diff reports against regression budgets.
pub struct GateEvaluator {
    budget: RegressionBudget,
}

impl GateEvaluator {
    /// Create an evaluator with the given budget.
    #[must_use]
    pub fn new(budget: RegressionBudget) -> Self {
        Self { budget }
    }

    /// Create an evaluator with the default budget.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self {
            budget: RegressionBudget::default(),
        }
    }

    /// Get a reference to the budget.
    #[must_use]
    pub fn budget(&self) -> &RegressionBudget {
        &self.budget
    }

    /// Evaluate a JSON report against the budget.
    #[must_use]
    pub fn evaluate(&self, report: &JsonReport, ctx: &EvaluationContext) -> GateResult {
        let mut violations = Vec::new();
        let mut warnings = Vec::new();

        // Compute effective counts (excluding annotated expected divergences).
        let excluded_positions: std::collections::HashSet<u64> = ctx
            .annotations
            .iter()
            .filter(|a| a.validate().is_ok())
            .map(|a| a.position)
            .collect();

        let mut effective_critical: u32 = 0;
        let mut effective_high: u32 = 0;
        let mut effective_medium: u32 = 0;
        let mut critical_rules = Vec::new();
        let mut high_rules = Vec::new();
        let mut medium_rules = Vec::new();

        for div in &report.divergences {
            if excluded_positions.contains(&div.position) {
                continue;
            }
            match div.severity.as_str() {
                "Critical" => {
                    effective_critical += 1;
                    critical_rules.push(div.rule_id.clone());
                }
                "High" => {
                    effective_high += 1;
                    high_rules.push(div.rule_id.clone());
                }
                "Medium" => {
                    effective_medium += 1;
                    medium_rules.push(div.rule_id.clone());
                }
                _ => {}
            }
        }

        // Check critical budget.
        if effective_critical > self.budget.max_critical {
            violations.push(Violation {
                budget_dimension: "max_critical".into(),
                limit: self.budget.max_critical.to_string(),
                actual: effective_critical.to_string(),
                excess: (effective_critical - self.budget.max_critical).to_string(),
                contributing_rule_ids: critical_rules.clone(),
            });
        } else if self.budget.max_critical > 0 {
            let usage = effective_critical as f64 / self.budget.max_critical as f64;
            if usage >= WARN_THRESHOLD {
                warnings.push(Warning {
                    message: format!("Critical divergences at {:.0}% of budget", usage * 100.0),
                    budget_dimension: "max_critical".into(),
                    usage_percent: usage * 100.0,
                });
            }
        }

        // Check high budget.
        if effective_high > self.budget.max_high {
            violations.push(Violation {
                budget_dimension: "max_high".into(),
                limit: self.budget.max_high.to_string(),
                actual: effective_high.to_string(),
                excess: (effective_high - self.budget.max_high).to_string(),
                contributing_rule_ids: high_rules.clone(),
            });
        } else if self.budget.max_high > 0 {
            let usage = effective_high as f64 / self.budget.max_high as f64;
            if usage >= WARN_THRESHOLD {
                warnings.push(Warning {
                    message: format!("High divergences at {:.0}% of budget", usage * 100.0),
                    budget_dimension: "max_high".into(),
                    usage_percent: usage * 100.0,
                });
            }
        }

        // Check medium budget.
        if effective_medium > self.budget.max_medium {
            violations.push(Violation {
                budget_dimension: "max_medium".into(),
                limit: self.budget.max_medium.to_string(),
                actual: effective_medium.to_string(),
                excess: (effective_medium - self.budget.max_medium).to_string(),
                contributing_rule_ids: medium_rules.clone(),
            });
        } else if self.budget.max_medium > 0 {
            let usage = effective_medium as f64 / self.budget.max_medium as f64;
            if usage >= WARN_THRESHOLD {
                warnings.push(Warning {
                    message: format!("Medium divergences at {:.0}% of budget", usage * 100.0),
                    budget_dimension: "max_medium".into(),
                    usage_percent: usage * 100.0,
                });
            }
        }

        // Check skip budget.
        if ctx.total_artifacts > 0 {
            let skip_pct = (ctx.skipped_artifacts as f64 / ctx.total_artifacts as f64) * 100.0;
            if skip_pct > self.budget.skip_budget_percent {
                violations.push(Violation {
                    budget_dimension: "skip_budget_percent".into(),
                    limit: format!("{:.1}%", self.budget.skip_budget_percent),
                    actual: format!("{:.1}%", skip_pct),
                    excess: format!("{:.1}%", skip_pct - self.budget.skip_budget_percent),
                    contributing_rule_ids: vec![],
                });
            } else if self.budget.skip_budget_percent > 0.0 {
                let usage = skip_pct / self.budget.skip_budget_percent;
                if usage >= WARN_THRESHOLD {
                    warnings.push(Warning {
                        message: format!("Skipped artifacts at {:.0}% of budget", usage * 100.0),
                        budget_dimension: "skip_budget_percent".into(),
                        usage_percent: usage * 100.0,
                    });
                }
            }
        }

        // Check time budget.
        if ctx.replay_duration_ms > self.budget.time_budget_ms {
            violations.push(Violation {
                budget_dimension: "time_budget_ms".into(),
                limit: self.budget.time_budget_ms.to_string(),
                actual: ctx.replay_duration_ms.to_string(),
                excess: (ctx.replay_duration_ms - self.budget.time_budget_ms).to_string(),
                contributing_rule_ids: vec![],
            });
        } else if self.budget.time_budget_ms > 0 {
            let usage = ctx.replay_duration_ms as f64 / self.budget.time_budget_ms as f64;
            if usage >= WARN_THRESHOLD {
                warnings.push(Warning {
                    message: format!("Replay duration at {:.0}% of budget", usage * 100.0),
                    budget_dimension: "time_budget_ms".into(),
                    usage_percent: usage * 100.0,
                });
            }
        }

        // Produce result.
        if !violations.is_empty() {
            GateResult::Fail(violations)
        } else if !warnings.is_empty() {
            GateResult::Warn(warnings)
        } else {
            GateResult::Pass
        }
    }

    /// Convenience: evaluate a report with no context.
    #[must_use]
    pub fn evaluate_simple(&self, report: &JsonReport) -> GateResult {
        self.evaluate(report, &EvaluationContext::default())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_report::{JsonDivergence, JsonRiskSummary};

    fn empty_report() -> JsonReport {
        JsonReport {
            replay_run_id: "test".into(),
            artifact_path: "a.replay".into(),
            override_path: String::new(),
            equivalence_level: "L2".into(),
            pass: true,
            recommendation: "Pass".into(),
            divergences: vec![],
            risk_summary: JsonRiskSummary {
                max_severity: "Info".into(),
                total_risk_score: 0,
                critical_count: 0,
                high_count: 0,
                medium_count: 0,
                low_count: 0,
                info_count: 0,
            },
            timestamp: "2026-02-24T00:00:00Z".into(),
        }
    }

    fn make_div(severity: &str, rule_id: &str, position: u64) -> JsonDivergence {
        JsonDivergence {
            position,
            divergence_type: "Modified".into(),
            severity: severity.into(),
            rule_id: rule_id.into(),
            root_cause: "Unknown".into(),
            baseline_output: "o1".into(),
            candidate_output: "o2".into(),
        }
    }

    fn report_with_divs(divs: Vec<JsonDivergence>) -> JsonReport {
        let mut r = empty_report();
        r.divergences = divs;
        r.pass = false;
        r.recommendation = "Block".into();
        r
    }

    // ── Default budget ────────────────────────────────────────────────

    #[test]
    fn default_budget_values() {
        let b = RegressionBudget::default();
        assert_eq!(b.max_critical, 0);
        assert_eq!(b.max_high, 0);
        assert_eq!(b.max_medium, 5);
        assert!((b.skip_budget_percent - 10.0).abs() < f64::EPSILON);
        assert_eq!(b.time_budget_ms, 1_800_000);
    }

    // ── Empty report passes ───────────────────────────────────────────

    #[test]
    fn empty_report_passes() {
        let eval = GateEvaluator::with_defaults();
        let result = eval.evaluate_simple(&empty_report());
        assert_eq!(result, GateResult::Pass);
    }

    // ── 1 critical fails default budget ───────────────────────────────

    #[test]
    fn one_critical_fails() {
        let eval = GateEvaluator::with_defaults();
        let report = report_with_divs(vec![make_div("Critical", "pol_auth", 0)]);
        let result = eval.evaluate_simple(&report);
        assert!(result.is_fail());
        assert_eq!(result.violations().len(), 1);
        assert_eq!(result.violations()[0].budget_dimension, "max_critical");
    }

    // ── 1 high fails default budget ───────────────────────────────────

    #[test]
    fn one_high_fails() {
        let eval = GateEvaluator::with_defaults();
        let report = report_with_divs(vec![make_div("High", "wf_deploy", 0)]);
        let result = eval.evaluate_simple(&report);
        assert!(result.is_fail());
        assert_eq!(result.violations()[0].budget_dimension, "max_high");
    }

    // ── 3 medium passes (under max_medium=5) ──────────────────────────

    #[test]
    fn three_medium_passes() {
        let eval = GateEvaluator::with_defaults();
        let divs: Vec<_> = (0..3)
            .map(|i| make_div("Medium", &format!("rule_{}", i), i))
            .collect();
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        assert!(result.is_pass());
    }

    // ── 6 medium fails (over max_medium=5) ────────────────────────────

    #[test]
    fn six_medium_fails() {
        let eval = GateEvaluator::with_defaults();
        let divs: Vec<_> = (0..6)
            .map(|i| make_div("Medium", &format!("rule_{}", i), i))
            .collect();
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        assert!(result.is_fail());
        assert_eq!(result.violations()[0].budget_dimension, "max_medium");
    }

    // ── Low/Info divergences never violate budget ─────────────────────

    #[test]
    fn low_info_passes() {
        let eval = GateEvaluator::with_defaults();
        let divs = vec![
            make_div("Low", "r1", 0),
            make_div("Info", "r2", 1),
            make_div("Low", "r3", 2),
            make_div("Info", "r4", 3),
        ];
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        assert_eq!(result, GateResult::Pass);
    }

    // ── Skip budget exceeded ──────────────────────────────────────────

    #[test]
    fn skip_budget_exceeded() {
        let eval = GateEvaluator::with_defaults();
        let ctx = EvaluationContext {
            total_artifacts: 100,
            skipped_artifacts: 11,
            ..Default::default()
        };
        let result = eval.evaluate(&empty_report(), &ctx);
        assert!(result.is_fail());
        assert_eq!(
            result.violations()[0].budget_dimension,
            "skip_budget_percent"
        );
    }

    // ── Skip budget within limit ──────────────────────────────────────

    #[test]
    fn skip_budget_ok() {
        let eval = GateEvaluator::with_defaults();
        let ctx = EvaluationContext {
            total_artifacts: 100,
            skipped_artifacts: 10,
            ..Default::default()
        };
        let result = eval.evaluate(&empty_report(), &ctx);
        assert!(result.is_pass());
    }

    // ── Time budget exceeded ──────────────────────────────────────────

    #[test]
    fn time_budget_exceeded() {
        let eval = GateEvaluator::with_defaults();
        let ctx = EvaluationContext {
            replay_duration_ms: 1_800_001,
            ..Default::default()
        };
        let result = eval.evaluate(&empty_report(), &ctx);
        assert!(result.is_fail());
        assert_eq!(result.violations()[0].budget_dimension, "time_budget_ms");
    }

    // ── Time budget within limit ──────────────────────────────────────

    #[test]
    fn time_budget_ok() {
        let eval = GateEvaluator::with_defaults();
        let ctx = EvaluationContext {
            replay_duration_ms: 1_800_000,
            ..Default::default()
        };
        let result = eval.evaluate(&empty_report(), &ctx);
        assert!(result.is_pass());
    }

    // ── Expected divergence annotation excludes from count ────────────

    #[test]
    fn annotation_excludes() {
        let eval = GateEvaluator::with_defaults();
        let report = report_with_divs(vec![make_div("Critical", "pol_auth", 42)]);
        let ctx = EvaluationContext {
            annotations: vec![ExpectedDivergenceAnnotation {
                position: 42,
                reason: "Intentional policy change".into(),
                pr_reference: "PR-123".into(),
                definition_change_hash: "abc".into(),
            }],
            ..Default::default()
        };
        let result = eval.evaluate(&report, &ctx);
        assert_eq!(result, GateResult::Pass);
    }

    // ── Annotation without PR reference is invalid ────────────────────

    #[test]
    fn annotation_without_pr_invalid() {
        let ann = ExpectedDivergenceAnnotation {
            position: 0,
            reason: "reason".into(),
            pr_reference: String::new(),
            definition_change_hash: String::new(),
        };
        assert!(ann.validate().is_err());
    }

    // ── Annotation without reason is invalid ──────────────────────────

    #[test]
    fn annotation_without_reason_invalid() {
        let ann = ExpectedDivergenceAnnotation {
            position: 0,
            reason: String::new(),
            pr_reference: "PR-1".into(),
            definition_change_hash: String::new(),
        };
        assert!(ann.validate().is_err());
    }

    // ── Invalid annotation doesn't exclude ────────────────────────────

    #[test]
    fn invalid_annotation_no_exclude() {
        let eval = GateEvaluator::with_defaults();
        let report = report_with_divs(vec![make_div("Critical", "pol_auth", 42)]);
        let ctx = EvaluationContext {
            annotations: vec![ExpectedDivergenceAnnotation {
                position: 42,
                reason: "reason".into(),
                pr_reference: String::new(), // invalid!
                definition_change_hash: String::new(),
            }],
            ..Default::default()
        };
        let result = eval.evaluate(&report, &ctx);
        assert!(result.is_fail());
    }

    // ── Budget from TOML ──────────────────────────────────────────────

    #[test]
    fn budget_from_toml() {
        let toml = r#"
max_critical = 1
max_high = 2
max_medium = 10
skip_budget_percent = 15.0
time_budget_ms = 3600000
"#;
        let b = RegressionBudget::from_toml(toml).unwrap();
        assert_eq!(b.max_critical, 1);
        assert_eq!(b.max_high, 2);
        assert_eq!(b.max_medium, 10);
        assert!((b.skip_budget_percent - 15.0).abs() < f64::EPSILON);
        assert_eq!(b.time_budget_ms, 3_600_000);
    }

    // ── Budget TOML roundtrip ─────────────────────────────────────────

    #[test]
    fn budget_toml_roundtrip() {
        let b = RegressionBudget::default();
        let toml_str = b.to_toml().unwrap();
        let restored = RegressionBudget::from_toml(&toml_str).unwrap();
        assert_eq!(restored.max_critical, b.max_critical);
        assert_eq!(restored.max_high, b.max_high);
        assert_eq!(restored.max_medium, b.max_medium);
    }

    // ── Custom budget relaxed thresholds ──────────────────────────────

    #[test]
    fn custom_budget_relaxed() {
        let budget = RegressionBudget {
            max_critical: 0,
            max_high: 2,
            max_medium: 20,
            skip_budget_percent: 25.0,
            time_budget_ms: 7_200_000,
        };
        let eval = GateEvaluator::new(budget);
        let divs = vec![
            make_div("High", "r1", 0),
            make_div("High", "r2", 1),
            make_div("Medium", "r3", 2),
        ];
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        assert!(result.is_pass());
    }

    // ── Per-artifact tolerance overrides ──────────────────────────────

    #[test]
    fn per_artifact_tolerance() {
        // Strict budget fails with 1 high.
        let strict = GateEvaluator::with_defaults();
        let report = report_with_divs(vec![make_div("High", "wf_deploy", 0)]);
        assert!(strict.evaluate_simple(&report).is_fail());

        // Relaxed budget passes with 1 high.
        let relaxed = GateEvaluator::new(RegressionBudget {
            max_high: 1,
            ..Default::default()
        });
        assert!(relaxed.evaluate_simple(&report).is_pass());
    }

    // ── Multiple violations ───────────────────────────────────────────

    #[test]
    fn multiple_violations() {
        let eval = GateEvaluator::with_defaults();
        let divs = vec![make_div("Critical", "r1", 0), make_div("High", "r2", 1)];
        let report = report_with_divs(divs);
        let ctx = EvaluationContext {
            total_artifacts: 100,
            skipped_artifacts: 15,
            replay_duration_ms: 2_000_000,
            ..Default::default()
        };
        let result = eval.evaluate(&report, &ctx);
        assert!(result.is_fail());
        // Critical + High + skip + time = 4 violations.
        assert_eq!(result.violations().len(), 4);
    }

    // ── Violation contains rule IDs ───────────────────────────────────

    #[test]
    fn violation_has_rule_ids() {
        let eval = GateEvaluator::with_defaults();
        let divs = vec![
            make_div("Critical", "pol_auth", 0),
            make_div("Critical", "pol_rate_limit", 1),
        ];
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        let violation = &result.violations()[0];
        assert!(violation.contributing_rule_ids.contains(&"pol_auth".into()));
        assert!(
            violation
                .contributing_rule_ids
                .contains(&"pol_rate_limit".into())
        );
    }

    // ── GateResult serde roundtrip ────────────────────────────────────

    #[test]
    fn gate_result_serde_pass() {
        let r = GateResult::Pass;
        let json = serde_json::to_string(&r).unwrap();
        let restored: GateResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, r);
    }

    #[test]
    fn gate_result_serde_fail() {
        let r = GateResult::Fail(vec![Violation {
            budget_dimension: "max_critical".into(),
            limit: "0".into(),
            actual: "1".into(),
            excess: "1".into(),
            contributing_rule_ids: vec!["r1".into()],
        }]);
        let json = serde_json::to_string(&r).unwrap();
        let restored: GateResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, r);
    }

    #[test]
    fn gate_result_serde_warn() {
        let r = GateResult::Warn(vec![Warning {
            message: "Medium at 80%".into(),
            budget_dimension: "max_medium".into(),
            usage_percent: 80.0,
        }]);
        let json = serde_json::to_string(&r).unwrap();
        let restored: GateResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, r);
    }

    // ── Annotation serde roundtrip ────────────────────────────────────

    #[test]
    fn annotation_serde() {
        let ann = ExpectedDivergenceAnnotation {
            position: 42,
            reason: "test".into(),
            pr_reference: "PR-1".into(),
            definition_change_hash: "abc".into(),
        };
        let json = serde_json::to_string(&ann).unwrap();
        let restored: ExpectedDivergenceAnnotation = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.position, 42);
        assert_eq!(restored.pr_reference, "PR-1");
    }

    // ── Budget accessor ───────────────────────────────────────────────

    #[test]
    fn budget_accessor() {
        let budget = RegressionBudget {
            max_critical: 3,
            ..Default::default()
        };
        let eval = GateEvaluator::new(budget);
        assert_eq!(eval.budget().max_critical, 3);
    }

    // ── Warning generation ────────────────────────────────────────────

    #[test]
    fn warning_at_threshold() {
        let budget = RegressionBudget {
            max_medium: 10,
            ..Default::default()
        };
        let eval = GateEvaluator::new(budget);
        // 8 medium = 80% of budget → warning.
        let divs: Vec<_> = (0..8)
            .map(|i| make_div("Medium", &format!("r_{}", i), i))
            .collect();
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        let is_warn = matches!(result, GateResult::Warn(_));
        assert!(is_warn);
        assert_eq!(result.warnings().len(), 1);
        assert_eq!(result.warnings()[0].budget_dimension, "max_medium");
    }

    // ── GateResult helper methods ─────────────────────────────────────

    #[test]
    fn gate_result_helpers() {
        assert!(GateResult::Pass.is_pass());
        assert!(!GateResult::Pass.is_fail());

        let fail = GateResult::Fail(vec![]);
        assert!(fail.is_fail());
        assert!(!fail.is_pass());

        let warn = GateResult::Warn(vec![]);
        assert!(warn.is_pass());
        assert!(!warn.is_fail());
    }

    // ── Malformed TOML rejected ───────────────────────────────────────

    #[test]
    fn malformed_toml_rejected() {
        let result = RegressionBudget::from_toml("not valid [[[toml");
        assert!(result.is_err());
    }

    // ── Zero total artifacts → no skip check ──────────────────────────

    #[test]
    fn zero_artifacts_no_skip_check() {
        let eval = GateEvaluator::with_defaults();
        let ctx = EvaluationContext {
            total_artifacts: 0,
            skipped_artifacts: 5,
            ..Default::default()
        };
        let result = eval.evaluate(&empty_report(), &ctx);
        assert_eq!(result, GateResult::Pass);
    }

    // ── Mixed severities counted correctly ────────────────────────────

    #[test]
    fn mixed_severities() {
        let eval = GateEvaluator::with_defaults();
        let divs = vec![
            make_div("Medium", "r1", 0),
            make_div("Low", "r2", 1),
            make_div("Medium", "r3", 2),
            make_div("Info", "r4", 3),
        ];
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        // Only 2 medium, budget is 5 → pass.
        assert!(result.is_pass());
    }

    // ── Excess calculation ────────────────────────────────────────────

    #[test]
    fn excess_calculation() {
        let budget = RegressionBudget {
            max_medium: 2,
            ..Default::default()
        };
        let eval = GateEvaluator::new(budget);
        let divs: Vec<_> = (0..5)
            .map(|i| make_div("Medium", &format!("r_{}", i), i))
            .collect();
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        assert_eq!(result.violations()[0].excess, "3"); // 5 - 2 = 3
    }
}
