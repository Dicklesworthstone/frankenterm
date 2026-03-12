//! Post-cutover soak and user-journey confidence gate (ft-e34d9.10.8.5).
//!
//! Defines the soak matrix, user-journey scenarios, failure-injection soak
//! profiles, and confidence gate evaluation for final migration closure.
//!
//! # Architecture
//!
//! ```text
//! SoakMatrix
//!   ├── UserJourneyScenario (ft watch, robot loops, session, SSH, restart)
//!   │     ├── WorkloadProfile (steady, burst, mixed, degraded)
//!   │     └── FailureInjectionProfile (none, light, heavy, cascade)
//!   │
//!   ├── SoakExecutionPlan (matrix → executable plan)
//!   │     └── SoakCell (scenario × profile × injection)
//!   │
//!   └── SoakExecutionResult
//!         ├── CellResult (per-cell pass/fail + telemetry)
//!         └── SoakInvariantCheck (task leaks, deadlocks, message loss, latency)
//!
//! ConfidenceGate
//!   ├── evaluate(results) → ConfidenceVerdict
//!   └── to_evidence() → SoakOutcome (for cutover_evidence.rs)
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::cutover_evidence::SoakOutcome;

// =============================================================================
// User journey scenarios
// =============================================================================

/// Categorization of user-facing workflows to validate during soak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum JourneyCategory {
    /// `ft watch` — continuous pane monitoring and pattern detection.
    Watch,
    /// Robot orchestration loops — MCP-driven agent workflows.
    RobotOrchestration,
    /// Session persistence — session save/restore across restart.
    SessionPersistence,
    /// Remote SSH flows — mux client over SSH transport.
    RemoteSsh,
    /// Restart cycles — clean shutdown and recovery.
    RestartCycle,
    /// Mixed workload bursts — concurrent multi-category operations.
    MixedBurst,
    /// Search — semantic + lexical search under load.
    Search,
    /// Recording/replay — event recording and deterministic replay.
    RecordingReplay,
}

impl JourneyCategory {
    /// All defined journey categories.
    pub const ALL: &'static [JourneyCategory] = &[
        Self::Watch,
        Self::RobotOrchestration,
        Self::SessionPersistence,
        Self::RemoteSsh,
        Self::RestartCycle,
        Self::MixedBurst,
        Self::Search,
        Self::RecordingReplay,
    ];

    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Watch => "ft watch",
            Self::RobotOrchestration => "robot orchestration",
            Self::SessionPersistence => "session persistence",
            Self::RemoteSsh => "remote SSH",
            Self::RestartCycle => "restart cycle",
            Self::MixedBurst => "mixed burst",
            Self::Search => "search",
            Self::RecordingReplay => "recording/replay",
        }
    }

    /// Whether this journey is critical-path (failure blocks cutover).
    #[must_use]
    pub fn is_critical(&self) -> bool {
        matches!(
            self,
            Self::Watch
                | Self::RobotOrchestration
                | Self::SessionPersistence
                | Self::RestartCycle
        )
    }
}

/// A single user-journey scenario definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserJourneyScenario {
    /// Scenario identifier.
    pub scenario_id: String,
    /// Which journey category this tests.
    pub category: JourneyCategory,
    /// Human-readable description.
    pub description: String,
    /// Expected duration for a single run (ms).
    pub expected_duration_ms: u64,
    /// Whether failure of this scenario blocks cutover.
    pub blocking: bool,
    /// Deterministic seed for reproducibility.
    pub seed: Option<u64>,
    /// Command to execute this scenario.
    pub command: String,
}

// =============================================================================
// Workload and failure injection profiles
// =============================================================================

/// Workload intensity profile for soak scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WorkloadProfile {
    /// Steady-state: constant low-moderate load.
    Steady,
    /// Burst: periodic high-load spikes.
    Burst,
    /// Mixed: varying concurrent workloads.
    Mixed,
    /// Degraded: running under resource pressure.
    Degraded,
}

impl WorkloadProfile {
    /// All defined workload profiles.
    pub const ALL: &'static [WorkloadProfile] = &[
        Self::Steady,
        Self::Burst,
        Self::Mixed,
        Self::Degraded,
    ];
}

/// Failure injection intensity for soak scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FailureInjectionProfile {
    /// No failure injection — baseline correctness.
    None,
    /// Light: occasional transient faults.
    Light,
    /// Heavy: frequent faults across multiple points.
    Heavy,
    /// Cascade: simultaneous multi-point failures.
    Cascade,
}

impl FailureInjectionProfile {
    /// All defined injection profiles.
    pub const ALL: &'static [FailureInjectionProfile] = &[
        Self::None,
        Self::Light,
        Self::Heavy,
        Self::Cascade,
    ];
}

// =============================================================================
// Soak matrix
// =============================================================================

/// The full soak matrix: scenarios × workload profiles × injection profiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakMatrix {
    /// Registered scenarios.
    pub scenarios: Vec<UserJourneyScenario>,
    /// Which workload profiles to test.
    pub workload_profiles: Vec<WorkloadProfile>,
    /// Which injection profiles to test.
    pub injection_profiles: Vec<FailureInjectionProfile>,
}

impl SoakMatrix {
    /// Create a default soak matrix with all standard scenarios and profiles.
    #[must_use]
    pub fn standard() -> Self {
        let scenarios: Vec<UserJourneyScenario> = JourneyCategory::ALL
            .iter()
            .enumerate()
            .map(|(i, cat)| UserJourneyScenario {
                scenario_id: format!("soak-{}", cat.label().replace(' ', "-")),
                category: *cat,
                description: format!("Standard soak scenario for {}", cat.label()),
                expected_duration_ms: 60_000,
                blocking: cat.is_critical(),
                seed: Some(42 + i as u64),
                command: format!("cargo test --test soak_{}", cat.label().replace(' ', "_")),
            })
            .collect();

        Self {
            scenarios,
            workload_profiles: WorkloadProfile::ALL.to_vec(),
            injection_profiles: FailureInjectionProfile::ALL.to_vec(),
        }
    }

    /// Create a minimal matrix for CI (fewer profiles, faster execution).
    #[must_use]
    pub fn ci_minimal() -> Self {
        let scenarios: Vec<UserJourneyScenario> = JourneyCategory::ALL
            .iter()
            .filter(|c| c.is_critical())
            .enumerate()
            .map(|(i, cat)| UserJourneyScenario {
                scenario_id: format!("ci-soak-{}", cat.label().replace(' ', "-")),
                category: *cat,
                description: format!("CI soak for {}", cat.label()),
                expected_duration_ms: 10_000,
                blocking: true,
                seed: Some(100 + i as u64),
                command: format!(
                    "cargo test --test soak_{} -- --ci",
                    cat.label().replace(' ', "_")
                ),
            })
            .collect();

        Self {
            scenarios,
            workload_profiles: vec![WorkloadProfile::Steady, WorkloadProfile::Burst],
            injection_profiles: vec![
                FailureInjectionProfile::None,
                FailureInjectionProfile::Light,
            ],
        }
    }

    /// Custom matrix builder.
    #[must_use]
    pub fn custom(
        scenarios: Vec<UserJourneyScenario>,
        workload_profiles: Vec<WorkloadProfile>,
        injection_profiles: Vec<FailureInjectionProfile>,
    ) -> Self {
        Self {
            scenarios,
            workload_profiles,
            injection_profiles,
        }
    }

    /// Total number of cells in the matrix (scenarios × workloads × injections).
    #[must_use]
    pub fn cell_count(&self) -> usize {
        self.scenarios.len()
            * self.workload_profiles.len()
            * self.injection_profiles.len()
    }

    /// Generate the execution plan from this matrix.
    #[must_use]
    pub fn to_plan(&self) -> SoakExecutionPlan {
        let mut cells = Vec::with_capacity(self.cell_count());
        for scenario in &self.scenarios {
            for workload in &self.workload_profiles {
                for injection in &self.injection_profiles {
                    cells.push(SoakCell {
                        cell_id: format!(
                            "{}/{:?}/{:?}",
                            scenario.scenario_id, workload, injection
                        ),
                        scenario_id: scenario.scenario_id.clone(),
                        category: scenario.category,
                        workload: *workload,
                        injection: *injection,
                        blocking: scenario.blocking,
                        seed: scenario.seed,
                    });
                }
            }
        }

        SoakExecutionPlan { cells }
    }

    /// Number of blocking scenarios.
    #[must_use]
    pub fn blocking_scenario_count(&self) -> usize {
        self.scenarios.iter().filter(|s| s.blocking).count()
    }
}

/// An executable soak plan derived from the matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakExecutionPlan {
    /// Individual cells to execute.
    pub cells: Vec<SoakCell>,
}

/// A single cell in the soak matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakCell {
    /// Unique cell identifier.
    pub cell_id: String,
    /// Which scenario this cell runs.
    pub scenario_id: String,
    /// Journey category.
    pub category: JourneyCategory,
    /// Workload profile for this cell.
    pub workload: WorkloadProfile,
    /// Failure injection profile.
    pub injection: FailureInjectionProfile,
    /// Whether this cell is blocking.
    pub blocking: bool,
    /// Deterministic seed.
    pub seed: Option<u64>,
}

impl SoakExecutionPlan {
    /// Total cells.
    #[must_use]
    pub fn total_cells(&self) -> usize {
        self.cells.len()
    }

    /// Blocking cells only.
    #[must_use]
    pub fn blocking_cells(&self) -> Vec<&SoakCell> {
        self.cells.iter().filter(|c| c.blocking).collect()
    }
}

// =============================================================================
// Soak execution results
// =============================================================================

/// Complete results from executing a soak matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakExecutionResult {
    /// Per-cell results.
    pub cell_results: Vec<CellResult>,
    /// Soak-wide invariant checks.
    pub invariant_checks: Vec<SoakInvariantCheck>,
    /// Total soak duration (ms).
    pub total_duration_ms: u64,
    /// When this soak was started (epoch ms).
    pub started_at_ms: u64,
    /// When this soak completed (epoch ms).
    pub completed_at_ms: u64,
}

/// Result from executing a single soak cell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellResult {
    /// Which cell this result is for.
    pub cell_id: String,
    /// Journey category.
    pub category: JourneyCategory,
    /// Workload profile.
    pub workload: WorkloadProfile,
    /// Injection profile.
    pub injection: FailureInjectionProfile,
    /// Whether this cell passed.
    pub passed: bool,
    /// Whether this cell was blocking.
    pub blocking: bool,
    /// Execution duration (ms).
    pub duration_ms: u64,
    /// Failure reason (if failed).
    pub failure_reason: Option<String>,
    /// Error rate during execution.
    pub error_rate: f64,
    /// P95 latency during execution (ms).
    pub p95_latency_ms: f64,
    /// Seed used.
    pub seed: Option<u64>,
    /// Structured telemetry from this cell.
    pub telemetry: CellTelemetry,
}

/// Telemetry captured during a single soak cell execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CellTelemetry {
    /// Total operations attempted.
    pub ops_attempted: u64,
    /// Operations that succeeded.
    pub ops_succeeded: u64,
    /// Operations that failed.
    pub ops_failed: u64,
    /// Tasks spawned.
    pub tasks_spawned: u64,
    /// Tasks completed normally.
    pub tasks_completed: u64,
    /// Tasks cancelled.
    pub tasks_cancelled: u64,
    /// Faults injected.
    pub faults_injected: u64,
    /// Recovery events.
    pub recoveries: u64,
}

/// A soak-wide invariant check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoakInvariantCheck {
    /// Invariant identifier.
    pub invariant_id: String,
    /// Human-readable description.
    pub description: String,
    /// Whether the invariant held.
    pub passed: bool,
    /// Evidence for the result.
    pub evidence: String,
    /// Whether this invariant is mandatory.
    pub mandatory: bool,
}

impl SoakExecutionResult {
    /// Create a new empty result.
    #[must_use]
    pub fn new(started_at_ms: u64) -> Self {
        Self {
            cell_results: Vec::new(),
            invariant_checks: Vec::new(),
            total_duration_ms: 0,
            started_at_ms,
            completed_at_ms: 0,
        }
    }

    /// Record a cell result.
    pub fn record_cell(&mut self, result: CellResult) {
        self.cell_results.push(result);
    }

    /// Record an invariant check.
    pub fn record_invariant(&mut self, check: SoakInvariantCheck) {
        self.invariant_checks.push(check);
    }

    /// Mark the soak as completed.
    pub fn complete(&mut self, completed_at_ms: u64) {
        self.completed_at_ms = completed_at_ms;
        self.total_duration_ms = completed_at_ms.saturating_sub(self.started_at_ms);
    }

    /// Count of passing cells.
    #[must_use]
    pub fn cells_passed(&self) -> usize {
        self.cell_results.iter().filter(|c| c.passed).count()
    }

    /// Count of failing cells.
    #[must_use]
    pub fn cells_failed(&self) -> usize {
        self.cell_results.iter().filter(|c| !c.passed).count()
    }

    /// Count of blocking cells that failed.
    #[must_use]
    pub fn blocking_failures(&self) -> usize {
        self.cell_results
            .iter()
            .filter(|c| c.blocking && !c.passed)
            .count()
    }

    /// Count of mandatory invariants that failed.
    #[must_use]
    pub fn mandatory_invariant_failures(&self) -> usize {
        self.invariant_checks
            .iter()
            .filter(|c| c.mandatory && !c.passed)
            .count()
    }

    /// Overall pass rate.
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        if self.cell_results.is_empty() {
            return 0.0;
        }
        self.cells_passed() as f64 / self.cell_results.len() as f64
    }

    /// Aggregate error rate across all cells.
    #[must_use]
    pub fn aggregate_error_rate(&self) -> f64 {
        if self.cell_results.is_empty() {
            return 0.0;
        }
        let total: f64 = self.cell_results.iter().map(|c| c.error_rate).sum();
        total / self.cell_results.len() as f64
    }

    /// Results grouped by journey category.
    #[must_use]
    pub fn by_category(&self) -> BTreeMap<JourneyCategory, Vec<&CellResult>> {
        let mut map: BTreeMap<JourneyCategory, Vec<&CellResult>> = BTreeMap::new();
        for result in &self.cell_results {
            map.entry(result.category).or_default().push(result);
        }
        map
    }

    /// Standard soak invariants to check after execution.
    #[must_use]
    pub fn standard_invariants(telemetry: &AggregatedSoakTelemetry) -> Vec<SoakInvariantCheck> {
        vec![
            SoakInvariantCheck {
                invariant_id: "SOAK-INV-01".into(),
                description: "No task leaks — all spawned tasks completed or cancelled".into(),
                passed: telemetry.tasks_spawned
                    == telemetry.tasks_completed + telemetry.tasks_cancelled,
                evidence: format!(
                    "spawned={}, completed={}, cancelled={}",
                    telemetry.tasks_spawned,
                    telemetry.tasks_completed,
                    telemetry.tasks_cancelled
                ),
                mandatory: true,
            },
            SoakInvariantCheck {
                invariant_id: "SOAK-INV-02".into(),
                description: "No deadlocks — all cells completed within timeout".into(),
                passed: telemetry.deadlock_detected_count == 0,
                evidence: format!(
                    "deadlocks_detected={}",
                    telemetry.deadlock_detected_count
                ),
                mandatory: true,
            },
            SoakInvariantCheck {
                invariant_id: "SOAK-INV-03".into(),
                description: "No message loss — ops_attempted == ops_succeeded + ops_failed".into(),
                passed: telemetry.ops_attempted
                    == telemetry.ops_succeeded + telemetry.ops_failed,
                evidence: format!(
                    "attempted={}, succeeded={}, failed={}",
                    telemetry.ops_attempted,
                    telemetry.ops_succeeded,
                    telemetry.ops_failed
                ),
                mandatory: true,
            },
            SoakInvariantCheck {
                invariant_id: "SOAK-INV-04".into(),
                description: "No unbounded latency — p95 < 5000ms across all cells".into(),
                passed: telemetry.max_p95_latency_ms < 5000.0,
                evidence: format!(
                    "max_p95_latency_ms={:.1}",
                    telemetry.max_p95_latency_ms
                ),
                mandatory: true,
            },
            SoakInvariantCheck {
                invariant_id: "SOAK-INV-05".into(),
                description: "Recovery completeness — all fault-injected cells attempted recovery"
                    .into(),
                passed: telemetry.faults_injected == 0
                    || telemetry.recoveries > 0,
                evidence: format!(
                    "faults_injected={}, recoveries={}",
                    telemetry.faults_injected, telemetry.recoveries
                ),
                mandatory: false,
            },
        ]
    }
}

/// Aggregated telemetry across all soak cells.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AggregatedSoakTelemetry {
    pub ops_attempted: u64,
    pub ops_succeeded: u64,
    pub ops_failed: u64,
    pub tasks_spawned: u64,
    pub tasks_completed: u64,
    pub tasks_cancelled: u64,
    pub faults_injected: u64,
    pub recoveries: u64,
    pub deadlock_detected_count: u64,
    pub max_p95_latency_ms: f64,
}

impl AggregatedSoakTelemetry {
    /// Aggregate from individual cell results.
    #[must_use]
    pub fn from_cells(cells: &[CellResult]) -> Self {
        let mut agg = Self::default();
        for cell in cells {
            agg.ops_attempted += cell.telemetry.ops_attempted;
            agg.ops_succeeded += cell.telemetry.ops_succeeded;
            agg.ops_failed += cell.telemetry.ops_failed;
            agg.tasks_spawned += cell.telemetry.tasks_spawned;
            agg.tasks_completed += cell.telemetry.tasks_completed;
            agg.tasks_cancelled += cell.telemetry.tasks_cancelled;
            agg.faults_injected += cell.telemetry.faults_injected;
            agg.recoveries += cell.telemetry.recoveries;
            if cell.p95_latency_ms > agg.max_p95_latency_ms {
                agg.max_p95_latency_ms = cell.p95_latency_ms;
            }
        }
        agg
    }
}

// =============================================================================
// Confidence gate
// =============================================================================

/// Confidence gate that evaluates soak results for cutover readiness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceGate {
    /// Minimum pass rate across all cells (0.0–1.0).
    pub min_pass_rate: f64,
    /// Maximum allowed aggregate error rate.
    pub max_error_rate: f64,
    /// Maximum allowed p95 latency (ms).
    pub max_p95_latency_ms: f64,
    /// Whether blocking cell failures are hard stops.
    pub blocking_failures_are_hard_stop: bool,
    /// Whether mandatory invariant failures are hard stops.
    pub mandatory_invariants_are_hard_stop: bool,
}

impl ConfidenceGate {
    /// Standard confidence gate with production-grade thresholds.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            min_pass_rate: 0.95,
            max_error_rate: 0.05,
            max_p95_latency_ms: 5000.0,
            blocking_failures_are_hard_stop: true,
            mandatory_invariants_are_hard_stop: true,
        }
    }

    /// Strict confidence gate (100% pass rate required).
    #[must_use]
    pub fn strict() -> Self {
        Self {
            min_pass_rate: 1.0,
            max_error_rate: 0.01,
            max_p95_latency_ms: 2000.0,
            blocking_failures_are_hard_stop: true,
            mandatory_invariants_are_hard_stop: true,
        }
    }

    /// Evaluate soak results against this gate.
    #[must_use]
    pub fn evaluate(&self, results: &SoakExecutionResult) -> ConfidenceVerdict {
        let mut checks = Vec::new();

        // Check 1: Pass rate.
        let pass_rate = results.pass_rate();
        checks.push(GateCondition {
            condition_id: "CONF-01-pass-rate".into(),
            description: format!(
                "Pass rate >= {:.0}%",
                self.min_pass_rate * 100.0
            ),
            passed: pass_rate >= self.min_pass_rate,
            measured: format!("{:.1}%", pass_rate * 100.0),
            blocking: true,
        });

        // Check 2: Blocking cell failures.
        let blocking_fails = results.blocking_failures();
        checks.push(GateCondition {
            condition_id: "CONF-02-blocking-cells".into(),
            description: "No blocking cell failures".into(),
            passed: blocking_fails == 0,
            measured: format!("{blocking_fails} blocking failures"),
            blocking: self.blocking_failures_are_hard_stop,
        });

        // Check 3: Mandatory invariants.
        let inv_fails = results.mandatory_invariant_failures();
        checks.push(GateCondition {
            condition_id: "CONF-03-invariants".into(),
            description: "All mandatory invariants hold".into(),
            passed: inv_fails == 0,
            measured: format!("{inv_fails} mandatory invariant failures"),
            blocking: self.mandatory_invariants_are_hard_stop,
        });

        // Check 4: Error rate.
        let error_rate = results.aggregate_error_rate();
        checks.push(GateCondition {
            condition_id: "CONF-04-error-rate".into(),
            description: format!(
                "Error rate <= {:.1}%",
                self.max_error_rate * 100.0
            ),
            passed: error_rate <= self.max_error_rate,
            measured: format!("{:.2}%", error_rate * 100.0),
            blocking: false,
        });

        // Check 5: Latency.
        let max_latency = results
            .cell_results
            .iter()
            .map(|c| c.p95_latency_ms)
            .fold(0.0_f64, f64::max);
        checks.push(GateCondition {
            condition_id: "CONF-05-latency".into(),
            description: format!(
                "Max p95 latency <= {:.0}ms",
                self.max_p95_latency_ms
            ),
            passed: max_latency <= self.max_p95_latency_ms,
            measured: format!("{max_latency:.1}ms"),
            blocking: false,
        });

        // Determine verdict.
        let blocking_check_failures = checks
            .iter()
            .filter(|c| c.blocking && !c.passed)
            .count();
        let non_blocking_failures = checks.iter().filter(|c| !c.blocking && !c.passed).count();

        let decision = if blocking_check_failures > 0 {
            ConfidenceDecision::NotConfident
        } else if non_blocking_failures > 0 {
            ConfidenceDecision::ConditionallyConfident
        } else {
            ConfidenceDecision::Confident
        };

        ConfidenceVerdict {
            decision,
            checks,
            cells_total: results.cell_results.len(),
            cells_passed: results.cells_passed(),
            cells_failed: results.cells_failed(),
            soak_duration_ms: results.total_duration_ms,
        }
    }

    /// Convert soak results to a SoakOutcome for the cutover evidence package.
    #[must_use]
    pub fn to_evidence(
        &self,
        results: &SoakExecutionResult,
        period_id: impl Into<String>,
    ) -> SoakOutcome {
        let verdict = self.evaluate(results);
        SoakOutcome {
            period_id: period_id.into(),
            start_ms: results.started_at_ms,
            end_ms: results.completed_at_ms,
            slo_conforming: verdict.decision != ConfidenceDecision::NotConfident,
            error_rate: results.aggregate_error_rate(),
            p95_latency_ms: results
                .cell_results
                .iter()
                .map(|c| c.p95_latency_ms)
                .fold(0.0_f64, f64::max),
            incident_count: results.cells_failed() as u32,
            rollback_triggered: false,
            notes: format!(
                "Soak verdict: {:?}. {}/{} cells passed.",
                verdict.decision, verdict.cells_passed, verdict.cells_total
            ),
        }
    }
}

/// Confidence verdict from gate evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceVerdict {
    /// The confidence decision.
    pub decision: ConfidenceDecision,
    /// Individual gate condition results.
    pub checks: Vec<GateCondition>,
    /// Total cells in the soak.
    pub cells_total: usize,
    /// Cells that passed.
    pub cells_passed: usize,
    /// Cells that failed.
    pub cells_failed: usize,
    /// Total soak duration.
    pub soak_duration_ms: u64,
}

/// Confidence decision outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfidenceDecision {
    /// All gates pass — high confidence in cutover.
    Confident,
    /// No blocking failures, but some non-blocking concerns.
    ConditionallyConfident,
    /// Blocking failures — not confident, cutover blocked.
    NotConfident,
}

/// A single gate condition check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateCondition {
    /// Condition identifier.
    pub condition_id: String,
    /// Description of what's being checked.
    pub description: String,
    /// Whether this condition passed.
    pub passed: bool,
    /// What was measured.
    pub measured: String,
    /// Whether failure blocks cutover.
    pub blocking: bool,
}

impl ConfidenceVerdict {
    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "=== Confidence Verdict: {:?} ===",
            self.decision
        ));
        lines.push(format!(
            "Cells: {}/{} passed ({:.1}%)",
            self.cells_passed,
            self.cells_total,
            if self.cells_total > 0 {
                self.cells_passed as f64 / self.cells_total as f64 * 100.0
            } else {
                0.0
            }
        ));
        lines.push(format!("Duration: {}ms", self.soak_duration_ms));
        lines.push(String::new());
        for check in &self.checks {
            let icon = if check.passed { "[PASS]" } else { "[FAIL]" };
            let blocking = if check.blocking { " (blocking)" } else { "" };
            lines.push(format!(
                "{} {} — {}{} [{}]",
                icon, check.condition_id, check.description, blocking, check.measured
            ));
        }
        lines.join("\n")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn passing_cell(id: &str, cat: JourneyCategory, blocking: bool) -> CellResult {
        CellResult {
            cell_id: id.into(),
            category: cat,
            workload: WorkloadProfile::Steady,
            injection: FailureInjectionProfile::None,
            passed: true,
            blocking,
            duration_ms: 1000,
            failure_reason: None,
            error_rate: 0.001,
            p95_latency_ms: 50.0,
            seed: Some(42),
            telemetry: CellTelemetry {
                ops_attempted: 100,
                ops_succeeded: 99,
                ops_failed: 1,
                tasks_spawned: 10,
                tasks_completed: 10,
                tasks_cancelled: 0,
                faults_injected: 0,
                recoveries: 0,
            },
        }
    }

    fn failing_cell(id: &str, cat: JourneyCategory, blocking: bool) -> CellResult {
        CellResult {
            cell_id: id.into(),
            category: cat,
            workload: WorkloadProfile::Burst,
            injection: FailureInjectionProfile::Heavy,
            passed: false,
            blocking,
            duration_ms: 5000,
            failure_reason: Some("timeout exceeded".into()),
            error_rate: 0.15,
            p95_latency_ms: 3000.0,
            seed: Some(43),
            telemetry: CellTelemetry {
                ops_attempted: 100,
                ops_succeeded: 85,
                ops_failed: 15,
                tasks_spawned: 10,
                tasks_completed: 8,
                tasks_cancelled: 2,
                faults_injected: 20,
                recoveries: 5,
            },
        }
    }

    fn sample_results(cells: Vec<CellResult>) -> SoakExecutionResult {
        let mut result = SoakExecutionResult::new(0);
        for cell in cells {
            result.record_cell(cell);
        }

        let agg = AggregatedSoakTelemetry::from_cells(&result.cell_results);
        for inv in SoakExecutionResult::standard_invariants(&agg) {
            result.record_invariant(inv);
        }
        result.complete(10000);
        result
    }

    #[test]
    fn test_confident_verdict() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, true),
            passing_cell("c2", JourneyCategory::RobotOrchestration, true),
            passing_cell("c3", JourneyCategory::Search, false),
        ]);

        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&results);
        assert_eq!(verdict.decision, ConfidenceDecision::Confident);
        assert_eq!(verdict.cells_passed, 3);
        assert_eq!(verdict.cells_failed, 0);
    }

    #[test]
    fn test_not_confident_blocking_failure() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, true),
            failing_cell("c2", JourneyCategory::RobotOrchestration, true), // blocking fail
        ]);

        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&results);
        assert_eq!(verdict.decision, ConfidenceDecision::NotConfident);
    }

    #[test]
    fn test_conditionally_confident_non_blocking_failure() {
        // Create results with high error rate on non-blocking cell.
        let mut custom = SoakExecutionResult::new(0);
        for c in [
            passing_cell("c1", JourneyCategory::Watch, true),
            passing_cell("c2", JourneyCategory::RobotOrchestration, true),
        ] {
            custom.record_cell(c);
        }
        // Add a cell with high error rate.
        let mut high_err = passing_cell("c3", JourneyCategory::Search, false);
        high_err.error_rate = 0.20; // 20% error rate
        custom.record_cell(high_err);
        custom.complete(10000);

        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&custom);
        // Error rate gate is non-blocking, so should be ConditionallyConfident.
        assert_eq!(verdict.decision, ConfidenceDecision::ConditionallyConfident);
    }

    #[test]
    fn test_pass_rate_below_threshold() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, false),
            failing_cell("c2", JourneyCategory::Search, false),
            failing_cell("c3", JourneyCategory::RecordingReplay, false),
        ]);

        let gate = ConfidenceGate::standard(); // min 95% pass rate
        let verdict = gate.evaluate(&results);
        // 1/3 = 33% pass rate < 95%
        assert_eq!(verdict.decision, ConfidenceDecision::NotConfident);
    }

    #[test]
    fn test_standard_invariants_pass() {
        let telemetry = AggregatedSoakTelemetry {
            ops_attempted: 100,
            ops_succeeded: 95,
            ops_failed: 5,
            tasks_spawned: 20,
            tasks_completed: 18,
            tasks_cancelled: 2,
            faults_injected: 10,
            recoveries: 5,
            deadlock_detected_count: 0,
            max_p95_latency_ms: 100.0,
        };

        let invariants = SoakExecutionResult::standard_invariants(&telemetry);
        assert_eq!(invariants.len(), 5);
        // All should pass.
        for inv in &invariants {
            assert!(inv.passed, "Invariant {} failed: {}", inv.invariant_id, inv.evidence);
        }
    }

    #[test]
    fn test_task_leak_invariant_fails() {
        let telemetry = AggregatedSoakTelemetry {
            tasks_spawned: 20,
            tasks_completed: 15,
            tasks_cancelled: 3, // 2 leaked
            ..Default::default()
        };

        let invariants = SoakExecutionResult::standard_invariants(&telemetry);
        let task_leak = invariants
            .iter()
            .find(|i| i.invariant_id == "SOAK-INV-01")
            .unwrap();
        assert!(!task_leak.passed);
        assert!(task_leak.mandatory);
    }

    #[test]
    fn test_deadlock_invariant_fails() {
        let telemetry = AggregatedSoakTelemetry {
            deadlock_detected_count: 1,
            ..Default::default()
        };

        let invariants = SoakExecutionResult::standard_invariants(&telemetry);
        let deadlock = invariants
            .iter()
            .find(|i| i.invariant_id == "SOAK-INV-02")
            .unwrap();
        assert!(!deadlock.passed);
    }

    #[test]
    fn test_message_loss_invariant_fails() {
        let telemetry = AggregatedSoakTelemetry {
            ops_attempted: 100,
            ops_succeeded: 90,
            ops_failed: 5, // 5 lost
            ..Default::default()
        };

        let invariants = SoakExecutionResult::standard_invariants(&telemetry);
        let msg_loss = invariants
            .iter()
            .find(|i| i.invariant_id == "SOAK-INV-03")
            .unwrap();
        assert!(!msg_loss.passed);
    }

    #[test]
    fn test_latency_invariant_fails() {
        let telemetry = AggregatedSoakTelemetry {
            max_p95_latency_ms: 10000.0,
            ..Default::default()
        };

        let invariants = SoakExecutionResult::standard_invariants(&telemetry);
        let latency = invariants
            .iter()
            .find(|i| i.invariant_id == "SOAK-INV-04")
            .unwrap();
        assert!(!latency.passed);
    }

    #[test]
    fn test_soak_matrix_standard() {
        let matrix = SoakMatrix::standard();
        assert_eq!(matrix.scenarios.len(), 8); // 8 journey categories
        assert_eq!(matrix.workload_profiles.len(), 4);
        assert_eq!(matrix.injection_profiles.len(), 4);
        assert_eq!(matrix.cell_count(), 8 * 4 * 4); // 128
    }

    #[test]
    fn test_soak_matrix_ci_minimal() {
        let matrix = SoakMatrix::ci_minimal();
        // Only critical categories.
        assert_eq!(matrix.scenarios.len(), 4);
        assert_eq!(matrix.workload_profiles.len(), 2);
        assert_eq!(matrix.injection_profiles.len(), 2);
        assert_eq!(matrix.cell_count(), 4 * 2 * 2); // 16
    }

    #[test]
    fn test_execution_plan_generation() {
        let matrix = SoakMatrix::ci_minimal();
        let plan = matrix.to_plan();
        assert_eq!(plan.total_cells(), 16);
        assert!(!plan.blocking_cells().is_empty());
    }

    #[test]
    fn test_by_category_grouping() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, true),
            passing_cell("c2", JourneyCategory::Watch, true),
            passing_cell("c3", JourneyCategory::Search, false),
        ]);

        let by_cat = results.by_category();
        assert_eq!(by_cat.get(&JourneyCategory::Watch).unwrap().len(), 2);
        assert_eq!(by_cat.get(&JourneyCategory::Search).unwrap().len(), 1);
    }

    #[test]
    fn test_confidence_to_evidence() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, true),
            passing_cell("c2", JourneyCategory::Search, false),
        ]);

        let gate = ConfidenceGate::standard();
        let evidence = gate.to_evidence(&results, "soak-period-1");

        assert_eq!(evidence.period_id, "soak-period-1");
        assert!(evidence.slo_conforming);
        assert_eq!(evidence.incident_count, 0);
        assert!(!evidence.rollback_triggered);
    }

    #[test]
    fn test_verdict_render_summary() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, true),
        ]);
        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&results);
        let summary = verdict.render_summary();
        assert!(summary.contains("Confident"));
        assert!(summary.contains("CONF-01"));
    }

    #[test]
    fn test_strict_gate_rejects_any_failure() {
        let mut results = SoakExecutionResult::new(0);
        results.record_cell(passing_cell("c1", JourneyCategory::Watch, true));
        results.record_cell(failing_cell("c2", JourneyCategory::Search, false));
        results.complete(10000);

        let gate = ConfidenceGate::strict(); // 100% pass rate required
        let verdict = gate.evaluate(&results);
        assert_eq!(verdict.decision, ConfidenceDecision::NotConfident);
    }

    #[test]
    fn test_aggregated_telemetry() {
        let cells = vec![
            passing_cell("c1", JourneyCategory::Watch, true),
            failing_cell("c2", JourneyCategory::Search, false),
        ];

        let agg = AggregatedSoakTelemetry::from_cells(&cells);
        assert_eq!(agg.ops_attempted, 200); // 100 + 100
        assert_eq!(agg.ops_succeeded, 184); // 99 + 85
        assert_eq!(agg.tasks_spawned, 20); // 10 + 10
        assert_eq!(agg.faults_injected, 20); // 0 + 20
        assert!((agg.max_p95_latency_ms - 3000.0).abs() < 0.1);
    }

    #[test]
    fn test_journey_category_properties() {
        assert!(JourneyCategory::Watch.is_critical());
        assert!(JourneyCategory::RobotOrchestration.is_critical());
        assert!(JourneyCategory::SessionPersistence.is_critical());
        assert!(JourneyCategory::RestartCycle.is_critical());
        assert!(!JourneyCategory::Search.is_critical());
        assert!(!JourneyCategory::MixedBurst.is_critical());
        assert_eq!(JourneyCategory::ALL.len(), 8);
    }

    #[test]
    fn test_empty_results_not_confident() {
        let results = SoakExecutionResult::new(0);
        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&results);
        // 0% pass rate < 95% threshold.
        assert_eq!(verdict.decision, ConfidenceDecision::NotConfident);
    }

    #[test]
    fn test_blocking_scenario_count() {
        let matrix = SoakMatrix::standard();
        // 4 critical categories.
        assert_eq!(matrix.blocking_scenario_count(), 4);
    }

    #[test]
    fn test_serde_roundtrip() {
        let results = sample_results(vec![
            passing_cell("c1", JourneyCategory::Watch, true),
        ]);
        let json = serde_json::to_string(&results).expect("serialize");
        let restored: SoakExecutionResult =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.cell_results.len(), 1);
        assert_eq!(restored.invariant_checks.len(), 5);
    }

    #[test]
    fn test_custom_matrix() {
        let scenarios = vec![UserJourneyScenario {
            scenario_id: "custom-1".into(),
            category: JourneyCategory::Watch,
            description: "Custom".into(),
            expected_duration_ms: 5000,
            blocking: true,
            seed: Some(1),
            command: "test".into(),
        }];

        let matrix = SoakMatrix::custom(
            scenarios,
            vec![WorkloadProfile::Steady],
            vec![FailureInjectionProfile::None],
        );
        assert_eq!(matrix.cell_count(), 1);
        let plan = matrix.to_plan();
        assert_eq!(plan.total_cells(), 1);
    }

    #[test]
    fn test_mandatory_invariant_failure_blocks() {
        let mut results = SoakExecutionResult::new(0);
        results.record_cell(passing_cell("c1", JourneyCategory::Watch, true));
        results.record_invariant(SoakInvariantCheck {
            invariant_id: "SOAK-INV-01".into(),
            description: "Task leaks".into(),
            passed: false,
            evidence: "2 tasks leaked".into(),
            mandatory: true,
        });
        results.complete(10000);

        let gate = ConfidenceGate::standard();
        let verdict = gate.evaluate(&results);
        assert_eq!(verdict.decision, ConfidenceDecision::NotConfident);
    }
}
