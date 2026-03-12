//! Go/no-go evidence package for the asupersync migration cutover (ft-e34d9.10.8.4).
//!
//! Assembles and validates the final technical decision package proving
//! migration goals are met, residual risks accepted/mitigated, and all
//! prerequisite gates pass.
//!
//! # Architecture
//!
//! ```text
//! EvidencePackage
//!   ├── PrerequisiteGate        — verify blocking beads are closed
//!   ├── RegressionGuardSuite    — compile-time + runtime regression checks
//!   ├── PersistenceProofSuite   — crash/restart deterministic recovery
//!   ├── TestGateSummary         — unit/integration/e2e/proptest roll-ups
//!   ├── BenchmarkSummary        — performance comparison evidence
//!   ├── IncidentRegistry        — migration-related incident records
//!   ├── RollbackRehearsalLog    — rollback drill results
//!   ├── SoakOutcome             — post-cutover soak period results
//!   ├── RiskRegistry            — unresolved risks with owners/mitigations
//!   └── GoNoGoChecklist         — machine-checkable gate conditions
//!         └── GoNoGoVerdict     — final Go/NoGo/Conditional
//! ```
//!
//! # Usage
//!
//! ```rust
//! use frankenterm_core::cutover_evidence::*;
//!
//! let mut pkg = EvidencePackage::new("asupersync-migration", 1);
//!
//! // Register prerequisites
//! pkg.prerequisites.require("ft-e34d9.10.6", "Verification track");
//! pkg.prerequisites.mark_closed("ft-e34d9.10.6");
//!
//! // Add test evidence
//! pkg.test_gates.record_suite(TestSuiteResult {
//!     suite_name: "unit".into(),
//!     passed: 23222,
//!     failed: 0,
//!     skipped: 64,
//!     duration_ms: 45000,
//!     seed: Some(42),
//!     command: "cargo test --lib".into(),
//! });
//!
//! // Evaluate
//! let verdict = pkg.evaluate();
//! assert!(verdict.decision != GoNoGoDecision::NoGo);
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Top-level evidence package
// =============================================================================

/// Complete evidence package for a migration go/no-go decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidencePackage {
    /// Migration identifier (e.g., "asupersync-migration").
    pub migration_id: String,
    /// Evidence package schema version.
    pub schema_version: u32,
    /// When this package was assembled (epoch ms).
    pub assembled_at_ms: u64,
    /// Prerequisite bead gate.
    pub prerequisites: PrerequisiteGate,
    /// Regression guard results.
    pub regression_guards: RegressionGuardSuite,
    /// Crash/restart persistence proofs.
    pub persistence_proofs: PersistenceProofSuite,
    /// Aggregated test results.
    pub test_gates: TestGateSummary,
    /// Performance benchmark comparisons.
    pub benchmarks: BenchmarkSummary,
    /// Migration-related incidents.
    pub incidents: IncidentRegistry,
    /// Rollback rehearsal outcomes.
    pub rollback_rehearsals: RollbackRehearsalLog,
    /// Post-cutover soak results.
    pub soak_outcomes: Vec<SoakOutcome>,
    /// Unresolved risk registry.
    pub risks: RiskRegistry,
    /// Telemetry for the evidence-gathering process.
    pub telemetry: EvidenceTelemetry,
}

impl EvidencePackage {
    /// Create a new empty evidence package.
    #[must_use]
    pub fn new(migration_id: impl Into<String>, schema_version: u32) -> Self {
        Self {
            migration_id: migration_id.into(),
            schema_version,
            assembled_at_ms: 0,
            prerequisites: PrerequisiteGate::new(),
            regression_guards: RegressionGuardSuite::new(),
            persistence_proofs: PersistenceProofSuite::new(),
            test_gates: TestGateSummary::new(),
            benchmarks: BenchmarkSummary::new(),
            incidents: IncidentRegistry::new(),
            rollback_rehearsals: RollbackRehearsalLog::new(),
            soak_outcomes: Vec::new(),
            risks: RiskRegistry::new(),
            telemetry: EvidenceTelemetry::default(),
        }
    }

    /// Set the assembly timestamp.
    pub fn set_assembled_at(&mut self, now_ms: u64) {
        self.assembled_at_ms = now_ms;
    }

    /// Evaluate all gates and produce a go/no-go verdict.
    #[must_use]
    pub fn evaluate(&self) -> GoNoGoVerdict {
        let mut checklist = GoNoGoChecklist::new();

        // Gate G-01: All prerequisites closed.
        let prereqs_ok = self.prerequisites.all_closed();
        checklist.add_check(ChecklistItem {
            gate_id: "G-01-prerequisites".into(),
            description: "All prerequisite beads are closed".into(),
            status: if prereqs_ok {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            detail: format!(
                "{}/{} closed",
                self.prerequisites.closed_count(),
                self.prerequisites.total_count()
            ),
            blocking: true,
        });

        // Gate G-02: No regression guard failures.
        let guards_ok = self.regression_guards.all_pass();
        checklist.add_check(ChecklistItem {
            gate_id: "G-02-regression-guards".into(),
            description: "All regression guards pass".into(),
            status: if guards_ok {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            detail: format!(
                "{}/{} pass",
                self.regression_guards.pass_count(),
                self.regression_guards.total_count()
            ),
            blocking: true,
        });

        // Gate G-03: Persistence proofs complete.
        let persistence_ok = self.persistence_proofs.all_verified();
        checklist.add_check(ChecklistItem {
            gate_id: "G-03-persistence".into(),
            description: "Crash/restart persistence proofs verified".into(),
            status: if persistence_ok {
                CheckStatus::Pass
            } else if self.persistence_proofs.total_count() == 0 {
                CheckStatus::Skip
            } else {
                CheckStatus::Fail
            },
            detail: format!(
                "{}/{} verified",
                self.persistence_proofs.verified_count(),
                self.persistence_proofs.total_count()
            ),
            blocking: true,
        });

        // Gate G-04: Test pass rate above threshold.
        let test_pass_rate = self.test_gates.pass_rate();
        let tests_ok = test_pass_rate >= 0.99;
        checklist.add_check(ChecklistItem {
            gate_id: "G-04-test-pass-rate".into(),
            description: "Test pass rate >= 99%".into(),
            status: if tests_ok {
                CheckStatus::Pass
            } else if self.test_gates.total_suites() == 0 {
                CheckStatus::Skip
            } else {
                CheckStatus::Fail
            },
            detail: format!("{:.2}%", test_pass_rate * 100.0),
            blocking: true,
        });

        // Gate G-05: No unresolved P1 incidents.
        let no_p1_incidents = self.incidents.unresolved_p1_count() == 0;
        checklist.add_check(ChecklistItem {
            gate_id: "G-05-no-p1-incidents".into(),
            description: "No unresolved P1 incidents".into(),
            status: if no_p1_incidents {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            detail: format!(
                "{} unresolved P1",
                self.incidents.unresolved_p1_count()
            ),
            blocking: true,
        });

        // Gate G-06: Rollback rehearsal success.
        let rollback_ok = self.rollback_rehearsals.has_successful_rehearsal();
        checklist.add_check(ChecklistItem {
            gate_id: "G-06-rollback-rehearsal".into(),
            description: "At least one successful rollback rehearsal".into(),
            status: if rollback_ok {
                CheckStatus::Pass
            } else if self.rollback_rehearsals.total_count() == 0 {
                CheckStatus::Skip
            } else {
                CheckStatus::Fail
            },
            detail: format!(
                "{}/{} successful",
                self.rollback_rehearsals.success_count(),
                self.rollback_rehearsals.total_count()
            ),
            blocking: false,
        });

        // Gate G-07: Performance within bounds.
        let perf_ok = self.benchmarks.all_within_threshold();
        checklist.add_check(ChecklistItem {
            gate_id: "G-07-performance".into(),
            description: "Benchmarks within regression threshold".into(),
            status: if perf_ok {
                CheckStatus::Pass
            } else if self.benchmarks.total_count() == 0 {
                CheckStatus::Skip
            } else {
                CheckStatus::Warn
            },
            detail: format!(
                "{}/{} within threshold",
                self.benchmarks.within_threshold_count(),
                self.benchmarks.total_count()
            ),
            blocking: false,
        });

        // Gate G-08: All critical risks have mitigations.
        let risks_ok = self.risks.all_critical_mitigated();
        checklist.add_check(ChecklistItem {
            gate_id: "G-08-risk-mitigations".into(),
            description: "All critical risks have documented mitigations".into(),
            status: if risks_ok {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            detail: format!(
                "{} critical unmitigated",
                self.risks.unmitigated_critical_count()
            ),
            blocking: true,
        });

        // Determine verdict.
        let blocking_failures = checklist.blocking_failure_count();
        let total_failures = checklist.failure_count();
        let total_warnings = checklist.warning_count();

        let decision = if blocking_failures > 0 {
            GoNoGoDecision::NoGo
        } else if total_failures > 0 || total_warnings > 0 {
            GoNoGoDecision::Conditional
        } else {
            GoNoGoDecision::Go
        };

        let rationale = match decision {
            GoNoGoDecision::Go => "All gates pass. Migration is approved.".into(),
            GoNoGoDecision::Conditional => format!(
                "{total_warnings} warnings, {total_failures} non-blocking failures. Review required."
            ),
            GoNoGoDecision::NoGo => format!(
                "{blocking_failures} blocking gate(s) failed. Migration blocked."
            ),
        };

        GoNoGoVerdict {
            decision,
            rationale,
            checklist,
            migration_id: self.migration_id.clone(),
            evaluated_at_ms: self.assembled_at_ms,
        }
    }

    /// Add a soak outcome.
    pub fn record_soak(&mut self, outcome: SoakOutcome) {
        self.soak_outcomes.push(outcome);
        self.telemetry.soak_outcomes_recorded += 1;
    }

    /// Summary statistics for the evidence package.
    #[must_use]
    pub fn summary(&self) -> EvidenceSummary {
        let verdict = self.evaluate();
        EvidenceSummary {
            migration_id: self.migration_id.clone(),
            decision: verdict.decision,
            prerequisites_closed: self.prerequisites.closed_count(),
            prerequisites_total: self.prerequisites.total_count(),
            guards_passing: self.regression_guards.pass_count(),
            guards_total: self.regression_guards.total_count(),
            test_pass_rate: self.test_gates.pass_rate(),
            test_suites: self.test_gates.total_suites(),
            benchmarks_ok: self.benchmarks.within_threshold_count(),
            benchmarks_total: self.benchmarks.total_count(),
            unresolved_p1_incidents: self.incidents.unresolved_p1_count(),
            critical_unmitigated_risks: self.risks.unmitigated_critical_count(),
            rollback_rehearsals: self.rollback_rehearsals.total_count(),
            soak_outcomes: self.soak_outcomes.len(),
        }
    }
}

/// Human-readable summary of evidence package state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceSummary {
    pub migration_id: String,
    pub decision: GoNoGoDecision,
    pub prerequisites_closed: usize,
    pub prerequisites_total: usize,
    pub guards_passing: usize,
    pub guards_total: usize,
    pub test_pass_rate: f64,
    pub test_suites: usize,
    pub benchmarks_ok: usize,
    pub benchmarks_total: usize,
    pub unresolved_p1_incidents: usize,
    pub critical_unmitigated_risks: usize,
    pub rollback_rehearsals: usize,
    pub soak_outcomes: usize,
}

// =============================================================================
// Go/No-Go verdict
// =============================================================================

/// Final migration go/no-go verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoNoGoVerdict {
    /// The decision.
    pub decision: GoNoGoDecision,
    /// Human-readable rationale.
    pub rationale: String,
    /// The full checklist that produced this verdict.
    pub checklist: GoNoGoChecklist,
    /// Migration this verdict applies to.
    pub migration_id: String,
    /// When this verdict was computed.
    pub evaluated_at_ms: u64,
}

/// Go/no-go decision outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoNoGoDecision {
    /// All gates pass — safe to cut over.
    Go,
    /// Blocking gates failed — migration blocked.
    NoGo,
    /// Non-blocking issues exist — manual review required.
    Conditional,
}

// =============================================================================
// Checklist
// =============================================================================

/// Machine-checkable go/no-go checklist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoNoGoChecklist {
    /// Individual gate checks.
    pub checks: Vec<ChecklistItem>,
}

impl GoNoGoChecklist {
    /// Create a new empty checklist.
    #[must_use]
    pub fn new() -> Self {
        Self { checks: Vec::new() }
    }

    /// Add a check to the checklist.
    pub fn add_check(&mut self, item: ChecklistItem) {
        self.checks.push(item);
    }

    /// Count of passing checks.
    #[must_use]
    pub fn pass_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Pass)
            .count()
    }

    /// Count of failing checks.
    #[must_use]
    pub fn failure_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .count()
    }

    /// Count of warnings.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Warn)
            .count()
    }

    /// Count of blocking failures.
    #[must_use]
    pub fn blocking_failure_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.blocking && c.status == CheckStatus::Fail)
            .count()
    }

    /// Whether all checks pass (no failures or warnings).
    #[must_use]
    pub fn all_pass(&self) -> bool {
        self.failure_count() == 0 && self.warning_count() == 0
    }

    /// Render a human-readable summary of the checklist.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        for check in &self.checks {
            let icon = match check.status {
                CheckStatus::Pass => "[PASS]",
                CheckStatus::Fail => "[FAIL]",
                CheckStatus::Warn => "[WARN]",
                CheckStatus::Skip => "[SKIP]",
            };
            let blocking = if check.blocking { " (blocking)" } else { "" };
            lines.push(format!(
                "{} {} — {}{} ({})",
                icon, check.gate_id, check.description, blocking, check.detail
            ));
        }
        lines.join("\n")
    }
}

impl Default for GoNoGoChecklist {
    fn default() -> Self {
        Self::new()
    }
}

/// Individual checklist gate check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChecklistItem {
    /// Gate identifier (e.g., "G-01-prerequisites").
    pub gate_id: String,
    /// Human-readable description.
    pub description: String,
    /// Check outcome.
    pub status: CheckStatus,
    /// Detailed result information.
    pub detail: String,
    /// Whether failure of this check blocks the migration.
    pub blocking: bool,
}

/// Status of a single checklist check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckStatus {
    /// Check passed.
    Pass,
    /// Check failed.
    Fail,
    /// Check produced a warning (non-blocking concern).
    Warn,
    /// Check was skipped (no data available).
    Skip,
}

// =============================================================================
// Prerequisite gate
// =============================================================================

/// Tracks whether all prerequisite beads are closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrerequisiteGate {
    /// Prerequisite beads: bead_id → (description, is_closed).
    prerequisites: BTreeMap<String, PrerequisiteEntry>,
}

/// A single prerequisite entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrerequisiteEntry {
    /// Human-readable description of what this prerequisite covers.
    pub description: String,
    /// Whether the prerequisite is satisfied (bead closed).
    pub closed: bool,
}

impl PrerequisiteGate {
    /// Create an empty prerequisite gate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            prerequisites: BTreeMap::new(),
        }
    }

    /// Register a required prerequisite.
    pub fn require(&mut self, bead_id: impl Into<String>, description: impl Into<String>) {
        self.prerequisites.insert(
            bead_id.into(),
            PrerequisiteEntry {
                description: description.into(),
                closed: false,
            },
        );
    }

    /// Mark a prerequisite as closed/satisfied.
    pub fn mark_closed(&mut self, bead_id: &str) {
        if let Some(entry) = self.prerequisites.get_mut(bead_id) {
            entry.closed = true;
        }
    }

    /// Check if all prerequisites are closed.
    #[must_use]
    pub fn all_closed(&self) -> bool {
        !self.prerequisites.is_empty()
            && self.prerequisites.values().all(|e| e.closed)
    }

    /// Number of closed prerequisites.
    #[must_use]
    pub fn closed_count(&self) -> usize {
        self.prerequisites.values().filter(|e| e.closed).count()
    }

    /// Total number of prerequisites.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.prerequisites.len()
    }

    /// List of unclosed prerequisites.
    #[must_use]
    pub fn unclosed(&self) -> Vec<(&str, &str)> {
        self.prerequisites
            .iter()
            .filter(|(_, e)| !e.closed)
            .map(|(id, e)| (id.as_str(), e.description.as_str()))
            .collect()
    }
}

impl Default for PrerequisiteGate {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Regression guards
// =============================================================================

/// Suite of compile-time and runtime regression guards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionGuardSuite {
    /// Individual guard results.
    pub guards: Vec<RegressionGuard>,
}

/// A single regression guard check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionGuard {
    /// Guard identifier.
    pub guard_id: String,
    /// What this guard protects against.
    pub description: String,
    /// Guard category.
    pub category: GuardCategory,
    /// Whether the guard passed.
    pub passed: bool,
    /// Evidence/detail for the result.
    pub evidence: String,
    /// Command used to verify (for reproducibility).
    pub command: String,
}

/// Category of regression guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuardCategory {
    /// Compile-time guard (e.g., no tokio imports).
    CompileTime,
    /// Runtime invariant check.
    Runtime,
    /// API contract guard.
    ApiContract,
    /// Determinism/reproducibility guard.
    Determinism,
    /// Safety/policy guard.
    Safety,
}

impl RegressionGuardSuite {
    /// Create an empty suite.
    #[must_use]
    pub fn new() -> Self {
        Self {
            guards: Vec::new(),
        }
    }

    /// Add a guard result.
    pub fn record(&mut self, guard: RegressionGuard) {
        self.guards.push(guard);
    }

    /// Whether all guards pass.
    #[must_use]
    pub fn all_pass(&self) -> bool {
        !self.guards.is_empty() && self.guards.iter().all(|g| g.passed)
    }

    /// Number of passing guards.
    #[must_use]
    pub fn pass_count(&self) -> usize {
        self.guards.iter().filter(|g| g.passed).count()
    }

    /// Total number of guards.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.guards.len()
    }

    /// List failing guards.
    #[must_use]
    pub fn failing(&self) -> Vec<&RegressionGuard> {
        self.guards.iter().filter(|g| !g.passed).collect()
    }
}

impl Default for RegressionGuardSuite {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Persistence proofs
// =============================================================================

/// Suite of crash/restart persistence proofs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceProofSuite {
    /// Individual persistence proofs.
    pub proofs: Vec<PersistenceProof>,
}

/// A single crash/restart persistence proof.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceProof {
    /// Proof identifier.
    pub proof_id: String,
    /// Workflow being tested (e.g., "session-restore", "recording-replay").
    pub workflow: String,
    /// What was verified.
    pub description: String,
    /// Whether deterministic recovery was confirmed.
    pub verified: bool,
    /// Seed used for deterministic reproduction.
    pub seed: Option<u64>,
    /// State hash before crash.
    pub state_hash_before: Option<String>,
    /// State hash after recovery.
    pub state_hash_after: Option<String>,
    /// Evidence notes.
    pub evidence: String,
    /// Command used to reproduce.
    pub command: String,
}

impl PersistenceProofSuite {
    /// Create an empty suite.
    #[must_use]
    pub fn new() -> Self {
        Self {
            proofs: Vec::new(),
        }
    }

    /// Record a persistence proof.
    pub fn record(&mut self, proof: PersistenceProof) {
        self.proofs.push(proof);
    }

    /// Whether all proofs are verified.
    #[must_use]
    pub fn all_verified(&self) -> bool {
        !self.proofs.is_empty() && self.proofs.iter().all(|p| p.verified)
    }

    /// Count of verified proofs.
    #[must_use]
    pub fn verified_count(&self) -> usize {
        self.proofs.iter().filter(|p| p.verified).count()
    }

    /// Total number of proofs.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.proofs.len()
    }

    /// Whether any proof has matching state hashes (deterministic recovery).
    #[must_use]
    pub fn has_deterministic_recovery(&self) -> bool {
        self.proofs.iter().any(|p| {
            p.verified
                && p.state_hash_before.is_some()
                && p.state_hash_before == p.state_hash_after
        })
    }
}

impl Default for PersistenceProofSuite {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Test gate summary
// =============================================================================

/// Aggregated test results across all test suites.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestGateSummary {
    /// Results from individual test suites.
    pub suites: Vec<TestSuiteResult>,
}

/// Result from a single test suite execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestSuiteResult {
    /// Suite name (e.g., "unit", "integration", "e2e", "proptest").
    pub suite_name: String,
    /// Number of tests that passed.
    pub passed: u64,
    /// Number of tests that failed.
    pub failed: u64,
    /// Number of tests skipped.
    pub skipped: u64,
    /// Total execution time in milliseconds.
    pub duration_ms: u64,
    /// Deterministic seed used (if applicable).
    pub seed: Option<u64>,
    /// Exact command used to run this suite.
    pub command: String,
}

impl TestGateSummary {
    /// Create an empty summary.
    #[must_use]
    pub fn new() -> Self {
        Self {
            suites: Vec::new(),
        }
    }

    /// Record a test suite result.
    pub fn record_suite(&mut self, result: TestSuiteResult) {
        self.suites.push(result);
    }

    /// Overall pass rate across all suites.
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        let total_passed: u64 = self.suites.iter().map(|s| s.passed).sum();
        let total_run: u64 = self
            .suites
            .iter()
            .map(|s| s.passed + s.failed)
            .sum();
        if total_run == 0 {
            0.0
        } else {
            total_passed as f64 / total_run as f64
        }
    }

    /// Total number of test suites recorded.
    #[must_use]
    pub fn total_suites(&self) -> usize {
        self.suites.len()
    }

    /// Total tests across all suites.
    #[must_use]
    pub fn total_tests(&self) -> u64 {
        self.suites
            .iter()
            .map(|s| s.passed + s.failed + s.skipped)
            .sum()
    }

    /// Total failures across all suites.
    #[must_use]
    pub fn total_failures(&self) -> u64 {
        self.suites.iter().map(|s| s.failed).sum()
    }
}

impl Default for TestGateSummary {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Benchmark summary
// =============================================================================

/// Performance benchmark comparison results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSummary {
    /// Individual benchmark comparisons.
    pub comparisons: Vec<BenchmarkComparison>,
    /// Regression threshold (ratio; 1.1 = 10% regression allowed).
    pub regression_threshold: f64,
}

/// A single benchmark comparison (before vs after migration).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkComparison {
    /// Benchmark name.
    pub name: String,
    /// Metric being measured.
    pub metric: String,
    /// Before-migration value.
    pub before: f64,
    /// After-migration value.
    pub after: f64,
    /// Units (e.g., "ms", "ops/sec", "bytes").
    pub unit: String,
    /// Whether lower is better (true for latency, false for throughput).
    pub lower_is_better: bool,
}

impl BenchmarkComparison {
    /// Ratio of after/before (> 1.0 means regression for lower-is-better).
    #[must_use]
    pub fn ratio(&self) -> f64 {
        if self.before == 0.0 {
            return if self.after == 0.0 { 1.0 } else { f64::INFINITY };
        }
        self.after / self.before
    }

    /// Whether this benchmark is within the given threshold.
    #[must_use]
    pub fn within_threshold(&self, threshold: f64) -> bool {
        if self.lower_is_better {
            self.ratio() <= threshold
        } else {
            // For throughput metrics (higher is better), invert.
            self.ratio() >= 1.0 / threshold
        }
    }
}

impl BenchmarkSummary {
    /// Create with default 10% regression threshold.
    #[must_use]
    pub fn new() -> Self {
        Self {
            comparisons: Vec::new(),
            regression_threshold: 1.10,
        }
    }

    /// Create with custom threshold.
    #[must_use]
    pub fn with_threshold(threshold: f64) -> Self {
        Self {
            comparisons: Vec::new(),
            regression_threshold: threshold,
        }
    }

    /// Record a benchmark comparison.
    pub fn record(&mut self, comparison: BenchmarkComparison) {
        self.comparisons.push(comparison);
    }

    /// Whether all benchmarks are within the regression threshold.
    #[must_use]
    pub fn all_within_threshold(&self) -> bool {
        self.comparisons.is_empty()
            || self
                .comparisons
                .iter()
                .all(|c| c.within_threshold(self.regression_threshold))
    }

    /// Count of benchmarks within threshold.
    #[must_use]
    pub fn within_threshold_count(&self) -> usize {
        self.comparisons
            .iter()
            .filter(|c| c.within_threshold(self.regression_threshold))
            .count()
    }

    /// Total number of benchmark comparisons.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.comparisons.len()
    }

    /// Regressions exceeding threshold.
    #[must_use]
    pub fn regressions(&self) -> Vec<&BenchmarkComparison> {
        self.comparisons
            .iter()
            .filter(|c| !c.within_threshold(self.regression_threshold))
            .collect()
    }
}

impl Default for BenchmarkSummary {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Incident registry
// =============================================================================

/// Registry of migration-related incidents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentRegistry {
    /// Recorded incidents.
    pub incidents: Vec<IncidentRecord>,
}

/// A single migration-related incident.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentRecord {
    /// Incident identifier.
    pub incident_id: String,
    /// Incident priority (1 = most severe).
    pub priority: u8,
    /// Short description.
    pub title: String,
    /// Detailed description.
    pub description: String,
    /// Resolution status.
    pub status: IncidentStatus,
    /// Root cause (if determined).
    pub root_cause: Option<String>,
    /// Remediation applied.
    pub remediation: Option<String>,
    /// When the incident was reported (epoch ms).
    pub reported_at_ms: u64,
    /// When the incident was resolved (epoch ms), if resolved.
    pub resolved_at_ms: Option<u64>,
    /// Related bead IDs.
    pub related_beads: Vec<String>,
}

/// Incident resolution status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IncidentStatus {
    /// Incident is open and unresolved.
    Open,
    /// Incident is being investigated.
    Investigating,
    /// Incident has been resolved.
    Resolved,
    /// Incident was a false positive.
    FalsePositive,
}

impl IncidentRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            incidents: Vec::new(),
        }
    }

    /// Record an incident.
    pub fn record(&mut self, incident: IncidentRecord) {
        self.incidents.push(incident);
    }

    /// Count of unresolved P1 incidents.
    #[must_use]
    pub fn unresolved_p1_count(&self) -> usize {
        self.incidents
            .iter()
            .filter(|i| {
                i.priority == 1
                    && !matches!(i.status, IncidentStatus::Resolved | IncidentStatus::FalsePositive)
            })
            .count()
    }

    /// Count of total unresolved incidents.
    #[must_use]
    pub fn unresolved_count(&self) -> usize {
        self.incidents
            .iter()
            .filter(|i| {
                !matches!(i.status, IncidentStatus::Resolved | IncidentStatus::FalsePositive)
            })
            .count()
    }

    /// All resolved incidents.
    #[must_use]
    pub fn resolved(&self) -> Vec<&IncidentRecord> {
        self.incidents
            .iter()
            .filter(|i| i.status == IncidentStatus::Resolved)
            .collect()
    }
}

impl Default for IncidentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Rollback rehearsal log
// =============================================================================

/// Log of rollback rehearsal outcomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackRehearsalLog {
    /// Rehearsal attempts.
    pub rehearsals: Vec<RollbackRehearsal>,
}

/// A single rollback rehearsal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackRehearsal {
    /// Rehearsal identifier.
    pub rehearsal_id: String,
    /// When the rehearsal was performed (epoch ms).
    pub performed_at_ms: u64,
    /// Whether rollback completed successfully.
    pub successful: bool,
    /// Time to complete rollback (ms).
    pub rollback_duration_ms: u64,
    /// Whether data integrity was maintained.
    pub data_integrity_preserved: bool,
    /// Notes on the rehearsal.
    pub notes: String,
    /// Command used to trigger rollback.
    pub command: String,
}

impl RollbackRehearsalLog {
    /// Create an empty log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rehearsals: Vec::new(),
        }
    }

    /// Record a rehearsal.
    pub fn record(&mut self, rehearsal: RollbackRehearsal) {
        self.rehearsals.push(rehearsal);
    }

    /// Whether at least one successful rehearsal exists.
    #[must_use]
    pub fn has_successful_rehearsal(&self) -> bool {
        self.rehearsals.iter().any(|r| r.successful)
    }

    /// Count of successful rehearsals.
    #[must_use]
    pub fn success_count(&self) -> usize {
        self.rehearsals.iter().filter(|r| r.successful).count()
    }

    /// Total rehearsals performed.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.rehearsals.len()
    }
}

impl Default for RollbackRehearsalLog {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Soak outcome
// =============================================================================

/// Post-cutover soak period observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakOutcome {
    /// Soak period identifier.
    pub period_id: String,
    /// Start of soak period (epoch ms).
    pub start_ms: u64,
    /// End of soak period (epoch ms).
    pub end_ms: u64,
    /// Whether the soak period passed all SLOs.
    pub slo_conforming: bool,
    /// Error rate observed during soak.
    pub error_rate: f64,
    /// P95 latency observed (ms).
    pub p95_latency_ms: f64,
    /// Number of incidents during soak.
    pub incident_count: u32,
    /// Whether any rollback was triggered.
    pub rollback_triggered: bool,
    /// Notes.
    pub notes: String,
}

// =============================================================================
// Risk registry
// =============================================================================

/// Registry of unresolved migration risks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskRegistry {
    /// Documented risks.
    pub risks: Vec<RiskRecord>,
}

/// A single risk record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskRecord {
    /// Risk identifier.
    pub risk_id: String,
    /// Risk severity.
    pub severity: RiskSeverity,
    /// Risk description.
    pub description: String,
    /// Mitigation plan (if any).
    pub mitigation: Option<String>,
    /// Risk owner.
    pub owner: Option<String>,
    /// Follow-up controls.
    pub follow_up: Option<String>,
    /// Whether the risk has been accepted.
    pub accepted: bool,
    /// Related bead IDs.
    pub related_beads: Vec<String>,
}

/// Risk severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RiskSeverity {
    /// Low severity — minimal impact.
    Low,
    /// Medium severity — manageable impact.
    Medium,
    /// High severity — significant impact.
    High,
    /// Critical severity — migration-blocking.
    Critical,
}

impl RiskRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self { risks: Vec::new() }
    }

    /// Record a risk.
    pub fn record(&mut self, risk: RiskRecord) {
        self.risks.push(risk);
    }

    /// Whether all critical risks have documented mitigations.
    #[must_use]
    pub fn all_critical_mitigated(&self) -> bool {
        self.risks
            .iter()
            .filter(|r| r.severity == RiskSeverity::Critical)
            .all(|r| r.mitigation.is_some() || r.accepted)
    }

    /// Count of unmitigated critical risks.
    #[must_use]
    pub fn unmitigated_critical_count(&self) -> usize {
        self.risks
            .iter()
            .filter(|r| {
                r.severity == RiskSeverity::Critical && r.mitigation.is_none() && !r.accepted
            })
            .count()
    }

    /// All risks by severity (descending).
    #[must_use]
    pub fn by_severity(&self) -> Vec<&RiskRecord> {
        let mut sorted: Vec<&RiskRecord> = self.risks.iter().collect();
        sorted.sort_by(|a, b| b.severity.cmp(&a.severity));
        sorted
    }

    /// Total number of risks.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.risks.len()
    }
}

impl Default for RiskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Telemetry
// =============================================================================

/// Counters for the evidence-gathering process itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidenceTelemetry {
    /// Number of prerequisite checks performed.
    pub prerequisite_checks: u64,
    /// Number of regression guards evaluated.
    pub guards_evaluated: u64,
    /// Number of persistence proofs collected.
    pub persistence_proofs_collected: u64,
    /// Number of test suites recorded.
    pub test_suites_recorded: u64,
    /// Number of benchmark comparisons recorded.
    pub benchmarks_recorded: u64,
    /// Number of incidents recorded.
    pub incidents_recorded: u64,
    /// Number of rollback rehearsals recorded.
    pub rehearsals_recorded: u64,
    /// Number of soak outcomes recorded.
    pub soak_outcomes_recorded: u64,
    /// Number of risks documented.
    pub risks_documented: u64,
    /// Number of go/no-go evaluations performed.
    pub evaluations_performed: u64,
}

// =============================================================================
// Render helpers
// =============================================================================

impl GoNoGoVerdict {
    /// Render a human-readable report.
    #[must_use]
    pub fn render_report(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "=== Go/No-Go Verdict: {} ===",
            self.migration_id
        ));
        lines.push(format!(
            "Decision: {:?}",
            self.decision
        ));
        lines.push(format!("Rationale: {}", self.rationale));
        lines.push(String::new());
        lines.push("--- Checklist ---".to_string());
        lines.push(self.checklist.render_summary());
        lines.join("\n")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_package() -> EvidencePackage {
        let mut pkg = EvidencePackage::new("test-migration", 1);
        pkg.set_assembled_at(1000);

        // Prerequisites
        pkg.prerequisites.require("bead-1", "First prerequisite");
        pkg.prerequisites.require("bead-2", "Second prerequisite");
        pkg.prerequisites.mark_closed("bead-1");
        pkg.prerequisites.mark_closed("bead-2");

        // Guards
        pkg.regression_guards.record(RegressionGuard {
            guard_id: "no-tokio".into(),
            description: "No tokio imports in core".into(),
            category: GuardCategory::CompileTime,
            passed: true,
            evidence: "grep found 0 matches".into(),
            command: "grep -r 'use tokio' src/".into(),
        });

        // Persistence
        pkg.persistence_proofs.record(PersistenceProof {
            proof_id: "session-restore-1".into(),
            workflow: "session-restore".into(),
            description: "Session state survives crash".into(),
            verified: true,
            seed: Some(42),
            state_hash_before: Some("abc123".into()),
            state_hash_after: Some("abc123".into()),
            evidence: "State hashes match".into(),
            command: "cargo test session_crash_recovery".into(),
        });

        // Tests
        pkg.test_gates.record_suite(TestSuiteResult {
            suite_name: "unit".into(),
            passed: 23000,
            failed: 0,
            skipped: 64,
            duration_ms: 45000,
            seed: None,
            command: "cargo test --lib".into(),
        });

        // Benchmarks
        pkg.benchmarks.record(BenchmarkComparison {
            name: "ingest_throughput".into(),
            metric: "events/sec".into(),
            before: 10000.0,
            after: 10500.0,
            unit: "events/sec".into(),
            lower_is_better: false,
        });

        // No incidents.

        // Rollback rehearsal
        pkg.rollback_rehearsals.record(RollbackRehearsal {
            rehearsal_id: "rehearsal-1".into(),
            performed_at_ms: 500,
            successful: true,
            rollback_duration_ms: 3000,
            data_integrity_preserved: true,
            notes: "Clean rollback".into(),
            command: "ft rollback --to v0.9".into(),
        });

        // Risks
        pkg.risks.record(RiskRecord {
            risk_id: "R-01".into(),
            severity: RiskSeverity::Medium,
            description: "Some edge case not covered".into(),
            mitigation: Some("Monitoring alert added".into()),
            owner: Some("eng-team".into()),
            follow_up: None,
            accepted: false,
            related_beads: vec![],
        });

        pkg
    }

    #[test]
    fn test_full_pass_verdict() {
        let pkg = sample_package();
        let verdict = pkg.evaluate();
        assert_eq!(verdict.decision, GoNoGoDecision::Go);
        assert!(verdict.checklist.all_pass());
    }

    #[test]
    fn test_prerequisite_failure_blocks() {
        let mut pkg = sample_package();
        pkg.prerequisites.require("bead-3", "Unclosed bead");
        // bead-3 not closed

        let verdict = pkg.evaluate();
        assert_eq!(verdict.decision, GoNoGoDecision::NoGo);
        assert_eq!(verdict.checklist.blocking_failure_count(), 1);
    }

    #[test]
    fn test_regression_guard_failure_blocks() {
        let mut pkg = sample_package();
        pkg.regression_guards.record(RegressionGuard {
            guard_id: "tokio-import-found".into(),
            description: "Tokio import detected".into(),
            category: GuardCategory::CompileTime,
            passed: false,
            evidence: "src/foo.rs:5: use tokio".into(),
            command: "grep -r 'use tokio' src/".into(),
        });

        let verdict = pkg.evaluate();
        assert_eq!(verdict.decision, GoNoGoDecision::NoGo);
    }

    #[test]
    fn test_test_pass_rate_below_threshold() {
        let mut pkg = sample_package();
        pkg.test_gates.suites.clear();
        pkg.test_gates.record_suite(TestSuiteResult {
            suite_name: "unit".into(),
            passed: 90,
            failed: 11,
            skipped: 0,
            duration_ms: 1000,
            seed: None,
            command: "cargo test".into(),
        });

        let verdict = pkg.evaluate();
        assert_eq!(verdict.decision, GoNoGoDecision::NoGo);
    }

    #[test]
    fn test_p1_incident_blocks() {
        let mut pkg = sample_package();
        pkg.incidents.record(IncidentRecord {
            incident_id: "INC-001".into(),
            priority: 1,
            title: "Data loss during migration".into(),
            description: "Events lost".into(),
            status: IncidentStatus::Open,
            root_cause: None,
            remediation: None,
            reported_at_ms: 100,
            resolved_at_ms: None,
            related_beads: vec![],
        });

        let verdict = pkg.evaluate();
        assert_eq!(verdict.decision, GoNoGoDecision::NoGo);
    }

    #[test]
    fn test_resolved_p1_does_not_block() {
        let mut pkg = sample_package();
        pkg.incidents.record(IncidentRecord {
            incident_id: "INC-001".into(),
            priority: 1,
            title: "Data loss (resolved)".into(),
            description: "Fixed".into(),
            status: IncidentStatus::Resolved,
            root_cause: Some("Missing flush".into()),
            remediation: Some("Added explicit flush".into()),
            reported_at_ms: 100,
            resolved_at_ms: Some(200),
            related_beads: vec![],
        });

        let verdict = pkg.evaluate();
        assert_eq!(verdict.decision, GoNoGoDecision::Go);
    }

    #[test]
    fn test_benchmark_regression_is_warning() {
        let mut pkg = sample_package();
        pkg.benchmarks.comparisons.clear();
        pkg.benchmarks.record(BenchmarkComparison {
            name: "latency".into(),
            metric: "p99".into(),
            before: 10.0,
            after: 20.0, // 2x regression
            unit: "ms".into(),
            lower_is_better: true,
        });

        let verdict = pkg.evaluate();
        // Benchmark gate is non-blocking, so it's Conditional, not NoGo.
        assert_eq!(verdict.decision, GoNoGoDecision::Conditional);
    }

    #[test]
    fn test_rollback_not_performed_is_non_blocking() {
        let mut pkg = sample_package();
        pkg.rollback_rehearsals.rehearsals.clear();

        let verdict = pkg.evaluate();
        // Rollback gate is non-blocking and skipped.
        assert_eq!(verdict.decision, GoNoGoDecision::Go);
    }

    #[test]
    fn test_critical_risk_unmitigated_blocks() {
        let mut pkg = sample_package();
        pkg.risks.record(RiskRecord {
            risk_id: "R-CRIT".into(),
            severity: RiskSeverity::Critical,
            description: "Potential data corruption".into(),
            mitigation: None,
            owner: None,
            follow_up: None,
            accepted: false,
            related_beads: vec![],
        });

        let verdict = pkg.evaluate();
        assert_eq!(verdict.decision, GoNoGoDecision::NoGo);
    }

    #[test]
    fn test_accepted_critical_risk_does_not_block() {
        let mut pkg = sample_package();
        pkg.risks.record(RiskRecord {
            risk_id: "R-CRIT".into(),
            severity: RiskSeverity::Critical,
            description: "Potential data corruption".into(),
            mitigation: None,
            owner: Some("team-lead".into()),
            follow_up: Some("Monitor for 30 days".into()),
            accepted: true,
            related_beads: vec![],
        });

        let verdict = pkg.evaluate();
        assert_eq!(verdict.decision, GoNoGoDecision::Go);
    }

    #[test]
    fn test_empty_package_is_nogo() {
        let pkg = EvidencePackage::new("empty", 1);
        let verdict = pkg.evaluate();
        // Empty prerequisites gate fails (requires at least 1).
        assert_eq!(verdict.decision, GoNoGoDecision::NoGo);
    }

    #[test]
    fn test_prerequisite_gate_operations() {
        let mut gate = PrerequisiteGate::new();
        assert!(!gate.all_closed()); // Empty = not all closed
        assert_eq!(gate.total_count(), 0);

        gate.require("a", "First");
        gate.require("b", "Second");
        assert_eq!(gate.total_count(), 2);
        assert_eq!(gate.closed_count(), 0);
        assert!(!gate.all_closed());
        assert_eq!(gate.unclosed().len(), 2);

        gate.mark_closed("a");
        assert_eq!(gate.closed_count(), 1);
        assert!(!gate.all_closed());

        gate.mark_closed("b");
        assert!(gate.all_closed());
        assert!(gate.unclosed().is_empty());
    }

    #[test]
    fn test_regression_guard_suite() {
        let mut suite = RegressionGuardSuite::new();
        assert!(!suite.all_pass()); // Empty = not passing

        suite.record(RegressionGuard {
            guard_id: "g1".into(),
            description: "Test".into(),
            category: GuardCategory::CompileTime,
            passed: true,
            evidence: "ok".into(),
            command: "test".into(),
        });
        assert!(suite.all_pass());
        assert_eq!(suite.pass_count(), 1);

        suite.record(RegressionGuard {
            guard_id: "g2".into(),
            description: "Failing".into(),
            category: GuardCategory::Runtime,
            passed: false,
            evidence: "failed".into(),
            command: "test".into(),
        });
        assert!(!suite.all_pass());
        assert_eq!(suite.failing().len(), 1);
    }

    #[test]
    fn test_persistence_proof_deterministic_recovery() {
        let mut suite = PersistenceProofSuite::new();
        assert!(!suite.has_deterministic_recovery());

        suite.record(PersistenceProof {
            proof_id: "p1".into(),
            workflow: "session".into(),
            description: "Test".into(),
            verified: true,
            seed: Some(42),
            state_hash_before: Some("hash1".into()),
            state_hash_after: Some("hash1".into()),
            evidence: "Match".into(),
            command: "test".into(),
        });
        assert!(suite.has_deterministic_recovery());

        // Non-matching hashes
        suite.record(PersistenceProof {
            proof_id: "p2".into(),
            workflow: "recording".into(),
            description: "Test".into(),
            verified: true,
            seed: Some(43),
            state_hash_before: Some("a".into()),
            state_hash_after: Some("b".into()),
            evidence: "Different".into(),
            command: "test".into(),
        });
        // Still has deterministic recovery from p1.
        assert!(suite.has_deterministic_recovery());
    }

    #[test]
    fn test_test_gate_pass_rate() {
        let mut summary = TestGateSummary::new();
        assert_eq!(summary.pass_rate(), 0.0);

        summary.record_suite(TestSuiteResult {
            suite_name: "unit".into(),
            passed: 99,
            failed: 1,
            skipped: 0,
            duration_ms: 100,
            seed: None,
            command: "test".into(),
        });
        assert!((summary.pass_rate() - 0.99).abs() < 0.001);
        assert_eq!(summary.total_tests(), 100);
        assert_eq!(summary.total_failures(), 1);

        summary.record_suite(TestSuiteResult {
            suite_name: "integration".into(),
            passed: 100,
            failed: 0,
            skipped: 5,
            duration_ms: 200,
            seed: None,
            command: "test".into(),
        });
        // 199/200 = 0.995
        assert!(summary.pass_rate() > 0.99);
    }

    #[test]
    fn test_benchmark_comparison_latency() {
        let comp = BenchmarkComparison {
            name: "p99_latency".into(),
            metric: "latency".into(),
            before: 10.0,
            after: 10.5, // 5% regression
            unit: "ms".into(),
            lower_is_better: true,
        };
        assert!(comp.within_threshold(1.10)); // 10% threshold
        assert!(!comp.within_threshold(1.04)); // 4% threshold
        assert!((comp.ratio() - 1.05).abs() < 0.001);
    }

    #[test]
    fn test_benchmark_comparison_throughput() {
        let comp = BenchmarkComparison {
            name: "ingest".into(),
            metric: "throughput".into(),
            before: 10000.0,
            after: 9500.0, // 5% regression
            unit: "events/sec".into(),
            lower_is_better: false,
        };
        assert!(comp.within_threshold(1.10)); // 10% threshold
        assert!(!comp.within_threshold(1.04)); // 4% threshold
    }

    #[test]
    fn test_benchmark_zero_before() {
        let comp = BenchmarkComparison {
            name: "new".into(),
            metric: "latency".into(),
            before: 0.0,
            after: 5.0,
            unit: "ms".into(),
            lower_is_better: true,
        };
        assert_eq!(comp.ratio(), f64::INFINITY);
        assert!(!comp.within_threshold(1.10));

        let comp_zero = BenchmarkComparison {
            name: "zero".into(),
            metric: "latency".into(),
            before: 0.0,
            after: 0.0,
            unit: "ms".into(),
            lower_is_better: true,
        };
        assert_eq!(comp_zero.ratio(), 1.0);
    }

    #[test]
    fn test_incident_registry() {
        let mut reg = IncidentRegistry::new();
        assert_eq!(reg.unresolved_p1_count(), 0);

        reg.record(IncidentRecord {
            incident_id: "I-1".into(),
            priority: 1,
            title: "Open P1".into(),
            description: "".into(),
            status: IncidentStatus::Open,
            root_cause: None,
            remediation: None,
            reported_at_ms: 0,
            resolved_at_ms: None,
            related_beads: vec![],
        });
        assert_eq!(reg.unresolved_p1_count(), 1);
        assert_eq!(reg.unresolved_count(), 1);

        reg.record(IncidentRecord {
            incident_id: "I-2".into(),
            priority: 2,
            title: "Open P2".into(),
            description: "".into(),
            status: IncidentStatus::Investigating,
            root_cause: None,
            remediation: None,
            reported_at_ms: 0,
            resolved_at_ms: None,
            related_beads: vec![],
        });
        assert_eq!(reg.unresolved_p1_count(), 1); // Only P1
        assert_eq!(reg.unresolved_count(), 2);
    }

    #[test]
    fn test_risk_registry_critical_mitigation() {
        let mut reg = RiskRegistry::new();
        assert!(reg.all_critical_mitigated()); // No risks = trivially true

        reg.record(RiskRecord {
            risk_id: "R1".into(),
            severity: RiskSeverity::Critical,
            description: "Unmitigated".into(),
            mitigation: None,
            owner: None,
            follow_up: None,
            accepted: false,
            related_beads: vec![],
        });
        assert!(!reg.all_critical_mitigated());
        assert_eq!(reg.unmitigated_critical_count(), 1);

        // Adding mitigation resolves it.
        reg.risks[0].mitigation = Some("Fixed".into());
        assert!(reg.all_critical_mitigated());
        assert_eq!(reg.unmitigated_critical_count(), 0);
    }

    #[test]
    fn test_risk_severity_ordering() {
        let mut reg = RiskRegistry::new();
        reg.record(RiskRecord {
            risk_id: "low".into(),
            severity: RiskSeverity::Low,
            description: "Low".into(),
            mitigation: None,
            owner: None,
            follow_up: None,
            accepted: false,
            related_beads: vec![],
        });
        reg.record(RiskRecord {
            risk_id: "critical".into(),
            severity: RiskSeverity::Critical,
            description: "Critical".into(),
            mitigation: Some("mitigated".into()),
            owner: None,
            follow_up: None,
            accepted: false,
            related_beads: vec![],
        });
        reg.record(RiskRecord {
            risk_id: "medium".into(),
            severity: RiskSeverity::Medium,
            description: "Medium".into(),
            mitigation: None,
            owner: None,
            follow_up: None,
            accepted: false,
            related_beads: vec![],
        });

        let sorted = reg.by_severity();
        assert_eq!(sorted[0].risk_id, "critical");
        assert_eq!(sorted[1].risk_id, "medium");
        assert_eq!(sorted[2].risk_id, "low");
    }

    #[test]
    fn test_soak_outcome_recording() {
        let mut pkg = sample_package();
        assert_eq!(pkg.soak_outcomes.len(), 0);

        pkg.record_soak(SoakOutcome {
            period_id: "soak-1".into(),
            start_ms: 0,
            end_ms: 86400000,
            slo_conforming: true,
            error_rate: 0.001,
            p95_latency_ms: 12.5,
            incident_count: 0,
            rollback_triggered: false,
            notes: "Clean soak".into(),
        });

        assert_eq!(pkg.soak_outcomes.len(), 1);
        assert_eq!(pkg.telemetry.soak_outcomes_recorded, 1);
    }

    #[test]
    fn test_checklist_render_summary() {
        let mut checklist = GoNoGoChecklist::new();
        checklist.add_check(ChecklistItem {
            gate_id: "G-01".into(),
            description: "Prerequisites".into(),
            status: CheckStatus::Pass,
            detail: "2/2".into(),
            blocking: true,
        });
        checklist.add_check(ChecklistItem {
            gate_id: "G-02".into(),
            description: "Guards".into(),
            status: CheckStatus::Fail,
            detail: "0/1".into(),
            blocking: true,
        });

        let summary = checklist.render_summary();
        assert!(summary.contains("[PASS]"));
        assert!(summary.contains("[FAIL]"));
        assert!(summary.contains("(blocking)"));
    }

    #[test]
    fn test_verdict_render_report() {
        let pkg = sample_package();
        let verdict = pkg.evaluate();
        let report = verdict.render_report();
        assert!(report.contains("Go/No-Go Verdict"));
        assert!(report.contains("test-migration"));
        assert!(report.contains("Checklist"));
    }

    #[test]
    fn test_evidence_summary() {
        let pkg = sample_package();
        let summary = pkg.summary();
        assert_eq!(summary.decision, GoNoGoDecision::Go);
        assert_eq!(summary.prerequisites_closed, 2);
        assert_eq!(summary.test_suites, 1);
    }

    #[test]
    fn test_serde_roundtrip() {
        let pkg = sample_package();
        let json = serde_json::to_string(&pkg).expect("serialize");
        let restored: EvidencePackage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.migration_id, pkg.migration_id);
        assert_eq!(restored.prerequisites.total_count(), 2);
        assert_eq!(restored.test_gates.total_suites(), 1);
    }

    #[test]
    fn test_conditional_verdict_with_performance_warning() {
        let mut pkg = sample_package();
        // Add a benchmark regression that exceeds threshold.
        pkg.benchmarks.comparisons.clear();
        pkg.benchmarks.record(BenchmarkComparison {
            name: "slow_path".into(),
            metric: "latency".into(),
            before: 10.0,
            after: 15.0, // 50% regression
            unit: "ms".into(),
            lower_is_better: true,
        });

        let verdict = pkg.evaluate();
        // Performance gate is non-blocking → Conditional.
        assert_eq!(verdict.decision, GoNoGoDecision::Conditional);
        assert!(verdict.rationale.contains("warnings"));
    }

    #[test]
    fn test_multiple_blocking_failures() {
        let mut pkg = EvidencePackage::new("multi-fail", 1);
        // Prerequisites not met (empty → fails).
        // Guards not met (empty → fails).
        // Tests not met (empty → skipped).
        // Persistence not met (empty → skipped).
        // Add a critical unmitigated risk.
        pkg.risks.record(RiskRecord {
            risk_id: "R-CRIT".into(),
            severity: RiskSeverity::Critical,
            description: "Bad".into(),
            mitigation: None,
            owner: None,
            follow_up: None,
            accepted: false,
            related_beads: vec![],
        });

        let verdict = pkg.evaluate();
        assert_eq!(verdict.decision, GoNoGoDecision::NoGo);
        // Multiple blocking failures.
        assert!(verdict.checklist.blocking_failure_count() >= 2);
    }

    #[test]
    fn test_rollback_failed_is_non_blocking() {
        let mut pkg = sample_package();
        pkg.rollback_rehearsals.rehearsals.clear();
        pkg.rollback_rehearsals.record(RollbackRehearsal {
            rehearsal_id: "r-fail".into(),
            performed_at_ms: 100,
            successful: false,
            rollback_duration_ms: 60000,
            data_integrity_preserved: false,
            notes: "Failed".into(),
            command: "ft rollback".into(),
        });

        let verdict = pkg.evaluate();
        // Rollback is non-blocking, so should still be Go (not NoGo).
        assert_ne!(verdict.decision, GoNoGoDecision::NoGo);
    }

    #[test]
    fn test_false_positive_incident_does_not_block() {
        let mut pkg = sample_package();
        pkg.incidents.record(IncidentRecord {
            incident_id: "INC-FP".into(),
            priority: 1,
            title: "False positive P1".into(),
            description: "Not real".into(),
            status: IncidentStatus::FalsePositive,
            root_cause: Some("Test flake".into()),
            remediation: None,
            reported_at_ms: 100,
            resolved_at_ms: None,
            related_beads: vec![],
        });

        let verdict = pkg.evaluate();
        assert_eq!(verdict.decision, GoNoGoDecision::Go);
    }
}
