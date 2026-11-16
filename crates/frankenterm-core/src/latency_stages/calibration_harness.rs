use serde::{Deserialize, Serialize};
use std::fmt;

/// Scenario class for calibration evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CalibrationScenario {
    /// Steady-state, no anomalies.
    Nominal,
    /// Gradual drift over time.
    GradualDrift,
    /// Sudden regime change.
    AbruptShift,
    /// High-noise environment.
    NoisyBaseline,
    /// Recovery after a stress event.
    PostStressRecovery,
}

impl fmt::Display for CalibrationScenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Nominal => write!(f, "nominal"),
            Self::GradualDrift => write!(f, "gradual_drift"),
            Self::AbruptShift => write!(f, "abrupt_shift"),
            Self::NoisyBaseline => write!(f, "noisy_baseline"),
            Self::PostStressRecovery => write!(f, "post_stress_recovery"),
        }
    }
}

/// Result of evaluating a detector/controller on one calibration scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationResult {
    /// Which scenario was run.
    pub scenario: CalibrationScenario,
    /// False positive rate (type I errors).
    pub false_positive_rate: f64,
    /// Miss rate (type II errors).
    pub miss_rate: f64,
    /// Detection delay in observations (for drift scenarios).
    pub detection_delay: f64,
    /// Mean expected loss over the scenario.
    pub mean_expected_loss: f64,
    /// Whether the result meets the promotion gate criteria.
    pub passes_gate: bool,
    /// Number of observations in the scenario.
    pub observation_count: u64,
    /// Timestamp when calibration was run.
    pub timestamp_us: u64,
}

/// Promotion gate configuration.
///
/// A controller/detector update is only promoted to production if
/// all gate criteria are met across all calibration scenarios.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionGateConfig {
    /// Maximum allowed false positive rate.
    pub max_fpr: f64,
    /// Maximum allowed miss rate.
    pub max_miss_rate: f64,
    /// Maximum allowed detection delay (observations).
    pub max_detection_delay: f64,
    /// Maximum allowed mean expected loss.
    pub max_expected_loss: f64,
    /// Minimum number of scenarios that must pass.
    pub min_passing_scenarios: usize,
    /// Whether to require all scenarios to pass (strict mode).
    pub strict: bool,
}

impl PromotionGateConfig {
    /// Sensible defaults.
    pub fn default_strict() -> Self {
        Self {
            max_fpr: 0.05,
            max_miss_rate: 0.10,
            max_detection_delay: 50.0,
            max_expected_loss: 5.0,
            min_passing_scenarios: 5,
            strict: true,
        }
    }
}

/// Verdict of the promotion gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PromotionVerdict {
    /// All gates passed — safe to promote.
    Approved,
    /// Some gates failed — review required.
    ConditionalHold,
    /// Critical gates failed — do not promote.
    Rejected,
}

impl fmt::Display for PromotionVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Approved => write!(f, "approved"),
            Self::ConditionalHold => write!(f, "conditional_hold"),
            Self::Rejected => write!(f, "rejected"),
        }
    }
}

/// Snapshot of the calibration harness state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationSnapshot {
    /// Total calibration runs.
    pub total_runs: u64,
    /// Results per scenario.
    pub scenario_results: Vec<CalibrationResult>,
    /// Overall verdict.
    pub verdict: PromotionVerdict,
    /// Number of passing scenarios.
    pub passing_count: usize,
    /// Number of failing scenarios.
    pub failing_count: usize,
}

/// The calibration harness.
///
/// Evaluates detector/controller quality across scenario classes and
/// gates promotions based on configurable thresholds.
#[derive(Debug, Clone)]
pub struct CalibrationHarness {
    config: PromotionGateConfig,
    /// Results from the most recent calibration run.
    results: Vec<CalibrationResult>,
    /// Total calibration runs ever.
    total_runs: u64,
    /// Last verdict.
    last_verdict: PromotionVerdict,
}

impl CalibrationHarness {
    /// Create a new harness.
    pub fn new(config: PromotionGateConfig) -> Self {
        Self {
            config,
            results: Vec::new(),
            total_runs: 0,
            last_verdict: PromotionVerdict::Rejected,
        }
    }

    /// Create with strict defaults.
    pub fn with_defaults() -> Self {
        Self::new(PromotionGateConfig::default_strict())
    }

    /// Submit a calibration result and evaluate against gates.
    pub fn submit(&mut self, result: CalibrationResult) {
        self.total_runs += 1;
        self.results.push(result);
    }

    /// Evaluate a single result against gate criteria.
    #[allow(dead_code)]
    fn evaluate_result(&self, result: &CalibrationResult) -> bool {
        result.false_positive_rate <= self.config.max_fpr
            && result.miss_rate <= self.config.max_miss_rate
            && result.detection_delay <= self.config.max_detection_delay
            && result.mean_expected_loss <= self.config.max_expected_loss
    }

    /// Compute the overall promotion verdict.
    pub fn evaluate(&mut self) -> PromotionVerdict {
        if self.results.is_empty() {
            self.last_verdict = PromotionVerdict::Rejected;
            return self.last_verdict;
        }

        let mut passing = 0_usize;
        let mut failing = 0_usize;
        for r in &mut self.results {
            let passes = r.false_positive_rate <= self.config.max_fpr
                && r.miss_rate <= self.config.max_miss_rate
                && r.detection_delay <= self.config.max_detection_delay
                && r.mean_expected_loss <= self.config.max_expected_loss;
            r.passes_gate = passes;
            if passes {
                passing += 1;
            } else {
                failing += 1;
            }
        }

        let verdict = if self.config.strict && failing > 0 {
            PromotionVerdict::Rejected
        } else if passing >= self.config.min_passing_scenarios {
            PromotionVerdict::Approved
        } else if passing > 0 {
            PromotionVerdict::ConditionalHold
        } else {
            PromotionVerdict::Rejected
        };

        self.last_verdict = verdict;
        verdict
    }

    /// Last computed verdict.
    pub fn verdict(&self) -> PromotionVerdict {
        self.last_verdict
    }

    /// Total calibration runs.
    pub fn total_runs(&self) -> u64 {
        self.total_runs
    }

    /// Number of results stored.
    pub fn result_count(&self) -> usize {
        self.results.len()
    }

    /// Snapshot of current state.
    pub fn snapshot(&self) -> CalibrationSnapshot {
        let passing = self.results.iter().filter(|r| r.passes_gate).count();
        let failing = self.results.len() - passing;
        CalibrationSnapshot {
            total_runs: self.total_runs,
            scenario_results: self.results.clone(),
            verdict: self.last_verdict,
            passing_count: passing,
            failing_count: failing,
        }
    }

    /// Human-readable status line.
    pub fn status_line(&self) -> String {
        let snap = self.snapshot();
        format!(
            "calibration[{}] runs={} pass={} fail={}",
            snap.verdict, snap.total_runs, snap.passing_count, snap.failing_count,
        )
    }

    /// Reset all results.
    pub fn reset(&mut self) {
        self.results.clear();
        self.total_runs = 0;
        self.last_verdict = PromotionVerdict::Rejected;
    }

    /// Clear results but keep total_runs count.
    pub fn clear_results(&mut self) {
        self.results.clear();
        self.last_verdict = PromotionVerdict::Rejected;
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> CalibrationDegradation {
        match self.last_verdict {
            PromotionVerdict::Approved => CalibrationDegradation::Healthy,
            PromotionVerdict::ConditionalHold => CalibrationDegradation::GateMarginal {
                passing: self.results.iter().filter(|r| r.passes_gate).count(),
                total: self.results.len(),
            },
            PromotionVerdict::Rejected => CalibrationDegradation::GateFailed {
                failing: self.results.iter().filter(|r| !r.passes_gate).count(),
                total: self.results.len(),
            },
        }
    }

    /// Generate structured log entry.
    pub fn log_entry(&self) -> CalibrationLogEntry {
        CalibrationLogEntry {
            total_runs: self.total_runs,
            result_count: self.results.len(),
            verdict: self.last_verdict,
            degradation: self.detect_degradation(),
        }
    }

    // ── D4 Impl: Bridge Methods and Convenience API ────────────────

    /// Submit a batch of results and evaluate.
    pub fn submit_batch(&mut self, results: Vec<CalibrationResult>) -> PromotionVerdict {
        for r in results {
            self.submit(r);
        }
        self.evaluate()
    }

    /// Average false positive rate across all results.
    pub fn avg_fpr(&self) -> f64 {
        if self.results.is_empty() {
            return 0.0;
        }
        self.results
            .iter()
            .map(|r| r.false_positive_rate)
            .sum::<f64>()
            / self.results.len() as f64
    }

    /// Average miss rate across all results.
    pub fn avg_miss_rate(&self) -> f64 {
        if self.results.is_empty() {
            return 0.0;
        }
        self.results.iter().map(|r| r.miss_rate).sum::<f64>() / self.results.len() as f64
    }

    /// Average detection delay across all results.
    pub fn avg_detection_delay(&self) -> f64 {
        if self.results.is_empty() {
            return 0.0;
        }
        self.results.iter().map(|r| r.detection_delay).sum::<f64>() / self.results.len() as f64
    }

    /// Passing count.
    pub fn passing_count(&self) -> usize {
        self.results.iter().filter(|r| r.passes_gate).count()
    }

    /// Failing count.
    pub fn failing_count(&self) -> usize {
        self.results.iter().filter(|r| !r.passes_gate).count()
    }

    /// Whether the harness has approved promotion.
    pub fn is_approved(&self) -> bool {
        self.last_verdict == PromotionVerdict::Approved
    }

    /// Set max FPR gate.
    pub fn set_max_fpr(&mut self, fpr: f64) {
        self.config.max_fpr = fpr;
    }

    /// Set strict mode.
    pub fn set_strict(&mut self, strict: bool) {
        self.config.strict = strict;
    }

    /// Results by scenario.
    pub fn results_for_scenario(&self, scenario: CalibrationScenario) -> Vec<&CalibrationResult> {
        self.results
            .iter()
            .filter(|r| r.scenario == scenario)
            .collect()
    }
}

/// Degradation status for the calibration harness.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CalibrationDegradation {
    Healthy,
    GateMarginal { passing: usize, total: usize },
    GateFailed { failing: usize, total: usize },
}

impl fmt::Display for CalibrationDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::GateMarginal { passing, total } => {
                write!(f, "marginal({passing}/{total})")
            }
            Self::GateFailed { failing, total } => {
                write!(f, "failed({failing}/{total})")
            }
        }
    }
}

/// Structured log entry for the calibration harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationLogEntry {
    pub total_runs: u64,
    pub result_count: usize,
    pub verdict: PromotionVerdict,
    pub degradation: CalibrationDegradation,
}
