//! Robot API contract/replay and compatibility test matrix (ft-3681t.4.5).
//!
//! Validates schema stability, deterministic semantics, replay correctness,
//! and NTM-compat migration guarantees for all machine interfaces.
//!
//! # Architecture
//!
//! ```text
//! ContractMatrix
//!   ├── ApiSurface[]              — enumerated API surfaces
//!   ├── ContractCheck[]           — individual checks per surface
//!   ├── SchemaStabilityCheck      — field presence/type invariants
//!   ├── DeterminismCheck          — same-input → same-output
//!   ├── ReplayCompatCheck         — backward-compat replay
//!   └── NtmMigrationCheck         — NTM parity verification
//!
//! ContractExecution
//!   ├── CheckResult[]             — per-check pass/fail + evidence
//!   └── ContractReport            — aggregate with verdict
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// API surface enumeration
// =============================================================================

/// A machine-facing API surface in the robot protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ApiSurface {
    // Core pane operations
    /// get-text — retrieve pane content.
    GetText,
    /// batch-get-text — retrieve content from multiple panes.
    BatchGetText,
    /// send-text — inject keystrokes into a pane.
    SendText,
    /// state — pane state listing.
    PaneState,

    // Search
    /// search — full-text and semantic search.
    Search,
    /// search-explain — search with scoring breakdown.
    SearchExplain,
    /// search-pipeline-status — pipeline health.
    SearchPipelineStatus,

    // Events
    /// events — list detected events.
    Events,
    /// events-mutate — annotate/handle events.
    EventsMutate,

    // Workflows
    /// workflow-run — trigger a workflow.
    WorkflowRun,
    /// workflow-list — list available workflows.
    WorkflowList,
    /// workflow-status — check workflow execution.
    WorkflowStatus,
    /// workflow-abort — abort a running workflow.
    WorkflowAbort,

    // Rules
    /// rules-list — list detection rules.
    RulesList,
    /// rules-test — test rules against text.
    RulesTest,
    /// rules-lint — lint rule definitions.
    RulesLint,

    // Agent management
    /// agent-inventory — list installed/running agents.
    AgentInventory,
    /// agent-configure — apply agent configurations.
    AgentConfigure,

    // Accounts
    /// accounts-list — list provider accounts.
    AccountsList,
    /// accounts-refresh — refresh quota data.
    AccountsRefresh,

    // Reservations
    /// reserve — acquire a pane reservation.
    Reserve,
    /// release — release a reservation.
    Release,

    // Mission execution
    /// mission-state — mission lifecycle state.
    MissionState,
    /// mission-decisions — mission assignment decisions.
    MissionDecisions,

    // Transactional execution
    /// tx-plan — transactional execution plan.
    TxPlan,
    /// tx-run — execute a transactional plan.
    TxRun,
    /// tx-show — inspect execution details.
    TxShow,

    // Replay
    /// replay-inspect — inspect replay artifacts.
    ReplayInspect,
    /// replay-diff — compare replay runs.
    ReplayDiff,
    /// replay-regression — regression test suite.
    ReplayRegression,

    // Meta
    /// quickstart — machine-readable quick-start guide.
    QuickStart,
    /// why — error code explanations.
    Why,
    /// approve — approval code validation.
    Approve,
}

impl ApiSurface {
    /// All defined surfaces.
    pub const ALL: &'static [ApiSurface] = &[
        Self::GetText,
        Self::BatchGetText,
        Self::SendText,
        Self::PaneState,
        Self::Search,
        Self::SearchExplain,
        Self::SearchPipelineStatus,
        Self::Events,
        Self::EventsMutate,
        Self::WorkflowRun,
        Self::WorkflowList,
        Self::WorkflowStatus,
        Self::WorkflowAbort,
        Self::RulesList,
        Self::RulesTest,
        Self::RulesLint,
        Self::AgentInventory,
        Self::AgentConfigure,
        Self::AccountsList,
        Self::AccountsRefresh,
        Self::Reserve,
        Self::Release,
        Self::MissionState,
        Self::MissionDecisions,
        Self::TxPlan,
        Self::TxRun,
        Self::TxShow,
        Self::ReplayInspect,
        Self::ReplayDiff,
        Self::ReplayRegression,
        Self::QuickStart,
        Self::Why,
        Self::Approve,
    ];

    /// Command name as used in the robot protocol.
    #[must_use]
    pub fn command_name(&self) -> &'static str {
        match self {
            Self::GetText => "get-text",
            Self::BatchGetText => "batch-get-text",
            Self::SendText => "send-text",
            Self::PaneState => "state",
            Self::Search => "search",
            Self::SearchExplain => "search-explain",
            Self::SearchPipelineStatus => "search-pipeline-status",
            Self::Events => "events",
            Self::EventsMutate => "events-mutate",
            Self::WorkflowRun => "workflow-run",
            Self::WorkflowList => "workflow-list",
            Self::WorkflowStatus => "workflow-status",
            Self::WorkflowAbort => "workflow-abort",
            Self::RulesList => "rules-list",
            Self::RulesTest => "rules-test",
            Self::RulesLint => "rules-lint",
            Self::AgentInventory => "agent-inventory",
            Self::AgentConfigure => "agent-configure",
            Self::AccountsList => "accounts-list",
            Self::AccountsRefresh => "accounts-refresh",
            Self::Reserve => "reserve",
            Self::Release => "release",
            Self::MissionState => "mission-state",
            Self::MissionDecisions => "mission-decisions",
            Self::TxPlan => "tx-plan",
            Self::TxRun => "tx-run",
            Self::TxShow => "tx-show",
            Self::ReplayInspect => "replay-inspect",
            Self::ReplayDiff => "replay-diff",
            Self::ReplayRegression => "replay-regression",
            Self::QuickStart => "quickstart",
            Self::Why => "why",
            Self::Approve => "approve",
        }
    }

    /// Whether this is a mutation (write) operation.
    #[must_use]
    pub fn is_mutation(&self) -> bool {
        matches!(
            self,
            Self::SendText
                | Self::EventsMutate
                | Self::WorkflowRun
                | Self::WorkflowAbort
                | Self::AgentConfigure
                | Self::AccountsRefresh
                | Self::Reserve
                | Self::Release
                | Self::TxRun
        )
    }

    /// Whether this surface has NTM compatibility requirements.
    #[must_use]
    pub fn has_ntm_compat(&self) -> bool {
        matches!(
            self,
            Self::GetText
                | Self::BatchGetText
                | Self::SendText
                | Self::PaneState
                | Self::Events
                | Self::WorkflowRun
                | Self::WorkflowList
                | Self::WorkflowStatus
                | Self::RulesList
        )
    }

    /// Category for grouping.
    #[must_use]
    pub fn category(&self) -> &'static str {
        match self {
            Self::GetText | Self::BatchGetText | Self::SendText | Self::PaneState => "pane",
            Self::Search | Self::SearchExplain | Self::SearchPipelineStatus => "search",
            Self::Events | Self::EventsMutate => "events",
            Self::WorkflowRun | Self::WorkflowList | Self::WorkflowStatus | Self::WorkflowAbort => {
                "workflow"
            }
            Self::RulesList | Self::RulesTest | Self::RulesLint => "rules",
            Self::AgentInventory | Self::AgentConfigure => "agent",
            Self::AccountsList | Self::AccountsRefresh => "accounts",
            Self::Reserve | Self::Release => "reservations",
            Self::MissionState | Self::MissionDecisions => "mission",
            Self::TxPlan | Self::TxRun | Self::TxShow => "tx",
            Self::ReplayInspect | Self::ReplayDiff | Self::ReplayRegression => "replay",
            Self::QuickStart | Self::Why | Self::Approve => "meta",
        }
    }
}

// =============================================================================
// Contract check types
// =============================================================================

/// Category of contract check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CheckCategory {
    /// Schema stability — required fields present, types correct.
    SchemaStability,
    /// Deterministic semantics — same input yields same output.
    Determinism,
    /// Replay correctness — recorded sessions replay identically.
    ReplayCorrectness,
    /// NTM compatibility — ft produces NTM-compatible responses.
    NtmCompatibility,
    /// Error contract — error codes and messages are well-formed.
    ErrorContract,
    /// Idempotency — mutation commands are safely retryable.
    Idempotency,
    /// Envelope contract — ok/error structure is consistent.
    EnvelopeContract,
}

impl CheckCategory {
    /// Human label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::SchemaStability => "Schema Stability",
            Self::Determinism => "Deterministic Semantics",
            Self::ReplayCorrectness => "Replay Correctness",
            Self::NtmCompatibility => "NTM Compatibility",
            Self::ErrorContract => "Error Contract",
            Self::Idempotency => "Idempotency",
            Self::EnvelopeContract => "Envelope Contract",
        }
    }
}

/// A single contract check definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractCheck {
    /// Unique check identifier.
    pub check_id: String,
    /// API surface being checked.
    pub surface: ApiSurface,
    /// Check category.
    pub category: CheckCategory,
    /// Human description.
    pub description: String,
    /// Whether this check is blocking (must pass).
    pub blocking: bool,
    /// Required fields that must be present in the response.
    pub required_fields: Vec<String>,
    /// Fields that must be deterministic across identical requests.
    pub deterministic_fields: Vec<String>,
}

impl ContractCheck {
    /// Create a new check.
    #[must_use]
    pub fn new(
        check_id: impl Into<String>,
        surface: ApiSurface,
        category: CheckCategory,
        description: impl Into<String>,
    ) -> Self {
        Self {
            check_id: check_id.into(),
            surface,
            category,
            description: description.into(),
            blocking: true,
            required_fields: Vec::new(),
            deterministic_fields: Vec::new(),
        }
    }

    /// Mark as advisory (non-blocking).
    #[must_use]
    pub fn advisory(mut self) -> Self {
        self.blocking = false;
        self
    }

    /// Set required fields.
    #[must_use]
    pub fn with_required_fields(mut self, fields: &[&str]) -> Self {
        self.required_fields = fields.iter().map(|s| (*s).to_string()).collect();
        self
    }

    /// Set deterministic fields.
    #[must_use]
    pub fn with_deterministic_fields(mut self, fields: &[&str]) -> Self {
        self.deterministic_fields = fields.iter().map(|s| (*s).to_string()).collect();
        self
    }
}

// =============================================================================
// Check results
// =============================================================================

/// Outcome of a contract check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckOutcome {
    /// Check passed.
    Pass,
    /// Check failed.
    Fail,
    /// Check was skipped.
    Skipped,
}

/// Result of executing a single contract check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    /// Which check was executed.
    pub check_id: String,
    /// API surface.
    pub surface: ApiSurface,
    /// Category.
    pub category: CheckCategory,
    /// Outcome.
    pub outcome: CheckOutcome,
    /// Whether this check is blocking.
    pub blocking: bool,
    /// Error or diff description if failed.
    pub error: String,
    /// Missing required fields (if any).
    pub missing_fields: Vec<String>,
    /// Non-deterministic fields observed (if any).
    pub nondeterministic_fields: Vec<String>,
    /// Evidence artifacts.
    pub artifacts: Vec<String>,
    /// Execution duration (ms).
    pub duration_ms: u64,
}

impl CheckResult {
    /// Create a passing result.
    #[must_use]
    pub fn pass(check_id: impl Into<String>, surface: ApiSurface, category: CheckCategory) -> Self {
        Self {
            check_id: check_id.into(),
            surface,
            category,
            outcome: CheckOutcome::Pass,
            blocking: true,
            error: String::new(),
            missing_fields: Vec::new(),
            nondeterministic_fields: Vec::new(),
            artifacts: Vec::new(),
            duration_ms: 0,
        }
    }

    /// Create a failing result.
    #[must_use]
    pub fn fail(
        check_id: impl Into<String>,
        surface: ApiSurface,
        category: CheckCategory,
        error: impl Into<String>,
    ) -> Self {
        Self {
            check_id: check_id.into(),
            surface,
            category,
            outcome: CheckOutcome::Fail,
            blocking: true,
            error: error.into(),
            missing_fields: Vec::new(),
            nondeterministic_fields: Vec::new(),
            artifacts: Vec::new(),
            duration_ms: 0,
        }
    }
}

// =============================================================================
// Contract matrix
// =============================================================================

/// The complete contract matrix — registers and evaluates all contract checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractMatrix {
    /// Matrix identifier.
    pub matrix_id: String,
    /// Schema version.
    pub schema_version: u32,
    /// All registered checks.
    pub checks: Vec<ContractCheck>,
    /// Telemetry.
    pub telemetry: ContractTelemetry,
}

/// Telemetry for the contract matrix.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContractTelemetry {
    /// Total checks registered.
    pub checks_registered: u64,
    /// Total executions.
    pub executions: u64,
    /// Checks passed.
    pub checks_passed: u64,
    /// Checks failed.
    pub checks_failed: u64,
    /// Checks skipped.
    pub checks_skipped: u64,
}

impl ContractMatrix {
    /// Create a new matrix.
    #[must_use]
    pub fn new(matrix_id: impl Into<String>) -> Self {
        Self {
            matrix_id: matrix_id.into(),
            schema_version: 1,
            checks: Vec::new(),
            telemetry: ContractTelemetry::default(),
        }
    }

    /// Register a check.
    pub fn register(&mut self, check: ContractCheck) {
        self.telemetry.checks_registered += 1;
        self.checks.push(check);
    }

    /// Total check count.
    #[must_use]
    pub fn check_count(&self) -> usize {
        self.checks.len()
    }

    /// Checks for a given surface.
    #[must_use]
    pub fn checks_for_surface(&self, surface: ApiSurface) -> Vec<&ContractCheck> {
        self.checks.iter().filter(|c| c.surface == surface).collect()
    }

    /// Checks for a given category.
    #[must_use]
    pub fn checks_for_category(&self, category: CheckCategory) -> Vec<&ContractCheck> {
        self.checks.iter().filter(|c| c.category == category).collect()
    }

    /// Blocking check count.
    #[must_use]
    pub fn blocking_count(&self) -> usize {
        self.checks.iter().filter(|c| c.blocking).count()
    }

    /// Coverage: how many API surfaces have at least one check.
    #[must_use]
    pub fn surface_coverage(&self) -> (usize, usize) {
        let covered: std::collections::HashSet<ApiSurface> =
            self.checks.iter().map(|c| c.surface).collect();
        (covered.len(), ApiSurface::ALL.len())
    }

    /// Surfaces without any checks.
    #[must_use]
    pub fn uncovered_surfaces(&self) -> Vec<ApiSurface> {
        let covered: std::collections::HashSet<ApiSurface> =
            self.checks.iter().map(|c| c.surface).collect();
        ApiSurface::ALL
            .iter()
            .filter(|s| !covered.contains(s))
            .copied()
            .collect()
    }

    /// Render a coverage matrix.
    #[must_use]
    pub fn render_coverage_matrix(&self) -> String {
        let mut out = String::new();
        out.push_str("# Robot API Contract Coverage Matrix\n\n");
        out.push_str("| Surface | Schema | Determinism | Replay | NTM-Compat | Error | Idempotency | Envelope |\n");
        out.push_str("|---------|--------|-------------|--------|------------|-------|-------------|----------|\n");

        for surface in ApiSurface::ALL {
            let checks: Vec<&ContractCheck> = self.checks.iter().filter(|c| c.surface == *surface).collect();
            let has = |cat: CheckCategory| -> &str {
                if checks.iter().any(|c| c.category == cat) {
                    "Y"
                } else {
                    "-"
                }
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                surface.command_name(),
                has(CheckCategory::SchemaStability),
                has(CheckCategory::Determinism),
                has(CheckCategory::ReplayCorrectness),
                has(CheckCategory::NtmCompatibility),
                has(CheckCategory::ErrorContract),
                has(CheckCategory::Idempotency),
                has(CheckCategory::EnvelopeContract),
            ));
        }

        let (covered, total) = self.surface_coverage();
        out.push_str(&format!(
            "\nCoverage: {}/{} surfaces ({:.0}%)\n",
            covered,
            total,
            covered as f64 / total as f64 * 100.0
        ));

        out
    }
}

// =============================================================================
// Contract execution
// =============================================================================

/// A complete contract execution with results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractExecution {
    /// Matrix that was executed.
    pub matrix_id: String,
    /// Run identifier.
    pub run_id: String,
    /// When execution started (epoch ms).
    pub started_at_ms: u64,
    /// When execution completed (epoch ms).
    pub completed_at_ms: u64,
    /// Per-check results.
    pub results: Vec<CheckResult>,
    /// Who executed.
    pub executed_by: String,
}

impl ContractExecution {
    /// Create a new execution.
    #[must_use]
    pub fn new(
        matrix_id: impl Into<String>,
        run_id: impl Into<String>,
        started_at_ms: u64,
    ) -> Self {
        Self {
            matrix_id: matrix_id.into(),
            run_id: run_id.into(),
            started_at_ms,
            completed_at_ms: 0,
            results: Vec::new(),
            executed_by: String::new(),
        }
    }

    /// Record a check result.
    pub fn record(&mut self, result: CheckResult) {
        self.results.push(result);
    }

    /// Mark as complete.
    pub fn complete(&mut self, completed_at_ms: u64) {
        self.completed_at_ms = completed_at_ms;
    }

    /// Pass count.
    #[must_use]
    pub fn passed(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.outcome == CheckOutcome::Pass)
            .count()
    }

    /// Fail count.
    #[must_use]
    pub fn failed(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.outcome == CheckOutcome::Fail)
            .count()
    }

    /// Whether all blocking checks passed.
    #[must_use]
    pub fn blocking_pass(&self) -> bool {
        !self.results.iter().any(|r| r.blocking && r.outcome == CheckOutcome::Fail)
    }

    /// Pass rate (excluding skipped).
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        let executed = self.passed() + self.failed();
        if executed == 0 {
            return 1.0;
        }
        self.passed() as f64 / executed as f64
    }

    /// Failure details grouped by category.
    #[must_use]
    pub fn failures_by_category(&self) -> BTreeMap<String, Vec<&CheckResult>> {
        let mut map: BTreeMap<String, Vec<&CheckResult>> = BTreeMap::new();
        for result in &self.results {
            if result.outcome == CheckOutcome::Fail {
                map.entry(result.category.label().to_string())
                    .or_default()
                    .push(result);
            }
        }
        map
    }
}

// =============================================================================
// Contract report
// =============================================================================

/// Overall verdict of a contract execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractVerdict {
    /// All checks pass.
    Compatible,
    /// Minor issues (non-blocking failures).
    ConditionallyCompatible,
    /// Blocking failures — not compatible.
    Incompatible,
}

/// Contract compatibility report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractReport {
    /// Matrix ID.
    pub matrix_id: String,
    /// Run ID.
    pub run_id: String,
    /// Verdict.
    pub verdict: ContractVerdict,
    /// Total checks.
    pub total: usize,
    /// Passed.
    pub passed: usize,
    /// Failed.
    pub failed: usize,
    /// Pass rate.
    pub pass_rate: f64,
    /// Whether all blocking checks pass.
    pub blocking_pass: bool,
    /// Duration (ms).
    pub duration_ms: u64,
    /// Failures with actionable diffs.
    pub failures: Vec<ContractFailure>,
    /// Category-level summary.
    pub category_summary: BTreeMap<String, CategorySummary>,
}

/// A contract failure with actionable diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractFailure {
    /// Check ID.
    pub check_id: String,
    /// Surface.
    pub surface: ApiSurface,
    /// Category.
    pub category: CheckCategory,
    /// Error description.
    pub error: String,
    /// Whether blocking.
    pub blocking: bool,
    /// Actionable diff or fix suggestion.
    pub suggested_fix: String,
}

/// Per-category summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CategorySummary {
    /// Total checks in this category.
    pub total: usize,
    /// Passed.
    pub passed: usize,
    /// Failed.
    pub failed: usize,
}

impl ContractReport {
    /// Generate a report from an execution.
    #[must_use]
    pub fn from_execution(exec: &ContractExecution) -> Self {
        let failures: Vec<ContractFailure> = exec
            .results
            .iter()
            .filter(|r| r.outcome == CheckOutcome::Fail)
            .map(|r| ContractFailure {
                check_id: r.check_id.clone(),
                surface: r.surface,
                category: r.category,
                error: r.error.clone(),
                blocking: r.blocking,
                suggested_fix: if !r.missing_fields.is_empty() {
                    format!("Add required fields: {}", r.missing_fields.join(", "))
                } else if !r.nondeterministic_fields.is_empty() {
                    format!(
                        "Make fields deterministic: {}",
                        r.nondeterministic_fields.join(", ")
                    )
                } else {
                    "Review error details and fix the underlying issue".into()
                },
            })
            .collect();

        let blocking_pass = exec.blocking_pass();
        let verdict = if blocking_pass {
            if failures.is_empty() {
                ContractVerdict::Compatible
            } else {
                ContractVerdict::ConditionallyCompatible
            }
        } else {
            ContractVerdict::Incompatible
        };

        // Category summary
        let mut category_summary: BTreeMap<String, CategorySummary> = BTreeMap::new();
        for result in &exec.results {
            let entry = category_summary
                .entry(result.category.label().to_string())
                .or_default();
            entry.total += 1;
            if result.outcome == CheckOutcome::Pass {
                entry.passed += 1;
            } else if result.outcome == CheckOutcome::Fail {
                entry.failed += 1;
            }
        }

        Self {
            matrix_id: exec.matrix_id.clone(),
            run_id: exec.run_id.clone(),
            verdict,
            total: exec.results.len(),
            passed: exec.passed(),
            failed: exec.failed(),
            pass_rate: exec.pass_rate(),
            blocking_pass,
            duration_ms: exec.completed_at_ms.saturating_sub(exec.started_at_ms),
            failures,
            category_summary,
        }
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "# Contract Report: {} (run {})\n\n",
            self.matrix_id, self.run_id
        ));
        out.push_str(&format!("Verdict: {:?}\n", self.verdict));
        out.push_str(&format!(
            "Results: {}/{} passed ({:.1}%)\n",
            self.passed,
            self.total,
            self.pass_rate * 100.0
        ));
        out.push_str(&format!("Blocking pass: {}\n\n", self.blocking_pass));

        if !self.category_summary.is_empty() {
            out.push_str("By category:\n");
            for (cat, summary) in &self.category_summary {
                out.push_str(&format!(
                    "  {}: {}/{} passed\n",
                    cat, summary.passed, summary.total
                ));
            }
        }

        if !self.failures.is_empty() {
            out.push_str("\nFailures:\n");
            for f in &self.failures {
                let tag = if f.blocking { "[BLOCKING]" } else { "[advisory]" };
                out.push_str(&format!(
                    "  {} {} ({}): {}\n    Fix: {}\n",
                    tag,
                    f.check_id,
                    f.surface.command_name(),
                    f.error,
                    f.suggested_fix
                ));
            }
        }

        out
    }
}

// =============================================================================
// Standard matrix factory
// =============================================================================

/// Create a standard contract matrix covering all API surfaces.
#[must_use]
pub fn standard_contract_matrix() -> ContractMatrix {
    let mut matrix = ContractMatrix::new("robot-api-contracts");

    // Envelope contract — applies to ALL surfaces
    for surface in ApiSurface::ALL {
        matrix.register(
            ContractCheck::new(
                format!("env-{}", surface.command_name()),
                *surface,
                CheckCategory::EnvelopeContract,
                format!("{}: RobotResponse envelope has ok+data|error", surface.command_name()),
            )
            .with_required_fields(&["ok", "version", "elapsed_ms", "now"]),
        );
    }

    // Schema stability for core pane ops
    matrix.register(
        ContractCheck::new(
            "schema-get-text",
            ApiSurface::GetText,
            CheckCategory::SchemaStability,
            "get-text response has required fields",
        )
        .with_required_fields(&["pane_id", "text", "tail_lines", "truncated"]),
    );
    matrix.register(
        ContractCheck::new(
            "schema-send-text",
            ApiSurface::SendText,
            CheckCategory::SchemaStability,
            "send-text response has required fields",
        )
        .with_required_fields(&["pane_id", "injection"]),
    );
    matrix.register(
        ContractCheck::new(
            "schema-state",
            ApiSurface::PaneState,
            CheckCategory::SchemaStability,
            "state response has pane list with required fields",
        )
        .with_required_fields(&["panes", "tail_lines"]),
    );
    matrix.register(
        ContractCheck::new(
            "schema-search",
            ApiSurface::Search,
            CheckCategory::SchemaStability,
            "search response has query, results, total_hits",
        )
        .with_required_fields(&["query", "results", "total_hits", "limit"]),
    );
    matrix.register(
        ContractCheck::new(
            "schema-events",
            ApiSurface::Events,
            CheckCategory::SchemaStability,
            "events response has event list and total_count",
        )
        .with_required_fields(&["events", "total_count", "limit"]),
    );

    // Determinism checks
    matrix.register(
        ContractCheck::new(
            "det-get-text",
            ApiSurface::GetText,
            CheckCategory::Determinism,
            "get-text returns identical text for same pane state",
        )
        .with_deterministic_fields(&["text", "tail_lines", "truncated"]),
    );
    matrix.register(
        ContractCheck::new(
            "det-search",
            ApiSurface::Search,
            CheckCategory::Determinism,
            "search returns stable ordering for same query and index state",
        )
        .with_deterministic_fields(&["total_hits", "results"]),
    );
    matrix.register(
        ContractCheck::new(
            "det-events",
            ApiSurface::Events,
            CheckCategory::Determinism,
            "events list is stable for same filter and event state",
        )
        .with_deterministic_fields(&["events", "total_count"]),
    );

    // Idempotency checks for mutations
    for surface in ApiSurface::ALL {
        if surface.is_mutation() {
            matrix.register(ContractCheck::new(
                format!("idem-{}", surface.command_name()),
                *surface,
                CheckCategory::Idempotency,
                format!("{}: repeated mutations are safely deduplicated", surface.command_name()),
            ));
        }
    }

    // NTM compatibility for core surfaces
    for surface in ApiSurface::ALL {
        if surface.has_ntm_compat() {
            matrix.register(ContractCheck::new(
                format!("ntm-{}", surface.command_name()),
                *surface,
                CheckCategory::NtmCompatibility,
                format!("{}: ft-native response is NTM-compatible", surface.command_name()),
            ));
        }
    }

    // Error contract — all surfaces have well-formed error responses
    matrix.register(
        ContractCheck::new(
            "err-code-format",
            ApiSurface::GetText,
            CheckCategory::ErrorContract,
            "Error codes follow category.NNN format",
        )
        .with_required_fields(&["code", "message"]),
    );
    matrix.register(
        ContractCheck::new(
            "err-unknown-command",
            ApiSurface::Why,
            CheckCategory::ErrorContract,
            "Unknown commands produce structured error, not crash",
        ),
    );

    // Replay correctness
    matrix.register(ContractCheck::new(
        "replay-deterministic",
        ApiSurface::ReplayDiff,
        CheckCategory::ReplayCorrectness,
        "Replay diffs produce identical results across runs",
    ));
    matrix.register(ContractCheck::new(
        "replay-regression-stable",
        ApiSurface::ReplayRegression,
        CheckCategory::ReplayCorrectness,
        "Regression suite passes against baseline artifacts",
    ));

    matrix
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ApiSurface ----

    #[test]
    fn all_surfaces_have_command_names() {
        for surface in ApiSurface::ALL {
            assert!(!surface.command_name().is_empty());
            assert!(!surface.category().is_empty());
        }
    }

    #[test]
    fn mutation_surfaces_identified() {
        assert!(ApiSurface::SendText.is_mutation());
        assert!(ApiSurface::Reserve.is_mutation());
        assert!(!ApiSurface::GetText.is_mutation());
        assert!(!ApiSurface::Search.is_mutation());
    }

    #[test]
    fn ntm_compat_surfaces_identified() {
        assert!(ApiSurface::GetText.has_ntm_compat());
        assert!(ApiSurface::PaneState.has_ntm_compat());
        assert!(!ApiSurface::TxPlan.has_ntm_compat());
        assert!(!ApiSurface::ReplayInspect.has_ntm_compat());
    }

    #[test]
    fn surface_count() {
        assert_eq!(ApiSurface::ALL.len(), 33);
    }

    // ---- ContractCheck ----

    #[test]
    fn check_builder() {
        let check = ContractCheck::new("test", ApiSurface::GetText, CheckCategory::SchemaStability, "desc")
            .advisory()
            .with_required_fields(&["ok", "data"])
            .with_deterministic_fields(&["text"]);

        assert!(!check.blocking);
        assert_eq!(check.required_fields.len(), 2);
        assert_eq!(check.deterministic_fields.len(), 1);
    }

    // ---- CheckResult ----

    #[test]
    fn check_result_constructors() {
        let pass = CheckResult::pass("c1", ApiSurface::GetText, CheckCategory::SchemaStability);
        assert_eq!(pass.outcome, CheckOutcome::Pass);

        let fail = CheckResult::fail("c2", ApiSurface::Search, CheckCategory::Determinism, "nondeterministic");
        assert_eq!(fail.outcome, CheckOutcome::Fail);
        assert_eq!(fail.error, "nondeterministic");
    }

    // ---- ContractMatrix ----

    #[test]
    fn matrix_registration() {
        let mut matrix = ContractMatrix::new("test");
        matrix.register(ContractCheck::new(
            "c1",
            ApiSurface::GetText,
            CheckCategory::SchemaStability,
            "test",
        ));
        matrix.register(ContractCheck::new(
            "c2",
            ApiSurface::Search,
            CheckCategory::Determinism,
            "test",
        ));

        assert_eq!(matrix.check_count(), 2);
        assert_eq!(matrix.checks_for_surface(ApiSurface::GetText).len(), 1);
        assert_eq!(matrix.checks_for_category(CheckCategory::Determinism).len(), 1);
    }

    #[test]
    fn matrix_coverage() {
        let mut matrix = ContractMatrix::new("test");
        matrix.register(ContractCheck::new(
            "c1",
            ApiSurface::GetText,
            CheckCategory::SchemaStability,
            "test",
        ));

        let (covered, total) = matrix.surface_coverage();
        assert_eq!(covered, 1);
        assert_eq!(total, 33);

        let uncovered = matrix.uncovered_surfaces();
        assert_eq!(uncovered.len(), 32);
    }

    // ---- Standard matrix ----

    #[test]
    fn standard_matrix_covers_all_surfaces() {
        let matrix = standard_contract_matrix();

        // Every surface should have at least an envelope check
        for surface in ApiSurface::ALL {
            let checks = matrix.checks_for_surface(*surface);
            assert!(
                !checks.is_empty(),
                "No checks for surface: {}",
                surface.command_name()
            );
        }

        // Full coverage
        let (covered, total) = matrix.surface_coverage();
        assert_eq!(covered, total);
    }

    #[test]
    fn standard_matrix_has_all_categories() {
        let matrix = standard_contract_matrix();

        let categories = [
            CheckCategory::EnvelopeContract,
            CheckCategory::SchemaStability,
            CheckCategory::Determinism,
            CheckCategory::Idempotency,
            CheckCategory::NtmCompatibility,
            CheckCategory::ErrorContract,
            CheckCategory::ReplayCorrectness,
        ];

        for cat in categories {
            assert!(
                !matrix.checks_for_category(cat).is_empty(),
                "No checks for category: {}",
                cat.label()
            );
        }
    }

    #[test]
    fn standard_matrix_mutation_idempotency_coverage() {
        let matrix = standard_contract_matrix();
        for surface in ApiSurface::ALL {
            if surface.is_mutation() {
                let idem_checks: Vec<_> = matrix
                    .checks_for_surface(*surface)
                    .into_iter()
                    .filter(|c| c.category == CheckCategory::Idempotency)
                    .collect();
                assert!(
                    !idem_checks.is_empty(),
                    "Mutation surface {} has no idempotency check",
                    surface.command_name()
                );
            }
        }
    }

    #[test]
    fn standard_matrix_ntm_compat_coverage() {
        let matrix = standard_contract_matrix();
        for surface in ApiSurface::ALL {
            if surface.has_ntm_compat() {
                let ntm_checks: Vec<_> = matrix
                    .checks_for_surface(*surface)
                    .into_iter()
                    .filter(|c| c.category == CheckCategory::NtmCompatibility)
                    .collect();
                assert!(
                    !ntm_checks.is_empty(),
                    "NTM-compat surface {} has no compat check",
                    surface.command_name()
                );
            }
        }
    }

    // ---- ContractExecution ----

    #[test]
    fn execution_counters() {
        let mut exec = ContractExecution::new("matrix-1", "run-1", 0);
        exec.record(CheckResult::pass("c1", ApiSurface::GetText, CheckCategory::SchemaStability));
        exec.record(CheckResult::fail("c2", ApiSurface::Search, CheckCategory::Determinism, "err"));
        exec.complete(100);

        assert_eq!(exec.passed(), 1);
        assert_eq!(exec.failed(), 1);
        assert_eq!(exec.pass_rate(), 0.5);
    }

    #[test]
    fn execution_blocking_pass() {
        let mut exec = ContractExecution::new("m", "r", 0);
        exec.record(CheckResult::pass("c1", ApiSurface::GetText, CheckCategory::SchemaStability));

        let mut advisory_fail =
            CheckResult::fail("c2", ApiSurface::Search, CheckCategory::Determinism, "minor");
        advisory_fail.blocking = false;
        exec.record(advisory_fail);

        assert!(exec.blocking_pass()); // only advisory failures
    }

    #[test]
    fn execution_failures_by_category() {
        let mut exec = ContractExecution::new("m", "r", 0);
        exec.record(CheckResult::fail(
            "c1",
            ApiSurface::GetText,
            CheckCategory::SchemaStability,
            "missing field",
        ));
        exec.record(CheckResult::fail(
            "c2",
            ApiSurface::Search,
            CheckCategory::SchemaStability,
            "wrong type",
        ));
        exec.record(CheckResult::fail(
            "c3",
            ApiSurface::Events,
            CheckCategory::Determinism,
            "nondeterministic",
        ));

        let by_cat = exec.failures_by_category();
        assert_eq!(by_cat["Schema Stability"].len(), 2);
        assert_eq!(by_cat["Deterministic Semantics"].len(), 1);
    }

    // ---- ContractReport ----

    #[test]
    fn report_compatible_verdict() {
        let mut exec = ContractExecution::new("m", "r", 0);
        exec.record(CheckResult::pass("c1", ApiSurface::GetText, CheckCategory::SchemaStability));
        exec.record(CheckResult::pass("c2", ApiSurface::Search, CheckCategory::Determinism));
        exec.complete(100);

        let report = ContractReport::from_execution(&exec);
        assert_eq!(report.verdict, ContractVerdict::Compatible);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn report_conditionally_compatible() {
        let mut exec = ContractExecution::new("m", "r", 0);
        exec.record(CheckResult::pass("c1", ApiSurface::GetText, CheckCategory::SchemaStability));

        let mut advisory =
            CheckResult::fail("c2", ApiSurface::Search, CheckCategory::Determinism, "minor");
        advisory.blocking = false;
        exec.record(advisory);
        exec.complete(100);

        let report = ContractReport::from_execution(&exec);
        assert_eq!(report.verdict, ContractVerdict::ConditionallyCompatible);
    }

    #[test]
    fn report_incompatible() {
        let mut exec = ContractExecution::new("m", "r", 0);
        exec.record(CheckResult::fail(
            "c1",
            ApiSurface::GetText,
            CheckCategory::SchemaStability,
            "missing required field",
        ));
        exec.complete(100);

        let report = ContractReport::from_execution(&exec);
        assert_eq!(report.verdict, ContractVerdict::Incompatible);
        assert!(!report.failures.is_empty());
    }

    #[test]
    fn report_suggested_fix_for_missing_fields() {
        let mut exec = ContractExecution::new("m", "r", 0);
        let mut result = CheckResult::fail(
            "c1",
            ApiSurface::GetText,
            CheckCategory::SchemaStability,
            "missing fields",
        );
        result.missing_fields = vec!["pane_id".into(), "text".into()];
        exec.record(result);
        exec.complete(100);

        let report = ContractReport::from_execution(&exec);
        assert!(report.failures[0].suggested_fix.contains("pane_id"));
    }

    #[test]
    fn report_category_summary() {
        let mut exec = ContractExecution::new("m", "r", 0);
        exec.record(CheckResult::pass("c1", ApiSurface::GetText, CheckCategory::SchemaStability));
        exec.record(CheckResult::pass("c2", ApiSurface::Search, CheckCategory::SchemaStability));
        exec.record(CheckResult::fail(
            "c3",
            ApiSurface::Events,
            CheckCategory::Determinism,
            "err",
        ));
        exec.complete(100);

        let report = ContractReport::from_execution(&exec);
        assert_eq!(report.category_summary["Schema Stability"].passed, 2);
        assert_eq!(report.category_summary["Deterministic Semantics"].failed, 1);
    }

    #[test]
    fn report_render_summary() {
        let mut exec = ContractExecution::new("robot-api", "run-001", 0);
        exec.record(CheckResult::pass("c1", ApiSurface::GetText, CheckCategory::SchemaStability));
        exec.complete(100);

        let report = ContractReport::from_execution(&exec);
        let summary = report.render_summary();
        assert!(summary.contains("robot-api"));
        assert!(summary.contains("Compatible"));
    }

    // ---- Coverage matrix rendering ----

    #[test]
    fn coverage_matrix_renders() {
        let matrix = standard_contract_matrix();
        let rendered = matrix.render_coverage_matrix();
        assert!(rendered.contains("get-text"));
        assert!(rendered.contains("Coverage:"));
        assert!(rendered.contains("100%"));
    }

    // ---- Serde ----

    #[test]
    fn matrix_serde_roundtrip() {
        let matrix = standard_contract_matrix();
        let json = serde_json::to_string(&matrix).unwrap();
        let matrix2: ContractMatrix = serde_json::from_str(&json).unwrap();
        assert_eq!(matrix2.check_count(), matrix.check_count());
    }

    #[test]
    fn report_serde_roundtrip() {
        let mut exec = ContractExecution::new("m", "r", 0);
        exec.record(CheckResult::pass("c1", ApiSurface::GetText, CheckCategory::SchemaStability));
        exec.complete(100);

        let report = ContractReport::from_execution(&exec);
        let json = serde_json::to_string(&report).unwrap();
        let report2: ContractReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report2.verdict, report.verdict);
    }

    // ---- E2E lifecycle ----

    #[test]
    fn e2e_full_contract_validation() {
        let matrix = standard_contract_matrix();

        // Execute all checks (simulate)
        let mut exec = ContractExecution::new(&matrix.matrix_id, "run-001", 1000);
        exec.executed_by = "PinkForge".into();

        for check in &matrix.checks {
            let result = CheckResult::pass(&check.check_id, check.surface, check.category);
            exec.record(result);
        }
        exec.complete(5000);

        let report = ContractReport::from_execution(&exec);
        assert_eq!(report.verdict, ContractVerdict::Compatible);
        assert!(report.blocking_pass);
        assert_eq!(report.passed, matrix.check_count());
        assert_eq!(report.failed, 0);
        assert_eq!(report.pass_rate, 1.0);

        // Verify all categories are present in summary
        assert!(!report.category_summary.is_empty());
        assert!(report.category_summary.values().all(|s| s.failed == 0));
    }

    #[test]
    fn e2e_contract_with_failures_and_diffs() {
        let matrix = standard_contract_matrix();

        let mut exec = ContractExecution::new(&matrix.matrix_id, "run-002", 1000);

        // Most pass, a few fail
        for (i, check) in matrix.checks.iter().enumerate() {
            if i == 0 {
                // Simulate schema failure
                let mut result = CheckResult::fail(
                    &check.check_id,
                    check.surface,
                    check.category,
                    "Missing required field 'version'",
                );
                result.missing_fields = vec!["version".into()];
                exec.record(result);
            } else if i == 5 {
                // Simulate determinism failure
                let mut result = CheckResult::fail(
                    &check.check_id,
                    check.surface,
                    check.category,
                    "Field 'elapsed_ms' varies between runs",
                );
                result.nondeterministic_fields = vec!["elapsed_ms".into()];
                result.blocking = false; // advisory
                exec.record(result);
            } else {
                exec.record(CheckResult::pass(&check.check_id, check.surface, check.category));
            }
        }
        exec.complete(5000);

        let report = ContractReport::from_execution(&exec);
        // One blocking failure → Incompatible
        assert_eq!(report.verdict, ContractVerdict::Incompatible);
        assert_eq!(report.failed, 2);
        assert!(!report.blocking_pass);

        // Check suggested fixes are present
        assert!(report.failures.iter().any(|f| f.suggested_fix.contains("version")));
        assert!(report
            .failures
            .iter()
            .any(|f| f.suggested_fix.contains("elapsed_ms")));

        // Render includes failure details
        let summary = report.render_summary();
        assert!(summary.contains("[BLOCKING]"));
        assert!(summary.contains("[advisory]"));
        assert!(summary.contains("Fix:"));
    }
}
