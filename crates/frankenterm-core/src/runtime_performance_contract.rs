//! User-facing latency/throughput contract and regression gates (ft-e34d9.10.2.4).
//!
//! Defines measurable performance SLOs for every user-facing operation in the
//! asupersync runtime migration.  Each operation declares p50/p95/p99 latency
//! targets plus throughput minimums.  Regression gates evaluate measured
//! benchmarks against contracts and produce a structured pass/fail verdict.
//!
//! # Architecture
//!
//! ```text
//! OperationContract (per-operation SLO)
//!   ├── latency_target: QuantileBudgetMs
//!   ├── throughput_min_ops_sec: f64
//!   └── startup_max_ms: Option<u64>
//!
//! RuntimePerformanceContract
//!   └── operations: Vec<OperationContract>
//!       ├── standard_cli_contract()
//!       ├── standard_robot_contract()
//!       └── standard_watch_contract()
//!
//! OperationBenchmark (measured result)
//!   ├── latency: QuantileBudgetMs
//!   ├── throughput_ops_sec: f64
//!   └── samples: u64
//!
//! RegressionGate
//!   ├── evaluate(contract, benchmark) → GateResult
//!   └── evaluate_all(contract, benchmarks) → RegressionReport
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::cutover_evidence::BenchmarkComparison;
use crate::latency_model::QuantileBudgetMs;

// =============================================================================
// Operation categories
// =============================================================================

/// Categories of user-facing operations covered by the performance contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OperationCategory {
    /// CLI commands (ft status, ft list, ft show).
    Cli,
    /// Robot-mode operations (get-text, send, wait-for, state).
    Robot,
    /// Watch flow (startup, steady-state capture loop).
    Watch,
    /// Search operations (lexical, semantic, hybrid).
    Search,
    /// Event operations (subscribe, poll, filter).
    Events,
    /// Session operations (save, restore, list).
    Session,
    /// Startup and shutdown lifecycle.
    Lifecycle,
}

impl OperationCategory {
    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Robot => "robot",
            Self::Watch => "watch",
            Self::Search => "search",
            Self::Events => "events",
            Self::Session => "session",
            Self::Lifecycle => "lifecycle",
        }
    }
}

// =============================================================================
// Operation contract
// =============================================================================

/// Performance contract for a single user-facing operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationContract {
    /// Unique operation identifier (e.g., "robot.get-text", "cli.status").
    pub operation_id: String,
    /// Category for grouping.
    pub category: OperationCategory,
    /// Human-readable description.
    pub description: String,
    /// Latency targets per quantile.
    pub latency_target: PercentileThresholds,
    /// Minimum throughput in operations per second (0 = no throughput SLO).
    pub throughput_min_ops_sec: f64,
    /// Maximum allowed startup time in ms (lifecycle operations only).
    pub startup_max_ms: Option<u64>,
    /// Regression tolerance ratio (e.g., 1.10 = 10% regression allowed).
    pub regression_tolerance: f64,
    /// Whether this operation is critical for cutover (blocks Go decision).
    pub critical: bool,
}

/// Per-percentile latency thresholds in milliseconds.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PercentileThresholds {
    /// p50 target (median).
    pub p50_ms: f64,
    /// p95 target.
    pub p95_ms: f64,
    /// p99 target.
    pub p99_ms: f64,
}

impl PercentileThresholds {
    /// Create new thresholds.
    #[must_use]
    pub fn new(p50_ms: f64, p95_ms: f64, p99_ms: f64) -> Self {
        Self {
            p50_ms,
            p95_ms,
            p99_ms,
        }
    }

    /// Whether all thresholds are satisfied by the given measurements.
    #[must_use]
    pub fn satisfied_by(&self, measured: &PercentileThresholds) -> bool {
        measured.p50_ms <= self.p50_ms
            && measured.p95_ms <= self.p95_ms
            && measured.p99_ms <= self.p99_ms
    }

    /// Per-quantile headroom (positive = within budget, negative = over).
    #[must_use]
    pub fn headroom(&self, measured: &PercentileThresholds) -> PercentileThresholds {
        PercentileThresholds {
            p50_ms: self.p50_ms - measured.p50_ms,
            p95_ms: self.p95_ms - measured.p95_ms,
            p99_ms: self.p99_ms - measured.p99_ms,
        }
    }

    /// Convert to a `QuantileBudgetMs` (with p999 = p99 * 2 as a safe default).
    #[must_use]
    pub fn to_quantile_budget(&self) -> QuantileBudgetMs {
        QuantileBudgetMs::try_new(self.p50_ms, self.p95_ms, self.p99_ms, self.p99_ms * 2.0)
            .unwrap_or_else(|_| QuantileBudgetMs::zero())
    }
}

// =============================================================================
// Runtime performance contract
// =============================================================================

/// Complete performance contract covering all user-facing operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePerformanceContract {
    /// Contract identifier.
    pub contract_id: String,
    /// Contract version for schema evolution.
    pub version: String,
    /// When this contract was established.
    pub established_at_ms: u64,
    /// Per-operation contracts.
    pub operations: Vec<OperationContract>,
}

impl RuntimePerformanceContract {
    /// Create a new empty contract.
    #[must_use]
    pub fn new(contract_id: impl Into<String>) -> Self {
        Self {
            contract_id: contract_id.into(),
            version: "1.0.0".into(),
            established_at_ms: 0,
            operations: Vec::new(),
        }
    }

    /// Add an operation contract.
    pub fn add(&mut self, op: OperationContract) {
        self.operations.push(op);
    }

    /// Look up a contract by operation ID.
    #[must_use]
    pub fn get(&self, operation_id: &str) -> Option<&OperationContract> {
        self.operations
            .iter()
            .find(|o| o.operation_id == operation_id)
    }

    /// All critical operations.
    #[must_use]
    pub fn critical_operations(&self) -> Vec<&OperationContract> {
        self.operations.iter().filter(|o| o.critical).collect()
    }

    /// Operations grouped by category.
    #[must_use]
    pub fn by_category(&self) -> BTreeMap<String, Vec<&OperationContract>> {
        let mut map: BTreeMap<String, Vec<&OperationContract>> = BTreeMap::new();
        for op in &self.operations {
            map.entry(op.category.label().to_string())
                .or_default()
                .push(op);
        }
        map
    }

    /// Standard contract for the asupersync migration covering all user workflows.
    #[must_use]
    pub fn standard() -> Self {
        let mut contract = Self::new("asupersync-migration-v1");

        // ── CLI operations ──
        contract.add(OperationContract {
            operation_id: "cli.status".into(),
            category: OperationCategory::Cli,
            description: "ft status — show pane/session overview".into(),
            latency_target: PercentileThresholds::new(50.0, 150.0, 300.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });

        contract.add(OperationContract {
            operation_id: "cli.list".into(),
            category: OperationCategory::Cli,
            description: "ft list — enumerate panes/sessions".into(),
            latency_target: PercentileThresholds::new(30.0, 100.0, 200.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: false,
        });

        // ── Robot-mode operations ──
        contract.add(OperationContract {
            operation_id: "robot.get-text".into(),
            category: OperationCategory::Robot,
            description: "Get pane text content for agent consumption".into(),
            latency_target: PercentileThresholds::new(20.0, 80.0, 150.0),
            throughput_min_ops_sec: 50.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });

        contract.add(OperationContract {
            operation_id: "robot.send".into(),
            category: OperationCategory::Robot,
            description: "Send text/keys to a pane".into(),
            latency_target: PercentileThresholds::new(10.0, 50.0, 100.0),
            throughput_min_ops_sec: 100.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });

        contract.add(OperationContract {
            operation_id: "robot.wait-for".into(),
            category: OperationCategory::Robot,
            description: "Wait for pattern match in pane output".into(),
            latency_target: PercentileThresholds::new(100.0, 500.0, 2000.0),
            throughput_min_ops_sec: 10.0,
            startup_max_ms: None,
            regression_tolerance: 1.15,
            critical: true,
        });

        contract.add(OperationContract {
            operation_id: "robot.state".into(),
            category: OperationCategory::Robot,
            description: "Get pane metadata and state".into(),
            latency_target: PercentileThresholds::new(15.0, 60.0, 120.0),
            throughput_min_ops_sec: 100.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });

        // ── Watch flow ──
        contract.add(OperationContract {
            operation_id: "watch.startup".into(),
            category: OperationCategory::Watch,
            description: "ft watch startup to first capture".into(),
            latency_target: PercentileThresholds::new(200.0, 500.0, 1000.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: Some(1000),
            regression_tolerance: 1.10,
            critical: true,
        });

        contract.add(OperationContract {
            operation_id: "watch.capture-loop".into(),
            category: OperationCategory::Watch,
            description: "Steady-state capture loop iteration".into(),
            latency_target: PercentileThresholds::new(5.0, 20.0, 50.0),
            throughput_min_ops_sec: 20.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });

        // ── Search ──
        contract.add(OperationContract {
            operation_id: "search.lexical".into(),
            category: OperationCategory::Search,
            description: "FTS5 lexical search query".into(),
            latency_target: PercentileThresholds::new(30.0, 100.0, 250.0),
            throughput_min_ops_sec: 20.0,
            startup_max_ms: None,
            regression_tolerance: 1.15,
            critical: false,
        });

        // ── Events ──
        contract.add(OperationContract {
            operation_id: "events.poll".into(),
            category: OperationCategory::Events,
            description: "Poll event bus for new detections".into(),
            latency_target: PercentileThresholds::new(5.0, 20.0, 50.0),
            throughput_min_ops_sec: 100.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: false,
        });

        // ── Session ──
        contract.add(OperationContract {
            operation_id: "session.list".into(),
            category: OperationCategory::Session,
            description: "List saved sessions/snapshots".into(),
            latency_target: PercentileThresholds::new(30.0, 100.0, 200.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: false,
        });

        // ── Lifecycle ──
        contract.add(OperationContract {
            operation_id: "lifecycle.startup".into(),
            category: OperationCategory::Lifecycle,
            description: "ft process startup to ready".into(),
            latency_target: PercentileThresholds::new(300.0, 800.0, 1500.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: Some(1500),
            regression_tolerance: 1.10,
            critical: true,
        });

        contract.add(OperationContract {
            operation_id: "lifecycle.shutdown".into(),
            category: OperationCategory::Lifecycle,
            description: "Graceful shutdown to process exit".into(),
            latency_target: PercentileThresholds::new(100.0, 300.0, 500.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });

        contract
    }
}

// =============================================================================
// Benchmark measurements
// =============================================================================

/// Measured performance for a single operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationBenchmark {
    /// Operation ID (must match an OperationContract).
    pub operation_id: String,
    /// Measured latency quantiles.
    pub latency: PercentileThresholds,
    /// Measured throughput in ops/sec.
    pub throughput_ops_sec: f64,
    /// Number of samples collected.
    pub samples: u64,
    /// Measured startup time in ms (if applicable).
    pub startup_ms: Option<u64>,
    /// Label for this measurement set (e.g., "baseline", "post-migration").
    pub label: String,
}

/// A full benchmark suite with before/after measurements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSuite {
    /// Suite identifier.
    pub suite_id: String,
    /// When benchmarks were collected.
    pub collected_at_ms: u64,
    /// Baseline measurements (pre-migration).
    pub baseline: Vec<OperationBenchmark>,
    /// Current measurements (post-migration).
    pub current: Vec<OperationBenchmark>,
}

impl BenchmarkSuite {
    /// Create a new suite.
    #[must_use]
    pub fn new(suite_id: impl Into<String>) -> Self {
        Self {
            suite_id: suite_id.into(),
            collected_at_ms: 0,
            baseline: Vec::new(),
            current: Vec::new(),
        }
    }

    /// Add a baseline measurement.
    pub fn add_baseline(&mut self, benchmark: OperationBenchmark) {
        self.baseline.push(benchmark);
    }

    /// Add a current measurement.
    pub fn add_current(&mut self, benchmark: OperationBenchmark) {
        self.current.push(benchmark);
    }

    /// Find baseline for a given operation.
    #[must_use]
    pub fn baseline_for(&self, operation_id: &str) -> Option<&OperationBenchmark> {
        self.baseline
            .iter()
            .find(|b| b.operation_id == operation_id)
    }

    /// Find current for a given operation.
    #[must_use]
    pub fn current_for(&self, operation_id: &str) -> Option<&OperationBenchmark> {
        self.current.iter().find(|b| b.operation_id == operation_id)
    }
}

// =============================================================================
// Regression gate evaluation
// =============================================================================

/// Result of evaluating one operation against its contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationGateResult {
    /// Operation ID.
    pub operation_id: String,
    /// Whether latency targets are met.
    pub latency_pass: bool,
    /// Whether throughput minimum is met.
    pub throughput_pass: bool,
    /// Whether startup maximum is met (if applicable).
    pub startup_pass: bool,
    /// Whether regression tolerance is met (vs baseline).
    pub regression_pass: bool,
    /// Overall pass.
    pub passed: bool,
    /// Per-quantile headroom (positive = within budget).
    pub latency_headroom: PercentileThresholds,
    /// Throughput headroom (positive = above minimum).
    pub throughput_headroom: f64,
    /// Regression ratio (current p95 / baseline p95, if baseline exists).
    pub regression_ratio: Option<f64>,
    /// Failure reasons (empty if passed).
    pub failure_reasons: Vec<String>,
}

/// Evaluate a single operation benchmark against its contract.
#[must_use]
pub fn evaluate_operation(
    contract: &OperationContract,
    current: &OperationBenchmark,
    baseline: Option<&OperationBenchmark>,
) -> OperationGateResult {
    let mut reasons = Vec::new();

    // Latency check.
    let latency_pass = contract.latency_target.satisfied_by(&current.latency);
    if !latency_pass {
        let headroom = contract.latency_target.headroom(&current.latency);
        if headroom.p50_ms < 0.0 {
            reasons.push(format!(
                "p50 latency {:.1}ms exceeds target {:.1}ms",
                current.latency.p50_ms, contract.latency_target.p50_ms
            ));
        }
        if headroom.p95_ms < 0.0 {
            reasons.push(format!(
                "p95 latency {:.1}ms exceeds target {:.1}ms",
                current.latency.p95_ms, contract.latency_target.p95_ms
            ));
        }
        if headroom.p99_ms < 0.0 {
            reasons.push(format!(
                "p99 latency {:.1}ms exceeds target {:.1}ms",
                current.latency.p99_ms, contract.latency_target.p99_ms
            ));
        }
    }

    // Throughput check.
    let throughput_pass = contract.throughput_min_ops_sec <= 0.0
        || current.throughput_ops_sec >= contract.throughput_min_ops_sec;
    if !throughput_pass {
        reasons.push(format!(
            "throughput {:.1} ops/sec below minimum {:.1}",
            current.throughput_ops_sec, contract.throughput_min_ops_sec
        ));
    }

    // Startup check.
    let startup_pass = match (contract.startup_max_ms, current.startup_ms) {
        (Some(max), Some(actual)) => {
            if actual > max {
                reasons.push(format!("startup {}ms exceeds maximum {}ms", actual, max));
                false
            } else {
                true
            }
        }
        _ => true,
    };

    // Regression check (compare p95 against baseline).
    let (regression_pass, regression_ratio) = if let Some(bl) = baseline {
        if bl.latency.p95_ms > 0.0 {
            let ratio = current.latency.p95_ms / bl.latency.p95_ms;
            let pass = ratio <= contract.regression_tolerance;
            if !pass {
                reasons.push(format!(
                    "p95 regression ratio {:.2}x exceeds tolerance {:.2}x (baseline {:.1}ms → current {:.1}ms)",
                    ratio, contract.regression_tolerance, bl.latency.p95_ms, current.latency.p95_ms
                ));
            }
            (pass, Some(ratio))
        } else {
            (true, None)
        }
    } else {
        (true, None)
    };

    let passed = latency_pass && throughput_pass && startup_pass && regression_pass;
    let latency_headroom = contract.latency_target.headroom(&current.latency);
    let throughput_headroom = current.throughput_ops_sec - contract.throughput_min_ops_sec;

    OperationGateResult {
        operation_id: contract.operation_id.clone(),
        latency_pass,
        throughput_pass,
        startup_pass,
        regression_pass,
        passed,
        latency_headroom,
        throughput_headroom,
        regression_ratio,
        failure_reasons: reasons,
    }
}

// =============================================================================
// Regression report
// =============================================================================

/// Verdict for the overall regression gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegressionVerdict {
    /// All operations pass their contracts.
    Pass,
    /// Non-critical operations failed but all critical ones pass.
    ConditionalPass,
    /// One or more critical operations failed.
    Fail,
}

/// Full regression evaluation report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionReport {
    /// Report identifier.
    pub report_id: String,
    /// When evaluation was performed.
    pub evaluated_at_ms: u64,
    /// Contract used.
    pub contract_id: String,
    /// Per-operation results.
    pub results: Vec<OperationGateResult>,
    /// Overall verdict.
    pub verdict: RegressionVerdict,
    /// Summary counts.
    pub total_operations: usize,
    pub passed_operations: usize,
    pub failed_operations: usize,
    pub critical_passed: usize,
    pub critical_failed: usize,
}

impl RegressionReport {
    /// Evaluate all operations in the contract against a benchmark suite.
    #[must_use]
    pub fn evaluate(contract: &RuntimePerformanceContract, suite: &BenchmarkSuite) -> Self {
        let mut results = Vec::new();

        for op in &contract.operations {
            let current = suite.current_for(&op.operation_id);
            let baseline = suite.baseline_for(&op.operation_id);

            if let Some(curr) = current {
                results.push(evaluate_operation(op, curr, baseline));
            }
        }

        let total_operations = results.len();
        let passed_operations = results.iter().filter(|r| r.passed).count();
        let failed_operations = total_operations - passed_operations;

        let critical_ops: Vec<&str> = contract
            .critical_operations()
            .iter()
            .map(|o| o.operation_id.as_str())
            .collect();

        let critical_results: Vec<&OperationGateResult> = results
            .iter()
            .filter(|r| critical_ops.contains(&r.operation_id.as_str()))
            .collect();

        let critical_passed = critical_results.iter().filter(|r| r.passed).count();
        let critical_failed = critical_results.len() - critical_passed;

        let verdict = if failed_operations == 0 {
            RegressionVerdict::Pass
        } else if critical_failed > 0 {
            RegressionVerdict::Fail
        } else {
            RegressionVerdict::ConditionalPass
        };

        Self {
            report_id: format!("{}-eval", contract.contract_id),
            evaluated_at_ms: 0,
            contract_id: contract.contract_id.clone(),
            results,
            verdict,
            total_operations,
            passed_operations,
            failed_operations,
            critical_passed,
            critical_failed,
        }
    }

    /// Convert results into `BenchmarkComparison` items for cutover evidence.
    #[must_use]
    pub fn to_benchmark_comparisons(&self, suite: &BenchmarkSuite) -> Vec<BenchmarkComparison> {
        let mut comparisons = Vec::new();

        for result in &self.results {
            let baseline = suite.baseline_for(&result.operation_id);
            let current = suite.current_for(&result.operation_id);

            if let (Some(bl), Some(curr)) = (baseline, current) {
                comparisons.push(BenchmarkComparison {
                    name: format!("{}.p95", result.operation_id),
                    metric: "latency_ms".into(),
                    before: bl.latency.p95_ms,
                    after: curr.latency.p95_ms,
                    unit: "ms".into(),
                    lower_is_better: true,
                });

                if bl.throughput_ops_sec > 0.0 && curr.throughput_ops_sec > 0.0 {
                    comparisons.push(BenchmarkComparison {
                        name: format!("{}.throughput", result.operation_id),
                        metric: "ops_per_sec".into(),
                        before: bl.throughput_ops_sec,
                        after: curr.throughput_ops_sec,
                        unit: "ops/sec".into(),
                        lower_is_better: false,
                    });
                }
            }
        }

        comparisons
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("=== Regression Report: {} ===", self.report_id));
        lines.push(format!("Verdict: {:?}", self.verdict));
        lines.push(format!(
            "Operations: {}/{} passed ({} critical: {}/{})",
            self.passed_operations,
            self.total_operations,
            self.critical_passed + self.critical_failed,
            self.critical_passed,
            self.critical_passed + self.critical_failed,
        ));
        lines.push(String::new());

        for result in &self.results {
            let status = if result.passed { "PASS" } else { "FAIL" };
            let regression = result
                .regression_ratio
                .map(|r| format!(" (ratio: {:.2}x)", r))
                .unwrap_or_default();
            lines.push(format!(
                "  [{}] {}{}",
                status, result.operation_id, regression
            ));
            for reason in &result.failure_reasons {
                lines.push(format!("       → {}", reason));
            }
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

    fn sample_contract() -> RuntimePerformanceContract {
        RuntimePerformanceContract::standard()
    }

    fn passing_benchmark(
        op_id: &str,
        p50: f64,
        p95: f64,
        p99: f64,
        throughput: f64,
    ) -> OperationBenchmark {
        OperationBenchmark {
            operation_id: op_id.into(),
            latency: PercentileThresholds::new(p50, p95, p99),
            throughput_ops_sec: throughput,
            samples: 1000,
            startup_ms: None,
            label: "current".into(),
        }
    }

    fn baseline_benchmark(
        op_id: &str,
        p50: f64,
        p95: f64,
        p99: f64,
        throughput: f64,
    ) -> OperationBenchmark {
        OperationBenchmark {
            operation_id: op_id.into(),
            latency: PercentileThresholds::new(p50, p95, p99),
            throughput_ops_sec: throughput,
            samples: 1000,
            startup_ms: None,
            label: "baseline".into(),
        }
    }

    #[test]
    fn standard_contract_has_all_categories() {
        let contract = sample_contract();
        let cats = contract.by_category();
        assert!(cats.contains_key("cli"));
        assert!(cats.contains_key("robot"));
        assert!(cats.contains_key("watch"));
        assert!(cats.contains_key("search"));
        assert!(cats.contains_key("events"));
        assert!(cats.contains_key("session"));
        assert!(cats.contains_key("lifecycle"));
    }

    #[test]
    fn standard_contract_has_critical_operations() {
        let contract = sample_contract();
        let critical = contract.critical_operations();
        assert!(critical.len() >= 8);
        assert!(critical.iter().any(|o| o.operation_id == "robot.get-text"));
        assert!(critical.iter().any(|o| o.operation_id == "robot.send"));
        assert!(
            critical
                .iter()
                .any(|o| o.operation_id == "lifecycle.startup")
        );
    }

    #[test]
    fn percentile_thresholds_satisfied() {
        let target = PercentileThresholds::new(50.0, 100.0, 200.0);
        let within = PercentileThresholds::new(30.0, 80.0, 150.0);
        let over = PercentileThresholds::new(60.0, 80.0, 150.0);

        assert!(target.satisfied_by(&within));
        assert!(!target.satisfied_by(&over));
    }

    #[test]
    fn percentile_headroom_positive_when_under() {
        let target = PercentileThresholds::new(50.0, 100.0, 200.0);
        let measured = PercentileThresholds::new(30.0, 80.0, 150.0);
        let headroom = target.headroom(&measured);
        assert!(headroom.p50_ms > 0.0);
        assert!(headroom.p95_ms > 0.0);
        assert!(headroom.p99_ms > 0.0);
    }

    #[test]
    fn percentile_headroom_negative_when_over() {
        let target = PercentileThresholds::new(50.0, 100.0, 200.0);
        let measured = PercentileThresholds::new(60.0, 120.0, 250.0);
        let headroom = target.headroom(&measured);
        assert!(headroom.p50_ms < 0.0);
        assert!(headroom.p95_ms < 0.0);
        assert!(headroom.p99_ms < 0.0);
    }

    #[test]
    fn evaluate_operation_passes_when_all_within_budget() {
        let contract = OperationContract {
            operation_id: "test.op".into(),
            category: OperationCategory::Cli,
            description: "test".into(),
            latency_target: PercentileThresholds::new(50.0, 100.0, 200.0),
            throughput_min_ops_sec: 10.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        };

        let current = passing_benchmark("test.op", 30.0, 80.0, 150.0, 50.0);
        let result = evaluate_operation(&contract, &current, None);
        assert!(result.passed);
        assert!(result.latency_pass);
        assert!(result.throughput_pass);
        assert!(result.failure_reasons.is_empty());
    }

    #[test]
    fn evaluate_operation_fails_on_latency_breach() {
        let contract = OperationContract {
            operation_id: "test.op".into(),
            category: OperationCategory::Cli,
            description: "test".into(),
            latency_target: PercentileThresholds::new(50.0, 100.0, 200.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        };

        let current = passing_benchmark("test.op", 60.0, 120.0, 150.0, 0.0);
        let result = evaluate_operation(&contract, &current, None);
        assert!(!result.passed);
        assert!(!result.latency_pass);
        assert!(result.failure_reasons.iter().any(|r| r.contains("p50")));
        assert!(result.failure_reasons.iter().any(|r| r.contains("p95")));
    }

    #[test]
    fn evaluate_operation_fails_on_throughput() {
        let contract = OperationContract {
            operation_id: "test.op".into(),
            category: OperationCategory::Cli,
            description: "test".into(),
            latency_target: PercentileThresholds::new(50.0, 100.0, 200.0),
            throughput_min_ops_sec: 100.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        };

        let current = passing_benchmark("test.op", 30.0, 80.0, 150.0, 50.0);
        let result = evaluate_operation(&contract, &current, None);
        assert!(!result.passed);
        assert!(!result.throughput_pass);
    }

    #[test]
    fn evaluate_operation_fails_on_startup_exceeded() {
        let contract = OperationContract {
            operation_id: "lifecycle.startup".into(),
            category: OperationCategory::Lifecycle,
            description: "startup".into(),
            latency_target: PercentileThresholds::new(300.0, 800.0, 1500.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: Some(1500),
            regression_tolerance: 1.10,
            critical: true,
        };

        let mut current = passing_benchmark("lifecycle.startup", 200.0, 600.0, 1000.0, 0.0);
        current.startup_ms = Some(2000);
        let result = evaluate_operation(&contract, &current, None);
        assert!(!result.passed);
        assert!(!result.startup_pass);
    }

    #[test]
    fn evaluate_operation_fails_on_regression() {
        let contract = OperationContract {
            operation_id: "test.op".into(),
            category: OperationCategory::Cli,
            description: "test".into(),
            latency_target: PercentileThresholds::new(100.0, 200.0, 400.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        };

        let baseline = baseline_benchmark("test.op", 30.0, 80.0, 150.0, 0.0);
        // Current is within absolute targets but regressed >10% from baseline.
        let current = passing_benchmark("test.op", 50.0, 100.0, 200.0, 0.0);
        let result = evaluate_operation(&contract, &current, Some(&baseline));
        assert!(!result.passed);
        assert!(!result.regression_pass);
        assert!(result.regression_ratio.unwrap() > 1.10);
    }

    #[test]
    fn regression_report_all_pass() {
        let contract = sample_contract();
        let mut suite = BenchmarkSuite::new("test-suite");

        for op in &contract.operations {
            let target = &op.latency_target;
            suite.add_current(passing_benchmark(
                &op.operation_id,
                target.p50_ms * 0.5,
                target.p95_ms * 0.5,
                target.p99_ms * 0.5,
                op.throughput_min_ops_sec.max(1.0) * 2.0,
            ));
        }

        let report = RegressionReport::evaluate(&contract, &suite);
        assert_eq!(report.verdict, RegressionVerdict::Pass);
        assert_eq!(report.failed_operations, 0);
    }

    #[test]
    fn regression_report_conditional_pass_on_non_critical_failure() {
        let mut contract = RuntimePerformanceContract::new("test");
        contract.add(OperationContract {
            operation_id: "critical.op".into(),
            category: OperationCategory::Cli,
            description: "critical".into(),
            latency_target: PercentileThresholds::new(100.0, 200.0, 400.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });
        contract.add(OperationContract {
            operation_id: "non-critical.op".into(),
            category: OperationCategory::Search,
            description: "non-critical".into(),
            latency_target: PercentileThresholds::new(50.0, 100.0, 200.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: false,
        });

        let mut suite = BenchmarkSuite::new("test-suite");
        suite.add_current(passing_benchmark("critical.op", 50.0, 100.0, 200.0, 0.0));
        // Non-critical fails.
        suite.add_current(passing_benchmark(
            "non-critical.op",
            60.0,
            120.0,
            250.0,
            0.0,
        ));

        let report = RegressionReport::evaluate(&contract, &suite);
        assert_eq!(report.verdict, RegressionVerdict::ConditionalPass);
        assert_eq!(report.critical_failed, 0);
        assert_eq!(report.failed_operations, 1);
    }

    #[test]
    fn regression_report_fail_on_critical_failure() {
        let mut contract = RuntimePerformanceContract::new("test");
        contract.add(OperationContract {
            operation_id: "critical.op".into(),
            category: OperationCategory::Cli,
            description: "critical".into(),
            latency_target: PercentileThresholds::new(50.0, 100.0, 200.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });

        let mut suite = BenchmarkSuite::new("test-suite");
        suite.add_current(passing_benchmark("critical.op", 60.0, 120.0, 250.0, 0.0));

        let report = RegressionReport::evaluate(&contract, &suite);
        assert_eq!(report.verdict, RegressionVerdict::Fail);
        assert_eq!(report.critical_failed, 1);
    }

    #[test]
    fn benchmark_comparisons_generated() {
        let mut contract = RuntimePerformanceContract::new("test");
        contract.add(OperationContract {
            operation_id: "test.op".into(),
            category: OperationCategory::Cli,
            description: "test".into(),
            latency_target: PercentileThresholds::new(100.0, 200.0, 400.0),
            throughput_min_ops_sec: 10.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });

        let mut suite = BenchmarkSuite::new("test-suite");
        suite.add_baseline(baseline_benchmark("test.op", 30.0, 80.0, 150.0, 50.0));
        suite.add_current(passing_benchmark("test.op", 35.0, 85.0, 160.0, 45.0));

        let report = RegressionReport::evaluate(&contract, &suite);
        let comparisons = report.to_benchmark_comparisons(&suite);
        assert_eq!(comparisons.len(), 2); // latency + throughput
        assert_eq!(comparisons[0].name, "test.op.p95");
        assert!(comparisons[0].lower_is_better);
        assert!(!comparisons[1].lower_is_better); // throughput
    }

    #[test]
    fn render_summary_includes_verdict() {
        let contract = sample_contract();
        let mut suite = BenchmarkSuite::new("test");
        for op in &contract.operations {
            let t = &op.latency_target;
            suite.add_current(passing_benchmark(
                &op.operation_id,
                t.p50_ms * 0.5,
                t.p95_ms * 0.5,
                t.p99_ms * 0.5,
                op.throughput_min_ops_sec.max(1.0) * 2.0,
            ));
        }

        let report = RegressionReport::evaluate(&contract, &suite);
        let summary = report.render_summary();
        assert!(summary.contains("Pass"));
        assert!(summary.contains("PASS"));
    }

    #[test]
    fn render_summary_shows_failures() {
        let mut contract = RuntimePerformanceContract::new("test");
        contract.add(OperationContract {
            operation_id: "test.op".into(),
            category: OperationCategory::Cli,
            description: "test".into(),
            latency_target: PercentileThresholds::new(50.0, 100.0, 200.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });
        let mut suite = BenchmarkSuite::new("test");
        suite.add_current(passing_benchmark("test.op", 60.0, 120.0, 250.0, 0.0));

        let report = RegressionReport::evaluate(&contract, &suite);
        let summary = report.render_summary();
        assert!(summary.contains("FAIL"));
        assert!(summary.contains("exceeds target"));
    }

    #[test]
    fn serde_roundtrip_contract() {
        let contract = sample_contract();
        let json = serde_json::to_string(&contract).expect("serialize");
        let restored: RuntimePerformanceContract =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.operations.len(), contract.operations.len());
        assert_eq!(restored.contract_id, contract.contract_id);
    }

    #[test]
    fn serde_roundtrip_report() {
        let contract = sample_contract();
        let mut suite = BenchmarkSuite::new("test");
        for op in &contract.operations {
            let t = &op.latency_target;
            suite.add_current(passing_benchmark(
                &op.operation_id,
                t.p50_ms * 0.5,
                t.p95_ms * 0.5,
                t.p99_ms * 0.5,
                op.throughput_min_ops_sec.max(1.0) * 2.0,
            ));
        }

        let report = RegressionReport::evaluate(&contract, &suite);
        let json = serde_json::to_string(&report).expect("serialize");
        let restored: RegressionReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.verdict, RegressionVerdict::Pass);
        assert_eq!(restored.results.len(), report.results.len());
    }

    #[test]
    fn operation_category_labels_distinct() {
        let cats = [
            OperationCategory::Cli,
            OperationCategory::Robot,
            OperationCategory::Watch,
            OperationCategory::Search,
            OperationCategory::Events,
            OperationCategory::Session,
            OperationCategory::Lifecycle,
        ];
        let labels: Vec<&str> = cats.iter().map(|c| c.label()).collect();
        for (i, a) in labels.iter().enumerate() {
            for (j, b) in labels.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "labels must be distinct");
                }
            }
        }
    }

    #[test]
    fn contract_get_by_id() {
        let contract = sample_contract();
        assert!(contract.get("robot.get-text").is_some());
        assert!(contract.get("nonexistent").is_none());
    }

    #[test]
    fn benchmark_suite_lookup() {
        let mut suite = BenchmarkSuite::new("test");
        suite.add_baseline(baseline_benchmark("op1", 10.0, 20.0, 30.0, 100.0));
        suite.add_current(passing_benchmark("op1", 12.0, 22.0, 32.0, 95.0));

        assert!(suite.baseline_for("op1").is_some());
        assert!(suite.current_for("op1").is_some());
        assert!(suite.baseline_for("op2").is_none());
    }

    #[test]
    fn to_quantile_budget_conversion() {
        let thresholds = PercentileThresholds::new(10.0, 50.0, 100.0);
        let budget = thresholds.to_quantile_budget();
        assert_eq!(budget.p50_ms, 10.0);
        assert_eq!(budget.p95_ms, 50.0);
        assert_eq!(budget.p99_ms, 100.0);
        assert_eq!(budget.p999_ms, 200.0); // p99 * 2
    }

    #[test]
    fn zero_throughput_always_passes() {
        let contract = OperationContract {
            operation_id: "test".into(),
            category: OperationCategory::Cli,
            description: "test".into(),
            latency_target: PercentileThresholds::new(100.0, 200.0, 400.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: false,
        };
        let current = passing_benchmark("test", 50.0, 100.0, 200.0, 0.0);
        let result = evaluate_operation(&contract, &current, None);
        assert!(result.throughput_pass);
    }

    #[test]
    fn missing_current_skipped_in_report() {
        let mut contract = RuntimePerformanceContract::new("test");
        contract.add(OperationContract {
            operation_id: "exists".into(),
            category: OperationCategory::Cli,
            description: "test".into(),
            latency_target: PercentileThresholds::new(100.0, 200.0, 400.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });
        contract.add(OperationContract {
            operation_id: "missing".into(),
            category: OperationCategory::Cli,
            description: "test".into(),
            latency_target: PercentileThresholds::new(100.0, 200.0, 400.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        });

        let mut suite = BenchmarkSuite::new("test");
        suite.add_current(passing_benchmark("exists", 50.0, 100.0, 200.0, 0.0));

        let report = RegressionReport::evaluate(&contract, &suite);
        // Only "exists" should have a result.
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].operation_id, "exists");
    }

    #[test]
    fn regression_tolerance_boundary() {
        let contract = OperationContract {
            operation_id: "test".into(),
            category: OperationCategory::Robot,
            description: "test".into(),
            latency_target: PercentileThresholds::new(200.0, 400.0, 800.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        };

        let baseline = baseline_benchmark("test", 50.0, 100.0, 200.0, 0.0);

        // Exactly at tolerance (1.10x = 110ms p95).
        let at_limit = passing_benchmark("test", 55.0, 110.0, 220.0, 0.0);
        let result = evaluate_operation(&contract, &at_limit, Some(&baseline));
        assert!(result.regression_pass);

        // Just over tolerance (1.11x = 111ms p95).
        let over_limit = passing_benchmark("test", 55.5, 111.0, 222.0, 0.0);
        let result = evaluate_operation(&contract, &over_limit, Some(&baseline));
        assert!(!result.regression_pass);
    }
}
