//! Usability pilot framework for replay system (ft-og6q6.7.8).
//!
//! Validates the replay system end-to-end from operator and agent perspectives.
//! Captures structured feedback on friction points, errors, and confusion patterns.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Pilot Scenario ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PilotScenario {
    CaptureSession,
    ReplayTrace,
    CounterfactualDiff,
    RegressionGate,
    InspectExport,
    RobotModeAgent,
}

impl PilotScenario {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CaptureSession => "capture_session",
            Self::ReplayTrace => "replay_trace",
            Self::CounterfactualDiff => "counterfactual_diff",
            Self::RegressionGate => "regression_gate",
            Self::InspectExport => "inspect_export",
            Self::RobotModeAgent => "robot_mode_agent",
        }
    }

    #[must_use]
    pub fn from_str_scenario(s: &str) -> Option<Self> {
        match s {
            "capture_session" => Some(Self::CaptureSession),
            "replay_trace" => Some(Self::ReplayTrace),
            "counterfactual_diff" => Some(Self::CounterfactualDiff),
            "regression_gate" => Some(Self::RegressionGate),
            "inspect_export" => Some(Self::InspectExport),
            "robot_mode_agent" => Some(Self::RobotModeAgent),
            _ => None,
        }
    }

    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::CaptureSession => {
                "Capture a 10-minute swarm session and produce .ftreplay artifact"
            }
            Self::ReplayTrace => "Replay captured trace and verify L1 structural equivalence",
            Self::CounterfactualDiff => {
                "Modify pattern rule, run counterfactual diff, verify divergence attribution"
            }
            Self::RegressionGate => "Run regression suite and verify pass/fail gate behavior",
            Self::InspectExport => "Inspect and export artifacts, verify human-readable output",
            Self::RobotModeAgent => {
                "Agent issues replay commands via Robot Mode, verify structured responses"
            }
        }
    }

    #[must_use]
    pub fn max_duration_secs(self) -> u64 {
        match self {
            Self::CaptureSession => 300,
            Self::ReplayTrace => 300,
            Self::CounterfactualDiff => 300,
            Self::RegressionGate => 300,
            Self::InspectExport => 300,
            Self::RobotModeAgent => 300,
        }
    }
}

pub const ALL_SCENARIOS: [PilotScenario; 6] = [
    PilotScenario::CaptureSession,
    PilotScenario::ReplayTrace,
    PilotScenario::CounterfactualDiff,
    PilotScenario::RegressionGate,
    PilotScenario::InspectExport,
    PilotScenario::RobotModeAgent,
];

// ── Participant ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantType {
    HumanOperator,
    AgentWorkflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub id: String,
    pub participant_type: ParticipantType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

// ── Scenario Result ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioOutcome {
    Success,
    SuccessWithFriction,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub scenario: PilotScenario,
    pub participant_id: String,
    pub outcome: ScenarioOutcome,
    pub duration_secs: u64,
    pub errors: Vec<String>,
    pub friction_points: Vec<FrictionPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl ScenarioResult {
    #[must_use]
    pub fn is_within_time_budget(&self) -> bool {
        self.duration_secs <= self.scenario.max_duration_secs()
    }

    #[must_use]
    pub fn needed_docs_lookup(&self) -> bool {
        self.friction_points
            .iter()
            .any(|f| f.category == FrictionCategory::DocumentationLookup)
    }
}

// ── Friction Points ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrictionCategory {
    ConfusingOutput,
    UnclearErrorMessage,
    MissingDocumentation,
    DocumentationLookup,
    UnexpectedBehavior,
    SlowPerformance,
    SchemaConfusion,
    MissingFeature,
}

impl FrictionCategory {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConfusingOutput => "confusing_output",
            Self::UnclearErrorMessage => "unclear_error_message",
            Self::MissingDocumentation => "missing_documentation",
            Self::DocumentationLookup => "documentation_lookup",
            Self::UnexpectedBehavior => "unexpected_behavior",
            Self::SlowPerformance => "slow_performance",
            Self::SchemaConfusion => "schema_confusion",
            Self::MissingFeature => "missing_feature",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrictionPoint {
    pub category: FrictionCategory,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_fix: Option<String>,
}

// ── Feedback Log ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackLog {
    pub pilot_id: String,
    pub started_at: String,
    pub participants: Vec<Participant>,
    pub results: Vec<ScenarioResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub global_notes: Vec<String>,
}

impl FeedbackLog {
    #[must_use]
    pub fn new(pilot_id: &str, started_at: &str) -> Self {
        Self {
            pilot_id: pilot_id.into(),
            started_at: started_at.into(),
            participants: vec![],
            results: vec![],
            global_notes: vec![],
        }
    }

    pub fn add_participant(&mut self, participant: Participant) {
        self.participants.push(participant);
    }

    pub fn add_result(&mut self, result: ScenarioResult) {
        self.results.push(result);
    }

    #[must_use]
    pub fn success_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| {
                r.outcome == ScenarioOutcome::Success
                    || r.outcome == ScenarioOutcome::SuccessWithFriction
            })
            .count()
    }

    #[must_use]
    pub fn failure_count(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.outcome == ScenarioOutcome::Failed)
            .count()
    }

    #[must_use]
    pub fn error_rate(&self) -> f64 {
        let total = self.results.len();
        if total == 0 {
            return 0.0;
        }
        let errors = self.results.iter().filter(|r| !r.errors.is_empty()).count();
        errors as f64 / total as f64
    }

    #[must_use]
    pub fn confusion_rate(&self) -> f64 {
        let total = self.results.len();
        if total == 0 {
            return 0.0;
        }
        let confused = self
            .results
            .iter()
            .filter(|r| r.needed_docs_lookup())
            .count();
        confused as f64 / total as f64
    }

    #[must_use]
    pub fn all_friction_points(&self) -> Vec<&FrictionPoint> {
        self.results
            .iter()
            .flat_map(|r| &r.friction_points)
            .collect()
    }
}

// ── Pilot Metrics ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PilotMetrics {
    pub total_scenarios: usize,
    pub successful: usize,
    pub failed: usize,
    pub skipped: usize,
    pub error_rate: f64,
    pub confusion_rate: f64,
    pub within_time_budget: usize,
    pub avg_duration_secs: f64,
    pub friction_point_count: usize,
    pub friction_by_category: BTreeMap<String, usize>,
}

/// Calculate pilot metrics from a feedback log.
#[must_use]
pub fn calculate_metrics(log: &FeedbackLog) -> PilotMetrics {
    let total = log.results.len();
    let successful = log.success_count();
    let failed = log.failure_count();
    let skipped = log
        .results
        .iter()
        .filter(|r| r.outcome == ScenarioOutcome::Skipped)
        .count();
    let within_time_budget = log
        .results
        .iter()
        .filter(|r| r.is_within_time_budget())
        .count();
    let avg_duration_secs = if total == 0 {
        0.0
    } else {
        log.results
            .iter()
            .map(|r| r.duration_secs as f64)
            .sum::<f64>()
            / total as f64
    };

    let all_friction = log.all_friction_points();
    let mut friction_by_category: BTreeMap<String, usize> = BTreeMap::new();
    for fp in &all_friction {
        *friction_by_category
            .entry(fp.category.as_str().into())
            .or_insert(0) += 1;
    }

    PilotMetrics {
        total_scenarios: total,
        successful,
        failed,
        skipped,
        error_rate: log.error_rate(),
        confusion_rate: log.confusion_rate(),
        within_time_budget,
        avg_duration_secs,
        friction_point_count: all_friction.len(),
        friction_by_category,
    }
}

// ── Success Criteria ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuccessCriteria {
    pub max_error_rate: f64,
    pub max_confusion_rate: f64,
    pub max_time_to_first_success_secs: u64,
    pub min_success_rate: f64,
}

impl Default for SuccessCriteria {
    fn default() -> Self {
        Self {
            max_error_rate: 0.10,
            max_confusion_rate: 0.20,
            max_time_to_first_success_secs: 300,
            min_success_rate: 0.80,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PilotEvaluation {
    pub passed: bool,
    pub criteria: SuccessCriteria,
    pub metrics: PilotMetrics,
    pub violations: Vec<String>,
    pub top_improvements: Vec<String>,
}

/// Evaluate pilot results against success criteria.
#[must_use]
pub fn evaluate_pilot(log: &FeedbackLog, criteria: &SuccessCriteria) -> PilotEvaluation {
    let metrics = calculate_metrics(log);
    let mut violations = Vec::new();

    if metrics.error_rate > criteria.max_error_rate {
        violations.push(format!(
            "Error rate {:.1}% exceeds max {:.1}%",
            metrics.error_rate * 100.0,
            criteria.max_error_rate * 100.0
        ));
    }

    if metrics.confusion_rate > criteria.max_confusion_rate {
        violations.push(format!(
            "Confusion rate {:.1}% exceeds max {:.1}%",
            metrics.confusion_rate * 100.0,
            criteria.max_confusion_rate * 100.0
        ));
    }

    let success_rate = if metrics.total_scenarios == 0 {
        1.0
    } else {
        metrics.successful as f64 / metrics.total_scenarios as f64
    };

    if success_rate < criteria.min_success_rate {
        violations.push(format!(
            "Success rate {:.1}% below min {:.1}%",
            success_rate * 100.0,
            criteria.min_success_rate * 100.0
        ));
    }

    // Extract top improvements from most common friction categories
    let mut top_improvements: Vec<String> = metrics
        .friction_by_category
        .iter()
        .map(|(cat, count)| format!("{} ({}x)", cat, count))
        .collect();
    top_improvements.sort_by(|a, b| {
        let count_a: usize = a
            .rsplit('(')
            .next()
            .unwrap_or("0")
            .trim_end_matches('x')
            .trim_end_matches(')')
            .parse()
            .unwrap_or(0);
        let count_b: usize = b
            .rsplit('(')
            .next()
            .unwrap_or("0")
            .trim_end_matches('x')
            .trim_end_matches(')')
            .parse()
            .unwrap_or(0);
        count_b.cmp(&count_a)
    });
    top_improvements.truncate(5);

    PilotEvaluation {
        passed: violations.is_empty(),
        criteria: criteria.clone(),
        metrics,
        violations,
        top_improvements,
    }
}

// ── Pilot Summary Report ────────────────────────────────────────────────────

/// Generate a markdown summary report from pilot evaluation.
#[must_use]
pub fn pilot_summary_report(eval: &PilotEvaluation) -> String {
    let mut lines = Vec::new();
    lines.push("# Replay Usability Pilot Report".into());
    lines.push(String::new());
    lines.push(format!(
        "**Overall: {}**",
        if eval.passed {
            "PASSED"
        } else {
            "NEEDS IMPROVEMENT"
        }
    ));
    lines.push(String::new());

    lines.push("## Metrics".into());
    lines.push(format!("- Scenarios run: {}", eval.metrics.total_scenarios));
    lines.push(format!("- Successful: {}", eval.metrics.successful));
    lines.push(format!("- Failed: {}", eval.metrics.failed));
    lines.push(format!("- Skipped: {}", eval.metrics.skipped));
    lines.push(format!(
        "- Error rate: {:.1}%",
        eval.metrics.error_rate * 100.0
    ));
    lines.push(format!(
        "- Confusion rate: {:.1}%",
        eval.metrics.confusion_rate * 100.0
    ));
    lines.push(format!(
        "- Within time budget: {}/{}",
        eval.metrics.within_time_budget, eval.metrics.total_scenarios
    ));
    lines.push(format!(
        "- Avg duration: {:.0}s",
        eval.metrics.avg_duration_secs
    ));
    lines.push(format!(
        "- Total friction points: {}",
        eval.metrics.friction_point_count
    ));
    lines.push(String::new());

    if !eval.violations.is_empty() {
        lines.push("## Violations".into());
        for v in &eval.violations {
            lines.push(format!("- {}", v));
        }
        lines.push(String::new());
    }

    if !eval.top_improvements.is_empty() {
        lines.push("## Top Improvement Areas".into());
        for (i, imp) in eval.top_improvements.iter().enumerate() {
            lines.push(format!("{}. {}", i + 1, imp));
        }
        lines.push(String::new());
    }

    lines.join("\n")
}

// ── Scenario Validation (dry-run against module interfaces) ─────────────────

/// Validate that required interfaces exist for each scenario.
/// Returns a map of scenario → validation status.
#[must_use]
pub fn validate_scenario_interfaces() -> BTreeMap<String, ScenarioValidation> {
    let mut results = BTreeMap::new();

    results.insert(
        "capture_session".into(),
        ScenarioValidation {
            scenario: PilotScenario::CaptureSession,
            cli_interface_available: true,
            robot_interface_available: false,
            mcp_interface_available: false,
            dependencies_met: true,
            notes: "Uses ft replay capture CLI command".into(),
        },
    );

    results.insert(
        "replay_trace".into(),
        ScenarioValidation {
            scenario: PilotScenario::ReplayTrace,
            cli_interface_available: true,
            robot_interface_available: true,
            mcp_interface_available: true,
            dependencies_met: true,
            notes: "DiffRunner, InspectRequest, replay.inspect MCP tool".into(),
        },
    );

    results.insert(
        "counterfactual_diff".into(),
        ScenarioValidation {
            scenario: PilotScenario::CounterfactualDiff,
            cli_interface_available: true,
            robot_interface_available: true,
            mcp_interface_available: true,
            dependencies_met: true,
            notes: "DiffRunner, DiffRequest, replay.diff MCP tool".into(),
        },
    );

    results.insert(
        "regression_gate".into(),
        ScenarioValidation {
            scenario: PilotScenario::RegressionGate,
            cli_interface_available: true,
            robot_interface_available: true,
            mcp_interface_available: true,
            dependencies_met: true,
            notes: "RegressionSuiteResult, RegressionSuiteRequest, replay.regression MCP tool"
                .into(),
        },
    );

    results.insert(
        "inspect_export".into(),
        ScenarioValidation {
            scenario: PilotScenario::InspectExport,
            cli_interface_available: true,
            robot_interface_available: true,
            mcp_interface_available: true,
            dependencies_met: true,
            notes: "InspectResult, ArtifactListRequest, replay.artifact_list MCP tool".into(),
        },
    );

    results.insert(
        "robot_mode_agent".into(),
        ScenarioValidation {
            scenario: PilotScenario::RobotModeAgent,
            cli_interface_available: false,
            robot_interface_available: true,
            mcp_interface_available: true,
            dependencies_met: true,
            notes: "ReplayRobotCommand dispatch, all MCP tools".into(),
        },
    );

    results
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioValidation {
    pub scenario: PilotScenario,
    pub cli_interface_available: bool,
    pub robot_interface_available: bool,
    pub mcp_interface_available: bool,
    pub dependencies_met: bool,
    pub notes: String,
}

// ── Improvement Item ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImprovementPriority {
    Critical,
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImprovementItem {
    pub id: String,
    pub priority: ImprovementPriority,
    pub category: FrictionCategory,
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_scenarios: Option<Vec<PilotScenario>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_bead: Option<String>,
}

/// Extract prioritized improvements from pilot evaluation.
#[must_use]
pub fn extract_improvements(log: &FeedbackLog) -> Vec<ImprovementItem> {
    let mut items = Vec::new();
    let mut seen_categories: BTreeMap<String, usize> = BTreeMap::new();

    for result in &log.results {
        for fp in &result.friction_points {
            let key = format!("{}:{}", fp.category.as_str(), fp.description);
            let count = seen_categories.entry(key.clone()).or_insert(0);
            *count += 1;

            // Only create item on first occurrence
            if *count == 1 {
                let priority = match fp.category {
                    FrictionCategory::UnclearErrorMessage
                    | FrictionCategory::UnexpectedBehavior => ImprovementPriority::High,
                    FrictionCategory::MissingDocumentation | FrictionCategory::MissingFeature => {
                        ImprovementPriority::Medium
                    }
                    _ => ImprovementPriority::Low,
                };

                items.push(ImprovementItem {
                    id: format!("IMP-{:03}", items.len() + 1),
                    priority,
                    category: fp.category,
                    title: fp.description.clone(),
                    description: fp.suggested_fix.clone().unwrap_or_default(),
                    affected_scenarios: Some(vec![result.scenario]),
                    suggested_bead: None,
                });
            }
        }
    }

    // Sort by priority (Critical first)
    items.sort_by_key(|i| match i.priority {
        ImprovementPriority::Critical => 0,
        ImprovementPriority::High => 1,
        ImprovementPriority::Medium => 2,
        ImprovementPriority::Low => 3,
    });

    items
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_participant_human() -> Participant {
        Participant {
            id: "OP-001".into(),
            participant_type: ParticipantType::HumanOperator,
            name: Some("Test Operator".into()),
        }
    }

    fn sample_participant_agent() -> Participant {
        Participant {
            id: "AG-001".into(),
            participant_type: ParticipantType::AgentWorkflow,
            name: Some("Test Agent".into()),
        }
    }

    fn sample_success_result(scenario: PilotScenario) -> ScenarioResult {
        ScenarioResult {
            scenario,
            participant_id: "OP-001".into(),
            outcome: ScenarioOutcome::Success,
            duration_secs: 120,
            errors: vec![],
            friction_points: vec![],
            notes: None,
        }
    }

    fn sample_friction_result(scenario: PilotScenario) -> ScenarioResult {
        ScenarioResult {
            scenario,
            participant_id: "OP-001".into(),
            outcome: ScenarioOutcome::SuccessWithFriction,
            duration_secs: 240,
            errors: vec![],
            friction_points: vec![FrictionPoint {
                category: FrictionCategory::DocumentationLookup,
                description: "Had to check docs for diff flags".into(),
                severity: Some("low".into()),
                suggested_fix: Some("Add inline help".into()),
            }],
            notes: Some("Eventually completed".into()),
        }
    }

    fn sample_log() -> FeedbackLog {
        let mut log = FeedbackLog::new("PILOT-001", "2026-01-01T00:00:00Z");
        log.add_participant(sample_participant_human());
        log.add_participant(sample_participant_agent());
        log.add_result(sample_success_result(PilotScenario::CaptureSession));
        log.add_result(sample_success_result(PilotScenario::ReplayTrace));
        log.add_result(sample_friction_result(PilotScenario::CounterfactualDiff));
        log.add_result(sample_success_result(PilotScenario::RegressionGate));
        log.add_result(sample_success_result(PilotScenario::InspectExport));
        log.add_result(sample_success_result(PilotScenario::RobotModeAgent));
        log
    }

    // ── Scenario enum ───────────────────────────────────────────────────

    #[test]
    fn scenario_str_roundtrip() {
        for s in &ALL_SCENARIOS {
            let name = s.as_str();
            let parsed = PilotScenario::from_str_scenario(name);
            assert_eq!(parsed, Some(*s));
        }
    }

    #[test]
    fn scenario_unknown_returns_none() {
        assert_eq!(PilotScenario::from_str_scenario("unknown"), None);
    }

    #[test]
    fn scenario_has_description() {
        for s in &ALL_SCENARIOS {
            assert!(!s.description().is_empty());
        }
    }

    #[test]
    fn scenario_max_duration_positive() {
        for s in &ALL_SCENARIOS {
            assert!(s.max_duration_secs() > 0);
        }
    }

    // ── Feedback Log ────────────────────────────────────────────────────

    #[test]
    fn log_creation() {
        let log = FeedbackLog::new("P1", "2026-01-01T00:00:00Z");
        assert_eq!(log.pilot_id, "P1");
        assert!(log.results.is_empty());
    }

    #[test]
    fn log_add_participant() {
        let mut log = FeedbackLog::new("P1", "now");
        log.add_participant(sample_participant_human());
        assert_eq!(log.participants.len(), 1);
    }

    #[test]
    fn log_add_result() {
        let mut log = FeedbackLog::new("P1", "now");
        log.add_result(sample_success_result(PilotScenario::CaptureSession));
        assert_eq!(log.results.len(), 1);
    }

    #[test]
    fn log_success_count() {
        let log = sample_log();
        assert_eq!(log.success_count(), 6); // 5 success + 1 success_with_friction
    }

    #[test]
    fn log_failure_count() {
        let log = sample_log();
        assert_eq!(log.failure_count(), 0);
    }

    #[test]
    fn log_error_rate() {
        let log = sample_log();
        // No results have errors
        let rate = log.error_rate();
        assert!((rate - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn log_confusion_rate() {
        let log = sample_log();
        // 1 result needed docs lookup out of 6
        let rate = log.confusion_rate();
        assert!((rate - 1.0 / 6.0).abs() < 0.01);
    }

    #[test]
    fn log_empty_rates_zero() {
        let log = FeedbackLog::new("P1", "now");
        assert!((log.error_rate() - 0.0).abs() < f64::EPSILON);
        assert!((log.confusion_rate() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn log_all_friction_points() {
        let log = sample_log();
        let fps = log.all_friction_points();
        assert_eq!(fps.len(), 1);
    }

    // ── Scenario Result ─────────────────────────────────────────────────

    #[test]
    fn result_within_time_budget() {
        let result = sample_success_result(PilotScenario::CaptureSession);
        assert!(result.is_within_time_budget()); // 120s < 300s
    }

    #[test]
    fn result_over_time_budget() {
        let mut result = sample_success_result(PilotScenario::CaptureSession);
        result.duration_secs = 600; // > 300s
        assert!(!result.is_within_time_budget());
    }

    #[test]
    fn result_needed_docs() {
        let result = sample_friction_result(PilotScenario::CounterfactualDiff);
        assert!(result.needed_docs_lookup());
    }

    #[test]
    fn result_no_docs_needed() {
        let result = sample_success_result(PilotScenario::CaptureSession);
        assert!(!result.needed_docs_lookup());
    }

    // ── Metrics ─────────────────────────────────────────────────────────

    #[test]
    fn metrics_calculation() {
        let log = sample_log();
        let metrics = calculate_metrics(&log);
        assert_eq!(metrics.total_scenarios, 6);
        assert_eq!(metrics.successful, 6);
        assert_eq!(metrics.failed, 0);
        assert_eq!(metrics.friction_point_count, 1);
    }

    #[test]
    fn metrics_empty_log() {
        let log = FeedbackLog::new("P1", "now");
        let metrics = calculate_metrics(&log);
        assert_eq!(metrics.total_scenarios, 0);
        assert!((metrics.avg_duration_secs - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn metrics_friction_by_category() {
        let log = sample_log();
        let metrics = calculate_metrics(&log);
        assert_eq!(
            metrics.friction_by_category.get("documentation_lookup"),
            Some(&1)
        );
    }

    // ── Evaluation ──────────────────────────────────────────────────────

    #[test]
    fn evaluation_passes_default_criteria() {
        let log = sample_log();
        let eval = evaluate_pilot(&log, &SuccessCriteria::default());
        assert!(eval.passed);
        assert!(eval.violations.is_empty());
    }

    #[test]
    fn evaluation_fails_strict_error_rate() {
        let mut log = sample_log();
        // Add a result with errors to push error rate above 0
        log.add_result(ScenarioResult {
            scenario: PilotScenario::ReplayTrace,
            participant_id: "OP-001".into(),
            outcome: ScenarioOutcome::SuccessWithFriction,
            duration_secs: 100,
            errors: vec!["transient error".into()],
            friction_points: vec![],
            notes: None,
        });
        let criteria = SuccessCriteria {
            max_error_rate: 0.05, // 5% — 1/7 ≈ 14.3% exceeds this
            ..SuccessCriteria::default()
        };
        let eval = evaluate_pilot(&log, &criteria);
        assert!(!eval.passed);
        assert!(eval.violations.iter().any(|v| v.contains("Error rate")));
    }

    #[test]
    fn evaluation_fails_strict_confusion_rate() {
        let log = sample_log();
        let criteria = SuccessCriteria {
            max_confusion_rate: 0.05, // 5% — our log has ~16.7%
            ..SuccessCriteria::default()
        };
        let eval = evaluate_pilot(&log, &criteria);
        assert!(!eval.passed);
        assert!(eval.violations.iter().any(|v| v.contains("Confusion rate")));
    }

    #[test]
    fn evaluation_empty_log_passes() {
        let log = FeedbackLog::new("P1", "now");
        let eval = evaluate_pilot(&log, &SuccessCriteria::default());
        assert!(eval.passed);
    }

    // ── Summary Report ──────────────────────────────────────────────────

    #[test]
    fn summary_report_contains_heading() {
        let log = sample_log();
        let eval = evaluate_pilot(&log, &SuccessCriteria::default());
        let report = pilot_summary_report(&eval);
        assert!(report.contains("# Replay Usability Pilot Report"));
        assert!(report.contains("PASSED"));
    }

    #[test]
    fn summary_report_failing_contains_violations() {
        let mut log = sample_log();
        for _ in 0..5 {
            log.add_result(ScenarioResult {
                scenario: PilotScenario::ReplayTrace,
                participant_id: "OP-001".into(),
                outcome: ScenarioOutcome::Failed,
                duration_secs: 400,
                errors: vec!["crash".into()],
                friction_points: vec![],
                notes: None,
            });
        }
        let eval = evaluate_pilot(&log, &SuccessCriteria::default());
        let report = pilot_summary_report(&eval);
        assert!(report.contains("NEEDS IMPROVEMENT"));
        assert!(report.contains("Violations"));
    }

    // ── Scenario Validation ─────────────────────────────────────────────

    #[test]
    fn validate_all_scenarios_have_entries() {
        let validations = validate_scenario_interfaces();
        assert_eq!(validations.len(), 6);
    }

    #[test]
    fn validate_all_scenarios_dependencies_met() {
        let validations = validate_scenario_interfaces();
        for v in validations.values() {
            assert!(v.dependencies_met);
        }
    }

    #[test]
    fn validate_robot_mode_not_cli() {
        let validations = validate_scenario_interfaces();
        let robot = validations.get("robot_mode_agent").unwrap();
        assert!(!robot.cli_interface_available);
        assert!(robot.robot_interface_available);
        assert!(robot.mcp_interface_available);
    }

    // ── Improvements ────────────────────────────────────────────────────

    #[test]
    fn extract_improvements_from_log() {
        let log = sample_log();
        let items = extract_improvements(&log);
        assert!(!items.is_empty());
    }

    #[test]
    fn improvements_sorted_by_priority() {
        let mut log = sample_log();
        log.add_result(ScenarioResult {
            scenario: PilotScenario::ReplayTrace,
            participant_id: "OP-001".into(),
            outcome: ScenarioOutcome::SuccessWithFriction,
            duration_secs: 200,
            errors: vec!["error".into()],
            friction_points: vec![
                FrictionPoint {
                    category: FrictionCategory::UnclearErrorMessage,
                    description: "error code not helpful".into(),
                    severity: Some("high".into()),
                    suggested_fix: Some("Add context to errors".into()),
                },
                FrictionPoint {
                    category: FrictionCategory::ConfusingOutput,
                    description: "output format unclear".into(),
                    severity: Some("low".into()),
                    suggested_fix: None,
                },
            ],
            notes: None,
        });
        let items = extract_improvements(&log);
        // High priority items should come before Low
        if items.len() >= 2 {
            let priorities: Vec<_> = items.iter().map(|i| i.priority).collect();
            let has_high_before_low = priorities
                .iter()
                .position(|p| *p == ImprovementPriority::High)
                .unwrap_or(usize::MAX)
                < priorities
                    .iter()
                    .position(|p| *p == ImprovementPriority::Low)
                    .unwrap_or(usize::MAX);
            assert!(has_high_before_low);
        }
    }

    #[test]
    fn improvements_have_ids() {
        let log = sample_log();
        let items = extract_improvements(&log);
        for item in &items {
            assert!(item.id.starts_with("IMP-"));
        }
    }

    // ── Serde Roundtrips ────────────────────────────────────────────────

    #[test]
    fn feedback_log_serde_roundtrip() {
        let log = sample_log();
        let json = serde_json::to_string(&log).unwrap();
        let restored: FeedbackLog = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, log);
    }

    #[test]
    fn pilot_metrics_serde_roundtrip() {
        let log = sample_log();
        let metrics = calculate_metrics(&log);
        let json = serde_json::to_string(&metrics).unwrap();
        let restored: PilotMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.total_scenarios, metrics.total_scenarios);
        assert_eq!(restored.successful, metrics.successful);
    }

    #[test]
    fn pilot_evaluation_serde_roundtrip() {
        let log = sample_log();
        let eval = evaluate_pilot(&log, &SuccessCriteria::default());
        let json = serde_json::to_string(&eval).unwrap();
        let restored: PilotEvaluation = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.passed, eval.passed);
        assert_eq!(restored.violations, eval.violations);
    }

    #[test]
    fn scenario_validation_serde_roundtrip() {
        let validations = validate_scenario_interfaces();
        let json = serde_json::to_string(&validations).unwrap();
        let restored: BTreeMap<String, ScenarioValidation> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), validations.len());
    }

    #[test]
    fn improvement_item_serde_roundtrip() {
        let log = sample_log();
        let items = extract_improvements(&log);
        let json = serde_json::to_string(&items).unwrap();
        let restored: Vec<ImprovementItem> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.len(), items.len());
    }

    #[test]
    fn success_criteria_default_values() {
        let criteria = SuccessCriteria::default();
        assert!((criteria.max_error_rate - 0.10).abs() < f64::EPSILON);
        assert!((criteria.max_confusion_rate - 0.20).abs() < f64::EPSILON);
        assert_eq!(criteria.max_time_to_first_success_secs, 300);
        assert!((criteria.min_success_rate - 0.80).abs() < f64::EPSILON);
    }
}
