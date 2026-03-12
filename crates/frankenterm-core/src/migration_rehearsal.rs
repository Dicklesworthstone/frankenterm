//! Migration rehearsal and rollback-drill suite (ft-3681t.8.6).
//!
//! Codifies repeatable migration rehearsals that exercise parity corpus,
//! dual-run shadow comparison, importer validation, cutover checkpoints,
//! and rollback drills before any production migration stage.
//!
//! # Architecture
//!
//! ```text
//! RehearsalSuite
//!   ├── RehearsalScenario[]    — individual test scenarios
//!   │     ├── ParityCheck      — run parity corpus against ft-native
//!   │     ├── ShadowComparison — dual-run NTM+ft divergence check
//!   │     ├── ImporterValidation — dry-run import of sessions/config
//!   │     ├── CutoverCheckpoint  — stage-gate evaluation checkpoint
//!   │     └── RollbackDrill      — forced rollback + recovery validation
//!   ├── RehearsalExecution     — timestamped execution with results
//!   ├── DrillMetrics           — time-to-recovery, data-integrity scores
//!   └── RehearsalReport        — aggregate evidence package
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Scenario types
// =============================================================================

/// Category of migration rehearsal scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ScenarioCategory {
    /// Run the parity corpus and check ft-native produces matching output.
    ParityCheck,
    /// Dual-run NTM and ft in shadow mode, measure divergence.
    ShadowComparison,
    /// Dry-run import of sessions, workflows, and config.
    ImporterValidation,
    /// Evaluate a cutover stage gate checkpoint.
    CutoverCheckpoint,
    /// Force a rollback and validate recovery.
    RollbackDrill,
}

impl ScenarioCategory {
    /// All categories.
    pub const ALL: &'static [ScenarioCategory] = &[
        Self::ParityCheck,
        Self::ShadowComparison,
        Self::ImporterValidation,
        Self::CutoverCheckpoint,
        Self::RollbackDrill,
    ];

    /// Human label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::ParityCheck => "Parity Check",
            Self::ShadowComparison => "Shadow Comparison",
            Self::ImporterValidation => "Importer Validation",
            Self::CutoverCheckpoint => "Cutover Checkpoint",
            Self::RollbackDrill => "Rollback Drill",
        }
    }

    /// Whether this category is blocking (must pass for rehearsal to succeed).
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        matches!(
            self,
            Self::ParityCheck | Self::RollbackDrill | Self::CutoverCheckpoint
        )
    }
}

/// Severity of a scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ScenarioSeverity {
    /// Informational — failure is logged but not blocking.
    Info,
    /// Warning — should be investigated but not blocking.
    Warning,
    /// Critical — failure blocks the migration.
    Critical,
}

// =============================================================================
// Scenarios
// =============================================================================

/// A single rehearsal scenario definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RehearsalScenario {
    /// Unique scenario identifier.
    pub scenario_id: String,
    /// Category.
    pub category: ScenarioCategory,
    /// Human-readable description.
    pub description: String,
    /// Severity level.
    pub severity: ScenarioSeverity,
    /// Expected duration in ms (for timeout planning).
    pub expected_duration_ms: u64,
    /// Command or script to execute (for automation).
    pub command: String,
    /// Tags for filtering.
    pub tags: Vec<String>,
    /// Preconditions that must be met.
    pub preconditions: Vec<String>,
}

impl RehearsalScenario {
    /// Create a new scenario.
    #[must_use]
    pub fn new(
        scenario_id: impl Into<String>,
        category: ScenarioCategory,
        description: impl Into<String>,
    ) -> Self {
        Self {
            scenario_id: scenario_id.into(),
            category,
            description: description.into(),
            severity: if category.is_blocking() {
                ScenarioSeverity::Critical
            } else {
                ScenarioSeverity::Warning
            },
            expected_duration_ms: 30_000,
            command: String::new(),
            tags: Vec::new(),
            preconditions: Vec::new(),
        }
    }

    /// Set severity.
    #[must_use]
    pub fn with_severity(mut self, severity: ScenarioSeverity) -> Self {
        self.severity = severity;
        self
    }

    /// Set expected duration.
    #[must_use]
    pub fn with_expected_duration(mut self, ms: u64) -> Self {
        self.expected_duration_ms = ms;
        self
    }

    /// Set command.
    #[must_use]
    pub fn with_command(mut self, cmd: impl Into<String>) -> Self {
        self.command = cmd.into();
        self
    }

    /// Add preconditions.
    #[must_use]
    pub fn with_preconditions(mut self, preconditions: &[&str]) -> Self {
        self.preconditions = preconditions.iter().map(|s| (*s).to_string()).collect();
        self
    }
}

// =============================================================================
// Execution results
// =============================================================================

/// Outcome of a single scenario execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScenarioOutcome {
    /// Scenario passed all checks.
    Pass,
    /// Scenario failed.
    Fail,
    /// Scenario was skipped (precondition not met).
    Skipped,
    /// Scenario timed out.
    Timeout,
}

impl ScenarioOutcome {
    /// Whether this is a passing outcome.
    #[must_use]
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    /// Whether this is a failure (not pass or skip).
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Fail | Self::Timeout)
    }
}

/// Result of executing a single scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    /// Which scenario was executed.
    pub scenario_id: String,
    /// Category.
    pub category: ScenarioCategory,
    /// Outcome.
    pub outcome: ScenarioOutcome,
    /// Duration in ms.
    pub duration_ms: u64,
    /// Error message if failed.
    pub error: String,
    /// Structured observations.
    pub observations: Vec<String>,
    /// Artifact paths produced.
    pub artifacts: Vec<String>,
    /// Metrics collected.
    pub metrics: BTreeMap<String, f64>,
}

impl ScenarioResult {
    /// Create a passing result.
    #[must_use]
    pub fn pass(scenario_id: impl Into<String>, category: ScenarioCategory, duration_ms: u64) -> Self {
        Self {
            scenario_id: scenario_id.into(),
            category,
            outcome: ScenarioOutcome::Pass,
            duration_ms,
            error: String::new(),
            observations: Vec::new(),
            artifacts: Vec::new(),
            metrics: BTreeMap::new(),
        }
    }

    /// Create a failing result.
    #[must_use]
    pub fn fail(
        scenario_id: impl Into<String>,
        category: ScenarioCategory,
        duration_ms: u64,
        error: impl Into<String>,
    ) -> Self {
        Self {
            scenario_id: scenario_id.into(),
            category,
            outcome: ScenarioOutcome::Fail,
            duration_ms,
            error: error.into(),
            observations: Vec::new(),
            artifacts: Vec::new(),
            metrics: BTreeMap::new(),
        }
    }

    /// Create a skipped result.
    #[must_use]
    pub fn skipped(scenario_id: impl Into<String>, category: ScenarioCategory, reason: impl Into<String>) -> Self {
        Self {
            scenario_id: scenario_id.into(),
            category,
            outcome: ScenarioOutcome::Skipped,
            duration_ms: 0,
            error: reason.into(),
            observations: Vec::new(),
            artifacts: Vec::new(),
            metrics: BTreeMap::new(),
        }
    }

    /// Add an observation.
    pub fn observe(&mut self, obs: impl Into<String>) {
        self.observations.push(obs.into());
    }
}

// =============================================================================
// Drill metrics
// =============================================================================

/// Metrics for rollback drill validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrillMetrics {
    /// Time from rollback trigger to restored service (ms).
    pub time_to_recovery_ms: u64,
    /// Target time-to-recovery (ms).
    pub target_ttr_ms: u64,
    /// Data integrity score (0.0–1.0, 1.0 = perfect).
    pub data_integrity_score: f64,
    /// Target data integrity score.
    pub target_integrity: f64,
    /// Number of events lost during rollback.
    pub events_lost: u64,
    /// Number of sessions disrupted.
    pub sessions_disrupted: u64,
    /// Whether audit chain continuity was maintained.
    pub audit_chain_intact: bool,
    /// Whether policy enforcement was continuous.
    pub policy_enforcement_continuous: bool,
}

impl DrillMetrics {
    /// Check if drill meets all targets.
    #[must_use]
    pub fn meets_targets(&self) -> bool {
        self.time_to_recovery_ms <= self.target_ttr_ms
            && self.data_integrity_score >= self.target_integrity
            && self.audit_chain_intact
            && self.policy_enforcement_continuous
    }

    /// Production-grade targets.
    #[must_use]
    pub fn production_targets() -> Self {
        Self {
            time_to_recovery_ms: 0,
            target_ttr_ms: 60_000, // 1 minute
            data_integrity_score: 0.0,
            target_integrity: 1.0, // perfect
            events_lost: 0,
            sessions_disrupted: 0,
            audit_chain_intact: true,
            policy_enforcement_continuous: true,
        }
    }

    /// Relaxed targets for rehearsal environments.
    #[must_use]
    pub fn rehearsal_targets() -> Self {
        Self {
            time_to_recovery_ms: 0,
            target_ttr_ms: 300_000, // 5 minutes
            data_integrity_score: 0.0,
            target_integrity: 0.99,
            events_lost: 0,
            sessions_disrupted: 0,
            audit_chain_intact: true,
            policy_enforcement_continuous: true,
        }
    }
}

/// Divergence metrics from a shadow comparison.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DivergenceMetrics {
    /// Total comparisons performed.
    pub total_comparisons: u64,
    /// Comparisons that matched.
    pub matches: u64,
    /// Divergences observed.
    pub divergences: u64,
    /// Divergence rate (0.0–1.0).
    pub divergence_rate: f64,
    /// Divergence budget threshold.
    pub budget: f64,
    /// Whether divergence is within budget.
    pub within_budget: bool,
    /// Categories of divergence observed.
    pub divergence_categories: BTreeMap<String, u64>,
}

impl DivergenceMetrics {
    /// Compute from raw counts with a budget threshold.
    #[must_use]
    pub fn compute(total: u64, divergences: u64, budget: f64) -> Self {
        let rate = if total > 0 {
            divergences as f64 / total as f64
        } else {
            0.0
        };
        Self {
            total_comparisons: total,
            matches: total.saturating_sub(divergences),
            divergences,
            divergence_rate: rate,
            budget,
            within_budget: rate <= budget,
            divergence_categories: BTreeMap::new(),
        }
    }
}

// =============================================================================
// Rehearsal suite
// =============================================================================

/// A complete rehearsal suite definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RehearsalSuite {
    /// Suite identifier.
    pub suite_id: String,
    /// Human-readable name.
    pub name: String,
    /// Scenarios in execution order.
    pub scenarios: Vec<RehearsalScenario>,
    /// Environment identifier (e.g., "staging", "rehearsal", "pre-prod").
    pub environment: String,
    /// Divergence budget for shadow comparisons.
    pub divergence_budget: f64,
    /// Time-to-recovery target for rollback drills (ms).
    pub ttr_target_ms: u64,
}

impl RehearsalSuite {
    /// Create a new suite.
    #[must_use]
    pub fn new(suite_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            suite_id: suite_id.into(),
            name: name.into(),
            scenarios: Vec::new(),
            environment: "rehearsal".into(),
            divergence_budget: 0.01, // 1%
            ttr_target_ms: 300_000,  // 5 minutes
        }
    }

    /// Add a scenario.
    pub fn add_scenario(&mut self, scenario: RehearsalScenario) {
        self.scenarios.push(scenario);
    }

    /// Number of scenarios.
    #[must_use]
    pub fn scenario_count(&self) -> usize {
        self.scenarios.len()
    }

    /// Number of blocking scenarios.
    #[must_use]
    pub fn blocking_count(&self) -> usize {
        self.scenarios
            .iter()
            .filter(|s| s.category.is_blocking())
            .count()
    }

    /// Scenarios by category.
    #[must_use]
    pub fn by_category(&self, category: ScenarioCategory) -> Vec<&RehearsalScenario> {
        self.scenarios
            .iter()
            .filter(|s| s.category == category)
            .collect()
    }

    /// Total expected duration (ms).
    #[must_use]
    pub fn estimated_duration_ms(&self) -> u64 {
        self.scenarios.iter().map(|s| s.expected_duration_ms).sum()
    }
}

// =============================================================================
// Rehearsal execution
// =============================================================================

/// A complete rehearsal execution with results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RehearsalExecution {
    /// Suite that was executed.
    pub suite_id: String,
    /// Execution run ID.
    pub run_id: String,
    /// Environment.
    pub environment: String,
    /// When execution started (epoch ms).
    pub started_at_ms: u64,
    /// When execution completed (epoch ms).
    pub completed_at_ms: u64,
    /// Per-scenario results.
    pub results: Vec<ScenarioResult>,
    /// Drill metrics (populated for rollback drill scenarios).
    pub drill_metrics: Option<DrillMetrics>,
    /// Divergence metrics (populated for shadow comparison scenarios).
    pub divergence_metrics: Option<DivergenceMetrics>,
    /// Who executed the rehearsal.
    pub executed_by: String,
}

impl RehearsalExecution {
    /// Create a new execution.
    #[must_use]
    pub fn new(
        suite_id: impl Into<String>,
        run_id: impl Into<String>,
        environment: impl Into<String>,
        started_at_ms: u64,
    ) -> Self {
        Self {
            suite_id: suite_id.into(),
            run_id: run_id.into(),
            environment: environment.into(),
            started_at_ms,
            completed_at_ms: 0,
            results: Vec::new(),
            drill_metrics: None,
            divergence_metrics: None,
            executed_by: String::new(),
        }
    }

    /// Record a scenario result.
    pub fn record(&mut self, result: ScenarioResult) {
        self.results.push(result);
    }

    /// Mark execution as complete.
    pub fn complete(&mut self, completed_at_ms: u64) {
        self.completed_at_ms = completed_at_ms;
    }

    /// Total duration (ms).
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        self.completed_at_ms.saturating_sub(self.started_at_ms)
    }

    /// Pass count.
    #[must_use]
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.outcome.is_pass()).count()
    }

    /// Fail count.
    #[must_use]
    pub fn failed(&self) -> usize {
        self.results.iter().filter(|r| r.outcome.is_failure()).count()
    }

    /// Skip count.
    #[must_use]
    pub fn skipped(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.outcome == ScenarioOutcome::Skipped)
            .count()
    }

    /// Pass rate (0.0–1.0, excluding skipped).
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        let executed = self.passed() + self.failed();
        if executed == 0 {
            return 1.0;
        }
        self.passed() as f64 / executed as f64
    }

    /// Whether all blocking scenarios passed.
    #[must_use]
    pub fn blocking_pass(&self) -> bool {
        !self.results.iter().any(|r| {
            r.category.is_blocking() && r.outcome.is_failure()
        })
    }
}

// =============================================================================
// Rehearsal report and verdict
// =============================================================================

/// Overall verdict of a rehearsal execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RehearsalVerdict {
    /// All checks pass — ready to proceed.
    Ready,
    /// Minor issues found — proceed with caution.
    Conditional,
    /// Blocking failures — not ready to proceed.
    NotReady,
}

/// Aggregate rehearsal report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RehearsalReport {
    /// Suite that was executed.
    pub suite_id: String,
    /// Run ID.
    pub run_id: String,
    /// Overall verdict.
    pub verdict: RehearsalVerdict,
    /// Total scenarios.
    pub total: usize,
    /// Passed.
    pub passed: usize,
    /// Failed.
    pub failed: usize,
    /// Skipped.
    pub skipped: usize,
    /// Pass rate.
    pub pass_rate: f64,
    /// Whether all blocking scenarios pass.
    pub blocking_pass: bool,
    /// Whether drill metrics meet targets (if applicable).
    pub drill_targets_met: Option<bool>,
    /// Whether divergence is within budget (if applicable).
    pub divergence_within_budget: Option<bool>,
    /// Total execution duration (ms).
    pub duration_ms: u64,
    /// Environment.
    pub environment: String,
    /// Failure details.
    pub failures: Vec<FailureDetail>,
    /// Evidence artifact paths.
    pub evidence_artifacts: Vec<String>,
}

/// Detail of a failed scenario for the report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureDetail {
    /// Scenario ID.
    pub scenario_id: String,
    /// Category.
    pub category: ScenarioCategory,
    /// Error message.
    pub error: String,
    /// Whether this failure is blocking.
    pub blocking: bool,
}

impl RehearsalReport {
    /// Generate a report from an execution.
    #[must_use]
    pub fn from_execution(exec: &RehearsalExecution) -> Self {
        let failures: Vec<FailureDetail> = exec
            .results
            .iter()
            .filter(|r| r.outcome.is_failure())
            .map(|r| FailureDetail {
                scenario_id: r.scenario_id.clone(),
                category: r.category,
                error: r.error.clone(),
                blocking: r.category.is_blocking(),
            })
            .collect();

        let blocking_pass = exec.blocking_pass();
        let drill_targets_met = exec.drill_metrics.as_ref().map(|m| m.meets_targets());
        let divergence_within_budget = exec.divergence_metrics.as_ref().map(|m| m.within_budget);

        let verdict = if blocking_pass
            && drill_targets_met.unwrap_or(true)
            && divergence_within_budget.unwrap_or(true)
        {
            if failures.is_empty() {
                RehearsalVerdict::Ready
            } else {
                RehearsalVerdict::Conditional
            }
        } else {
            RehearsalVerdict::NotReady
        };

        let evidence_artifacts: Vec<String> = exec
            .results
            .iter()
            .flat_map(|r| r.artifacts.iter().cloned())
            .collect();

        Self {
            suite_id: exec.suite_id.clone(),
            run_id: exec.run_id.clone(),
            verdict,
            total: exec.results.len(),
            passed: exec.passed(),
            failed: exec.failed(),
            skipped: exec.skipped(),
            pass_rate: exec.pass_rate(),
            blocking_pass,
            drill_targets_met,
            divergence_within_budget,
            duration_ms: exec.duration_ms(),
            environment: exec.environment.clone(),
            failures,
            evidence_artifacts,
        }
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# Rehearsal Report: {} (run {})\n\n", self.suite_id, self.run_id));
        out.push_str(&format!(
            "Verdict: {:?}\nEnvironment: {}\nDuration: {}ms\n\n",
            self.verdict, self.environment, self.duration_ms
        ));
        out.push_str(&format!(
            "Results: {}/{} passed, {} failed, {} skipped ({:.1}% pass rate)\n",
            self.passed,
            self.total,
            self.failed,
            self.skipped,
            self.pass_rate * 100.0
        ));
        out.push_str(&format!("Blocking pass: {}\n", self.blocking_pass));

        if let Some(drill) = self.drill_targets_met {
            out.push_str(&format!("Drill targets met: {}\n", drill));
        }
        if let Some(div) = self.divergence_within_budget {
            out.push_str(&format!("Divergence within budget: {}\n", div));
        }

        if !self.failures.is_empty() {
            out.push_str("\nFailures:\n");
            for f in &self.failures {
                let blocking_tag = if f.blocking { " [BLOCKING]" } else { "" };
                out.push_str(&format!(
                    "  - {}{}: {}\n",
                    f.scenario_id, blocking_tag, f.error
                ));
            }
        }

        out
    }
}

// =============================================================================
// Standard suite factory
// =============================================================================

/// Create a standard pre-production rehearsal suite.
#[must_use]
pub fn standard_rehearsal_suite() -> RehearsalSuite {
    let mut suite = RehearsalSuite::new("pre-prod-rehearsal", "Pre-Production Migration Rehearsal");
    suite.environment = "pre-prod".into();

    // Parity checks
    suite.add_scenario(
        RehearsalScenario::new(
            "parity-blocking",
            ScenarioCategory::ParityCheck,
            "Run all blocking parity scenarios from acceptance matrix",
        )
        .with_expected_duration(60_000)
        .with_command("cargo test --lib parity -- --test-threads=1")
        .with_preconditions(&["acceptance_matrix.v1.json exists", "parity corpus loaded"]),
    );
    suite.add_scenario(
        RehearsalScenario::new(
            "parity-high-priority",
            ScenarioCategory::ParityCheck,
            "Run high-priority parity scenarios (>= 90% pass target)",
        )
        .with_expected_duration(45_000)
        .with_severity(ScenarioSeverity::Warning),
    );

    // Shadow comparison
    suite.add_scenario(
        RehearsalScenario::new(
            "shadow-dual-run",
            ScenarioCategory::ShadowComparison,
            "Dual-run NTM and ft-native, compare outputs for divergence",
        )
        .with_expected_duration(120_000)
        .with_command("ft shadow-compare --duration 120s")
        .with_preconditions(&["NTM server running", "ft-native server running"]),
    );
    suite.add_scenario(
        RehearsalScenario::new(
            "shadow-idempotency",
            ScenarioCategory::ShadowComparison,
            "Verify event ordering and idempotency across repeated runs",
        )
        .with_expected_duration(90_000)
        .with_severity(ScenarioSeverity::Warning),
    );

    // Importer validation
    suite.add_scenario(
        RehearsalScenario::new(
            "import-sessions",
            ScenarioCategory::ImporterValidation,
            "Dry-run import of representative NTM session snapshots",
        )
        .with_expected_duration(30_000),
    );
    suite.add_scenario(
        RehearsalScenario::new(
            "import-workflows",
            ScenarioCategory::ImporterValidation,
            "Dry-run import of workflow definitions and state",
        )
        .with_expected_duration(30_000),
    );
    suite.add_scenario(
        RehearsalScenario::new(
            "import-config",
            ScenarioCategory::ImporterValidation,
            "Dry-run import of user configuration and preferences",
        )
        .with_expected_duration(15_000),
    );

    // Cutover checkpoints
    suite.add_scenario(
        RehearsalScenario::new(
            "checkpoint-preflight",
            ScenarioCategory::CutoverCheckpoint,
            "Evaluate preflight gate conditions (G-01, G-02, G-03)",
        )
        .with_expected_duration(10_000),
    );
    suite.add_scenario(
        RehearsalScenario::new(
            "checkpoint-shadow",
            ScenarioCategory::CutoverCheckpoint,
            "Evaluate shadow-stage gates (G-04, G-05)",
        )
        .with_expected_duration(10_000),
    );

    // Rollback drills
    suite.add_scenario(
        RehearsalScenario::new(
            "rollback-canary",
            ScenarioCategory::RollbackDrill,
            "Forced rollback from canary stage, validate recovery",
        )
        .with_expected_duration(120_000)
        .with_preconditions(&["canary cohort active", "rollback path configured"]),
    );
    suite.add_scenario(
        RehearsalScenario::new(
            "rollback-progressive",
            ScenarioCategory::RollbackDrill,
            "Forced rollback from progressive expansion, validate data integrity",
        )
        .with_expected_duration(180_000)
        .with_preconditions(&["progressive traffic > 0%"]),
    );
    suite.add_scenario(
        RehearsalScenario::new(
            "rollback-partial-failure",
            ScenarioCategory::RollbackDrill,
            "Rollback with injected partial failure (50% cohort unreachable)",
        )
        .with_expected_duration(180_000)
        .with_severity(ScenarioSeverity::Critical),
    );

    suite
}

// =============================================================================
// Rehearsal telemetry
// =============================================================================

/// Telemetry for rehearsal execution tracking.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RehearsalTelemetry {
    /// Total rehearsals executed.
    pub total_executions: u64,
    /// Executions that produced Ready verdict.
    pub ready_verdicts: u64,
    /// Executions that produced Conditional verdict.
    pub conditional_verdicts: u64,
    /// Executions that produced NotReady verdict.
    pub not_ready_verdicts: u64,
    /// Total scenarios executed across all runs.
    pub total_scenarios: u64,
    /// Total scenario passes.
    pub total_passes: u64,
    /// Total scenario failures.
    pub total_failures: u64,
}

impl RehearsalTelemetry {
    /// Record a report's results.
    pub fn record(&mut self, report: &RehearsalReport) {
        self.total_executions += 1;
        match report.verdict {
            RehearsalVerdict::Ready => self.ready_verdicts += 1,
            RehearsalVerdict::Conditional => self.conditional_verdicts += 1,
            RehearsalVerdict::NotReady => self.not_ready_verdicts += 1,
        }
        self.total_scenarios += report.total as u64;
        self.total_passes += report.passed as u64;
        self.total_failures += report.failed as u64;
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ScenarioCategory ----

    #[test]
    fn category_blocking_classification() {
        assert!(ScenarioCategory::ParityCheck.is_blocking());
        assert!(ScenarioCategory::RollbackDrill.is_blocking());
        assert!(ScenarioCategory::CutoverCheckpoint.is_blocking());
        assert!(!ScenarioCategory::ShadowComparison.is_blocking());
        assert!(!ScenarioCategory::ImporterValidation.is_blocking());
    }

    #[test]
    fn category_labels_non_empty() {
        for cat in ScenarioCategory::ALL {
            assert!(!cat.label().is_empty());
        }
    }

    // ---- ScenarioOutcome ----

    #[test]
    fn outcome_classification() {
        assert!(ScenarioOutcome::Pass.is_pass());
        assert!(!ScenarioOutcome::Pass.is_failure());
        assert!(ScenarioOutcome::Fail.is_failure());
        assert!(ScenarioOutcome::Timeout.is_failure());
        assert!(!ScenarioOutcome::Skipped.is_pass());
        assert!(!ScenarioOutcome::Skipped.is_failure());
    }

    // ---- DrillMetrics ----

    #[test]
    fn drill_meets_production_targets() {
        let mut metrics = DrillMetrics::production_targets();
        metrics.time_to_recovery_ms = 30_000;
        metrics.data_integrity_score = 1.0;
        assert!(metrics.meets_targets());
    }

    #[test]
    fn drill_fails_on_slow_recovery() {
        let mut metrics = DrillMetrics::production_targets();
        metrics.time_to_recovery_ms = 120_000; // exceeds 60s target
        metrics.data_integrity_score = 1.0;
        assert!(!metrics.meets_targets());
    }

    #[test]
    fn drill_fails_on_integrity_loss() {
        let mut metrics = DrillMetrics::production_targets();
        metrics.time_to_recovery_ms = 30_000;
        metrics.data_integrity_score = 0.95; // below 1.0 target
        assert!(!metrics.meets_targets());
    }

    #[test]
    fn drill_fails_on_audit_break() {
        let mut metrics = DrillMetrics::production_targets();
        metrics.time_to_recovery_ms = 30_000;
        metrics.data_integrity_score = 1.0;
        metrics.audit_chain_intact = false;
        assert!(!metrics.meets_targets());
    }

    // ---- DivergenceMetrics ----

    #[test]
    fn divergence_within_budget() {
        let metrics = DivergenceMetrics::compute(1000, 5, 0.01);
        assert_eq!(metrics.divergence_rate, 0.005);
        assert!(metrics.within_budget);
    }

    #[test]
    fn divergence_exceeds_budget() {
        let metrics = DivergenceMetrics::compute(1000, 20, 0.01);
        assert_eq!(metrics.divergence_rate, 0.02);
        assert!(!metrics.within_budget);
    }

    #[test]
    fn divergence_zero_total() {
        let metrics = DivergenceMetrics::compute(0, 0, 0.01);
        assert_eq!(metrics.divergence_rate, 0.0);
        assert!(metrics.within_budget);
    }

    // ---- RehearsalSuite ----

    #[test]
    fn suite_scenario_management() {
        let mut suite = RehearsalSuite::new("test", "Test Suite");
        suite.add_scenario(RehearsalScenario::new(
            "s1",
            ScenarioCategory::ParityCheck,
            "test",
        ));
        suite.add_scenario(RehearsalScenario::new(
            "s2",
            ScenarioCategory::ShadowComparison,
            "test",
        ));

        assert_eq!(suite.scenario_count(), 2);
        assert_eq!(suite.blocking_count(), 1); // only parity is blocking
    }

    #[test]
    fn suite_by_category() {
        let mut suite = RehearsalSuite::new("test", "Test");
        suite.add_scenario(RehearsalScenario::new("s1", ScenarioCategory::ParityCheck, "a"));
        suite.add_scenario(RehearsalScenario::new("s2", ScenarioCategory::ParityCheck, "b"));
        suite.add_scenario(RehearsalScenario::new("s3", ScenarioCategory::RollbackDrill, "c"));

        assert_eq!(suite.by_category(ScenarioCategory::ParityCheck).len(), 2);
        assert_eq!(suite.by_category(ScenarioCategory::RollbackDrill).len(), 1);
        assert_eq!(suite.by_category(ScenarioCategory::ShadowComparison).len(), 0);
    }

    // ---- RehearsalExecution ----

    #[test]
    fn execution_counters() {
        let mut exec = RehearsalExecution::new("suite-1", "run-1", "test", 1000);
        exec.record(ScenarioResult::pass("s1", ScenarioCategory::ParityCheck, 100));
        exec.record(ScenarioResult::pass("s2", ScenarioCategory::ShadowComparison, 200));
        exec.record(ScenarioResult::fail("s3", ScenarioCategory::ImporterValidation, 50, "import error"));
        exec.record(ScenarioResult::skipped("s4", ScenarioCategory::RollbackDrill, "precondition unmet"));
        exec.complete(2000);

        assert_eq!(exec.passed(), 2);
        assert_eq!(exec.failed(), 1);
        assert_eq!(exec.skipped(), 1);
        assert_eq!(exec.duration_ms(), 1000);
        assert!((exec.pass_rate() - 0.6667).abs() < 0.01);
    }

    #[test]
    fn execution_blocking_pass_with_non_blocking_failure() {
        let mut exec = RehearsalExecution::new("suite-1", "run-1", "test", 0);
        exec.record(ScenarioResult::pass("s1", ScenarioCategory::ParityCheck, 100));
        // ImporterValidation is non-blocking
        exec.record(ScenarioResult::fail(
            "s2",
            ScenarioCategory::ImporterValidation,
            50,
            "import err",
        ));

        assert!(exec.blocking_pass());
    }

    #[test]
    fn execution_blocking_fail_on_parity_failure() {
        let mut exec = RehearsalExecution::new("suite-1", "run-1", "test", 0);
        exec.record(ScenarioResult::fail(
            "s1",
            ScenarioCategory::ParityCheck,
            100,
            "parity regression",
        ));

        assert!(!exec.blocking_pass());
    }

    // ---- RehearsalReport ----

    #[test]
    fn report_ready_verdict() {
        let mut exec = RehearsalExecution::new("suite-1", "run-1", "test", 0);
        exec.record(ScenarioResult::pass("s1", ScenarioCategory::ParityCheck, 100));
        exec.record(ScenarioResult::pass("s2", ScenarioCategory::RollbackDrill, 200));
        exec.complete(500);

        let report = RehearsalReport::from_execution(&exec);
        assert_eq!(report.verdict, RehearsalVerdict::Ready);
        assert!(report.failures.is_empty());
        assert_eq!(report.pass_rate, 1.0);
    }

    #[test]
    fn report_conditional_on_non_blocking_failure() {
        let mut exec = RehearsalExecution::new("suite-1", "run-1", "test", 0);
        exec.record(ScenarioResult::pass("s1", ScenarioCategory::ParityCheck, 100));
        exec.record(ScenarioResult::fail(
            "s2",
            ScenarioCategory::ImporterValidation,
            50,
            "non-critical",
        ));
        exec.complete(500);

        let report = RehearsalReport::from_execution(&exec);
        assert_eq!(report.verdict, RehearsalVerdict::Conditional);
        assert_eq!(report.failures.len(), 1);
        assert!(!report.failures[0].blocking);
    }

    #[test]
    fn report_not_ready_on_blocking_failure() {
        let mut exec = RehearsalExecution::new("suite-1", "run-1", "test", 0);
        exec.record(ScenarioResult::fail(
            "s1",
            ScenarioCategory::ParityCheck,
            100,
            "parity failed",
        ));
        exec.complete(500);

        let report = RehearsalReport::from_execution(&exec);
        assert_eq!(report.verdict, RehearsalVerdict::NotReady);
        assert!(report.failures[0].blocking);
    }

    #[test]
    fn report_not_ready_on_drill_target_miss() {
        let mut exec = RehearsalExecution::new("suite-1", "run-1", "test", 0);
        exec.record(ScenarioResult::pass("s1", ScenarioCategory::ParityCheck, 100));
        exec.drill_metrics = Some(DrillMetrics {
            time_to_recovery_ms: 120_000,
            target_ttr_ms: 60_000, // miss
            data_integrity_score: 1.0,
            target_integrity: 1.0,
            events_lost: 0,
            sessions_disrupted: 0,
            audit_chain_intact: true,
            policy_enforcement_continuous: true,
        });
        exec.complete(500);

        let report = RehearsalReport::from_execution(&exec);
        assert_eq!(report.verdict, RehearsalVerdict::NotReady);
        assert_eq!(report.drill_targets_met, Some(false));
    }

    #[test]
    fn report_not_ready_on_divergence_over_budget() {
        let mut exec = RehearsalExecution::new("suite-1", "run-1", "test", 0);
        exec.record(ScenarioResult::pass("s1", ScenarioCategory::ParityCheck, 100));
        exec.divergence_metrics = Some(DivergenceMetrics::compute(1000, 50, 0.01));
        exec.complete(500);

        let report = RehearsalReport::from_execution(&exec);
        assert_eq!(report.verdict, RehearsalVerdict::NotReady);
        assert_eq!(report.divergence_within_budget, Some(false));
    }

    #[test]
    fn report_render_summary() {
        let mut exec = RehearsalExecution::new("suite-1", "run-1", "test", 0);
        exec.record(ScenarioResult::pass("s1", ScenarioCategory::ParityCheck, 100));
        exec.complete(500);

        let report = RehearsalReport::from_execution(&exec);
        let summary = report.render_summary();
        assert!(summary.contains("suite-1"));
        assert!(summary.contains("Ready"));
    }

    // ---- Standard suite factory ----

    #[test]
    fn standard_suite_has_all_categories() {
        let suite = standard_rehearsal_suite();
        assert!(suite.scenario_count() >= 12);

        for cat in ScenarioCategory::ALL {
            assert!(
                !suite.by_category(*cat).is_empty(),
                "Missing category: {:?}",
                cat
            );
        }
    }

    #[test]
    fn standard_suite_has_blocking_scenarios() {
        let suite = standard_rehearsal_suite();
        assert!(suite.blocking_count() >= 5);
    }

    // ---- RehearsalTelemetry ----

    #[test]
    fn telemetry_accumulates() {
        let mut telemetry = RehearsalTelemetry::default();

        let report = RehearsalReport {
            suite_id: "test".into(),
            run_id: "1".into(),
            verdict: RehearsalVerdict::Ready,
            total: 10,
            passed: 9,
            failed: 1,
            skipped: 0,
            pass_rate: 0.9,
            blocking_pass: true,
            drill_targets_met: None,
            divergence_within_budget: None,
            duration_ms: 1000,
            environment: "test".into(),
            failures: Vec::new(),
            evidence_artifacts: Vec::new(),
        };

        telemetry.record(&report);
        assert_eq!(telemetry.total_executions, 1);
        assert_eq!(telemetry.ready_verdicts, 1);
        assert_eq!(telemetry.total_scenarios, 10);
        assert_eq!(telemetry.total_passes, 9);
    }

    // ---- Serde ----

    #[test]
    fn suite_serde_roundtrip() {
        let suite = standard_rehearsal_suite();
        let json = serde_json::to_string(&suite).unwrap();
        let suite2: RehearsalSuite = serde_json::from_str(&json).unwrap();
        assert_eq!(suite2.scenario_count(), suite.scenario_count());
    }

    #[test]
    fn report_serde_roundtrip() {
        let mut exec = RehearsalExecution::new("s", "r", "test", 0);
        exec.record(ScenarioResult::pass("s1", ScenarioCategory::ParityCheck, 100));
        exec.complete(500);
        let report = RehearsalReport::from_execution(&exec);

        let json = serde_json::to_string(&report).unwrap();
        let report2: RehearsalReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report2.verdict, report.verdict);
    }

    // ---- E2E ----

    #[test]
    fn e2e_full_rehearsal_lifecycle() {
        let suite = standard_rehearsal_suite();
        let mut exec = RehearsalExecution::new(&suite.suite_id, "run-001", &suite.environment, 1000);
        exec.executed_by = "PinkForge".into();

        // Execute all scenarios (simulate)
        for scenario in &suite.scenarios {
            let result = if scenario.scenario_id == "rollback-partial-failure" {
                // Simulate partial failure recovery
                let mut r = ScenarioResult::pass(&scenario.scenario_id, scenario.category, 150_000);
                r.observe("50% cohort unreachable, recovery completed in 120s");
                r.artifacts
                    .push("artifacts/migration/rehearsal/rollback-partial-failure.json".into());
                r
            } else {
                let mut r = ScenarioResult::pass(
                    &scenario.scenario_id,
                    scenario.category,
                    scenario.expected_duration_ms / 2,
                );
                r.artifacts.push(format!(
                    "artifacts/migration/rehearsal/{}.json",
                    scenario.scenario_id
                ));
                r
            };
            exec.record(result);
        }

        // Add drill metrics
        exec.drill_metrics = Some(DrillMetrics {
            time_to_recovery_ms: 45_000,
            target_ttr_ms: 300_000,
            data_integrity_score: 1.0,
            target_integrity: 0.99,
            events_lost: 0,
            sessions_disrupted: 0,
            audit_chain_intact: true,
            policy_enforcement_continuous: true,
        });

        // Add divergence metrics
        exec.divergence_metrics = Some(DivergenceMetrics::compute(10_000, 5, 0.01));

        exec.complete(500_000);

        // Generate report
        let report = RehearsalReport::from_execution(&exec);
        assert_eq!(report.verdict, RehearsalVerdict::Ready);
        assert!(report.blocking_pass);
        assert_eq!(report.drill_targets_met, Some(true));
        assert_eq!(report.divergence_within_budget, Some(true));
        assert_eq!(report.passed, suite.scenario_count());
        assert_eq!(report.failed, 0);
        assert!(!report.evidence_artifacts.is_empty());

        // Track telemetry
        let mut telemetry = RehearsalTelemetry::default();
        telemetry.record(&report);
        assert_eq!(telemetry.ready_verdicts, 1);

        // Verify summary includes key info
        let summary = report.render_summary();
        assert!(summary.contains("Ready"));
        assert!(summary.contains("pre-prod"));
    }

    #[test]
    fn e2e_rehearsal_with_failure_and_retry() {
        let suite = standard_rehearsal_suite();
        let mut telemetry = RehearsalTelemetry::default();

        // First run: parity failure
        let mut exec1 = RehearsalExecution::new(&suite.suite_id, "run-001", "pre-prod", 0);
        exec1.record(ScenarioResult::fail(
            "parity-blocking",
            ScenarioCategory::ParityCheck,
            30_000,
            "3 blocking scenarios failed",
        ));
        exec1.record(ScenarioResult::pass(
            "shadow-dual-run",
            ScenarioCategory::ShadowComparison,
            60_000,
        ));
        exec1.complete(100_000);

        let report1 = RehearsalReport::from_execution(&exec1);
        assert_eq!(report1.verdict, RehearsalVerdict::NotReady);
        telemetry.record(&report1);

        // Second run: all pass after fix
        let mut exec2 = RehearsalExecution::new(&suite.suite_id, "run-002", "pre-prod", 200_000);
        exec2.record(ScenarioResult::pass(
            "parity-blocking",
            ScenarioCategory::ParityCheck,
            30_000,
        ));
        exec2.record(ScenarioResult::pass(
            "shadow-dual-run",
            ScenarioCategory::ShadowComparison,
            60_000,
        ));
        exec2.record(ScenarioResult::pass(
            "rollback-canary",
            ScenarioCategory::RollbackDrill,
            90_000,
        ));
        exec2.drill_metrics = Some(DrillMetrics {
            time_to_recovery_ms: 45_000,
            target_ttr_ms: 300_000,
            data_integrity_score: 1.0,
            target_integrity: 0.99,
            events_lost: 0,
            sessions_disrupted: 0,
            audit_chain_intact: true,
            policy_enforcement_continuous: true,
        });
        exec2.complete(400_000);

        let report2 = RehearsalReport::from_execution(&exec2);
        assert_eq!(report2.verdict, RehearsalVerdict::Ready);
        telemetry.record(&report2);

        // Verify telemetry
        assert_eq!(telemetry.total_executions, 2);
        assert_eq!(telemetry.not_ready_verdicts, 1);
        assert_eq!(telemetry.ready_verdicts, 1);
    }
}
