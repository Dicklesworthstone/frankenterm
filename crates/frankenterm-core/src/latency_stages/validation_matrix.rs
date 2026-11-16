//! Unified E2E, chaos, soak, and performance validation matrix.

use super::{InvariantDomain, LatencyStage};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Test scenario category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScenarioCategory {
    /// Happy-path end-to-end.
    E2E,
    /// Fault injection / chaos engineering.
    Chaos,
    /// Long-running soak / endurance.
    Soak,
    /// Performance / latency regression.
    Performance,
}

impl fmt::Display for ScenarioCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::E2E => write!(f, "e2e"),
            Self::Chaos => write!(f, "chaos"),
            Self::Soak => write!(f, "soak"),
            Self::Performance => write!(f, "performance"),
        }
    }
}

/// Verdict from running a scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScenarioVerdict {
    Pass,
    Fail,
    Skip,
    Flaky,
}

impl fmt::Display for ScenarioVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass => write!(f, "pass"),
            Self::Fail => write!(f, "fail"),
            Self::Skip => write!(f, "skip"),
            Self::Flaky => write!(f, "flaky"),
        }
    }
}

/// A single scenario in the validation matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixScenario {
    /// Unique scenario ID.
    pub scenario_id: String,
    /// Category.
    pub category: ScenarioCategory,
    /// Human-readable description.
    pub description: String,
    /// Stages touched by this scenario.
    pub stages: Vec<LatencyStage>,
    /// Invariant domain under test.
    pub domain: InvariantDomain,
    /// Whether this scenario is required for promotion.
    pub required_for_promotion: bool,
}

/// Result of running a scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioResult {
    /// Scenario ID.
    pub scenario_id: String,
    /// Verdict.
    pub verdict: ScenarioVerdict,
    /// Duration in microseconds.
    pub duration_us: u64,
    /// Optional failure message.
    pub failure_message: Option<String>,
    /// Artifacts produced (file paths, checksums, etc.).
    pub artifacts: Vec<String>,
}

/// Promotion gate: a set of scenarios that must pass for CI promotion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionGate {
    /// Gate name, for example "canary", "staging", or "production".
    pub name: String,
    /// Required scenario IDs that must pass.
    pub required_scenarios: Vec<String>,
    /// Minimum pass rate across all scenarios (0.0..=1.0).
    pub min_pass_rate: f64,
    /// Max allowed flaky scenario count.
    pub max_flaky_count: u32,
}

/// Snapshot of the validation matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixSnapshot {
    /// Total scenarios.
    pub total_scenarios: u64,
    pub pass_count: u64,
    pub fail_count: u64,
    pub skip_count: u64,
    pub flaky_count: u64,
}

/// Degradation state for the matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatrixDegradation {
    /// All required scenarios passing.
    Healthy,
    /// Some flaky scenarios.
    FlakyDetected { flaky_count: u64 },
    /// Required scenarios failing; blocks promotion.
    GateFailure { failed_scenarios: Vec<String> },
}

impl fmt::Display for MatrixDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::FlakyDetected { flaky_count } => write!(f, "flaky({flaky_count})"),
            Self::GateFailure { failed_scenarios } => {
                write!(f, "gate-failure({})", failed_scenarios.len())
            }
        }
    }
}

/// Log entry for matrix events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Scenario that triggered the event.
    pub scenario_id: String,
    /// Event description.
    pub event: String,
}

/// Manages the validation matrix.
#[derive(Default)]
pub struct ValidationMatrix {
    scenarios: Vec<MatrixScenario>,
    results: Vec<ScenarioResult>,
    gates: Vec<PromotionGate>,
}

impl ValidationMatrix {
    /// Create a new empty matrix.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a scenario.
    pub fn add_scenario(&mut self, scenario: MatrixScenario) {
        self.scenarios.push(scenario);
    }

    /// Register a promotion gate.
    pub fn add_gate(&mut self, gate: PromotionGate) {
        self.gates.push(gate);
    }

    /// Record a scenario result.
    pub fn record_result(&mut self, result: ScenarioResult) {
        self.results.push(result);
    }

    /// Get all results for a scenario.
    pub fn results_for(&self, scenario_id: &str) -> Vec<&ScenarioResult> {
        self.results
            .iter()
            .filter(|r| r.scenario_id == scenario_id)
            .collect()
    }

    /// Latest result for a scenario.
    pub fn latest_result(&self, scenario_id: &str) -> Option<&ScenarioResult> {
        self.results
            .iter()
            .rev()
            .find(|r| r.scenario_id == scenario_id)
    }

    /// Check if a promotion gate passes.
    pub fn check_gate(&self, gate_name: &str) -> bool {
        let gate = match self.gates.iter().find(|g| g.name == gate_name) {
            Some(g) => g,
            None => return false,
        };
        for sid in &gate.required_scenarios {
            match self.latest_result(sid) {
                Some(r) if r.verdict == ScenarioVerdict::Pass => {}
                _ => return false,
            }
        }
        let total = self.results.len() as f64;
        if total == 0.0 {
            return false;
        }
        let passes = self
            .results
            .iter()
            .filter(|r| r.verdict == ScenarioVerdict::Pass)
            .count() as f64;
        if passes / total < gate.min_pass_rate {
            return false;
        }
        let flaky = self
            .results
            .iter()
            .filter(|r| r.verdict == ScenarioVerdict::Flaky)
            .count() as u32;
        flaky <= gate.max_flaky_count
    }

    /// Number of scenarios.
    pub fn scenario_count(&self) -> usize {
        self.scenarios.len()
    }

    /// Number of results recorded.
    pub fn result_count(&self) -> usize {
        self.results.len()
    }

    /// Get a snapshot.
    pub fn snapshot(&self) -> MatrixSnapshot {
        let pass_count = self
            .results
            .iter()
            .filter(|r| r.verdict == ScenarioVerdict::Pass)
            .count() as u64;
        let fail_count = self
            .results
            .iter()
            .filter(|r| r.verdict == ScenarioVerdict::Fail)
            .count() as u64;
        let skip_count = self
            .results
            .iter()
            .filter(|r| r.verdict == ScenarioVerdict::Skip)
            .count() as u64;
        let flaky_count = self
            .results
            .iter()
            .filter(|r| r.verdict == ScenarioVerdict::Flaky)
            .count() as u64;
        MatrixSnapshot {
            total_scenarios: self.scenarios.len() as u64,
            pass_count,
            fail_count,
            skip_count,
            flaky_count,
        }
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> MatrixDegradation {
        let flaky_count = self
            .results
            .iter()
            .filter(|r| r.verdict == ScenarioVerdict::Flaky)
            .count() as u64;
        let failed_required: Vec<String> = self
            .scenarios
            .iter()
            .filter(|s| s.required_for_promotion)
            .filter(|s| {
                self.latest_result(&s.scenario_id)
                    .is_none_or(|r| r.verdict != ScenarioVerdict::Pass)
            })
            .map(|s| s.scenario_id.clone())
            .collect();
        if !failed_required.is_empty() {
            MatrixDegradation::GateFailure {
                failed_scenarios: failed_required,
            }
        } else if flaky_count > 0 {
            MatrixDegradation::FlakyDetected { flaky_count }
        } else {
            MatrixDegradation::Healthy
        }
    }

    /// Create a log entry.
    pub fn log_entry(
        &self,
        scenario_id: String,
        event: String,
        timestamp_us: u64,
    ) -> MatrixLogEntry {
        MatrixLogEntry {
            timestamp_us,
            scenario_id,
            event,
        }
    }

    /// Scenarios by category.
    pub fn scenarios_by_category(&self, category: ScenarioCategory) -> Vec<&MatrixScenario> {
        self.scenarios
            .iter()
            .filter(|s| s.category == category)
            .collect()
    }

    /// Reset all results.
    pub fn reset_results(&mut self) {
        self.results.clear();
    }

    /// Access gates.
    pub fn gates(&self) -> &[PromotionGate] {
        &self.gates
    }

    /// Access scenarios.
    pub fn scenarios(&self) -> &[MatrixScenario] {
        &self.scenarios
    }

    /// Pass rate across all results (0.0..=1.0).
    pub fn pass_rate(&self) -> f64 {
        let total = self.results.len() as f64;
        if total == 0.0 {
            return 1.0;
        }
        let passes = self
            .results
            .iter()
            .filter(|r| r.verdict == ScenarioVerdict::Pass)
            .count() as f64;
        passes / total
    }

    /// Flaky rate across all results (0.0..=1.0).
    pub fn flaky_rate(&self) -> f64 {
        let total = self.results.len() as f64;
        if total == 0.0 {
            return 0.0;
        }
        let flaky = self
            .results
            .iter()
            .filter(|r| r.verdict == ScenarioVerdict::Flaky)
            .count() as f64;
        flaky / total
    }

    /// Mean duration across all pass results, in microseconds.
    pub fn mean_pass_duration_us(&self) -> f64 {
        let passes: Vec<u64> = self
            .results
            .iter()
            .filter(|r| r.verdict == ScenarioVerdict::Pass)
            .map(|r| r.duration_us)
            .collect();
        if passes.is_empty() {
            return 0.0;
        }
        passes.iter().sum::<u64>() as f64 / passes.len() as f64
    }

    /// Check all gates; returns list of gate names that pass.
    pub fn passing_gates(&self) -> Vec<String> {
        self.gates
            .iter()
            .filter(|g| self.check_gate(&g.name))
            .map(|g| g.name.clone())
            .collect()
    }

    /// Check all gates; returns list of gate names that fail.
    pub fn failing_gates(&self) -> Vec<String> {
        self.gates
            .iter()
            .filter(|g| !self.check_gate(&g.name))
            .map(|g| g.name.clone())
            .collect()
    }

    /// Get required scenarios that don't have a passing result.
    pub fn missing_required(&self) -> Vec<String> {
        self.scenarios
            .iter()
            .filter(|s| s.required_for_promotion)
            .filter(|s| {
                self.latest_result(&s.scenario_id)
                    .is_none_or(|r| r.verdict != ScenarioVerdict::Pass)
            })
            .map(|s| s.scenario_id.clone())
            .collect()
    }

    /// All artifacts across all results.
    pub fn all_artifacts(&self) -> Vec<String> {
        self.results
            .iter()
            .flat_map(|r| r.artifacts.clone())
            .collect()
    }

    /// Map to InvariantDomain.
    pub fn to_invariant_domain() -> InvariantDomain {
        InvariantDomain::Composition
    }
}
