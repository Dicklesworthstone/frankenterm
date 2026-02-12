//! LabRuntime test helpers for deterministic async testing.
//!
//! Provides ergonomic wrappers around asupersync's `LabRuntime` for:
//! - Deterministic seed-based testing with `run_lab_test`
//! - Chaos fault injection with `run_chaos_test`
//! - Schedule exploration (DPOR) with `run_exploration_test`
//!
//! All helpers emit structured tracing logs with seed, test name, and
//! outcome information for debugging.

use asupersync::lab::chaos::ChaosConfig;
use asupersync::lab::explorer::{ExplorerConfig, ExplorationReport, ScheduleExplorer};
use asupersync::{LabConfig, LabRuntime, Time};

// ---------------------------------------------------------------------------
// LabTestConfig — ergonomic builder for test-level config
// ---------------------------------------------------------------------------

/// Configuration for a single lab test run.
#[derive(Debug, Clone)]
pub struct LabTestConfig {
    /// Deterministic seed for reproducibility.
    pub seed: u64,
    /// Human-readable test identifier (used in tracing output).
    pub test_name: String,
    /// Number of virtual workers to simulate.
    pub worker_count: usize,
    /// Maximum steps before the runtime forcibly terminates.
    pub max_steps: u64,
    /// Whether to panic when obligation leaks are detected.
    pub panic_on_leak: bool,
}

impl LabTestConfig {
    /// Create a config with the given seed and test name.
    #[must_use]
    pub fn new(seed: u64, test_name: impl Into<String>) -> Self {
        Self {
            seed,
            test_name: test_name.into(),
            worker_count: 2,
            max_steps: 100_000,
            panic_on_leak: true,
        }
    }

    /// Set the number of virtual workers.
    #[must_use]
    pub fn worker_count(mut self, count: usize) -> Self {
        self.worker_count = count;
        self
    }

    /// Set the maximum number of steps.
    #[must_use]
    pub fn max_steps(mut self, steps: u64) -> Self {
        self.max_steps = steps;
        self
    }

    /// Whether to panic on obligation leaks (default: true).
    #[must_use]
    pub fn panic_on_leak(mut self, value: bool) -> Self {
        self.panic_on_leak = value;
        self
    }

    /// Convert to an asupersync `LabConfig`.
    #[must_use]
    pub fn to_lab_config(&self) -> LabConfig {
        LabConfig::new(self.seed)
            .worker_count(self.worker_count)
            .max_steps(self.max_steps)
            .panic_on_leak(self.panic_on_leak)
    }
}

// ---------------------------------------------------------------------------
// LabTestReport — structured output from a lab test run
// ---------------------------------------------------------------------------

/// Report from a single lab test execution.
#[derive(Debug)]
pub struct LabTestReport {
    /// Seed used for this run.
    pub seed: u64,
    /// Test name.
    pub test_name: String,
    /// Number of steps executed.
    pub steps: u64,
    /// Final virtual time.
    pub final_time: Time,
    /// Whether all oracles passed.
    pub oracles_passed: bool,
    /// Whether any invariant violations were found.
    pub invariant_violations_found: bool,
}

impl LabTestReport {
    /// Returns true if the test passed all checks.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.oracles_passed && !self.invariant_violations_found
    }
}

// ---------------------------------------------------------------------------
// run_lab_test — deterministic test with oracle assertions
// ---------------------------------------------------------------------------

/// Run a deterministic lab test with the given configuration.
///
/// The `test_fn` receives a mutable reference to a freshly constructed
/// `LabRuntime`. It should set up tasks, drive execution (e.g. via
/// `runtime.run_until_quiescent()`), and perform assertions.
///
/// After `test_fn` returns, this helper automatically:
/// 1. Generates a structured `LabRunReport` with oracle results
/// 2. Asserts all oracles pass and no invariant violations exist
/// 3. Logs the outcome with structured tracing
///
/// # Panics
///
/// Panics if any oracle fails or invariant violations are detected.
///
/// # Example
///
/// ```ignore
/// use crate::common::lab::{LabTestConfig, run_lab_test};
///
/// run_lab_test(
///     LabTestConfig::new(42, "my_deterministic_test"),
///     |runtime| {
///         // Set up tasks in runtime...
///         runtime.run_until_quiescent();
///     },
/// );
/// ```
pub fn run_lab_test<F>(config: LabTestConfig, test_fn: F) -> LabTestReport
where
    F: FnOnce(&mut LabRuntime),
{
    let seed = config.seed;
    let test_name = config.test_name.clone();

    tracing::info!(
        seed,
        test_name = %test_name,
        workers = config.worker_count,
        max_steps = config.max_steps,
        "Starting LabRuntime test"
    );

    let lab_config = config.to_lab_config();
    let mut runtime = LabRuntime::new(lab_config);

    // Run the test closure
    test_fn(&mut runtime);

    // Collect oracle report
    let report = runtime.run_until_quiescent_with_report();
    let oracles_passed = report.oracle_report.all_passed();
    let invariant_violations_found = !report.invariant_violations.is_empty();

    let lab_report = LabTestReport {
        seed,
        test_name: test_name.clone(),
        steps: runtime.steps(),
        final_time: runtime.now(),
        oracles_passed,
        invariant_violations_found,
    };

    tracing::info!(
        seed,
        test_name = %test_name,
        steps = runtime.steps(),
        oracles_passed,
        violations = report.invariant_violations.len(),
        "LabRuntime test completed"
    );

    // Assert oracle health
    assert!(
        oracles_passed,
        "[{test_name}] Oracle failure at seed {seed}: {:?}",
        report.oracle_report
    );
    assert!(
        !invariant_violations_found,
        "[{test_name}] Invariant violations at seed {seed}: {:?}",
        report.invariant_violations
    );

    lab_report
}

/// Convenience: run a lab test with just a seed and closure.
///
/// Uses default configuration (2 workers, 100k max steps).
pub fn run_lab_test_simple<F>(seed: u64, test_name: &str, test_fn: F) -> LabTestReport
where
    F: FnOnce(&mut LabRuntime),
{
    run_lab_test(LabTestConfig::new(seed, test_name), test_fn)
}

// ---------------------------------------------------------------------------
// run_chaos_test — fault injection testing
// ---------------------------------------------------------------------------

/// Configuration for a chaos test run.
#[derive(Debug, Clone)]
pub struct ChaosTestConfig {
    /// Base deterministic test config.
    pub base: LabTestConfig,
    /// Chaos injection preset or custom config.
    pub chaos: ChaosPreset,
}

/// Chaos injection preset.
#[derive(Debug, Clone)]
pub enum ChaosPreset {
    /// Low-probability faults (suitable for CI).
    Light,
    /// High-probability faults (thorough testing).
    Heavy,
    /// Custom chaos configuration.
    Custom(ChaosConfig),
}

impl ChaosTestConfig {
    /// Create a chaos test config with light fault injection.
    #[must_use]
    pub fn light(seed: u64, test_name: impl Into<String>) -> Self {
        Self {
            base: LabTestConfig::new(seed, test_name),
            chaos: ChaosPreset::Light,
        }
    }

    /// Create a chaos test config with heavy fault injection.
    #[must_use]
    pub fn heavy(seed: u64, test_name: impl Into<String>) -> Self {
        Self {
            base: LabTestConfig::new(seed, test_name),
            chaos: ChaosPreset::Heavy,
        }
    }

    /// Create with a custom ChaosConfig.
    #[must_use]
    pub fn custom(seed: u64, test_name: impl Into<String>, chaos: ChaosConfig) -> Self {
        Self {
            base: LabTestConfig::new(seed, test_name),
            chaos: ChaosPreset::Custom(chaos),
        }
    }

    /// Set worker count.
    #[must_use]
    pub fn worker_count(mut self, count: usize) -> Self {
        self.base.worker_count = count;
        self
    }

    /// Set max steps.
    #[must_use]
    pub fn max_steps(mut self, steps: u64) -> Self {
        self.base.max_steps = steps;
        self
    }
}

/// Report from a chaos test run.
#[derive(Debug)]
pub struct ChaosTestReport {
    /// Base test report.
    pub base: LabTestReport,
    /// Whether chaos injection was active.
    pub chaos_active: bool,
}

impl ChaosTestReport {
    /// Returns true if the test passed despite fault injection.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.base.passed()
    }
}

/// Run a chaos test with fault injection.
///
/// Same as `run_lab_test` but with chaos injection enabled. The test
/// closure should be resilient to injected faults (cancellations, delays,
/// I/O errors).
///
/// # Panics
///
/// Panics if oracles fail or invariant violations are detected.
pub fn run_chaos_test<F>(config: ChaosTestConfig, test_fn: F) -> ChaosTestReport
where
    F: FnOnce(&mut LabRuntime),
{
    let seed = config.base.seed;
    let test_name = config.base.test_name.clone();

    let mut lab_config = config.base.to_lab_config();
    lab_config = match config.chaos {
        ChaosPreset::Light => lab_config.with_light_chaos(),
        ChaosPreset::Heavy => lab_config.with_heavy_chaos(),
        ChaosPreset::Custom(c) => lab_config.with_chaos(c),
    };

    tracing::info!(
        seed,
        test_name = %test_name,
        chaos = true,
        "Starting chaos LabRuntime test"
    );

    let mut runtime = LabRuntime::new(lab_config);
    let chaos_active = runtime.has_chaos();

    test_fn(&mut runtime);

    let report = runtime.run_until_quiescent_with_report();
    let oracles_passed = report.oracle_report.all_passed();
    let invariant_violations_found = !report.invariant_violations.is_empty();

    let chaos_stats = runtime.chaos_stats();
    tracing::info!(
        seed,
        test_name = %test_name,
        steps = runtime.steps(),
        chaos_active,
        chaos_decision_points = chaos_stats.decision_points,
        chaos_delays = chaos_stats.delays,
        oracles_passed,
        violations = report.invariant_violations.len(),
        "Chaos LabRuntime test completed"
    );

    let base_report = LabTestReport {
        seed,
        test_name: test_name.clone(),
        steps: runtime.steps(),
        final_time: runtime.now(),
        oracles_passed,
        invariant_violations_found,
    };

    assert!(
        oracles_passed,
        "[{test_name}] Chaos oracle failure at seed {seed}: {:?}",
        report.oracle_report
    );
    assert!(
        !invariant_violations_found,
        "[{test_name}] Chaos invariant violations at seed {seed}: {:?}",
        report.invariant_violations
    );

    ChaosTestReport {
        base: base_report,
        chaos_active,
    }
}

// ---------------------------------------------------------------------------
// run_exploration_test — DPOR schedule exploration
// ---------------------------------------------------------------------------

/// Configuration for a schedule exploration run.
#[derive(Debug, Clone)]
pub struct ExplorationTestConfig {
    /// Human-readable test name.
    pub test_name: String,
    /// Base seed for exploration (seeds sweep from base_seed..base_seed+max_runs).
    pub base_seed: u64,
    /// Maximum number of exploration runs.
    pub max_runs: usize,
    /// Maximum steps per individual run.
    pub max_steps_per_run: u64,
    /// Number of simulated workers per run.
    pub worker_count: usize,
}

impl ExplorationTestConfig {
    /// Create an exploration config.
    #[must_use]
    pub fn new(test_name: impl Into<String>, max_runs: usize) -> Self {
        Self {
            test_name: test_name.into(),
            base_seed: 0,
            max_runs,
            max_steps_per_run: 100_000,
            worker_count: 2,
        }
    }

    /// Set the base seed.
    #[must_use]
    pub fn base_seed(mut self, seed: u64) -> Self {
        self.base_seed = seed;
        self
    }

    /// Set the worker count.
    #[must_use]
    pub fn worker_count(mut self, count: usize) -> Self {
        self.worker_count = count;
        self
    }

    /// Set max steps per run.
    #[must_use]
    pub fn max_steps_per_run(mut self, steps: u64) -> Self {
        self.max_steps_per_run = steps;
        self
    }

    /// Convert to asupersync `ExplorerConfig`.
    #[must_use]
    pub fn to_explorer_config(&self) -> ExplorerConfig {
        ExplorerConfig {
            base_seed: self.base_seed,
            max_runs: self.max_runs,
            max_steps_per_run: self.max_steps_per_run,
            worker_count: self.worker_count,
            record_traces: true,
        }
    }
}

/// Report from a schedule exploration.
#[derive(Debug)]
pub struct ExplorationTestReport {
    /// Test name.
    pub test_name: String,
    /// Total runs executed.
    pub total_runs: usize,
    /// Number of unique equivalence classes discovered.
    pub unique_classes: usize,
    /// Whether any violations were found.
    pub has_violations: bool,
    /// Seeds that produced violations (for reproduction).
    pub violation_seeds: Vec<u64>,
    /// The underlying exploration report.
    pub inner: ExplorationReport,
}

impl ExplorationTestReport {
    /// Returns true if no violations were found.
    #[must_use]
    pub fn passed(&self) -> bool {
        !self.has_violations
    }
}

/// Run schedule exploration (DPOR seed-sweep) across multiple seeds.
///
/// The `test_fn` receives a mutable reference to a `LabRuntime` for each
/// seed. It should set up concurrent tasks and drive execution. The explorer
/// automatically varies the scheduling seed to cover different interleavings.
///
/// # Panics
///
/// Panics if any violation is found during exploration.
///
/// # Example
///
/// ```ignore
/// use crate::common::lab::{ExplorationTestConfig, run_exploration_test};
///
/// run_exploration_test(
///     ExplorationTestConfig::new("concurrent_access", 50),
///     |runtime| {
///         // Set up concurrent tasks...
///         runtime.run_until_quiescent();
///     },
/// );
/// ```
pub fn run_exploration_test<F>(
    config: ExplorationTestConfig,
    test_fn: F,
) -> ExplorationTestReport
where
    F: Fn(&mut LabRuntime),
{
    let test_name = config.test_name.clone();

    tracing::info!(
        test_name = %test_name,
        base_seed = config.base_seed,
        max_runs = config.max_runs,
        workers = config.worker_count,
        "Starting schedule exploration"
    );

    let explorer_config = config.to_explorer_config();
    let mut explorer = ScheduleExplorer::new(explorer_config);
    let inner = explorer.explore(test_fn);

    let has_violations = inner.has_violations();
    let violation_seeds: Vec<u64> = inner.violation_seeds().to_vec();

    tracing::info!(
        test_name = %test_name,
        total_runs = inner.total_runs,
        unique_classes = inner.unique_classes,
        has_violations,
        violation_count = violation_seeds.len(),
        "Schedule exploration completed"
    );

    let report = ExplorationTestReport {
        test_name: test_name.clone(),
        total_runs: inner.total_runs,
        unique_classes: inner.unique_classes,
        has_violations,
        violation_seeds: violation_seeds.clone(),
        inner,
    };

    assert!(
        !has_violations,
        "[{test_name}] Schedule exploration found violations at seeds: {violation_seeds:?}"
    );

    report
}

// ---------------------------------------------------------------------------
// Convenience: multi-seed lab test (run same test across N seeds)
// ---------------------------------------------------------------------------

/// Run the same test across multiple seeds, asserting all pass.
///
/// Useful for catching seed-dependent bugs without full DPOR exploration.
pub fn run_multi_seed_test<F>(
    test_name: &str,
    seeds: &[u64],
    test_fn: F,
) -> Vec<LabTestReport>
where
    F: Fn(&mut LabRuntime),
{
    let mut reports = Vec::with_capacity(seeds.len());
    for &seed in seeds {
        let name = format!("{test_name}/seed-{seed}");
        let config = LabTestConfig::new(seed, name);
        let mut runtime = LabRuntime::new(config.to_lab_config());
        test_fn(&mut runtime);

        let report = runtime.run_until_quiescent_with_report();
        let oracles_passed = report.oracle_report.all_passed();
        let invariant_violations_found = !report.invariant_violations.is_empty();

        assert!(
            oracles_passed,
            "[{test_name}] Oracle failure at seed {seed}"
        );
        assert!(
            !invariant_violations_found,
            "[{test_name}] Invariant violations at seed {seed}"
        );

        reports.push(LabTestReport {
            seed,
            test_name: format!("{test_name}/seed-{seed}"),
            steps: runtime.steps(),
            final_time: runtime.now(),
            oracles_passed,
            invariant_violations_found,
        });
    }
    reports
}
