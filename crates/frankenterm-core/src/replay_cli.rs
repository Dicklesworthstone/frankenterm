//! Replay CLI types and output formatting (ft-og6q6.6.1).
//!
//! Provides:
//! - [`ReplayOutputMode`] — Human / Robot / Verbose / Quiet.
//! - [`ReplayExitCode`] — 0=Pass, 1=Regression, 2=InvalidInput, 3=InternalError.
//! - [`DiffOutputFormatter`] — Formats diff results for CLI display.
//! - [`InspectResult`] — Artifact inspection metadata.
//! - [`RegressionSuiteResult`] — Suite run aggregate.

use serde::{Deserialize, Serialize};

use crate::replay_decision_diff::{DecisionDiff, DiffConfig, EquivalenceLevel};
use crate::replay_decision_graph::{DecisionEvent, DecisionGraph};
use crate::replay_guardrails_gate::{
    EvaluationContext, GateEvaluator, GateResult, RegressionBudget,
};
use crate::replay_report::{ReportFormat, ReportGenerator, ReportMeta};
use crate::replay_risk_scoring::RiskScorer;

// ============================================================================
// ReplayOutputMode — controls how output is rendered
// ============================================================================

/// Output mode for replay CLI commands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReplayOutputMode {
    /// Colored terminal output for humans (default).
    #[default]
    Human,
    /// Machine-readable JSON (one envelope per line) for robot consumers.
    Robot,
    /// Human output with structured tracing and correlation IDs.
    Verbose,
    /// Errors only.
    Quiet,
}

// ============================================================================
// ReplayExitCode — process exit codes
// ============================================================================

/// Exit codes for replay CLI commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReplayExitCode {
    /// Pass (or capture completed successfully).
    Pass = 0,
    /// Regression detected / equivalence failure.
    Regression = 1,
    /// Invalid input (bad trace, missing file, schema mismatch).
    InvalidInput = 2,
    /// Internal error.
    InternalError = 3,
}

impl ReplayExitCode {
    /// Get the numeric exit code.
    #[must_use]
    pub fn code(self) -> i32 {
        self as i32
    }
}

// ============================================================================
// EquivalenceLevelArg — CLI equivalence level parameter
// ============================================================================

/// Equivalence level argument for `ft replay run`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EquivalenceLevelArg {
    /// Structural: same decisions, order may differ.
    Structural,
    /// Decision: same decisions, same attributes (ignoring timing).
    #[default]
    Decision,
    /// Full: exact match including timing.
    Full,
}

impl EquivalenceLevelArg {
    /// Convert to internal [`EquivalenceLevel`].
    #[must_use]
    pub fn to_equivalence_level(self) -> EquivalenceLevel {
        match self {
            Self::Structural => EquivalenceLevel::L0,
            Self::Decision => EquivalenceLevel::L1,
            Self::Full => EquivalenceLevel::L2,
        }
    }
}

// ============================================================================
// SpeedArg — CLI speed parameter
// ============================================================================

/// Playback speed for `ft replay run`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum SpeedArg {
    /// Real-time (1x).
    #[default]
    Normal,
    /// Double speed (2x).
    Double,
    /// As fast as possible.
    Instant,
    /// Custom multiplier.
    Custom(f64),
}

impl SpeedArg {
    /// Get the speed multiplier.
    #[must_use]
    pub fn multiplier(self) -> f64 {
        match self {
            Self::Normal => 1.0,
            Self::Double => 2.0,
            Self::Instant => f64::INFINITY,
            Self::Custom(m) => m,
        }
    }

    /// Parse from CLI string.
    pub fn from_str_arg(s: &str) -> Result<Self, String> {
        match s {
            "1x" | "1" | "normal" => Ok(Self::Normal),
            "2x" | "2" | "double" => Ok(Self::Double),
            "instant" | "inf" => Ok(Self::Instant),
            other => other
                .trim_end_matches('x')
                .parse::<f64>()
                .map(Self::Custom)
                .map_err(|_| format!("invalid speed: {}", other)),
        }
    }
}

// ============================================================================
// InspectResult — artifact metadata inspection
// ============================================================================

/// Result of `ft replay inspect` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectResult {
    /// Artifact file path.
    pub artifact_path: String,
    /// Total decision events in the trace.
    pub event_count: u64,
    /// Unique pane IDs seen.
    pub pane_count: u64,
    /// Unique rule IDs seen.
    pub rule_count: u64,
    /// Time span in ms (first to last event).
    pub time_span_ms: u64,
    /// Decision types present.
    pub decision_types: Vec<String>,
    /// Whether the artifact integrity is valid.
    pub integrity_ok: bool,
}

impl InspectResult {
    /// Build from a list of decision events.
    #[must_use]
    pub fn from_events(artifact_path: &str, events: &[DecisionEvent]) -> Self {
        let mut panes = std::collections::HashSet::new();
        let mut rules = std::collections::HashSet::new();
        let mut types = std::collections::HashSet::new();
        let mut min_ts = u64::MAX;
        let mut max_ts = 0u64;

        for ev in events {
            panes.insert(ev.pane_id);
            rules.insert(ev.rule_id.clone());
            types.insert(format!("{:?}", ev.decision_type));
            min_ts = min_ts.min(ev.timestamp_ms);
            max_ts = max_ts.max(ev.timestamp_ms);
        }

        let time_span = if events.is_empty() {
            0
        } else {
            max_ts - min_ts
        };

        let mut type_vec: Vec<String> = types.into_iter().collect();
        type_vec.sort();

        Self {
            artifact_path: artifact_path.into(),
            event_count: events.len() as u64,
            pane_count: panes.len() as u64,
            rule_count: rules.len() as u64,
            time_span_ms: time_span,
            decision_types: type_vec,
            integrity_ok: true,
        }
    }

    /// Render for human output.
    #[must_use]
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Artifact:     {}\n", self.artifact_path));
        out.push_str(&format!("Events:       {}\n", self.event_count));
        out.push_str(&format!("Panes:        {}\n", self.pane_count));
        out.push_str(&format!("Rules:        {}\n", self.rule_count));
        out.push_str(&format!("Time span:    {}ms\n", self.time_span_ms));
        out.push_str(&format!(
            "Types:        {}\n",
            self.decision_types.join(", ")
        ));
        out.push_str(&format!(
            "Integrity:    {}\n",
            if self.integrity_ok { "OK" } else { "FAILED" }
        ));
        out
    }
}

// ============================================================================
// DiffRunner — orchestrates a diff run from CLI args
// ============================================================================

/// Orchestrates a full diff run given CLI parameters.
pub struct DiffRunner {
    scorer: RiskScorer,
    budget: RegressionBudget,
}

impl DiffRunner {
    /// Create with default scorer and budget.
    #[must_use]
    pub fn new() -> Self {
        Self {
            scorer: RiskScorer::new(),
            budget: RegressionBudget::default(),
        }
    }

    /// Create with custom budget.
    #[must_use]
    pub fn with_budget(budget: RegressionBudget) -> Self {
        Self {
            scorer: RiskScorer::new(),
            budget,
        }
    }

    /// Run a diff between baseline and candidate events.
    #[must_use]
    pub fn run(
        &self,
        baseline_events: &[DecisionEvent],
        candidate_events: &[DecisionEvent],
        config: &DiffConfig,
    ) -> DiffRunResult {
        let baseline = DecisionGraph::from_decisions(baseline_events);
        let candidate = DecisionGraph::from_decisions(candidate_events);
        let diff = DecisionDiff::diff(&baseline, &candidate, config);
        let risk = self.scorer.aggregate(&diff.divergences);

        let meta = ReportMeta::default();
        let generator = ReportGenerator::new(meta);
        let json_str = generator.generate(&diff, ReportFormat::Json);
        let json_report = serde_json::from_str(&json_str).ok();

        let gate_result = json_report
            .as_ref()
            .map(|r| {
                let eval = GateEvaluator::new(self.budget.clone());
                eval.evaluate(r, &EvaluationContext::default())
            })
            .unwrap_or(GateResult::Pass);

        let exit_code = match &gate_result {
            GateResult::Pass | GateResult::Warn(_) => ReplayExitCode::Pass,
            GateResult::Fail(_) => ReplayExitCode::Regression,
        };

        DiffRunResult {
            diff,
            gate_result,
            exit_code,
            recommendation: format!("{:?}", risk.recommendation),
        }
    }

    /// Format a run result for the given output mode.
    #[must_use]
    pub fn format_result(
        &self,
        result: &DiffRunResult,
        mode: ReplayOutputMode,
        meta: &ReportMeta,
    ) -> String {
        let generator = ReportGenerator::new(meta.clone());
        match mode {
            ReplayOutputMode::Human | ReplayOutputMode::Verbose => {
                generator.generate(&result.diff, ReportFormat::Human)
            }
            ReplayOutputMode::Robot => generator.generate(&result.diff, ReportFormat::Json),
            ReplayOutputMode::Quiet => {
                if result.exit_code == ReplayExitCode::Pass {
                    String::new()
                } else {
                    format!("FAIL: {}\n", result.recommendation)
                }
            }
        }
    }
}

impl Default for DiffRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a diff run.
pub struct DiffRunResult {
    /// The computed diff.
    pub diff: DecisionDiff,
    /// Gate evaluation result.
    pub gate_result: GateResult,
    /// Process exit code.
    pub exit_code: ReplayExitCode,
    /// Human-readable recommendation.
    pub recommendation: String,
}

// ============================================================================
// RegressionSuiteResult — suite aggregate
// ============================================================================

/// Result of a regression suite run (`ft replay regression-suite`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionSuiteResult {
    /// Number of artifacts tested.
    pub total_artifacts: u64,
    /// Number that passed.
    pub passed: u64,
    /// Number that failed.
    pub failed: u64,
    /// Number that errored (couldn't run).
    pub errored: u64,
    /// Per-artifact results.
    pub results: Vec<ArtifactResult>,
    /// Overall pass/fail.
    pub overall_pass: bool,
}

/// Per-artifact result in a suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactResult {
    /// Artifact path.
    pub artifact_path: String,
    /// Pass/fail.
    pub passed: bool,
    /// Gate result.
    pub gate_result_summary: String,
    /// Error message (if errored).
    pub error: Option<String>,
}

impl RegressionSuiteResult {
    /// Build from a list of artifact results.
    #[must_use]
    pub fn from_results(results: Vec<ArtifactResult>) -> Self {
        let total = results.len() as u64;
        let passed = results.iter().filter(|r| r.passed).count() as u64;
        let errored = results.iter().filter(|r| r.error.is_some()).count() as u64;
        let failed = total - passed - errored;
        let overall_pass = failed == 0 && errored == 0;
        Self {
            total_artifacts: total,
            passed,
            failed,
            errored,
            results,
            overall_pass,
        }
    }

    /// Render for human output.
    #[must_use]
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str("=== Regression Suite Results ===\n\n");
        out.push_str(&format!("Total:   {}\n", self.total_artifacts));
        out.push_str(&format!("Passed:  {}\n", self.passed));
        out.push_str(&format!("Failed:  {}\n", self.failed));
        out.push_str(&format!("Errors:  {}\n", self.errored));
        out.push_str(&format!(
            "Overall: {}\n\n",
            if self.overall_pass { "PASS" } else { "FAIL" }
        ));

        for r in &self.results {
            let status = if r.passed {
                "PASS"
            } else if r.error.is_some() {
                "ERROR"
            } else {
                "FAIL"
            };
            out.push_str(&format!("[{status}] {}\n", r.artifact_path));
            if let Some(err) = &r.error {
                out.push_str(&format!("       {}\n", err));
            }
        }
        out
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_decision_graph::DecisionType;

    fn make_event(rule_id: &str, ts: u64, pane: u64, def: &str, out: &str) -> DecisionEvent {
        DecisionEvent {
            decision_type: DecisionType::PatternMatch,
            rule_id: rule_id.into(),
            definition_hash: def.into(),
            input_hash: format!("in_{}", ts),
            output_hash: out.into(),
            timestamp_ms: ts,
            pane_id: pane,
            triggered_by: None,
            overrides: None,
            input_summary: String::new(),
            parent_event_id: None,
            confidence: None,
            wall_clock_ms: 0,
            replay_run_id: String::new(),
        }
    }

    // ── ReplayExitCode ────────────────────────────────────────────────

    #[test]
    fn exit_code_values() {
        assert_eq!(ReplayExitCode::Pass.code(), 0);
        assert_eq!(ReplayExitCode::Regression.code(), 1);
        assert_eq!(ReplayExitCode::InvalidInput.code(), 2);
        assert_eq!(ReplayExitCode::InternalError.code(), 3);
    }

    // ── ReplayOutputMode default ──────────────────────────────────────

    #[test]
    fn output_mode_default() {
        assert_eq!(ReplayOutputMode::default(), ReplayOutputMode::Human);
    }

    // ── SpeedArg parsing ──────────────────────────────────────────────

    #[test]
    fn speed_parse_1x() {
        assert_eq!(SpeedArg::from_str_arg("1x").unwrap(), SpeedArg::Normal);
    }

    #[test]
    fn speed_parse_2x() {
        assert_eq!(SpeedArg::from_str_arg("2x").unwrap(), SpeedArg::Double);
    }

    #[test]
    fn speed_parse_instant() {
        assert_eq!(
            SpeedArg::from_str_arg("instant").unwrap(),
            SpeedArg::Instant
        );
    }

    #[test]
    fn speed_parse_custom() {
        let s = SpeedArg::from_str_arg("4x").unwrap();
        if let SpeedArg::Custom(m) = s {
            assert!((m - 4.0).abs() < f64::EPSILON);
        } else {
            panic!("expected Custom");
        }
    }

    #[test]
    fn speed_parse_invalid() {
        assert!(SpeedArg::from_str_arg("abc").is_err());
    }

    #[test]
    fn speed_multiplier() {
        assert!((SpeedArg::Normal.multiplier() - 1.0).abs() < f64::EPSILON);
        assert!((SpeedArg::Double.multiplier() - 2.0).abs() < f64::EPSILON);
        assert!(SpeedArg::Instant.multiplier().is_infinite());
        assert!((SpeedArg::Custom(3.5).multiplier() - 3.5).abs() < f64::EPSILON);
    }

    // ── EquivalenceLevelArg ───────────────────────────────────────────

    #[test]
    fn equiv_arg_to_level() {
        assert_eq!(
            EquivalenceLevelArg::Structural.to_equivalence_level(),
            EquivalenceLevel::L0
        );
        assert_eq!(
            EquivalenceLevelArg::Decision.to_equivalence_level(),
            EquivalenceLevel::L1
        );
        assert_eq!(
            EquivalenceLevelArg::Full.to_equivalence_level(),
            EquivalenceLevel::L2
        );
    }

    #[test]
    fn equiv_arg_default() {
        assert_eq!(
            EquivalenceLevelArg::default(),
            EquivalenceLevelArg::Decision
        );
    }

    // ── InspectResult ─────────────────────────────────────────────────

    #[test]
    fn inspect_from_events() {
        let events = vec![
            make_event("r1", 100, 1, "d1", "o1"),
            make_event("r2", 200, 2, "d2", "o2"),
            make_event("r1", 300, 1, "d1", "o3"),
        ];
        let result = InspectResult::from_events("test.ftreplay", &events);
        assert_eq!(result.event_count, 3);
        assert_eq!(result.pane_count, 2);
        assert_eq!(result.rule_count, 2);
        assert_eq!(result.time_span_ms, 200);
        assert!(result.integrity_ok);
    }

    #[test]
    fn inspect_empty() {
        let result = InspectResult::from_events("empty.ftreplay", &[]);
        assert_eq!(result.event_count, 0);
        assert_eq!(result.time_span_ms, 0);
    }

    #[test]
    fn inspect_human_render() {
        let events = vec![make_event("r1", 100, 1, "d1", "o1")];
        let result = InspectResult::from_events("test.ftreplay", &events);
        let human = result.render_human();
        assert!(human.contains("test.ftreplay"));
        assert!(human.contains("Events:"));
        assert!(human.contains("Integrity:"));
    }

    // ── DiffRunner ────────────────────────────────────────────────────

    #[test]
    fn diff_runner_identical() {
        let runner = DiffRunner::new();
        let events = vec![
            make_event("r1", 100, 1, "d1", "o1"),
            make_event("r2", 200, 1, "d2", "o2"),
        ];
        let result = runner.run(&events, &events, &DiffConfig::default());
        assert_eq!(result.exit_code, ReplayExitCode::Pass);
    }

    #[test]
    fn diff_runner_regression() {
        let runner = DiffRunner::new();
        let base = vec![make_event("pol_auth", 100, 1, "d1", "o1")];
        let cand = vec![make_event("pol_auth", 100, 1, "d2", "o2")];
        let result = runner.run(&base, &cand, &DiffConfig::default());
        // Policy rule with definition change → Critical → Block → Regression.
        assert_eq!(result.exit_code, ReplayExitCode::Regression);
    }

    #[test]
    fn diff_runner_custom_budget() {
        let budget = RegressionBudget {
            max_critical: 10,
            max_high: 10,
            ..Default::default()
        };
        let runner = DiffRunner::with_budget(budget);
        let base = vec![make_event("pol_auth", 100, 1, "d1", "o1")];
        let cand = vec![make_event("pol_auth", 100, 1, "d2", "o2")];
        let result = runner.run(&base, &cand, &DiffConfig::default());
        assert_eq!(result.exit_code, ReplayExitCode::Pass);
    }

    #[test]
    fn diff_runner_format_human() {
        let runner = DiffRunner::new();
        let events = vec![make_event("r1", 100, 1, "d1", "o1")];
        let result = runner.run(&events, &events, &DiffConfig::default());
        let formatted =
            runner.format_result(&result, ReplayOutputMode::Human, &ReportMeta::default());
        assert!(!formatted.is_empty());
    }

    #[test]
    fn diff_runner_format_robot() {
        let runner = DiffRunner::new();
        let events = vec![make_event("r1", 100, 1, "d1", "o1")];
        let result = runner.run(&events, &events, &DiffConfig::default());
        let formatted =
            runner.format_result(&result, ReplayOutputMode::Robot, &ReportMeta::default());
        // Robot mode should be valid JSON.
        let parsed: serde_json::Value = serde_json::from_str(&formatted).unwrap();
        assert!(parsed.is_object());
    }

    #[test]
    fn diff_runner_format_quiet_pass() {
        let runner = DiffRunner::new();
        let events = vec![make_event("r1", 100, 1, "d1", "o1")];
        let result = runner.run(&events, &events, &DiffConfig::default());
        let formatted =
            runner.format_result(&result, ReplayOutputMode::Quiet, &ReportMeta::default());
        assert!(formatted.is_empty(), "quiet pass should produce no output");
    }

    #[test]
    fn diff_runner_format_quiet_fail() {
        let runner = DiffRunner::new();
        let base = vec![make_event("pol_auth", 100, 1, "d1", "o1")];
        let cand = vec![make_event("pol_auth", 100, 1, "d2", "o2")];
        let result = runner.run(&base, &cand, &DiffConfig::default());
        let formatted =
            runner.format_result(&result, ReplayOutputMode::Quiet, &ReportMeta::default());
        assert!(formatted.contains("FAIL"));
    }

    // ── RegressionSuiteResult ─────────────────────────────────────────

    #[test]
    fn suite_result_all_pass() {
        let results = vec![
            ArtifactResult {
                artifact_path: "a.replay".into(),
                passed: true,
                gate_result_summary: "Pass".into(),
                error: None,
            },
            ArtifactResult {
                artifact_path: "b.replay".into(),
                passed: true,
                gate_result_summary: "Pass".into(),
                error: None,
            },
        ];
        let suite = RegressionSuiteResult::from_results(results);
        assert!(suite.overall_pass);
        assert_eq!(suite.total_artifacts, 2);
        assert_eq!(suite.passed, 2);
        assert_eq!(suite.failed, 0);
    }

    #[test]
    fn suite_result_with_failure() {
        let results = vec![
            ArtifactResult {
                artifact_path: "a.replay".into(),
                passed: true,
                gate_result_summary: "Pass".into(),
                error: None,
            },
            ArtifactResult {
                artifact_path: "b.replay".into(),
                passed: false,
                gate_result_summary: "Fail".into(),
                error: None,
            },
        ];
        let suite = RegressionSuiteResult::from_results(results);
        assert!(!suite.overall_pass);
        assert_eq!(suite.failed, 1);
    }

    #[test]
    fn suite_result_with_error() {
        let results = vec![ArtifactResult {
            artifact_path: "bad.replay".into(),
            passed: false,
            gate_result_summary: "Error".into(),
            error: Some("file not found".into()),
        }];
        let suite = RegressionSuiteResult::from_results(results);
        assert!(!suite.overall_pass);
        assert_eq!(suite.errored, 1);
    }

    #[test]
    fn suite_result_render_human() {
        let results = vec![ArtifactResult {
            artifact_path: "a.replay".into(),
            passed: true,
            gate_result_summary: "Pass".into(),
            error: None,
        }];
        let suite = RegressionSuiteResult::from_results(results);
        let human = suite.render_human();
        assert!(human.contains("Regression Suite Results"));
        assert!(human.contains("[PASS] a.replay"));
    }

    // ── ReplayOutputMode serde ────────────────────────────────────────

    #[test]
    fn output_mode_serde() {
        for mode in &[
            ReplayOutputMode::Human,
            ReplayOutputMode::Robot,
            ReplayOutputMode::Verbose,
            ReplayOutputMode::Quiet,
        ] {
            let json = serde_json::to_string(mode).unwrap();
            let restored: ReplayOutputMode = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, *mode);
        }
    }

    // ── ReplayExitCode serde ──────────────────────────────────────────

    #[test]
    fn exit_code_serde() {
        for ec in &[
            ReplayExitCode::Pass,
            ReplayExitCode::Regression,
            ReplayExitCode::InvalidInput,
            ReplayExitCode::InternalError,
        ] {
            let json = serde_json::to_string(ec).unwrap();
            let restored: ReplayExitCode = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, *ec);
        }
    }

    // ── InspectResult serde ───────────────────────────────────────────

    #[test]
    fn inspect_result_serde() {
        let events = vec![make_event("r1", 100, 1, "d1", "o1")];
        let result = InspectResult::from_events("test.replay", &events);
        let json = serde_json::to_string(&result).unwrap();
        let restored: InspectResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.event_count, 1);
        assert_eq!(restored.artifact_path, "test.replay");
    }

    // ── SpeedArg serde ────────────────────────────────────────────────

    #[test]
    fn speed_arg_serde() {
        for sa in &[
            SpeedArg::Normal,
            SpeedArg::Double,
            SpeedArg::Instant,
            SpeedArg::Custom(3.5),
        ] {
            let json = serde_json::to_string(sa).unwrap();
            let _restored: SpeedArg = serde_json::from_str(&json).unwrap();
        }
    }

    // ── RegressionSuiteResult serde ───────────────────────────────────

    #[test]
    fn suite_result_serde() {
        let results = vec![ArtifactResult {
            artifact_path: "a.replay".into(),
            passed: true,
            gate_result_summary: "Pass".into(),
            error: None,
        }];
        let suite = RegressionSuiteResult::from_results(results);
        let json = serde_json::to_string(&suite).unwrap();
        let restored: RegressionSuiteResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.total_artifacts, 1);
        assert!(restored.overall_pass);
    }
}
