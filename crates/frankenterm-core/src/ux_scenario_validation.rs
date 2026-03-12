//! Operator UX scenario validation and usability telemetry (ft-3681t.9.1).
//!
//! Provides scenario-driven UX validation for critical operator workflows
//! (launch, triage, intervention, approval, incident handling, migration
//! oversight) with measurable efficiency and safety metrics.
//!
//! # Architecture
//!
//! ```text
//! UxScenarioRunner
//!   ├── ScenarioSpec (declarative scenario definition)
//!   │     ├── ScenarioPhase[] (ordered workflow steps)
//!   │     │     ├── phase_id, description, step_type
//!   │     │     └── acceptance: PhaseAcceptance (latency, error constraints)
//!   │     └── UxThresholds (go/no-go gates)
//!   │
//!   ├── ScenarioExecution (recorded run)
//!   │     ├── PhaseResult[] per phase
//!   │     │     ├── elapsed_ms, success, friction_events
//!   │     │     └── telemetry snapshots
//!   │     └── ScenarioVerdict (Pass/Fail/Degraded)
//!   │
//!   └── UxTelemetry (aggregate metrics)
//!         ├── task_completion_rate, mean_friction, p95_latency
//!         ├── error_recovery_rate, intervention_success_rate
//!         └── GoNoGoEvaluation → release gate
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Scenario step types
// =============================================================================

/// The type of operator workflow being validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkflowClass {
    /// Fleet launch and initial pane setup.
    Launch,
    /// Error/anomaly triage and investigation.
    Triage,
    /// Live intervention (pause, takeover, quarantine).
    Intervention,
    /// Approval queue processing.
    Approval,
    /// Incident detection and response.
    IncidentHandling,
    /// Migration oversight and cutover monitoring.
    MigrationOversight,
    /// Context budget management and compaction.
    ContextManagement,
    /// Dashboard review and fleet health assessment.
    DashboardReview,
}

impl WorkflowClass {
    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::Triage => "triage",
            Self::Intervention => "intervention",
            Self::Approval => "approval",
            Self::IncidentHandling => "incident-handling",
            Self::MigrationOversight => "migration-oversight",
            Self::ContextManagement => "context-management",
            Self::DashboardReview => "dashboard-review",
        }
    }
}

/// Type of step within a scenario phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StepType {
    /// Navigate to a view or panel.
    Navigate,
    /// Inspect status or detail.
    Inspect,
    /// Execute a control action.
    Execute,
    /// Wait for system response or feedback.
    WaitForFeedback,
    /// Confirm or approve a pending action.
    Confirm,
    /// Recover from an error condition.
    Recover,
    /// Verify an outcome against expectations.
    Verify,
}

// =============================================================================
// Scenario specification
// =============================================================================

/// Acceptance criteria for a single phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseAcceptance {
    /// Maximum allowed latency in milliseconds.
    pub max_latency_ms: u64,
    /// Whether the phase must succeed for the scenario to pass.
    pub required: bool,
    /// Maximum friction events (unexpected prompts, retries, confusion points).
    pub max_friction_events: u32,
}

impl PhaseAcceptance {
    /// Lenient defaults for non-critical phases.
    #[must_use]
    pub fn lenient() -> Self {
        Self {
            max_latency_ms: 5000,
            required: false,
            max_friction_events: 3,
        }
    }

    /// Strict defaults for critical phases.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            max_latency_ms: 1000,
            required: true,
            max_friction_events: 0,
        }
    }
}

/// A single phase (step) in a scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioPhase {
    /// Unique phase ID within the scenario.
    pub phase_id: String,
    /// Human-readable description.
    pub description: String,
    /// Step type.
    pub step_type: StepType,
    /// Acceptance criteria.
    pub acceptance: PhaseAcceptance,
}

/// Go/no-go thresholds for UX quality.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UxThresholds {
    /// Minimum task completion rate (0.0–1.0).
    pub min_completion_rate: f64,
    /// Maximum p95 phase latency in milliseconds.
    pub max_p95_latency_ms: u64,
    /// Maximum mean friction events per scenario.
    pub max_mean_friction: f64,
    /// Minimum error recovery rate (0.0–1.0) — fraction of errors that are
    /// successfully recovered by the operator within the scenario.
    pub min_error_recovery_rate: f64,
    /// Minimum intervention success rate (0.0–1.0).
    pub min_intervention_success_rate: f64,
}

impl UxThresholds {
    /// Conservative thresholds suitable for release gates.
    #[must_use]
    pub fn release_gate() -> Self {
        Self {
            min_completion_rate: 0.95,
            max_p95_latency_ms: 2000,
            max_mean_friction: 1.0,
            min_error_recovery_rate: 0.90,
            min_intervention_success_rate: 0.95,
        }
    }

    /// Relaxed thresholds for early development validation.
    #[must_use]
    pub fn development() -> Self {
        Self {
            min_completion_rate: 0.80,
            max_p95_latency_ms: 5000,
            max_mean_friction: 3.0,
            min_error_recovery_rate: 0.70,
            min_intervention_success_rate: 0.80,
        }
    }
}

/// Complete scenario specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioSpec {
    /// Unique scenario ID.
    pub scenario_id: String,
    /// Human-readable name.
    pub name: String,
    /// Workflow class being tested.
    pub workflow_class: WorkflowClass,
    /// Ordered phases.
    pub phases: Vec<ScenarioPhase>,
    /// Go/no-go thresholds.
    pub thresholds: UxThresholds,
}

impl ScenarioSpec {
    /// Number of required (must-pass) phases.
    #[must_use]
    pub fn required_phase_count(&self) -> usize {
        self.phases.iter().filter(|p| p.acceptance.required).count()
    }

    /// Total phase count.
    #[must_use]
    pub fn total_phase_count(&self) -> usize {
        self.phases.len()
    }
}

// =============================================================================
// Scenario execution results
// =============================================================================

/// A friction event observed during a phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrictionEvent {
    /// What caused friction.
    pub description: String,
    /// Friction category.
    pub category: FrictionCategory,
    /// Timestamp within the phase (relative ms).
    pub at_ms: u64,
}

/// Categories of UX friction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FrictionCategory {
    /// Unexpected prompt or confirmation dialog.
    UnexpectedPrompt,
    /// Had to retry an action.
    Retry,
    /// Confusing or misleading feedback.
    ConfusingFeedback,
    /// Missing information needed for a decision.
    MissingInfo,
    /// Slow or unresponsive system.
    Sluggish,
    /// Accessibility barrier.
    AccessibilityBarrier,
    /// Navigation confusion (wrong view, backtracking).
    NavigationConfusion,
}

impl FrictionCategory {
    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::UnexpectedPrompt => "unexpected-prompt",
            Self::Retry => "retry",
            Self::ConfusingFeedback => "confusing-feedback",
            Self::MissingInfo => "missing-info",
            Self::Sluggish => "sluggish",
            Self::AccessibilityBarrier => "accessibility-barrier",
            Self::NavigationConfusion => "navigation-confusion",
        }
    }
}

/// Result of executing a single phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseResult {
    /// Phase ID (matches ScenarioPhase::phase_id).
    pub phase_id: String,
    /// Whether the phase succeeded.
    pub success: bool,
    /// Elapsed time in milliseconds.
    pub elapsed_ms: u64,
    /// Friction events observed.
    pub friction_events: Vec<FrictionEvent>,
    /// Error message if failed.
    pub error: Option<String>,
    /// Whether acceptance criteria were met.
    pub acceptance_met: bool,
}

impl PhaseResult {
    /// Create a successful phase result.
    #[must_use]
    pub fn success(phase_id: impl Into<String>, elapsed_ms: u64) -> Self {
        Self {
            phase_id: phase_id.into(),
            success: true,
            elapsed_ms,
            friction_events: Vec::new(),
            error: None,
            acceptance_met: true,
        }
    }

    /// Create a failed phase result.
    #[must_use]
    pub fn failure(phase_id: impl Into<String>, elapsed_ms: u64, error: impl Into<String>) -> Self {
        Self {
            phase_id: phase_id.into(),
            success: false,
            elapsed_ms,
            friction_events: Vec::new(),
            error: Some(error.into()),
            acceptance_met: false,
        }
    }

    /// Add a friction event.
    pub fn add_friction(&mut self, event: FrictionEvent) {
        self.friction_events.push(event);
    }

    /// Friction count.
    #[must_use]
    pub fn friction_count(&self) -> u32 {
        self.friction_events.len() as u32
    }
}

/// Overall scenario verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScenarioVerdict {
    /// All phases passed and all thresholds met.
    Pass,
    /// All required phases passed but some optional phases or thresholds missed.
    Degraded,
    /// One or more required phases failed.
    Fail,
}

/// Complete execution record for a scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioExecution {
    /// Scenario ID.
    pub scenario_id: String,
    /// Workflow class.
    pub workflow_class: WorkflowClass,
    /// When the execution started (epoch ms).
    pub started_at_ms: u64,
    /// When the execution ended (epoch ms).
    pub ended_at_ms: u64,
    /// Per-phase results.
    pub phase_results: Vec<PhaseResult>,
    /// Overall verdict.
    pub verdict: ScenarioVerdict,
    /// Scenario-level notes.
    pub notes: Vec<String>,
}

impl ScenarioExecution {
    /// Total elapsed time.
    #[must_use]
    pub fn total_elapsed_ms(&self) -> u64 {
        self.ended_at_ms.saturating_sub(self.started_at_ms)
    }

    /// Count of passed phases.
    #[must_use]
    pub fn phases_passed(&self) -> usize {
        self.phase_results.iter().filter(|p| p.success).count()
    }

    /// Count of failed phases.
    #[must_use]
    pub fn phases_failed(&self) -> usize {
        self.phase_results.iter().filter(|p| !p.success).count()
    }

    /// Total friction events across all phases.
    #[must_use]
    pub fn total_friction(&self) -> u32 {
        self.phase_results.iter().map(|p| p.friction_count()).sum()
    }

    /// Mean friction events per phase.
    #[must_use]
    pub fn mean_friction(&self) -> f64 {
        if self.phase_results.is_empty() {
            return 0.0;
        }
        self.total_friction() as f64 / self.phase_results.len() as f64
    }

    /// P95 phase latency.
    #[must_use]
    pub fn p95_latency_ms(&self) -> u64 {
        if self.phase_results.is_empty() {
            return 0;
        }
        let mut latencies: Vec<u64> = self.phase_results.iter().map(|p| p.elapsed_ms).collect();
        latencies.sort_unstable();
        let idx = ((latencies.len() as f64 * 0.95).ceil() as usize).min(latencies.len()) - 1;
        latencies[idx]
    }

    /// Completion rate (phases passed / total).
    #[must_use]
    pub fn completion_rate(&self) -> f64 {
        if self.phase_results.is_empty() {
            return 0.0;
        }
        self.phases_passed() as f64 / self.phase_results.len() as f64
    }
}

// =============================================================================
// Scenario runner
// =============================================================================

/// Evaluates a scenario execution against its spec.
pub struct ScenarioEvaluator;

impl ScenarioEvaluator {
    /// Evaluate phase acceptance.
    #[must_use]
    pub fn evaluate_phase(phase: &ScenarioPhase, result: &PhaseResult) -> bool {
        result.success
            && result.elapsed_ms <= phase.acceptance.max_latency_ms
            && result.friction_count() <= phase.acceptance.max_friction_events
    }

    /// Compute verdict for a complete execution against its spec.
    #[must_use]
    pub fn compute_verdict(spec: &ScenarioSpec, execution: &ScenarioExecution) -> ScenarioVerdict {
        let mut any_required_failed = false;
        let mut all_passed = true;

        for phase_spec in &spec.phases {
            let result = execution
                .phase_results
                .iter()
                .find(|r| r.phase_id == phase_spec.phase_id);

            match result {
                Some(r) => {
                    let met = Self::evaluate_phase(phase_spec, r);
                    if !met {
                        all_passed = false;
                        if phase_spec.acceptance.required {
                            any_required_failed = true;
                        }
                    }
                }
                None => {
                    // Missing phase result — treat as failure.
                    all_passed = false;
                    if phase_spec.acceptance.required {
                        any_required_failed = true;
                    }
                }
            }
        }

        if any_required_failed {
            ScenarioVerdict::Fail
        } else if all_passed {
            ScenarioVerdict::Pass
        } else {
            ScenarioVerdict::Degraded
        }
    }
}

// =============================================================================
// UX telemetry aggregation
// =============================================================================

/// Aggregate UX telemetry across multiple scenario executions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UxTelemetry {
    /// Total scenarios executed.
    pub scenarios_executed: u64,
    /// Scenarios that passed.
    pub scenarios_passed: u64,
    /// Scenarios that were degraded.
    pub scenarios_degraded: u64,
    /// Scenarios that failed.
    pub scenarios_failed: u64,
    /// Total phases executed.
    pub total_phases: u64,
    /// Total phases passed.
    pub phases_passed: u64,
    /// Total friction events.
    pub total_friction_events: u64,
    /// Friction events by category.
    pub friction_by_category: HashMap<String, u64>,
    /// Per-workflow-class pass rates.
    pub workflow_pass_rates: HashMap<String, (u64, u64)>, // (passed, total)
    /// All recorded phase latencies (for percentile computation).
    phase_latencies_ms: Vec<u64>,
    /// Error recovery attempts and successes.
    pub error_recovery_attempts: u64,
    pub error_recovery_successes: u64,
    /// Intervention attempts and successes.
    pub intervention_attempts: u64,
    pub intervention_successes: u64,
}

impl UxTelemetry {
    /// Create empty telemetry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            scenarios_executed: 0,
            scenarios_passed: 0,
            scenarios_degraded: 0,
            scenarios_failed: 0,
            total_phases: 0,
            phases_passed: 0,
            total_friction_events: 0,
            friction_by_category: HashMap::new(),
            workflow_pass_rates: HashMap::new(),
            phase_latencies_ms: Vec::new(),
            error_recovery_attempts: 0,
            error_recovery_successes: 0,
            intervention_attempts: 0,
            intervention_successes: 0,
        }
    }

    /// Record a scenario execution.
    pub fn record_execution(&mut self, spec: &ScenarioSpec, execution: &ScenarioExecution) {
        self.scenarios_executed += 1;

        match execution.verdict {
            ScenarioVerdict::Pass => self.scenarios_passed += 1,
            ScenarioVerdict::Degraded => self.scenarios_degraded += 1,
            ScenarioVerdict::Fail => self.scenarios_failed += 1,
        }

        let wf = spec.workflow_class.label().to_string();
        let entry = self.workflow_pass_rates.entry(wf).or_insert((0, 0));
        entry.1 += 1;
        if execution.verdict == ScenarioVerdict::Pass {
            entry.0 += 1;
        }

        for result in &execution.phase_results {
            self.total_phases += 1;
            if result.success {
                self.phases_passed += 1;
            }
            self.phase_latencies_ms.push(result.elapsed_ms);

            for friction in &result.friction_events {
                self.total_friction_events += 1;
                *self
                    .friction_by_category
                    .entry(friction.category.label().to_string())
                    .or_insert(0) += 1;
            }

            // Track recovery and intervention metrics from step types.
            if let Some(phase_spec) = spec.phases.iter().find(|p| p.phase_id == result.phase_id) {
                if phase_spec.step_type == StepType::Recover {
                    self.error_recovery_attempts += 1;
                    if result.success {
                        self.error_recovery_successes += 1;
                    }
                }
                if phase_spec.step_type == StepType::Execute
                    && spec.workflow_class == WorkflowClass::Intervention
                {
                    self.intervention_attempts += 1;
                    if result.success {
                        self.intervention_successes += 1;
                    }
                }
            }
        }
    }

    /// Task completion rate (phases passed / total).
    #[must_use]
    pub fn task_completion_rate(&self) -> f64 {
        if self.total_phases == 0 {
            return 0.0;
        }
        self.phases_passed as f64 / self.total_phases as f64
    }

    /// Mean friction per scenario.
    #[must_use]
    pub fn mean_friction_per_scenario(&self) -> f64 {
        if self.scenarios_executed == 0 {
            return 0.0;
        }
        self.total_friction_events as f64 / self.scenarios_executed as f64
    }

    /// P95 phase latency.
    #[must_use]
    pub fn p95_latency_ms(&self) -> u64 {
        if self.phase_latencies_ms.is_empty() {
            return 0;
        }
        let mut sorted = self.phase_latencies_ms.clone();
        sorted.sort_unstable();
        let idx = ((sorted.len() as f64 * 0.95).ceil() as usize).min(sorted.len()) - 1;
        sorted[idx]
    }

    /// Error recovery rate.
    #[must_use]
    pub fn error_recovery_rate(&self) -> f64 {
        if self.error_recovery_attempts == 0 {
            return 1.0; // No errors → perfect recovery.
        }
        self.error_recovery_successes as f64 / self.error_recovery_attempts as f64
    }

    /// Intervention success rate.
    #[must_use]
    pub fn intervention_success_rate(&self) -> f64 {
        if self.intervention_attempts == 0 {
            return 1.0; // No interventions → perfect.
        }
        self.intervention_successes as f64 / self.intervention_attempts as f64
    }

    /// Scenario pass rate.
    #[must_use]
    pub fn scenario_pass_rate(&self) -> f64 {
        if self.scenarios_executed == 0 {
            return 0.0;
        }
        self.scenarios_passed as f64 / self.scenarios_executed as f64
    }
}

impl Default for UxTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Go/no-go evaluation
// =============================================================================

/// Individual gate check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateCheck {
    /// Gate name.
    pub name: String,
    /// Whether the gate passed.
    pub passed: bool,
    /// Observed value.
    pub observed: String,
    /// Required threshold.
    pub threshold: String,
}

/// Go/no-go evaluation result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoNoGoEvaluation {
    /// Individual gate checks.
    pub checks: Vec<GateCheck>,
    /// Overall verdict.
    pub go: bool,
    /// Summary message.
    pub summary: String,
}

impl GoNoGoEvaluation {
    /// Evaluate UX telemetry against thresholds.
    #[must_use]
    pub fn evaluate(telemetry: &UxTelemetry, thresholds: &UxThresholds) -> Self {
        let mut checks = Vec::new();

        let completion = telemetry.task_completion_rate();
        checks.push(GateCheck {
            name: "task-completion-rate".into(),
            passed: completion >= thresholds.min_completion_rate,
            observed: format!("{:.2}", completion),
            threshold: format!(">= {:.2}", thresholds.min_completion_rate),
        });

        let p95 = telemetry.p95_latency_ms();
        checks.push(GateCheck {
            name: "p95-latency-ms".into(),
            passed: p95 <= thresholds.max_p95_latency_ms,
            observed: format!("{}", p95),
            threshold: format!("<= {}", thresholds.max_p95_latency_ms),
        });

        let friction = telemetry.mean_friction_per_scenario();
        checks.push(GateCheck {
            name: "mean-friction".into(),
            passed: friction <= thresholds.max_mean_friction,
            observed: format!("{:.2}", friction),
            threshold: format!("<= {:.2}", thresholds.max_mean_friction),
        });

        let recovery = telemetry.error_recovery_rate();
        checks.push(GateCheck {
            name: "error-recovery-rate".into(),
            passed: recovery >= thresholds.min_error_recovery_rate,
            observed: format!("{:.2}", recovery),
            threshold: format!(">= {:.2}", thresholds.min_error_recovery_rate),
        });

        let intervention = telemetry.intervention_success_rate();
        checks.push(GateCheck {
            name: "intervention-success-rate".into(),
            passed: intervention >= thresholds.min_intervention_success_rate,
            observed: format!("{:.2}", intervention),
            threshold: format!(">= {:.2}", thresholds.min_intervention_success_rate),
        });

        let go = checks.iter().all(|c| c.passed);
        let pass_count = checks.iter().filter(|c| c.passed).count();
        let total = checks.len();
        let summary = if go {
            format!("GO: all {total} gates passed")
        } else {
            format!("NO-GO: {pass_count}/{total} gates passed")
        };

        Self {
            checks,
            go,
            summary,
        }
    }

    /// Render a human-readable report.
    #[must_use]
    pub fn render(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("=== UX Go/No-Go Evaluation ==="));
        lines.push(format!("Verdict: {}", self.summary));
        lines.push(String::new());

        for check in &self.checks {
            let icon = if check.passed { "PASS" } else { "FAIL" };
            lines.push(format!(
                "  [{}] {} — observed: {}, threshold: {}",
                icon, check.name, check.observed, check.threshold
            ));
        }

        lines.join("\n")
    }
}

// =============================================================================
// Standard scenario catalog
// =============================================================================

/// Build a standard launch scenario spec.
#[must_use]
pub fn launch_scenario() -> ScenarioSpec {
    ScenarioSpec {
        scenario_id: "UX-SC-001-launch".into(),
        name: "Fleet launch and initial health check".into(),
        workflow_class: WorkflowClass::Launch,
        phases: vec![
            ScenarioPhase {
                phase_id: "launch-01-start".into(),
                description: "Invoke fleet launch command".into(),
                step_type: StepType::Execute,
                acceptance: PhaseAcceptance::strict(),
            },
            ScenarioPhase {
                phase_id: "launch-02-panes-visible".into(),
                description: "Verify all requested panes are visible and active".into(),
                step_type: StepType::Verify,
                acceptance: PhaseAcceptance {
                    max_latency_ms: 3000,
                    required: true,
                    max_friction_events: 1,
                },
            },
            ScenarioPhase {
                phase_id: "launch-03-dashboard-check".into(),
                description: "Open fleet dashboard and confirm health indicators".into(),
                step_type: StepType::Inspect,
                acceptance: PhaseAcceptance::lenient(),
            },
        ],
        thresholds: UxThresholds::release_gate(),
    }
}

/// Build a standard triage scenario spec.
#[must_use]
pub fn triage_scenario() -> ScenarioSpec {
    ScenarioSpec {
        scenario_id: "UX-SC-002-triage".into(),
        name: "Error detection and triage workflow".into(),
        workflow_class: WorkflowClass::Triage,
        phases: vec![
            ScenarioPhase {
                phase_id: "triage-01-alert".into(),
                description: "Observe error alert in dashboard".into(),
                step_type: StepType::Navigate,
                acceptance: PhaseAcceptance::strict(),
            },
            ScenarioPhase {
                phase_id: "triage-02-inspect".into(),
                description: "Inspect error details and explainability trace".into(),
                step_type: StepType::Inspect,
                acceptance: PhaseAcceptance {
                    max_latency_ms: 2000,
                    required: true,
                    max_friction_events: 1,
                },
            },
            ScenarioPhase {
                phase_id: "triage-03-decide".into(),
                description: "Decide remediation action".into(),
                step_type: StepType::Execute,
                acceptance: PhaseAcceptance::strict(),
            },
            ScenarioPhase {
                phase_id: "triage-04-verify".into(),
                description: "Verify remediation took effect".into(),
                step_type: StepType::Verify,
                acceptance: PhaseAcceptance::lenient(),
            },
        ],
        thresholds: UxThresholds::release_gate(),
    }
}

/// Build a standard intervention scenario spec.
#[must_use]
pub fn intervention_scenario() -> ScenarioSpec {
    ScenarioSpec {
        scenario_id: "UX-SC-003-intervention".into(),
        name: "Live pane intervention workflow".into(),
        workflow_class: WorkflowClass::Intervention,
        phases: vec![
            ScenarioPhase {
                phase_id: "intv-01-identify".into(),
                description: "Identify problematic pane".into(),
                step_type: StepType::Navigate,
                acceptance: PhaseAcceptance::strict(),
            },
            ScenarioPhase {
                phase_id: "intv-02-pause".into(),
                description: "Pause pane to prevent further damage".into(),
                step_type: StepType::Execute,
                acceptance: PhaseAcceptance::strict(),
            },
            ScenarioPhase {
                phase_id: "intv-03-investigate".into(),
                description: "Inspect pane state and context budget".into(),
                step_type: StepType::Inspect,
                acceptance: PhaseAcceptance {
                    max_latency_ms: 2000,
                    required: true,
                    max_friction_events: 1,
                },
            },
            ScenarioPhase {
                phase_id: "intv-04-recover".into(),
                description: "Resume or takeover pane".into(),
                step_type: StepType::Recover,
                acceptance: PhaseAcceptance::strict(),
            },
            ScenarioPhase {
                phase_id: "intv-05-verify".into(),
                description: "Verify pane is operational".into(),
                step_type: StepType::Verify,
                acceptance: PhaseAcceptance::lenient(),
            },
        ],
        thresholds: UxThresholds::release_gate(),
    }
}

/// Build a standard approval scenario spec.
#[must_use]
pub fn approval_scenario() -> ScenarioSpec {
    ScenarioSpec {
        scenario_id: "UX-SC-004-approval".into(),
        name: "Approval queue processing workflow".into(),
        workflow_class: WorkflowClass::Approval,
        phases: vec![
            ScenarioPhase {
                phase_id: "appr-01-queue".into(),
                description: "Open approval queue view".into(),
                step_type: StepType::Navigate,
                acceptance: PhaseAcceptance::strict(),
            },
            ScenarioPhase {
                phase_id: "appr-02-review".into(),
                description: "Review pending approval details and risk level".into(),
                step_type: StepType::Inspect,
                acceptance: PhaseAcceptance {
                    max_latency_ms: 2000,
                    required: true,
                    max_friction_events: 1,
                },
            },
            ScenarioPhase {
                phase_id: "appr-03-decide".into(),
                description: "Approve or reject request".into(),
                step_type: StepType::Confirm,
                acceptance: PhaseAcceptance::strict(),
            },
            ScenarioPhase {
                phase_id: "appr-04-feedback".into(),
                description: "Verify action was applied and pane state updated".into(),
                step_type: StepType::WaitForFeedback,
                acceptance: PhaseAcceptance::lenient(),
            },
        ],
        thresholds: UxThresholds::release_gate(),
    }
}

/// Build a standard incident handling scenario spec.
#[must_use]
pub fn incident_handling_scenario() -> ScenarioSpec {
    ScenarioSpec {
        scenario_id: "UX-SC-005-incident".into(),
        name: "Incident detection and response workflow".into(),
        workflow_class: WorkflowClass::IncidentHandling,
        phases: vec![
            ScenarioPhase {
                phase_id: "inc-01-detect".into(),
                description: "Detect critical alert in fleet dashboard".into(),
                step_type: StepType::Navigate,
                acceptance: PhaseAcceptance::strict(),
            },
            ScenarioPhase {
                phase_id: "inc-02-scope".into(),
                description: "Scope blast radius using explainability traces".into(),
                step_type: StepType::Inspect,
                acceptance: PhaseAcceptance {
                    max_latency_ms: 3000,
                    required: true,
                    max_friction_events: 1,
                },
            },
            ScenarioPhase {
                phase_id: "inc-03-emergency".into(),
                description: "Execute emergency stop if warranted".into(),
                step_type: StepType::Execute,
                acceptance: PhaseAcceptance::strict(),
            },
            ScenarioPhase {
                phase_id: "inc-04-recover".into(),
                description: "Recover affected panes and verify stability".into(),
                step_type: StepType::Recover,
                acceptance: PhaseAcceptance {
                    max_latency_ms: 5000,
                    required: true,
                    max_friction_events: 2,
                },
            },
            ScenarioPhase {
                phase_id: "inc-05-postmortem".into(),
                description: "Review audit trail and decision log".into(),
                step_type: StepType::Inspect,
                acceptance: PhaseAcceptance::lenient(),
            },
        ],
        thresholds: UxThresholds::release_gate(),
    }
}

/// Build all standard scenarios.
#[must_use]
pub fn standard_scenarios() -> Vec<ScenarioSpec> {
    vec![
        launch_scenario(),
        triage_scenario(),
        intervention_scenario(),
        approval_scenario(),
        incident_handling_scenario(),
    ]
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Scenario spec tests ---

    #[test]
    fn standard_scenarios_cover_all_workflow_classes() {
        let scenarios = standard_scenarios();
        assert!(scenarios.len() >= 5);

        let classes: Vec<WorkflowClass> = scenarios.iter().map(|s| s.workflow_class).collect();
        assert!(classes.contains(&WorkflowClass::Launch));
        assert!(classes.contains(&WorkflowClass::Triage));
        assert!(classes.contains(&WorkflowClass::Intervention));
        assert!(classes.contains(&WorkflowClass::Approval));
        assert!(classes.contains(&WorkflowClass::IncidentHandling));
    }

    #[test]
    fn scenario_specs_have_nonempty_phases() {
        for spec in standard_scenarios() {
            assert!(
                !spec.phases.is_empty(),
                "scenario {} has no phases",
                spec.scenario_id
            );
            assert!(
                spec.required_phase_count() > 0,
                "scenario {} has no required phases",
                spec.scenario_id
            );
            assert_eq!(spec.total_phase_count(), spec.phases.len());
        }
    }

    #[test]
    fn phase_ids_unique_within_scenario() {
        for spec in standard_scenarios() {
            let mut ids: Vec<&str> = spec.phases.iter().map(|p| p.phase_id.as_str()).collect();
            ids.sort();
            ids.dedup();
            assert_eq!(
                ids.len(),
                spec.phases.len(),
                "duplicate phase IDs in {}",
                spec.scenario_id
            );
        }
    }

    // --- Phase acceptance tests ---

    #[test]
    fn strict_acceptance_is_stricter_than_lenient() {
        let strict = PhaseAcceptance::strict();
        let lenient = PhaseAcceptance::lenient();
        assert!(strict.max_latency_ms <= lenient.max_latency_ms);
        assert!(strict.max_friction_events <= lenient.max_friction_events);
        assert!(strict.required);
    }

    // --- Phase result tests ---

    #[test]
    fn phase_result_success() {
        let r = PhaseResult::success("test-01", 500);
        assert!(r.success);
        assert_eq!(r.elapsed_ms, 500);
        assert!(r.error.is_none());
        assert_eq!(r.friction_count(), 0);
    }

    #[test]
    fn phase_result_failure() {
        let r = PhaseResult::failure("test-01", 1500, "timed out");
        assert!(!r.success);
        assert_eq!(r.error.as_deref(), Some("timed out"));
    }

    #[test]
    fn phase_result_friction_tracking() {
        let mut r = PhaseResult::success("test-01", 300);
        r.add_friction(FrictionEvent {
            description: "had to retry".into(),
            category: FrictionCategory::Retry,
            at_ms: 100,
        });
        r.add_friction(FrictionEvent {
            description: "confusing error message".into(),
            category: FrictionCategory::ConfusingFeedback,
            at_ms: 200,
        });
        assert_eq!(r.friction_count(), 2);
    }

    // --- Evaluator tests ---

    #[test]
    fn evaluate_phase_pass() {
        let phase = ScenarioPhase {
            phase_id: "p1".into(),
            description: "test".into(),
            step_type: StepType::Execute,
            acceptance: PhaseAcceptance {
                max_latency_ms: 1000,
                required: true,
                max_friction_events: 1,
            },
        };
        let result = PhaseResult::success("p1", 800);
        assert!(ScenarioEvaluator::evaluate_phase(&phase, &result));
    }

    #[test]
    fn evaluate_phase_fail_latency() {
        let phase = ScenarioPhase {
            phase_id: "p1".into(),
            description: "test".into(),
            step_type: StepType::Execute,
            acceptance: PhaseAcceptance {
                max_latency_ms: 500,
                required: true,
                max_friction_events: 1,
            },
        };
        let result = PhaseResult::success("p1", 1200);
        assert!(!ScenarioEvaluator::evaluate_phase(&phase, &result));
    }

    #[test]
    fn evaluate_phase_fail_friction() {
        let phase = ScenarioPhase {
            phase_id: "p1".into(),
            description: "test".into(),
            step_type: StepType::Execute,
            acceptance: PhaseAcceptance {
                max_latency_ms: 5000,
                required: true,
                max_friction_events: 0,
            },
        };
        let mut result = PhaseResult::success("p1", 300);
        result.add_friction(FrictionEvent {
            description: "retry".into(),
            category: FrictionCategory::Retry,
            at_ms: 50,
        });
        assert!(!ScenarioEvaluator::evaluate_phase(&phase, &result));
    }

    #[test]
    fn verdict_pass_all_phases_ok() {
        let spec = ScenarioSpec {
            scenario_id: "test".into(),
            name: "Test".into(),
            workflow_class: WorkflowClass::Launch,
            phases: vec![
                ScenarioPhase {
                    phase_id: "p1".into(),
                    description: "a".into(),
                    step_type: StepType::Execute,
                    acceptance: PhaseAcceptance::strict(),
                },
                ScenarioPhase {
                    phase_id: "p2".into(),
                    description: "b".into(),
                    step_type: StepType::Verify,
                    acceptance: PhaseAcceptance::lenient(),
                },
            ],
            thresholds: UxThresholds::release_gate(),
        };

        let execution = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Launch,
            started_at_ms: 1000,
            ended_at_ms: 2000,
            phase_results: vec![
                PhaseResult::success("p1", 500),
                PhaseResult::success("p2", 800),
            ],
            verdict: ScenarioVerdict::Pass, // Will be recomputed.
            notes: vec![],
        };

        let verdict = ScenarioEvaluator::compute_verdict(&spec, &execution);
        assert_eq!(verdict, ScenarioVerdict::Pass);
    }

    #[test]
    fn verdict_fail_required_phase_fails() {
        let spec = ScenarioSpec {
            scenario_id: "test".into(),
            name: "Test".into(),
            workflow_class: WorkflowClass::Launch,
            phases: vec![ScenarioPhase {
                phase_id: "p1".into(),
                description: "critical step".into(),
                step_type: StepType::Execute,
                acceptance: PhaseAcceptance::strict(),
            }],
            thresholds: UxThresholds::release_gate(),
        };

        let execution = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Launch,
            started_at_ms: 1000,
            ended_at_ms: 3000,
            phase_results: vec![PhaseResult::failure("p1", 2000, "crashed")],
            verdict: ScenarioVerdict::Fail,
            notes: vec![],
        };

        let verdict = ScenarioEvaluator::compute_verdict(&spec, &execution);
        assert_eq!(verdict, ScenarioVerdict::Fail);
    }

    #[test]
    fn verdict_degraded_optional_phase_fails() {
        let spec = ScenarioSpec {
            scenario_id: "test".into(),
            name: "Test".into(),
            workflow_class: WorkflowClass::Launch,
            phases: vec![
                ScenarioPhase {
                    phase_id: "p1".into(),
                    description: "required".into(),
                    step_type: StepType::Execute,
                    acceptance: PhaseAcceptance::strict(),
                },
                ScenarioPhase {
                    phase_id: "p2".into(),
                    description: "optional".into(),
                    step_type: StepType::Inspect,
                    acceptance: PhaseAcceptance::lenient(),
                },
            ],
            thresholds: UxThresholds::release_gate(),
        };

        let execution = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Launch,
            started_at_ms: 1000,
            ended_at_ms: 3000,
            phase_results: vec![
                PhaseResult::success("p1", 500),
                PhaseResult::failure("p2", 6000, "slow"),
            ],
            verdict: ScenarioVerdict::Degraded,
            notes: vec![],
        };

        let verdict = ScenarioEvaluator::compute_verdict(&spec, &execution);
        assert_eq!(verdict, ScenarioVerdict::Degraded);
    }

    #[test]
    fn verdict_fail_missing_required_phase() {
        let spec = ScenarioSpec {
            scenario_id: "test".into(),
            name: "Test".into(),
            workflow_class: WorkflowClass::Launch,
            phases: vec![ScenarioPhase {
                phase_id: "p1".into(),
                description: "must exist".into(),
                step_type: StepType::Execute,
                acceptance: PhaseAcceptance::strict(),
            }],
            thresholds: UxThresholds::release_gate(),
        };

        let execution = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Launch,
            started_at_ms: 1000,
            ended_at_ms: 1500,
            phase_results: vec![], // No results at all.
            verdict: ScenarioVerdict::Fail,
            notes: vec![],
        };

        let verdict = ScenarioEvaluator::compute_verdict(&spec, &execution);
        assert_eq!(verdict, ScenarioVerdict::Fail);
    }

    // --- Execution stats tests ---

    #[test]
    fn execution_stats() {
        let execution = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Triage,
            started_at_ms: 1000,
            ended_at_ms: 5000,
            phase_results: vec![
                PhaseResult::success("p1", 200),
                PhaseResult::success("p2", 500),
                PhaseResult::failure("p3", 3000, "error"),
            ],
            verdict: ScenarioVerdict::Degraded,
            notes: vec![],
        };

        assert_eq!(execution.total_elapsed_ms(), 4000);
        assert_eq!(execution.phases_passed(), 2);
        assert_eq!(execution.phases_failed(), 1);
        assert!((execution.completion_rate() - 0.6667).abs() < 0.01);
    }

    #[test]
    fn execution_p95_latency() {
        let execution = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Launch,
            started_at_ms: 0,
            ended_at_ms: 5000,
            phase_results: vec![
                PhaseResult::success("p1", 100),
                PhaseResult::success("p2", 200),
                PhaseResult::success("p3", 300),
                PhaseResult::success("p4", 400),
                PhaseResult::success("p5", 500),
                PhaseResult::success("p6", 600),
                PhaseResult::success("p7", 700),
                PhaseResult::success("p8", 800),
                PhaseResult::success("p9", 900),
                PhaseResult::success("p10", 1000),
            ],
            verdict: ScenarioVerdict::Pass,
            notes: vec![],
        };

        assert_eq!(execution.p95_latency_ms(), 1000);
    }

    #[test]
    fn execution_friction_aggregation() {
        let mut r1 = PhaseResult::success("p1", 200);
        r1.add_friction(FrictionEvent {
            description: "retry".into(),
            category: FrictionCategory::Retry,
            at_ms: 50,
        });

        let mut r2 = PhaseResult::success("p2", 300);
        r2.add_friction(FrictionEvent {
            description: "confusing".into(),
            category: FrictionCategory::ConfusingFeedback,
            at_ms: 100,
        });
        r2.add_friction(FrictionEvent {
            description: "slow".into(),
            category: FrictionCategory::Sluggish,
            at_ms: 200,
        });

        let execution = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Triage,
            started_at_ms: 0,
            ended_at_ms: 600,
            phase_results: vec![r1, r2],
            verdict: ScenarioVerdict::Degraded,
            notes: vec![],
        };

        assert_eq!(execution.total_friction(), 3);
        assert!((execution.mean_friction() - 1.5).abs() < 0.01);
    }

    // --- UX Telemetry tests ---

    #[test]
    fn telemetry_empty_defaults() {
        let t = UxTelemetry::new();
        assert_eq!(t.scenarios_executed, 0);
        assert_eq!(t.task_completion_rate(), 0.0);
        assert_eq!(t.p95_latency_ms(), 0);
        assert_eq!(t.mean_friction_per_scenario(), 0.0);
        assert_eq!(t.error_recovery_rate(), 1.0);
        assert_eq!(t.intervention_success_rate(), 1.0);
    }

    #[test]
    fn telemetry_records_execution() {
        let spec = launch_scenario();
        let execution = ScenarioExecution {
            scenario_id: spec.scenario_id.clone(),
            workflow_class: spec.workflow_class,
            started_at_ms: 0,
            ended_at_ms: 3000,
            phase_results: vec![
                PhaseResult::success("launch-01-start", 500),
                PhaseResult::success("launch-02-panes-visible", 800),
                PhaseResult::success("launch-03-dashboard-check", 600),
            ],
            verdict: ScenarioVerdict::Pass,
            notes: vec![],
        };

        let mut telem = UxTelemetry::new();
        telem.record_execution(&spec, &execution);

        assert_eq!(telem.scenarios_executed, 1);
        assert_eq!(telem.scenarios_passed, 1);
        assert_eq!(telem.total_phases, 3);
        assert_eq!(telem.phases_passed, 3);
        assert_eq!(telem.task_completion_rate(), 1.0);
        assert_eq!(telem.scenario_pass_rate(), 1.0);
    }

    #[test]
    fn telemetry_tracks_intervention_metrics() {
        let spec = intervention_scenario();
        let execution = ScenarioExecution {
            scenario_id: spec.scenario_id.clone(),
            workflow_class: spec.workflow_class,
            started_at_ms: 0,
            ended_at_ms: 5000,
            phase_results: vec![
                PhaseResult::success("intv-01-identify", 300),
                PhaseResult::success("intv-02-pause", 400), // Execute step in Intervention
                PhaseResult::success("intv-03-investigate", 800),
                PhaseResult::success("intv-04-recover", 600), // Recover step
                PhaseResult::success("intv-05-verify", 500),
            ],
            verdict: ScenarioVerdict::Pass,
            notes: vec![],
        };

        let mut telem = UxTelemetry::new();
        telem.record_execution(&spec, &execution);

        assert_eq!(telem.intervention_attempts, 1);
        assert_eq!(telem.intervention_successes, 1);
        assert_eq!(telem.error_recovery_attempts, 1);
        assert_eq!(telem.error_recovery_successes, 1);
        assert_eq!(telem.intervention_success_rate(), 1.0);
        assert_eq!(telem.error_recovery_rate(), 1.0);
    }

    #[test]
    fn telemetry_tracks_failed_recovery() {
        let spec = intervention_scenario();
        let execution = ScenarioExecution {
            scenario_id: spec.scenario_id.clone(),
            workflow_class: spec.workflow_class,
            started_at_ms: 0,
            ended_at_ms: 5000,
            phase_results: vec![
                PhaseResult::success("intv-01-identify", 300),
                PhaseResult::failure("intv-02-pause", 1500, "permission denied"),
                PhaseResult::success("intv-03-investigate", 800),
                PhaseResult::failure("intv-04-recover", 2000, "pane unresponsive"),
                PhaseResult::success("intv-05-verify", 500),
            ],
            verdict: ScenarioVerdict::Fail,
            notes: vec![],
        };

        let mut telem = UxTelemetry::new();
        telem.record_execution(&spec, &execution);

        assert_eq!(telem.intervention_attempts, 1);
        assert_eq!(telem.intervention_successes, 0);
        assert_eq!(telem.error_recovery_attempts, 1);
        assert_eq!(telem.error_recovery_successes, 0);
        assert_eq!(telem.intervention_success_rate(), 0.0);
        assert_eq!(telem.error_recovery_rate(), 0.0);
    }

    #[test]
    fn telemetry_friction_by_category() {
        let spec = triage_scenario();
        let mut r2 = PhaseResult::success("triage-02-inspect", 800);
        r2.add_friction(FrictionEvent {
            description: "missing context".into(),
            category: FrictionCategory::MissingInfo,
            at_ms: 100,
        });
        r2.add_friction(FrictionEvent {
            description: "another missing".into(),
            category: FrictionCategory::MissingInfo,
            at_ms: 200,
        });

        let execution = ScenarioExecution {
            scenario_id: spec.scenario_id.clone(),
            workflow_class: spec.workflow_class,
            started_at_ms: 0,
            ended_at_ms: 3000,
            phase_results: vec![
                PhaseResult::success("triage-01-alert", 300),
                r2,
                PhaseResult::success("triage-03-decide", 400),
                PhaseResult::success("triage-04-verify", 500),
            ],
            verdict: ScenarioVerdict::Pass,
            notes: vec![],
        };

        let mut telem = UxTelemetry::new();
        telem.record_execution(&spec, &execution);

        assert_eq!(telem.total_friction_events, 2);
        assert_eq!(
            *telem.friction_by_category.get("missing-info").unwrap_or(&0),
            2
        );
    }

    #[test]
    fn telemetry_workflow_pass_rates() {
        let launch = launch_scenario();
        let triage = triage_scenario();

        let pass_exec = |spec: &ScenarioSpec| -> ScenarioExecution {
            ScenarioExecution {
                scenario_id: spec.scenario_id.clone(),
                workflow_class: spec.workflow_class,
                started_at_ms: 0,
                ended_at_ms: 1000,
                phase_results: spec
                    .phases
                    .iter()
                    .map(|p| PhaseResult::success(&p.phase_id, 200))
                    .collect(),
                verdict: ScenarioVerdict::Pass,
                notes: vec![],
            }
        };

        let fail_exec = |spec: &ScenarioSpec| -> ScenarioExecution {
            ScenarioExecution {
                scenario_id: spec.scenario_id.clone(),
                workflow_class: spec.workflow_class,
                started_at_ms: 0,
                ended_at_ms: 3000,
                phase_results: spec
                    .phases
                    .iter()
                    .map(|p| PhaseResult::failure(&p.phase_id, 2000, "fail"))
                    .collect(),
                verdict: ScenarioVerdict::Fail,
                notes: vec![],
            }
        };

        let mut telem = UxTelemetry::new();
        telem.record_execution(&launch, &pass_exec(&launch));
        telem.record_execution(&launch, &pass_exec(&launch));
        telem.record_execution(&triage, &pass_exec(&triage));
        telem.record_execution(&triage, &fail_exec(&triage));

        assert_eq!(telem.scenarios_executed, 4);
        assert_eq!(telem.scenarios_passed, 3);

        let launch_rate = telem.workflow_pass_rates.get("launch").unwrap();
        assert_eq!(launch_rate, &(2, 2));

        let triage_rate = telem.workflow_pass_rates.get("triage").unwrap();
        assert_eq!(triage_rate, &(1, 2));
    }

    // --- Go/no-go evaluation tests ---

    #[test]
    fn go_nogo_all_pass() {
        let mut telem = UxTelemetry::new();
        let spec = launch_scenario();
        let execution = ScenarioExecution {
            scenario_id: spec.scenario_id.clone(),
            workflow_class: spec.workflow_class,
            started_at_ms: 0,
            ended_at_ms: 2000,
            phase_results: vec![
                PhaseResult::success("launch-01-start", 300),
                PhaseResult::success("launch-02-panes-visible", 500),
                PhaseResult::success("launch-03-dashboard-check", 400),
            ],
            verdict: ScenarioVerdict::Pass,
            notes: vec![],
        };
        telem.record_execution(&spec, &execution);

        let eval = GoNoGoEvaluation::evaluate(&telem, &UxThresholds::release_gate());
        assert!(eval.go);
        assert!(eval.checks.iter().all(|c| c.passed));
        assert!(eval.summary.contains("GO"));
    }

    #[test]
    fn go_nogo_completion_rate_fail() {
        let mut telem = UxTelemetry::new();
        let spec = launch_scenario();

        // Fail most phases to drop completion rate below threshold.
        let execution = ScenarioExecution {
            scenario_id: spec.scenario_id.clone(),
            workflow_class: spec.workflow_class,
            started_at_ms: 0,
            ended_at_ms: 5000,
            phase_results: vec![
                PhaseResult::failure("launch-01-start", 500, "crash"),
                PhaseResult::failure("launch-02-panes-visible", 500, "timeout"),
                PhaseResult::failure("launch-03-dashboard-check", 500, "error"),
            ],
            verdict: ScenarioVerdict::Fail,
            notes: vec![],
        };
        telem.record_execution(&spec, &execution);

        let eval = GoNoGoEvaluation::evaluate(&telem, &UxThresholds::release_gate());
        assert!(!eval.go);
        let completion_check = eval
            .checks
            .iter()
            .find(|c| c.name == "task-completion-rate")
            .unwrap();
        assert!(!completion_check.passed);
    }

    #[test]
    fn go_nogo_p95_latency_fail() {
        let mut telem = UxTelemetry::new();
        let spec = launch_scenario();

        let execution = ScenarioExecution {
            scenario_id: spec.scenario_id.clone(),
            workflow_class: spec.workflow_class,
            started_at_ms: 0,
            ended_at_ms: 10000,
            phase_results: vec![
                PhaseResult::success("launch-01-start", 300),
                PhaseResult::success("launch-02-panes-visible", 500),
                PhaseResult::success("launch-03-dashboard-check", 5000), // Very slow.
            ],
            verdict: ScenarioVerdict::Pass,
            notes: vec![],
        };
        telem.record_execution(&spec, &execution);

        let eval = GoNoGoEvaluation::evaluate(&telem, &UxThresholds::release_gate());
        assert!(!eval.go);
        let latency_check = eval
            .checks
            .iter()
            .find(|c| c.name == "p95-latency-ms")
            .unwrap();
        assert!(!latency_check.passed);
    }

    #[test]
    fn go_nogo_development_thresholds_more_lenient() {
        let mut telem = UxTelemetry::new();
        let spec = launch_scenario();

        let execution = ScenarioExecution {
            scenario_id: spec.scenario_id.clone(),
            workflow_class: spec.workflow_class,
            started_at_ms: 0,
            ended_at_ms: 8000,
            phase_results: vec![
                PhaseResult::success("launch-01-start", 1500),
                PhaseResult::success("launch-02-panes-visible", 3000),
                PhaseResult::success("launch-03-dashboard-check", 4000),
            ],
            verdict: ScenarioVerdict::Pass,
            notes: vec![],
        };
        telem.record_execution(&spec, &execution);

        // Fails release gate (p95 = 4000 > 2000).
        let release = GoNoGoEvaluation::evaluate(&telem, &UxThresholds::release_gate());
        assert!(!release.go);

        // Passes development gate (p95 = 4000 < 5000).
        let dev = GoNoGoEvaluation::evaluate(&telem, &UxThresholds::development());
        assert!(dev.go);
    }

    #[test]
    fn go_nogo_render_contains_details() {
        let telem = UxTelemetry::new();
        let eval = GoNoGoEvaluation::evaluate(&telem, &UxThresholds::release_gate());
        let rendered = eval.render();
        assert!(rendered.contains("Go/No-Go Evaluation"));
        assert!(rendered.contains("task-completion-rate"));
        assert!(rendered.contains("p95-latency-ms"));
    }

    // --- Serde roundtrip tests ---

    #[test]
    fn scenario_spec_serde_roundtrip() {
        let spec = launch_scenario();
        let json = serde_json::to_string(&spec).expect("serialize");
        let restored: ScenarioSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.scenario_id, spec.scenario_id);
        assert_eq!(restored.phases.len(), spec.phases.len());
    }

    #[test]
    fn execution_serde_roundtrip() {
        let execution = ScenarioExecution {
            scenario_id: "test".into(),
            workflow_class: WorkflowClass::Launch,
            started_at_ms: 1000,
            ended_at_ms: 3000,
            phase_results: vec![PhaseResult::success("p1", 500)],
            verdict: ScenarioVerdict::Pass,
            notes: vec!["ok".into()],
        };

        let json = serde_json::to_string(&execution).expect("serialize");
        let restored: ScenarioExecution = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.scenario_id, "test");
        assert_eq!(restored.verdict, ScenarioVerdict::Pass);
    }

    #[test]
    fn go_nogo_serde_roundtrip() {
        let telem = UxTelemetry::new();
        let eval = GoNoGoEvaluation::evaluate(&telem, &UxThresholds::release_gate());
        let json = serde_json::to_string(&eval).expect("serialize");
        let restored: GoNoGoEvaluation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.checks.len(), eval.checks.len());
    }

    // --- E2E lifecycle test ---

    #[test]
    fn e2e_full_validation_lifecycle() {
        // 1. Define scenarios.
        let scenarios = standard_scenarios();
        let mut telem = UxTelemetry::new();

        // 2. Execute all scenarios with passing results.
        for spec in &scenarios {
            let results: Vec<PhaseResult> = spec
                .phases
                .iter()
                .map(|p| PhaseResult::success(&p.phase_id, 400))
                .collect();

            let mut execution = ScenarioExecution {
                scenario_id: spec.scenario_id.clone(),
                workflow_class: spec.workflow_class,
                started_at_ms: 0,
                ended_at_ms: 2000,
                phase_results: results,
                verdict: ScenarioVerdict::Pass,
                notes: vec![],
            };

            execution.verdict = ScenarioEvaluator::compute_verdict(spec, &execution);
            assert_eq!(execution.verdict, ScenarioVerdict::Pass);

            telem.record_execution(spec, &execution);
        }

        // 3. Aggregate telemetry.
        assert_eq!(telem.scenarios_executed, 5);
        assert_eq!(telem.scenarios_passed, 5);
        assert_eq!(telem.task_completion_rate(), 1.0);

        // 4. Go/no-go evaluation.
        let eval = GoNoGoEvaluation::evaluate(&telem, &UxThresholds::release_gate());
        assert!(eval.go);
        assert!(eval.summary.contains("GO"));
    }

    #[test]
    fn e2e_mixed_results_lifecycle() {
        let launch = launch_scenario();
        let intervention = intervention_scenario();
        let mut telem = UxTelemetry::new();

        // Launch passes.
        let launch_exec = ScenarioExecution {
            scenario_id: launch.scenario_id.clone(),
            workflow_class: launch.workflow_class,
            started_at_ms: 0,
            ended_at_ms: 2000,
            phase_results: vec![
                PhaseResult::success("launch-01-start", 300),
                PhaseResult::success("launch-02-panes-visible", 500),
                PhaseResult::success("launch-03-dashboard-check", 400),
            ],
            verdict: ScenarioVerdict::Pass,
            notes: vec![],
        };
        telem.record_execution(&launch, &launch_exec);

        // Intervention fails recovery.
        let intv_exec = ScenarioExecution {
            scenario_id: intervention.scenario_id.clone(),
            workflow_class: intervention.workflow_class,
            started_at_ms: 0,
            ended_at_ms: 5000,
            phase_results: vec![
                PhaseResult::success("intv-01-identify", 300),
                PhaseResult::success("intv-02-pause", 400),
                PhaseResult::success("intv-03-investigate", 800),
                PhaseResult::failure("intv-04-recover", 2000, "unrecoverable"),
                PhaseResult::failure("intv-05-verify", 500, "skip"),
            ],
            verdict: ScenarioVerdict::Fail,
            notes: vec![],
        };
        telem.record_execution(&intervention, &intv_exec);

        // Mixed results should fail release gate.
        assert_eq!(telem.scenarios_passed, 1);
        assert_eq!(telem.scenarios_failed, 1);
        assert!(telem.task_completion_rate() < 1.0);
        assert_eq!(telem.error_recovery_rate(), 0.0);

        let eval = GoNoGoEvaluation::evaluate(&telem, &UxThresholds::release_gate());
        assert!(!eval.go);
    }

    // --- Workflow class label tests ---

    #[test]
    fn workflow_class_labels_unique() {
        let classes = [
            WorkflowClass::Launch,
            WorkflowClass::Triage,
            WorkflowClass::Intervention,
            WorkflowClass::Approval,
            WorkflowClass::IncidentHandling,
            WorkflowClass::MigrationOversight,
            WorkflowClass::ContextManagement,
            WorkflowClass::DashboardReview,
        ];

        let labels: Vec<&str> = classes.iter().map(|c| c.label()).collect();
        let mut deduped = labels.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(labels.len(), deduped.len());
    }

    #[test]
    fn friction_category_labels_unique() {
        let cats = [
            FrictionCategory::UnexpectedPrompt,
            FrictionCategory::Retry,
            FrictionCategory::ConfusingFeedback,
            FrictionCategory::MissingInfo,
            FrictionCategory::Sluggish,
            FrictionCategory::AccessibilityBarrier,
            FrictionCategory::NavigationConfusion,
        ];

        let labels: Vec<&str> = cats.iter().map(|c| c.label()).collect();
        let mut deduped = labels.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(labels.len(), deduped.len());
    }

    #[test]
    fn violation_severity_ordering_in_thresholds() {
        let release = UxThresholds::release_gate();
        let dev = UxThresholds::development();

        assert!(release.min_completion_rate >= dev.min_completion_rate);
        assert!(release.max_p95_latency_ms <= dev.max_p95_latency_ms);
        assert!(release.max_mean_friction <= dev.max_mean_friction);
    }
}
