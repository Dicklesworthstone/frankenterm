//! Finish-line release-readiness gate evaluation for the `ft-xbnl0` program.
//!
//! This module turns finish-line evidence into explicit go/no-go checks so
//! release work can consume a stable gate contract instead of re-deriving
//! thresholds from chat history or ad hoc spreadsheets.

use serde::{Deserialize, Serialize};

/// Explicit release decision produced from the gate set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseDecision {
    /// All blocking gates passed.
    Ready,
    /// One or more blocking gates failed.
    Blocked,
}

/// A single gate check result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseGateCheck {
    /// Stable check identifier.
    pub gate_id: String,
    /// Human-readable description of the gate.
    pub description: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Whether a failure blocks release.
    pub blocking: bool,
    /// Observed value or state.
    pub observed: String,
    /// Required threshold or condition.
    pub required: String,
    /// Action to take when the gate fails.
    pub action: String,
}

/// Overall release gate report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseGateReport {
    /// Final release decision.
    pub decision: ReleaseDecision,
    /// Individual gate results.
    pub checks: Vec<ReleaseGateCheck>,
}

impl ReleaseGateReport {
    /// Number of failed checks.
    #[must_use]
    pub fn failed_count(&self) -> usize {
        self.checks.iter().filter(|check| !check.passed).count()
    }

    /// Render a concise operator summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("=== Release Decision: {:?} ===", self.decision));
        for check in &self.checks {
            let icon = if check.passed { "[PASS]" } else { "[FAIL]" };
            let blocking = if check.blocking { " (blocking)" } else { "" };
            lines.push(format!(
                "{} {}{} — {} [observed: {}; required: {}]",
                icon, check.gate_id, blocking, check.description, check.observed, check.required
            ));
            if !check.passed {
                lines.push(format!("  action: {}", check.action));
            }
        }
        lines.join("\n")
    }
}

/// Threshold policy for finish-line release gates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseGatePolicy {
    /// Required pane-scale set for soak validation.
    pub required_pane_scales: Vec<u64>,
    /// Exact number of stress metrics expected in each soak profile.
    pub required_metric_count: usize,
    /// Minimum number of release cycles in the soak matrix.
    pub min_release_cycles: usize,
    /// Maximum allowed aggregate peak RSS across validated soak summaries.
    pub max_peak_rss_mb: f64,
    /// Maximum allowed aggregate max-duration across validated soak summaries.
    pub max_duration_s: f64,
}

impl ReleaseGatePolicy {
    /// Default finish-line policy for the `ft-xbnl0` release path.
    #[must_use]
    pub fn finish_line() -> Self {
        Self {
            required_pane_scales: vec![1, 50, 100, 200],
            required_metric_count: 8,
            min_release_cycles: 3,
            max_peak_rss_mb: 32.0,
            max_duration_s: 3.0,
        }
    }

    /// Evaluate the release gate inputs against this policy.
    #[must_use]
    pub fn evaluate(&self, inputs: &ReleaseGateInputs) -> ReleaseGateReport {
        let checks = vec![
            ReleaseGateCheck {
                gate_id: "REL-01-leak-oracle".into(),
                description: "Deterministic leak-oracle evidence exists and passed".into(),
                passed: inputs.leak.summary_present && inputs.leak.summary_passed,
                blocking: true,
                observed: if inputs.leak.summary_present {
                    if inputs.leak.summary_passed {
                        format!("summary passed at {}", inputs.leak.summary_path)
                    } else {
                        format!("summary failed at {}", inputs.leak.summary_path)
                    }
                } else {
                    "no leak summary artifact".into()
                },
                required: "latest ft-xbnl0.4.4 summary.json present with status=passed".into(),
                action: "Re-run the ft-xbnl0.4.4 leak-oracle harness or fix the failing deterministic leak regression lane.".into(),
            },
            ReleaseGateCheck {
                gate_id: "REL-02-guard-surface".into(),
                description: "Permanent finish-line guard surface is green".into(),
                passed: inputs.guard_contract_passed,
                blocking: true,
                observed: if inputs.guard_contract_passed {
                    "finish-line guards passed".into()
                } else {
                    "finish-line guards failed or missing".into()
                },
                required: "docs/ft-xbnl0-5-2-finish-line-guards-validation.json status=passed".into(),
                action: "Fix the failing permanent guard before relying on release-readiness claims.".into(),
            },
            ReleaseGateCheck {
                gate_id: "REL-03-soak-confidence".into(),
                description: "Representative soak matrix includes smoke plus stable release cycles".into(),
                passed: inputs.soak.wrapper_present
                    && inputs.soak.wrapper_passed
                    && inputs.soak.smoke_cycles >= 1
                    && inputs.soak.release_cycles >= self.min_release_cycles
                    && inputs.soak.release_consistent
                    && inputs.soak.pane_scales == self.required_pane_scales,
                blocking: true,
                observed: format!(
                    "wrapper_present={} wrapper_passed={} smoke_cycles={} release_cycles={} consistent={} pane_scales={:?}",
                    inputs.soak.wrapper_present,
                    inputs.soak.wrapper_passed,
                    inputs.soak.smoke_cycles,
                    inputs.soak.release_cycles,
                    inputs.soak.release_consistent,
                    inputs.soak.pane_scales
                ),
                required: format!(
                    "wrapper passed, >=1 smoke cycle, >={} release cycles, consistent profiles, pane scales {:?}",
                    self.min_release_cycles, self.required_pane_scales
                ),
                action: "Re-run ft-xbnl0.4.5 soak coverage until the smoke/release matrix is complete and internally consistent.".into(),
            },
            ReleaseGateCheck {
                gate_id: "REL-04-performance-budget".into(),
                description: "Soak summaries stay within explicit performance budgets".into(),
                passed: inputs.soak.metric_count == self.required_metric_count
                    && inputs.soak.peak_rss_mb <= self.max_peak_rss_mb
                    && inputs.soak.max_duration_s <= self.max_duration_s
                    && inputs.soak.backpressure_exercised,
                blocking: true,
                observed: format!(
                    "metric_count={} peak_rss_mb={:.3} max_duration_s={:.3} backpressure_exercised={}",
                    inputs.soak.metric_count,
                    inputs.soak.peak_rss_mb,
                    inputs.soak.max_duration_s,
                    inputs.soak.backpressure_exercised
                ),
                required: format!(
                    "metric_count={} peak_rss_mb<={:.1} max_duration_s<={:.1} and intentional backpressure reached",
                    self.required_metric_count, self.max_peak_rss_mb, self.max_duration_s
                ),
                action: "Tighten the workload implementation or tune the runtime until the 4.5 soak matrix stays within the declared RSS/duration budgets.".into(),
            },
        ];

        let decision = if checks.iter().any(|check| check.blocking && !check.passed) {
            ReleaseDecision::Blocked
        } else {
            ReleaseDecision::Ready
        };

        ReleaseGateReport { decision, checks }
    }
}

/// Leak-oracle evidence status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeakEvidenceStatus {
    /// Whether a summary artifact was found.
    pub summary_present: bool,
    /// Whether the summary artifact passed.
    pub summary_passed: bool,
    /// Path used for the evaluation.
    pub summary_path: String,
}

/// Soak-matrix evidence status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoakEvidenceStatus {
    /// Whether the wrapper summary artifact was found.
    pub wrapper_present: bool,
    /// Whether the wrapper summary passed.
    pub wrapper_passed: bool,
    /// Number of smoke cycles captured.
    pub smoke_cycles: usize,
    /// Number of release cycles captured.
    pub release_cycles: usize,
    /// Whether release profiles agreed on the core scenario shape.
    pub release_consistent: bool,
    /// Exact pane scales observed in the soak summary set.
    pub pane_scales: Vec<u64>,
    /// Exact metric count observed in the soak summary set.
    pub metric_count: usize,
    /// Highest observed peak RSS across the validated soak summaries.
    pub peak_rss_mb: f64,
    /// Highest observed duration across the validated soak summaries.
    pub max_duration_s: f64,
    /// Whether the intentional backpressure scenario reached the saturated tier.
    pub backpressure_exercised: bool,
}

/// Inputs needed for a release readiness evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseGateInputs {
    /// Deterministic leak evidence.
    pub leak: LeakEvidenceStatus,
    /// Soak confidence and performance evidence.
    pub soak: SoakEvidenceStatus,
    /// Whether the permanent finish-line guard surface is green.
    pub guard_contract_passed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing_inputs() -> ReleaseGateInputs {
        ReleaseGateInputs {
            leak: LeakEvidenceStatus {
                summary_present: true,
                summary_passed: true,
                summary_path: "tests/e2e/artifacts/goal-line/ft-xbnl0.4.4/run/summary.json".into(),
            },
            soak: SoakEvidenceStatus {
                wrapper_present: true,
                wrapper_passed: true,
                smoke_cycles: 1,
                release_cycles: 3,
                release_consistent: true,
                pane_scales: vec![1, 50, 100, 200],
                metric_count: 8,
                peak_rss_mb: 16.61,
                max_duration_s: 2.23,
                backpressure_exercised: true,
            },
            guard_contract_passed: true,
        }
    }

    #[test]
    fn ft_xbnl0_4_6_release_gate_ready_when_all_blocking_checks_pass() {
        let report = ReleaseGatePolicy::finish_line().evaluate(&passing_inputs());
        assert_eq!(report.decision, ReleaseDecision::Ready);
        assert_eq!(report.failed_count(), 0);
    }

    #[test]
    fn ft_xbnl0_4_6_release_gate_blocks_missing_leak_summary() {
        let mut inputs = passing_inputs();
        inputs.leak.summary_present = false;
        inputs.leak.summary_passed = false;
        inputs.leak.summary_path.clear();

        let report = ReleaseGatePolicy::finish_line().evaluate(&inputs);
        assert_eq!(report.decision, ReleaseDecision::Blocked);
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.gate_id == "REL-01-leak-oracle" && !check.passed)
        );
    }

    #[test]
    fn ft_xbnl0_4_6_release_gate_blocks_incomplete_soak_matrix() {
        let mut inputs = passing_inputs();
        inputs.soak.release_cycles = 2;
        inputs.soak.release_consistent = false;

        let report = ReleaseGatePolicy::finish_line().evaluate(&inputs);
        assert_eq!(report.decision, ReleaseDecision::Blocked);
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.gate_id == "REL-03-soak-confidence" && !check.passed)
        );
    }

    #[test]
    fn ft_xbnl0_4_6_release_gate_blocks_budget_regressions() {
        let mut inputs = passing_inputs();
        inputs.soak.metric_count = 7;
        inputs.soak.peak_rss_mb = 48.0;
        inputs.soak.max_duration_s = 4.5;
        inputs.soak.backpressure_exercised = false;

        let report = ReleaseGatePolicy::finish_line().evaluate(&inputs);
        assert_eq!(report.decision, ReleaseDecision::Blocked);
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.gate_id == "REL-04-performance-budget" && !check.passed)
        );
    }

    #[test]
    fn ft_xbnl0_4_6_release_gate_blocks_guard_contract_failure() {
        let mut inputs = passing_inputs();
        inputs.guard_contract_passed = false;

        let report = ReleaseGatePolicy::finish_line().evaluate(&inputs);
        assert_eq!(report.decision, ReleaseDecision::Blocked);
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.gate_id == "REL-02-guard-surface" && !check.passed)
        );
    }
}
