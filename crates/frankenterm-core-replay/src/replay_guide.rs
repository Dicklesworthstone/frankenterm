//! Guided replay UX flows for operators and agents (ft-og6q6.6.6).
//!
//! Provides stateless, step-based guided workflows for common replay
//! operations: incident investigation, rule testing, and regression check.
//!
//! # Workflows
//!
//! | Workflow            | Steps | Description |
//! |---------------------|-------|-------------|
//! | `investigate`       | 4     | Incident investigation with anomaly highlights |
//! | `test_rule`         | 5     | Rule-change testing with diff and scoring |
//! | `regression_check`  | 4     | Pre-merge regression suite evaluation |
//!
//! # Design
//!
//! All workflows are stateless: each step receives the full context needed
//! to produce its output. No server-side session state is required. This
//! makes them equally usable via CLI, Robot Mode, and MCP.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Workflow types
// ============================================================================

/// Available guided workflow types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuideWorkflow {
    /// Incident investigation: select artifact, replay, highlight anomalies.
    Investigate,
    /// Rule testing: baseline + candidate replay with diff report.
    TestRule,
    /// Regression check: run suite, evaluate gate, show breakdown.
    RegressionCheck,
}

impl GuideWorkflow {
    /// Parse from CLI string.
    pub fn from_str_arg(s: &str) -> Option<Self> {
        match s {
            "investigate" => Some(Self::Investigate),
            "test-rule" | "test_rule" => Some(Self::TestRule),
            "regression-check" | "regression_check" => Some(Self::RegressionCheck),
            _ => None,
        }
    }

    /// Canonical string representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Investigate => "investigate",
            Self::TestRule => "test_rule",
            Self::RegressionCheck => "regression_check",
        }
    }

    /// Human-readable description.
    #[must_use]
    pub fn description(&self) -> &'static str {
        match self {
            Self::Investigate => "Investigate an incident using replay artifacts",
            Self::TestRule => "Test a rule change against baseline artifacts",
            Self::RegressionCheck => "Run pre-merge regression check on artifact library",
        }
    }

    /// Number of steps in this workflow.
    #[must_use]
    pub fn step_count(&self) -> usize {
        match self {
            Self::Investigate => 4,
            Self::TestRule => 5,
            Self::RegressionCheck => 4,
        }
    }

    /// Step descriptions for progress display.
    #[must_use]
    pub fn step_descriptions(&self) -> Vec<GuideStepInfo> {
        match self {
            Self::Investigate => vec![
                GuideStepInfo::new(
                    0,
                    "select_artifact",
                    "Select artifact from registry or recent captures",
                ),
                GuideStepInfo::new(
                    1,
                    "replay_verbose",
                    "Replay with verbose provenance logging",
                ),
                GuideStepInfo::new(2, "highlight_anomalies", "Highlight anomalous decisions"),
                GuideStepInfo::new(3, "suggest_remediation", "Present remediation suggestions"),
            ],
            Self::TestRule => vec![
                GuideStepInfo::new(0, "select_baseline", "Select baseline artifact(s)"),
                GuideStepInfo::new(1, "load_overrides", "Create or load override package"),
                GuideStepInfo::new(2, "run_diff", "Run baseline + candidate replay with diff"),
                GuideStepInfo::new(
                    3,
                    "show_report",
                    "Show decision-diff report with risk scoring",
                ),
                GuideStepInfo::new(4, "suggest_next", "Suggest next steps"),
            ],
            Self::RegressionCheck => vec![
                GuideStepInfo::new(0, "run_suite", "Run full regression suite"),
                GuideStepInfo::new(1, "evaluate_gate", "Evaluate gate pass/fail"),
                GuideStepInfo::new(2, "show_breakdown", "Show detailed breakdown"),
                GuideStepInfo::new(3, "suggest_remediation", "Show remediation if needed"),
            ],
        }
    }
}

/// All available workflow types.
pub const ALL_WORKFLOWS: &[GuideWorkflow] = &[
    GuideWorkflow::Investigate,
    GuideWorkflow::TestRule,
    GuideWorkflow::RegressionCheck,
];

// ============================================================================
// Step metadata
// ============================================================================

/// Metadata for a single workflow step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideStepInfo {
    /// Zero-indexed step number.
    pub step: usize,
    /// Machine-readable step ID.
    pub step_id: String,
    /// Human-readable description.
    pub description: String,
}

impl GuideStepInfo {
    fn new(step: usize, id: &str, desc: &str) -> Self {
        Self {
            step,
            step_id: id.into(),
            description: desc.into(),
        }
    }
}

// ============================================================================
// Step input/output
// ============================================================================

/// Input for a single guide step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideStepInput {
    /// Which workflow is running.
    pub workflow: GuideWorkflow,
    /// Which step to execute (0-indexed).
    pub step: usize,
    /// Context accumulated from previous steps.
    pub context: GuideContext,
}

/// Accumulated context passed between steps.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuideContext {
    /// Selected artifact path(s).
    #[serde(default)]
    pub artifact_paths: Vec<String>,
    /// Baseline artifact path (for test-rule).
    #[serde(default)]
    pub baseline_path: Option<String>,
    /// Candidate artifact path (for test-rule).
    #[serde(default)]
    pub candidate_path: Option<String>,
    /// Override package path (for test-rule).
    #[serde(default)]
    pub override_path: Option<String>,
    /// Suite directory (for regression-check).
    #[serde(default)]
    pub suite_dir: Option<String>,
    /// Budget TOML path.
    #[serde(default)]
    pub budget_path: Option<String>,
    /// Tolerance in ms for diff operations.
    #[serde(default = "default_tolerance")]
    pub tolerance_ms: u64,
    /// Key-value store for step results.
    #[serde(default)]
    pub results: BTreeMap<String, serde_json::Value>,
}

fn default_tolerance() -> u64 {
    100
}

/// Output from a single guide step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideStepOutput {
    /// Which workflow produced this.
    pub workflow: GuideWorkflow,
    /// Which step produced this.
    pub step: usize,
    /// Step info for display.
    pub step_info: GuideStepInfo,
    /// Status of this step.
    pub status: GuideStepStatus,
    /// Human-readable summary.
    pub summary: String,
    /// Structured data from this step.
    pub data: serde_json::Value,
    /// Whether there is a next step.
    pub has_next: bool,
    /// Next step number (if any).
    pub next_step: Option<usize>,
}

/// Status of a guide step execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuideStepStatus {
    /// Step completed successfully.
    Complete,
    /// Step needs user input to continue.
    NeedsInput,
    /// Step failed with an error.
    Error,
}

// ============================================================================
// Progress tracking
// ============================================================================

/// Progress update for long-running operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuideProgress {
    /// Which workflow is running.
    pub workflow: GuideWorkflow,
    /// Current step.
    pub step: usize,
    /// Progress percentage (0.0 to 1.0).
    pub progress: f64,
    /// Events processed so far.
    pub events_processed: u64,
    /// Total events expected (0 if unknown).
    pub events_total: u64,
    /// Events per second throughput.
    pub events_per_sec: f64,
    /// Estimated time remaining (ms), 0 if unknown.
    pub eta_ms: u64,
    /// Human-readable status message.
    pub message: String,
}

impl GuideProgress {
    /// Create a new progress update.
    pub fn new(workflow: GuideWorkflow, step: usize) -> Self {
        Self {
            workflow,
            step,
            progress: 0.0,
            events_processed: 0,
            events_total: 0,
            events_per_sec: 0.0,
            eta_ms: 0,
            message: String::new(),
        }
    }

    /// Update with event counts.
    pub fn update(&mut self, processed: u64, total: u64, elapsed_ms: u64) {
        self.events_processed = processed;
        self.events_total = total;
        if total > 0 {
            self.progress = (processed as f64) / (total as f64);
        }
        if elapsed_ms > 0 {
            self.events_per_sec = (processed as f64) / (elapsed_ms as f64) * 1000.0;
            if total > processed && self.events_per_sec > 0.0 {
                let remaining = total - processed;
                self.eta_ms = ((remaining as f64) / self.events_per_sec * 1000.0).round() as u64;
            }
        }
    }

    /// Check if this represents a complete operation.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        (self.progress - 1.0).abs() < f64::EPSILON
            || (self.events_total > 0 && self.events_processed >= self.events_total)
    }
}

// ============================================================================
// Robot Mode envelopes
// ============================================================================

/// Robot Mode command for guide operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuideRobotCommand {
    /// Start a new guided workflow.
    Start,
    /// Execute a specific step.
    Step,
    /// Query available workflows.
    List,
}

impl GuideRobotCommand {
    /// Parse from command string.
    pub fn from_str_command(s: &str) -> Option<Self> {
        match s {
            "replay.guide.start" => Some(Self::Start),
            "replay.guide.step" => Some(Self::Step),
            "replay.guide.list" => Some(Self::List),
            _ => None,
        }
    }

    /// Canonical command string.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Start => "replay.guide.start",
            Self::Step => "replay.guide.step",
            Self::List => "replay.guide.list",
        }
    }
}

/// Robot request to start a guided workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideStartRequest {
    /// Which workflow to run.
    pub workflow: GuideWorkflow,
    /// Initial context (optional).
    #[serde(default)]
    pub context: GuideContext,
}

/// Robot request to execute a guide step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuideStepRequest {
    /// Which workflow.
    pub workflow: GuideWorkflow,
    /// Which step.
    pub step: usize,
    /// Accumulated context.
    pub context: GuideContext,
}

/// Robot response for guide start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideStartData {
    /// Workflow that was started.
    pub workflow: GuideWorkflow,
    /// Total steps.
    pub total_steps: usize,
    /// Step descriptions.
    pub steps: Vec<GuideStepInfo>,
    /// First step output.
    pub first_step: GuideStepOutput,
}

/// Robot response listing available workflows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideListData {
    /// Available workflows.
    pub workflows: Vec<GuideWorkflowInfo>,
}

/// Summary info for a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuideWorkflowInfo {
    /// Workflow name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Number of steps.
    pub step_count: usize,
}

// ============================================================================
// MCP tool schema
// ============================================================================

/// MCP tool name for guided replay.
pub const TOOL_REPLAY_GUIDE: &str = "wa.replay.guide";

/// MCP tool schema for guided replay.
#[must_use]
pub fn guide_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "workflow": {
                "type": "string",
                "description": "Workflow type: investigate, test_rule, regression_check",
                "enum": ["investigate", "test_rule", "regression_check"]
            },
            "step": {
                "type": "integer",
                "description": "Step number to execute (0-indexed)",
                "minimum": 0
            },
            "context": {
                "type": "object",
                "description": "Accumulated context from previous steps",
                "properties": {
                    "artifact_paths": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "baseline_path": { "type": "string" },
                    "candidate_path": { "type": "string" },
                    "override_path": { "type": "string" },
                    "suite_dir": { "type": "string" },
                    "budget_path": { "type": "string" },
                    "tolerance_ms": {
                        "type": "integer",
                        "default": 100,
                        "minimum": 0
                    }
                }
            }
        },
        "required": ["workflow", "step"],
        "additionalProperties": false
    })
}

// ============================================================================
// Step execution (stateless)
// ============================================================================

/// Execute a guide step and return the output.
///
/// This is the core dispatch function. Each step is stateless: it takes
/// the full context and produces an output. The caller is responsible
/// for accumulating context between steps.
pub fn execute_step(input: &GuideStepInput) -> GuideStepOutput {
    let steps = input.workflow.step_descriptions();
    let step_info = steps
        .get(input.step)
        .cloned()
        .unwrap_or_else(|| GuideStepInfo::new(input.step, "unknown", "Unknown step"));

    let total_steps = input.workflow.step_count();
    let has_next = input.step + 1 < total_steps;
    let next_step = if has_next { Some(input.step + 1) } else { None };

    if input.step >= total_steps {
        return GuideStepOutput {
            workflow: input.workflow,
            step: input.step,
            step_info,
            status: GuideStepStatus::Error,
            summary: format!(
                "Step {} is out of range (workflow has {} steps)",
                input.step, total_steps
            ),
            data: serde_json::json!({"error": "step_out_of_range"}),
            has_next: false,
            next_step: None,
        };
    }

    match input.workflow {
        GuideWorkflow::Investigate => {
            execute_investigate_step(input, step_info, has_next, next_step)
        }
        GuideWorkflow::TestRule => execute_test_rule_step(input, step_info, has_next, next_step),
        GuideWorkflow::RegressionCheck => {
            execute_regression_check_step(input, step_info, has_next, next_step)
        }
    }
}

/// Start a workflow by executing step 0.
pub fn start_workflow(workflow: GuideWorkflow, context: GuideContext) -> GuideStartData {
    let steps = workflow.step_descriptions();
    let total_steps = workflow.step_count();

    let input = GuideStepInput {
        workflow,
        step: 0,
        context,
    };
    let first_step = execute_step(&input);

    GuideStartData {
        workflow,
        total_steps,
        steps,
        first_step,
    }
}

/// List all available workflows.
#[must_use]
pub fn list_workflows() -> GuideListData {
    let workflows = ALL_WORKFLOWS
        .iter()
        .map(|w| GuideWorkflowInfo {
            name: w.as_str().into(),
            description: w.description().into(),
            step_count: w.step_count(),
        })
        .collect();
    GuideListData { workflows }
}

// ============================================================================
// Investigate workflow steps
// ============================================================================

fn execute_investigate_step(
    input: &GuideStepInput,
    step_info: GuideStepInfo,
    has_next: bool,
    next_step: Option<usize>,
) -> GuideStepOutput {
    match input.step {
        0 => {
            // Step 0: Select artifact
            let has_selection = !input.context.artifact_paths.is_empty();
            if has_selection {
                let paths = &input.context.artifact_paths;
                GuideStepOutput {
                    workflow: input.workflow,
                    step: 0,
                    step_info,
                    status: GuideStepStatus::Complete,
                    summary: format!("Selected {} artifact(s): {}", paths.len(), paths.join(", ")),
                    data: serde_json::json!({
                        "selected": paths,
                        "count": paths.len()
                    }),
                    has_next,
                    next_step,
                }
            } else {
                GuideStepOutput {
                    workflow: input.workflow,
                    step: 0,
                    step_info,
                    status: GuideStepStatus::NeedsInput,
                    summary: "Please select artifact(s) to investigate. \
                              Set context.artifact_paths with one or more .ftreplay file paths."
                        .into(),
                    data: serde_json::json!({"needs": "artifact_paths"}),
                    has_next,
                    next_step,
                }
            }
        }
        1 => {
            // Step 1: Replay with verbose provenance logging
            if input.context.artifact_paths.is_empty() {
                return make_error_output(input, step_info, "No artifacts selected");
            }
            let paths = &input.context.artifact_paths;
            GuideStepOutput {
                workflow: input.workflow,
                step: 1,
                step_info,
                status: GuideStepStatus::Complete,
                summary: format!(
                    "Replay queued for {} artifact(s) with verbose provenance logging",
                    paths.len()
                ),
                data: serde_json::json!({
                    "action": "replay_verbose",
                    "artifact_paths": paths,
                    "mode": "verbose"
                }),
                has_next,
                next_step,
            }
        }
        2 => {
            // Step 2: Highlight anomalous decisions
            let anomaly_count = input
                .context
                .results
                .get("anomaly_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            GuideStepOutput {
                workflow: input.workflow,
                step: 2,
                step_info,
                status: GuideStepStatus::Complete,
                summary: if anomaly_count > 0 {
                    format!(
                        "Found {} anomalous decision(s) requiring attention",
                        anomaly_count
                    )
                } else {
                    "No anomalous decisions detected — artifact looks clean".into()
                },
                data: serde_json::json!({
                    "anomaly_count": anomaly_count,
                    "has_anomalies": anomaly_count > 0
                }),
                has_next,
                next_step,
            }
        }
        3 => {
            // Step 3: Present remediation suggestions
            let has_anomalies = input
                .context
                .results
                .get("anomaly_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0;
            GuideStepOutput {
                workflow: input.workflow,
                step: 3,
                step_info,
                status: GuideStepStatus::Complete,
                summary: if has_anomalies {
                    "Remediation suggestions generated. Review and apply as needed.".into()
                } else {
                    "Investigation complete — no remediation needed.".into()
                },
                data: serde_json::json!({
                    "has_suggestions": has_anomalies,
                    "workflow_complete": true
                }),
                has_next: false,
                next_step: None,
            }
        }
        _ => make_error_output(input, step_info, "Unknown investigate step"),
    }
}

// ============================================================================
// Test-rule workflow steps
// ============================================================================

fn execute_test_rule_step(
    input: &GuideStepInput,
    step_info: GuideStepInfo,
    has_next: bool,
    next_step: Option<usize>,
) -> GuideStepOutput {
    match input.step {
        0 => {
            // Step 0: Select baseline artifact(s)
            let has_baseline =
                input.context.baseline_path.is_some() || !input.context.artifact_paths.is_empty();
            if has_baseline {
                let path = input
                    .context
                    .baseline_path
                    .as_deref()
                    .or_else(|| input.context.artifact_paths.first().map(String::as_str))
                    .unwrap_or("");
                GuideStepOutput {
                    workflow: input.workflow,
                    step: 0,
                    step_info,
                    status: GuideStepStatus::Complete,
                    summary: format!("Baseline selected: {path}"),
                    data: serde_json::json!({"baseline": path}),
                    has_next,
                    next_step,
                }
            } else {
                GuideStepOutput {
                    workflow: input.workflow,
                    step: 0,
                    step_info,
                    status: GuideStepStatus::NeedsInput,
                    summary: "Please select a baseline artifact. \
                              Set context.baseline_path or context.artifact_paths[0]."
                        .into(),
                    data: serde_json::json!({"needs": "baseline_path"}),
                    has_next,
                    next_step,
                }
            }
        }
        1 => {
            // Step 1: Create or load override package
            if let Some(ref override_path) = input.context.override_path {
                GuideStepOutput {
                    workflow: input.workflow,
                    step: 1,
                    step_info,
                    status: GuideStepStatus::Complete,
                    summary: format!("Override package loaded: {override_path}"),
                    data: serde_json::json!({"override_path": override_path}),
                    has_next,
                    next_step,
                }
            } else if let Some(ref candidate) = input.context.candidate_path {
                GuideStepOutput {
                    workflow: input.workflow,
                    step: 1,
                    step_info,
                    status: GuideStepStatus::Complete,
                    summary: format!("Candidate artifact provided: {candidate}"),
                    data: serde_json::json!({"candidate_path": candidate}),
                    has_next,
                    next_step,
                }
            } else {
                GuideStepOutput {
                    workflow: input.workflow,
                    step: 1,
                    step_info,
                    status: GuideStepStatus::NeedsInput,
                    summary: "Please provide candidate artifact or override package. \
                              Set context.candidate_path or context.override_path."
                        .into(),
                    data: serde_json::json!({"needs": "candidate_path_or_override_path"}),
                    has_next,
                    next_step,
                }
            }
        }
        2 => {
            // Step 2: Run baseline + candidate replay with diff
            let baseline = input
                .context
                .baseline_path
                .as_deref()
                .or_else(|| input.context.artifact_paths.first().map(String::as_str));
            let candidate = input.context.candidate_path.as_deref();

            if baseline.is_none() || candidate.is_none() {
                return make_error_output(input, step_info, "Missing baseline or candidate path");
            }

            GuideStepOutput {
                workflow: input.workflow,
                step: 2,
                step_info,
                status: GuideStepStatus::Complete,
                summary: format!(
                    "Diff queued: {} vs {} (tolerance: {}ms)",
                    baseline.unwrap(),
                    candidate.unwrap(),
                    input.context.tolerance_ms
                ),
                data: serde_json::json!({
                    "action": "run_diff",
                    "baseline": baseline,
                    "candidate": candidate,
                    "tolerance_ms": input.context.tolerance_ms
                }),
                has_next,
                next_step,
            }
        }
        3 => {
            // Step 3: Show decision-diff report with risk scoring
            let divergence_count = input
                .context
                .results
                .get("divergence_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let gate_result = input
                .context
                .results
                .get("gate_result")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            GuideStepOutput {
                workflow: input.workflow,
                step: 3,
                step_info,
                status: GuideStepStatus::Complete,
                summary: format!(
                    "Diff complete: {} divergence(s), gate: {}",
                    divergence_count, gate_result
                ),
                data: serde_json::json!({
                    "divergence_count": divergence_count,
                    "gate_result": gate_result
                }),
                has_next,
                next_step,
            }
        }
        4 => {
            // Step 4: Suggest next steps
            let gate_result = input
                .context
                .results
                .get("gate_result")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let suggestions = match gate_result {
                "Pass" | "pass" => vec![
                    "Rule change looks safe to merge",
                    "Consider adding as regression baseline",
                ],
                "Warn" | "warn" => vec![
                    "Review warnings before merging",
                    "Annotate expected divergences in PR",
                ],
                _ => vec![
                    "Rule change causes regressions — consider reverting",
                    "Add expected-divergence annotations for intentional changes",
                    "Adjust regression budget if thresholds are too strict",
                ],
            };
            GuideStepOutput {
                workflow: input.workflow,
                step: 4,
                step_info,
                status: GuideStepStatus::Complete,
                summary: format!("Suggested {} next step(s)", suggestions.len()),
                data: serde_json::json!({
                    "suggestions": suggestions,
                    "workflow_complete": true
                }),
                has_next: false,
                next_step: None,
            }
        }
        _ => make_error_output(input, step_info, "Unknown test_rule step"),
    }
}

// ============================================================================
// Regression-check workflow steps
// ============================================================================

fn execute_regression_check_step(
    input: &GuideStepInput,
    step_info: GuideStepInfo,
    has_next: bool,
    next_step: Option<usize>,
) -> GuideStepOutput {
    match input.step {
        0 => {
            // Step 0: Run full regression suite
            let suite_dir = input
                .context
                .suite_dir
                .as_deref()
                .unwrap_or("tests/regression/replay/");
            GuideStepOutput {
                workflow: input.workflow,
                step: 0,
                step_info,
                status: GuideStepStatus::Complete,
                summary: format!("Regression suite queued from: {suite_dir}"),
                data: serde_json::json!({
                    "action": "run_suite",
                    "suite_dir": suite_dir
                }),
                has_next,
                next_step,
            }
        }
        1 => {
            // Step 1: Evaluate gate pass/fail
            let passed = input
                .context
                .results
                .get("suite_passed")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let total = input
                .context
                .results
                .get("total_artifacts")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let failed = input
                .context
                .results
                .get("failed_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            GuideStepOutput {
                workflow: input.workflow,
                step: 1,
                step_info,
                status: GuideStepStatus::Complete,
                summary: if passed {
                    format!("Gate PASS: all {total} artifact(s) passed")
                } else {
                    format!("Gate FAIL: {failed}/{total} artifact(s) failed")
                },
                data: serde_json::json!({
                    "gate_passed": passed,
                    "total_artifacts": total,
                    "failed_count": failed
                }),
                has_next,
                next_step,
            }
        }
        2 => {
            // Step 2: Show detailed breakdown
            let results = input
                .context
                .results
                .get("artifact_results")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([]));
            GuideStepOutput {
                workflow: input.workflow,
                step: 2,
                step_info,
                status: GuideStepStatus::Complete,
                summary: "Detailed breakdown generated".into(),
                data: serde_json::json!({
                    "artifact_results": results
                }),
                has_next,
                next_step,
            }
        }
        3 => {
            // Step 3: Show remediation if needed
            let passed = input
                .context
                .results
                .get("suite_passed")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            GuideStepOutput {
                workflow: input.workflow,
                step: 3,
                step_info,
                status: GuideStepStatus::Complete,
                summary: if passed {
                    "Regression check complete — all clear for merge.".into()
                } else {
                    "Regression check complete — remediation suggestions provided.".into()
                },
                data: serde_json::json!({
                    "needs_remediation": !passed,
                    "workflow_complete": true
                }),
                has_next: false,
                next_step: None,
            }
        }
        _ => make_error_output(input, step_info, "Unknown regression_check step"),
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn make_error_output(
    input: &GuideStepInput,
    step_info: GuideStepInfo,
    message: &str,
) -> GuideStepOutput {
    GuideStepOutput {
        workflow: input.workflow,
        step: input.step,
        step_info,
        status: GuideStepStatus::Error,
        summary: message.into(),
        data: serde_json::json!({"error": message}),
        has_next: false,
        next_step: None,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── GuideWorkflow ───────────────────────────────────────────────

    #[test]
    fn workflow_from_str() {
        assert_eq!(
            GuideWorkflow::from_str_arg("investigate"),
            Some(GuideWorkflow::Investigate)
        );
        assert_eq!(
            GuideWorkflow::from_str_arg("test-rule"),
            Some(GuideWorkflow::TestRule)
        );
        assert_eq!(
            GuideWorkflow::from_str_arg("test_rule"),
            Some(GuideWorkflow::TestRule)
        );
        assert_eq!(
            GuideWorkflow::from_str_arg("regression-check"),
            Some(GuideWorkflow::RegressionCheck)
        );
        assert_eq!(
            GuideWorkflow::from_str_arg("regression_check"),
            Some(GuideWorkflow::RegressionCheck)
        );
        assert_eq!(GuideWorkflow::from_str_arg("unknown"), None);
    }

    #[test]
    fn workflow_as_str_roundtrip() {
        for w in ALL_WORKFLOWS {
            let s = w.as_str();
            let parsed = GuideWorkflow::from_str_arg(s);
            assert_eq!(parsed, Some(*w));
        }
    }

    #[test]
    fn workflow_step_count() {
        assert_eq!(GuideWorkflow::Investigate.step_count(), 4);
        assert_eq!(GuideWorkflow::TestRule.step_count(), 5);
        assert_eq!(GuideWorkflow::RegressionCheck.step_count(), 4);
    }

    #[test]
    fn workflow_descriptions_match_count() {
        for w in ALL_WORKFLOWS {
            let descs = w.step_descriptions();
            assert_eq!(
                descs.len(),
                w.step_count(),
                "step description count mismatch for {:?}",
                w
            );
        }
    }

    #[test]
    fn workflow_serde() {
        for w in ALL_WORKFLOWS {
            let json = serde_json::to_string(w).unwrap();
            let restored: GuideWorkflow = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, *w);
        }
    }

    // ── GuideRobotCommand ───────────────────────────────────────────

    #[test]
    fn robot_command_roundtrip() {
        let cmds = [
            GuideRobotCommand::Start,
            GuideRobotCommand::Step,
            GuideRobotCommand::List,
        ];
        for cmd in &cmds {
            let s = cmd.as_str();
            let parsed = GuideRobotCommand::from_str_command(s);
            assert_eq!(parsed.as_ref(), Some(cmd));
        }
    }

    #[test]
    fn robot_command_unknown() {
        assert!(GuideRobotCommand::from_str_command("replay.guide.unknown").is_none());
    }

    // ── GuideProgress ───────────────────────────────────────────────

    #[test]
    fn progress_new() {
        let p = GuideProgress::new(GuideWorkflow::Investigate, 0);
        assert!(!p.is_complete());
        assert_eq!(p.events_processed, 0);
    }

    #[test]
    fn progress_update_calculates_eta() {
        let mut p = GuideProgress::new(GuideWorkflow::Investigate, 1);
        p.update(500, 1000, 1000);
        assert!((p.progress - 0.5).abs() < f64::EPSILON);
        assert!((p.events_per_sec - 500.0).abs() < f64::EPSILON);
        assert!(p.eta_ms > 0);
        assert!(!p.is_complete());
    }

    #[test]
    fn progress_complete() {
        let mut p = GuideProgress::new(GuideWorkflow::Investigate, 1);
        p.update(1000, 1000, 2000);
        assert!(p.is_complete());
    }

    #[test]
    fn progress_serde() {
        let mut p = GuideProgress::new(GuideWorkflow::TestRule, 2);
        p.update(50, 100, 500);
        let json = serde_json::to_string(&p).unwrap();
        let restored: GuideProgress = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, p);
    }

    // ── execute_step ────────────────────────────────────────────────

    #[test]
    fn step_out_of_range() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::Investigate,
            step: 99,
            context: GuideContext::default(),
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Error);
        assert!(!output.has_next);
    }

    // ── Investigate workflow ────────────────────────────────────────

    #[test]
    fn investigate_step0_needs_input() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::Investigate,
            step: 0,
            context: GuideContext::default(),
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::NeedsInput);
        assert!(output.has_next);
    }

    #[test]
    fn investigate_step0_with_artifact() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::Investigate,
            step: 0,
            context: GuideContext {
                artifact_paths: vec!["test.ftreplay".into()],
                ..Default::default()
            },
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
        assert!(output.has_next);
        assert_eq!(output.next_step, Some(1));
    }

    #[test]
    fn investigate_step1_replay() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::Investigate,
            step: 1,
            context: GuideContext {
                artifact_paths: vec!["test.ftreplay".into()],
                ..Default::default()
            },
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
        assert!(output.summary.contains("verbose provenance"));
    }

    #[test]
    fn investigate_step1_no_artifact_errors() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::Investigate,
            step: 1,
            context: GuideContext::default(),
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Error);
    }

    #[test]
    fn investigate_step2_no_anomalies() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::Investigate,
            step: 2,
            context: GuideContext::default(),
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
        assert!(output.summary.contains("clean"));
    }

    #[test]
    fn investigate_step2_with_anomalies() {
        let mut ctx = GuideContext::default();
        ctx.results
            .insert("anomaly_count".into(), serde_json::json!(3));
        let input = GuideStepInput {
            workflow: GuideWorkflow::Investigate,
            step: 2,
            context: ctx,
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
        assert!(output.summary.contains("3 anomalous"));
    }

    #[test]
    fn investigate_step3_no_anomalies() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::Investigate,
            step: 3,
            context: GuideContext::default(),
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
        assert!(!output.has_next);
        assert!(output.summary.contains("no remediation"));
    }

    #[test]
    fn investigate_step3_with_anomalies() {
        let mut ctx = GuideContext::default();
        ctx.results
            .insert("anomaly_count".into(), serde_json::json!(5));
        let input = GuideStepInput {
            workflow: GuideWorkflow::Investigate,
            step: 3,
            context: ctx,
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
        assert!(output.summary.contains("Remediation suggestions"));
    }

    // ── Test-rule workflow ──────────────────────────────────────────

    #[test]
    fn test_rule_step0_needs_baseline() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::TestRule,
            step: 0,
            context: GuideContext::default(),
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::NeedsInput);
    }

    #[test]
    fn test_rule_step0_with_baseline() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::TestRule,
            step: 0,
            context: GuideContext {
                baseline_path: Some("base.ftreplay".into()),
                ..Default::default()
            },
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
        assert!(output.summary.contains("base.ftreplay"));
    }

    #[test]
    fn test_rule_step1_needs_candidate() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::TestRule,
            step: 1,
            context: GuideContext::default(),
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::NeedsInput);
    }

    #[test]
    fn test_rule_step1_with_candidate() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::TestRule,
            step: 1,
            context: GuideContext {
                candidate_path: Some("cand.ftreplay".into()),
                ..Default::default()
            },
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
    }

    #[test]
    fn test_rule_step2_runs_diff() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::TestRule,
            step: 2,
            context: GuideContext {
                baseline_path: Some("base.ftreplay".into()),
                candidate_path: Some("cand.ftreplay".into()),
                tolerance_ms: 200,
                ..Default::default()
            },
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
        assert!(output.summary.contains("200ms"));
    }

    #[test]
    fn test_rule_step2_missing_paths() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::TestRule,
            step: 2,
            context: GuideContext::default(),
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Error);
    }

    #[test]
    fn test_rule_step4_pass_suggestions() {
        let mut ctx = GuideContext::default();
        ctx.results
            .insert("gate_result".into(), serde_json::json!("Pass"));
        let input = GuideStepInput {
            workflow: GuideWorkflow::TestRule,
            step: 4,
            context: ctx,
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
        assert!(!output.has_next);
        let suggestions = output.data["suggestions"].as_array().unwrap();
        assert!(
            suggestions
                .iter()
                .any(|s| s.as_str().unwrap().contains("safe to merge"))
        );
    }

    #[test]
    fn test_rule_step4_fail_suggestions() {
        let mut ctx = GuideContext::default();
        ctx.results
            .insert("gate_result".into(), serde_json::json!("Fail"));
        let input = GuideStepInput {
            workflow: GuideWorkflow::TestRule,
            step: 4,
            context: ctx,
        };
        let output = execute_step(&input);
        let suggestions = output.data["suggestions"].as_array().unwrap();
        assert!(
            suggestions
                .iter()
                .any(|s| s.as_str().unwrap().contains("reverting"))
        );
    }

    // ── Regression-check workflow ───────────────────────────────────

    #[test]
    fn regression_step0_default_suite_dir() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::RegressionCheck,
            step: 0,
            context: GuideContext::default(),
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
        assert!(output.summary.contains("tests/regression/replay/"));
    }

    #[test]
    fn regression_step0_custom_suite_dir() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::RegressionCheck,
            step: 0,
            context: GuideContext {
                suite_dir: Some("custom/path/".into()),
                ..Default::default()
            },
        };
        let output = execute_step(&input);
        assert!(output.summary.contains("custom/path/"));
    }

    #[test]
    fn regression_step1_pass() {
        let mut ctx = GuideContext::default();
        ctx.results
            .insert("suite_passed".into(), serde_json::json!(true));
        ctx.results
            .insert("total_artifacts".into(), serde_json::json!(5));
        let input = GuideStepInput {
            workflow: GuideWorkflow::RegressionCheck,
            step: 1,
            context: ctx,
        };
        let output = execute_step(&input);
        assert!(output.summary.contains("PASS"));
        assert!(output.summary.contains("5"));
    }

    #[test]
    fn regression_step1_fail() {
        let mut ctx = GuideContext::default();
        ctx.results
            .insert("suite_passed".into(), serde_json::json!(false));
        ctx.results
            .insert("total_artifacts".into(), serde_json::json!(10));
        ctx.results
            .insert("failed_count".into(), serde_json::json!(3));
        let input = GuideStepInput {
            workflow: GuideWorkflow::RegressionCheck,
            step: 1,
            context: ctx,
        };
        let output = execute_step(&input);
        assert!(output.summary.contains("FAIL"));
        assert!(output.summary.contains("3/10"));
    }

    #[test]
    fn regression_step3_complete() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::RegressionCheck,
            step: 3,
            context: GuideContext::default(),
        };
        let output = execute_step(&input);
        assert_eq!(output.status, GuideStepStatus::Complete);
        assert!(!output.has_next);
    }

    // ── start_workflow ──────────────────────────────────────────────

    #[test]
    fn start_investigate() {
        let data = start_workflow(
            GuideWorkflow::Investigate,
            GuideContext {
                artifact_paths: vec!["test.ftreplay".into()],
                ..Default::default()
            },
        );
        assert_eq!(data.workflow, GuideWorkflow::Investigate);
        assert_eq!(data.total_steps, 4);
        assert_eq!(data.steps.len(), 4);
        assert_eq!(data.first_step.status, GuideStepStatus::Complete);
    }

    #[test]
    fn start_test_rule_needs_input() {
        let data = start_workflow(GuideWorkflow::TestRule, GuideContext::default());
        assert_eq!(data.first_step.status, GuideStepStatus::NeedsInput);
    }

    #[test]
    fn start_regression_check() {
        let data = start_workflow(GuideWorkflow::RegressionCheck, GuideContext::default());
        assert_eq!(data.total_steps, 4);
        assert_eq!(data.first_step.status, GuideStepStatus::Complete);
    }

    // ── list_workflows ──────────────────────────────────────────────

    #[test]
    fn list_all_workflows() {
        let list = list_workflows();
        assert_eq!(list.workflows.len(), 3);
        let names: Vec<&str> = list.workflows.iter().map(|w| w.name.as_str()).collect();
        assert!(names.contains(&"investigate"));
        assert!(names.contains(&"test_rule"));
        assert!(names.contains(&"regression_check"));
    }

    // ── MCP schema ──────────────────────────────────────────────────

    #[test]
    fn guide_schema_valid() {
        let schema = guide_tool_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("workflow")));
        assert!(required.iter().any(|v| v.as_str() == Some("step")));
    }

    #[test]
    fn guide_schema_workflow_enum() {
        let schema = guide_tool_schema();
        let wf_enum = schema["properties"]["workflow"]["enum"].as_array().unwrap();
        assert_eq!(wf_enum.len(), 3);
    }

    // ── Context serde ───────────────────────────────────────────────

    #[test]
    fn context_default_tolerance() {
        let ctx: GuideContext = serde_json::from_str("{}").unwrap();
        assert_eq!(ctx.tolerance_ms, 100);
    }

    #[test]
    fn context_serde_roundtrip() {
        let ctx = GuideContext {
            artifact_paths: vec!["a.ftreplay".into()],
            baseline_path: Some("base.ftreplay".into()),
            candidate_path: Some("cand.ftreplay".into()),
            override_path: None,
            suite_dir: Some("tests/".into()),
            budget_path: None,
            tolerance_ms: 200,
            results: BTreeMap::new(),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let restored: GuideContext = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.artifact_paths, ctx.artifact_paths);
        assert_eq!(restored.baseline_path, ctx.baseline_path);
        assert_eq!(restored.tolerance_ms, 200);
    }

    // ── GuideStepOutput serde ───────────────────────────────────────

    #[test]
    fn step_output_serde() {
        let input = GuideStepInput {
            workflow: GuideWorkflow::Investigate,
            step: 0,
            context: GuideContext {
                artifact_paths: vec!["test.ftreplay".into()],
                ..Default::default()
            },
        };
        let output = execute_step(&input);
        let json = serde_json::to_string(&output).unwrap();
        let restored: GuideStepOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, output);
    }

    // ── Full workflow traversal ─────────────────────────────────────

    #[test]
    fn investigate_full_traversal() {
        let ctx = GuideContext {
            artifact_paths: vec!["incident.ftreplay".into()],
            ..Default::default()
        };

        for step in 0..4 {
            let input = GuideStepInput {
                workflow: GuideWorkflow::Investigate,
                step,
                context: ctx.clone(),
            };
            let output = execute_step(&input);
            assert_ne!(output.status, GuideStepStatus::Error, "step {step} errored");

            if step < 3 {
                assert!(output.has_next, "step {step} should have next");
                assert_eq!(output.next_step, Some(step + 1));
            } else {
                assert!(!output.has_next, "last step should not have next");
            }
        }
    }

    #[test]
    fn regression_full_traversal() {
        let mut ctx = GuideContext::default();
        ctx.results
            .insert("suite_passed".into(), serde_json::json!(true));
        ctx.results
            .insert("total_artifacts".into(), serde_json::json!(3));

        for step in 0..4 {
            let input = GuideStepInput {
                workflow: GuideWorkflow::RegressionCheck,
                step,
                context: ctx.clone(),
            };
            let output = execute_step(&input);
            assert_ne!(output.status, GuideStepStatus::Error, "step {step} errored");
        }
    }
}
