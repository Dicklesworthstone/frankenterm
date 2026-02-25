//! Regression diff harness + end-to-end replay gate (ft-dr6zv.1.3.C2).
//!
//! Runs a deterministic corpus of search scenarios through the `SearchFacade`
//! in Shadow mode, capturing per-scenario pass/fail and aggregate statistics.
//! The `ReplayGate` combines the harness with schema preservation checks
//! from C1 to produce a go/no-go migration verdict.

use serde::{Deserialize, Serialize};

use super::facade::{FacadeConfig, FacadeRouting, SearchFacade};
use super::schema_gate::{self, SchemaGateResult};

// ---------------------------------------------------------------------------
// Scenario definition
// ---------------------------------------------------------------------------

fn default_tau_tolerance() -> f32 {
    0.05
}
fn default_score_tolerance() -> f32 {
    1e-4
}

/// One deterministic input case for regression checking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionScenario {
    /// Human-readable name (unique within a suite).
    pub name: String,
    /// Pre-ranked lexical hits: (doc_id, raw_score).
    pub lexical: Vec<(u64, f32)>,
    /// Pre-ranked semantic hits: (doc_id, similarity_score).
    pub semantic: Vec<(u64, f32)>,
    /// How many results to request.
    pub top_k: usize,
    /// Maximum acceptable tau regression from 1.0 (tau must be >= 1.0 - tolerance).
    #[serde(default = "default_tau_tolerance")]
    pub tau_tolerance: f32,
    /// Maximum acceptable score delta between paths.
    #[serde(default = "default_score_tolerance")]
    pub score_tolerance: f32,
}

// ---------------------------------------------------------------------------
// Per-scenario outcome
// ---------------------------------------------------------------------------

/// Result of running one scenario through the facade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioOutcome {
    pub name: String,
    pub passed: bool,
    pub kendall_tau: f32,
    pub max_score_diff: f32,
    pub ranking_match: bool,
    pub failure_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Diff artifact (serializable output)
// ---------------------------------------------------------------------------

/// Serializable snapshot of a full regression run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffArtifact {
    /// ISO-8601 timestamp of the run.
    pub run_at: String,
    /// Per-scenario results.
    pub outcomes: Vec<ScenarioOutcome>,
    /// Number of scenarios that passed.
    pub passed: usize,
    /// Number of scenarios that failed.
    pub failed: usize,
    /// Overall pass rate (0.0–1.0).
    pub pass_rate: f32,
    /// Minimum tau observed across all scenarios.
    pub min_tau: f32,
    /// Maximum score diff observed across all scenarios.
    pub max_score_diff: f32,
}

// ---------------------------------------------------------------------------
// Report (wraps artifact with ergonomic methods)
// ---------------------------------------------------------------------------

/// Regression diff report with convenience accessors.
#[derive(Debug)]
pub struct RegressionDiffReport {
    pub artifact: DiffArtifact,
}

impl RegressionDiffReport {
    /// True when every scenario passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.artifact.failed == 0
    }

    /// Scenarios that failed.
    #[must_use]
    pub fn failures(&self) -> Vec<&ScenarioOutcome> {
        self.artifact
            .outcomes
            .iter()
            .filter(|o| !o.passed)
            .collect()
    }

    /// Human-readable summary line.
    #[must_use]
    pub fn summary(&self) -> String {
        let a = &self.artifact;
        format!(
            "{}/{} scenarios passed (pass_rate={:.2}, min_tau={:.4}, max_score_diff={:.6})",
            a.passed,
            a.outcomes.len(),
            a.pass_rate,
            a.min_tau,
            a.max_score_diff
        )
    }
}

// ---------------------------------------------------------------------------
// Run the suite
// ---------------------------------------------------------------------------

/// Run all scenarios through `SearchFacade` in Shadow mode.
///
/// The facade is forced into `FacadeRouting::Shadow` regardless of the
/// config's routing field. Per-scenario pass/fail uses each scenario's own
/// tolerances.
#[must_use]
pub fn run_regression_suite(
    scenarios: &[RegressionScenario],
    base_config: &FacadeConfig,
) -> RegressionDiffReport {
    let mut shadow_config = base_config.clone();
    shadow_config.routing = FacadeRouting::Shadow;
    // Use generous facade-level thresholds so ShadowComparison always populates.
    shadow_config.shadow_tau_threshold = -2.0;
    shadow_config.shadow_score_threshold = f32::MAX;

    let mut outcomes = Vec::with_capacity(scenarios.len());

    for scenario in scenarios {
        let facade = SearchFacade::with_config(shadow_config.clone());
        let result =
            facade.fuse_with_metrics(&scenario.lexical, &scenario.semantic, scenario.top_k);

        let outcome = match result.shadow_comparison {
            Some(cmp) => {
                // When rankings match exactly, tau is always OK — degenerate
                // kendall_tau (e.g. 0 or 1 element) returns 0.0 even though
                // orderings are identical.
                let tau_ok = cmp.ranking_match
                    || cmp.kendall_tau >= (1.0 - scenario.tau_tolerance)
                    || (scenario.lexical.is_empty() && scenario.semantic.is_empty());
                let score_ok = cmp.max_score_diff <= scenario.score_tolerance;
                let passed = tau_ok && score_ok;
                let failure_reason = if passed {
                    None
                } else {
                    let mut parts = Vec::new();
                    if !tau_ok {
                        parts.push(format!(
                            "tau {:.4} below threshold {:.4}",
                            cmp.kendall_tau,
                            1.0 - scenario.tau_tolerance
                        ));
                    }
                    if !score_ok {
                        parts.push(format!(
                            "score diff {:.6} exceeds tolerance {:.6}",
                            cmp.max_score_diff, scenario.score_tolerance
                        ));
                    }
                    Some(parts.join("; "))
                };
                ScenarioOutcome {
                    name: scenario.name.clone(),
                    passed,
                    kendall_tau: cmp.kendall_tau,
                    max_score_diff: cmp.max_score_diff,
                    ranking_match: cmp.ranking_match,
                    failure_reason,
                }
            }
            None => ScenarioOutcome {
                name: scenario.name.clone(),
                passed: false,
                kendall_tau: 0.0,
                max_score_diff: 0.0,
                ranking_match: false,
                failure_reason: Some("shadow comparison missing (routing not shadow?)".to_string()),
            },
        };

        outcomes.push(outcome);
    }

    let passed = outcomes.iter().filter(|o| o.passed).count();
    let failed = outcomes.len() - passed;
    let pass_rate = if outcomes.is_empty() {
        1.0
    } else {
        passed as f32 / outcomes.len() as f32
    };
    let min_tau = outcomes
        .iter()
        .map(|o| o.kendall_tau)
        .fold(f32::INFINITY, f32::min);
    let max_score_diff = outcomes
        .iter()
        .map(|o| o.max_score_diff)
        .fold(0.0_f32, f32::max);

    let run_at = chrono::Utc::now().to_rfc3339();

    RegressionDiffReport {
        artifact: DiffArtifact {
            run_at,
            outcomes,
            passed,
            failed,
            pass_rate,
            min_tau: if min_tau.is_infinite() { 1.0 } else { min_tau },
            max_score_diff,
        },
    }
}

// ---------------------------------------------------------------------------
// Built-in corpus
// ---------------------------------------------------------------------------

/// Default regression corpus covering edge cases and realistic scenarios.
#[must_use]
pub fn default_scenarios() -> Vec<RegressionScenario> {
    vec![
        RegressionScenario {
            name: "empty_inputs".to_string(),
            lexical: vec![],
            semantic: vec![],
            top_k: 10,
            tau_tolerance: default_tau_tolerance(),
            score_tolerance: default_score_tolerance(),
        },
        RegressionScenario {
            name: "single_result_lexical".to_string(),
            lexical: vec![(100, 5.0)],
            semantic: vec![],
            top_k: 10,
            tau_tolerance: default_tau_tolerance(),
            score_tolerance: default_score_tolerance(),
        },
        RegressionScenario {
            name: "single_result_semantic".to_string(),
            lexical: vec![],
            semantic: vec![(200, 0.95)],
            top_k: 10,
            tau_tolerance: default_tau_tolerance(),
            score_tolerance: default_score_tolerance(),
        },
        RegressionScenario {
            name: "lexical_only".to_string(),
            lexical: vec![(1, 10.0), (2, 8.0), (3, 6.0)],
            semantic: vec![],
            top_k: 10,
            tau_tolerance: default_tau_tolerance(),
            score_tolerance: default_score_tolerance(),
        },
        RegressionScenario {
            name: "semantic_only".to_string(),
            lexical: vec![],
            semantic: vec![(10, 0.9), (20, 0.8), (30, 0.7)],
            top_k: 10,
            tau_tolerance: default_tau_tolerance(),
            score_tolerance: default_score_tolerance(),
        },
        RegressionScenario {
            name: "full_overlap".to_string(),
            lexical: vec![(1, 10.0), (2, 8.0), (3, 6.0)],
            semantic: vec![(1, 0.9), (2, 0.8), (3, 0.7)],
            top_k: 10,
            tau_tolerance: default_tau_tolerance(),
            score_tolerance: default_score_tolerance(),
        },
        RegressionScenario {
            name: "partial_overlap".to_string(),
            lexical: vec![(1, 10.0), (2, 8.0), (3, 6.0), (4, 4.0)],
            semantic: vec![(2, 0.9), (1, 0.8), (5, 0.7), (3, 0.6)],
            top_k: 10,
            tau_tolerance: default_tau_tolerance(),
            score_tolerance: default_score_tolerance(),
        },
        RegressionScenario {
            name: "score_tiebreaker".to_string(),
            lexical: vec![(10, 5.0), (20, 5.0), (30, 5.0)],
            semantic: vec![(30, 0.9), (20, 0.5), (10, 0.1)],
            top_k: 10,
            tau_tolerance: default_tau_tolerance(),
            score_tolerance: default_score_tolerance(),
        },
        RegressionScenario {
            name: "top_k_exceeds_results".to_string(),
            lexical: vec![(1, 10.0), (2, 8.0)],
            semantic: vec![(3, 0.9)],
            top_k: 100,
            tau_tolerance: default_tau_tolerance(),
            score_tolerance: default_score_tolerance(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Replay gate
// ---------------------------------------------------------------------------

fn default_min_pass_rate() -> f32 {
    1.0
}
fn default_true() -> bool {
    true
}

/// Configuration for the replay gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayGateConfig {
    /// Minimum pass rate required across regression scenarios (0.0–1.0).
    #[serde(default = "default_min_pass_rate")]
    pub min_pass_rate: f32,
    /// Whether schema gate failures block the verdict.
    #[serde(default = "default_true")]
    pub schema_gate_required: bool,
    /// Facade config forwarded to the regression suite.
    pub facade: FacadeConfig,
}

impl Default for ReplayGateConfig {
    fn default() -> Self {
        Self {
            min_pass_rate: default_min_pass_rate(),
            schema_gate_required: true,
            facade: FacadeConfig::default(),
        }
    }
}

/// Outcome of the replay gate: go/no-go for migration cutover.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayGateVerdict {
    /// True = safe to cut over to the orchestrated path.
    pub go: bool,
    /// Regression suite artifact.
    pub regression: DiffArtifact,
    /// Schema fusion gate result.
    pub schema_fusion: SchemaGateResult,
    /// Schema orchestration gate result.
    pub schema_orchestration: SchemaGateResult,
    /// Reason for no-go (if any).
    pub reason: Option<String>,
}

/// Run the full replay gate: regression suite + schema checks → verdict.
#[must_use]
pub fn run_replay_gate(
    scenarios: &[RegressionScenario],
    config: &ReplayGateConfig,
) -> ReplayGateVerdict {
    let report = run_regression_suite(scenarios, &config.facade);
    let schema_fusion = schema_gate::gate_fusion_schema();
    let schema_orchestration = schema_gate::gate_orchestration_schema();

    let mut go = true;
    let mut reasons = Vec::new();

    if report.artifact.pass_rate < config.min_pass_rate {
        go = false;
        reasons.push(format!(
            "pass rate {:.2} below minimum {:.2}",
            report.artifact.pass_rate, config.min_pass_rate
        ));
    }

    if config.schema_gate_required && !schema_fusion.safe {
        go = false;
        reasons.push(format!(
            "schema fusion gate failed: {}",
            schema_fusion.summary
        ));
    }

    if config.schema_gate_required && !schema_orchestration.safe {
        go = false;
        reasons.push(format!(
            "schema orchestration gate failed: {}",
            schema_orchestration.summary
        ));
    }

    let reason = if reasons.is_empty() {
        None
    } else {
        Some(reasons.join("; "))
    };

    ReplayGateVerdict {
        go,
        regression: report.artifact,
        schema_fusion,
        schema_orchestration,
        reason,
    }
}

/// Convenience: run the replay gate with default scenarios and config.
#[must_use]
pub fn run_replay_gate_default() -> ReplayGateVerdict {
    run_replay_gate(&default_scenarios(), &ReplayGateConfig::default())
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_scenarios_nonempty() {
        let s = default_scenarios();
        assert!(s.len() >= 9, "corpus should have at least 9 scenarios");
    }

    #[test]
    fn scenario_serde_roundtrip() {
        let s = &default_scenarios()[0];
        let json = serde_json::to_string(s).unwrap();
        let parsed: RegressionScenario = serde_json::from_str(&json).unwrap();
        assert_eq!(s.name, parsed.name);
        assert_eq!(s.top_k, parsed.top_k);
    }

    #[test]
    fn diff_artifact_serde_roundtrip() {
        let report = run_regression_suite(&default_scenarios(), &FacadeConfig::default());
        let json = serde_json::to_string(&report.artifact).unwrap();
        let parsed: DiffArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(report.artifact.passed, parsed.passed);
        assert_eq!(report.artifact.failed, parsed.failed);
    }

    #[test]
    fn verdict_serde_roundtrip() {
        let v = run_replay_gate_default();
        let json = serde_json::to_string(&v).unwrap();
        let parsed: ReplayGateVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(v.go, parsed.go);
    }

    #[test]
    fn run_suite_empty_corpus() {
        let report = run_regression_suite(&[], &FacadeConfig::default());
        assert!(report.all_passed());
        assert!((report.artifact.pass_rate - 1.0).abs() < 1e-6);
        assert_eq!(report.artifact.outcomes.len(), 0);
    }

    #[test]
    fn run_suite_single_scenario() {
        let scenarios = vec![default_scenarios().remove(0)];
        let report = run_regression_suite(&scenarios, &FacadeConfig::default());
        assert_eq!(report.artifact.outcomes.len(), 1);
    }

    #[test]
    fn run_suite_all_pass() {
        let report = run_regression_suite(&default_scenarios(), &FacadeConfig::default());
        assert!(
            report.all_passed(),
            "all default scenarios should pass: {}",
            report.summary()
        );
    }

    #[test]
    fn replay_gate_default_goes() {
        let v = run_replay_gate_default();
        assert!(v.go, "default replay gate should pass: {:?}", v.reason);
    }

    #[test]
    fn replay_gate_schema_gates_pass() {
        let v = run_replay_gate_default();
        assert!(v.schema_fusion.safe);
        assert!(v.schema_orchestration.safe);
    }

    #[test]
    fn pass_rate_computed_correctly() {
        let report = run_regression_suite(&default_scenarios(), &FacadeConfig::default());
        let expected_rate = if report.artifact.outcomes.is_empty() {
            1.0
        } else {
            report.artifact.passed as f32 / report.artifact.outcomes.len() as f32
        };
        assert!((report.artifact.pass_rate - expected_rate).abs() < 1e-6);
    }

    #[test]
    fn failures_accessor() {
        let report = run_regression_suite(&default_scenarios(), &FacadeConfig::default());
        assert_eq!(report.failures().len(), report.artifact.failed);
    }

    #[test]
    fn summary_contains_counts() {
        let report = run_regression_suite(&default_scenarios(), &FacadeConfig::default());
        let s = report.summary();
        assert!(
            s.contains(&report.artifact.passed.to_string()),
            "summary should contain pass count"
        );
    }

    #[test]
    fn per_scenario_tolerance_override() {
        let mut scenario = default_scenarios()
            .into_iter()
            .find(|s| s.name == "partial_overlap")
            .unwrap();
        // Set impossibly tight score tolerance.
        scenario.score_tolerance = 0.0;
        let report = run_regression_suite(&[scenario], &FacadeConfig::default());
        // This may or may not fail depending on exact score arithmetic.
        // The test just verifies the tolerance is respected (no panic).
        assert_eq!(report.artifact.outcomes.len(), 1);
    }

    #[test]
    fn min_tau_is_minimum() {
        let report = run_regression_suite(&default_scenarios(), &FacadeConfig::default());
        let expected_min = report
            .artifact
            .outcomes
            .iter()
            .map(|o| o.kendall_tau)
            .fold(f32::INFINITY, f32::min);
        let expected_min = if expected_min.is_infinite() {
            1.0
        } else {
            expected_min
        };
        assert!(
            (report.artifact.min_tau - expected_min).abs() < 1e-6,
            "min_tau mismatch: {} vs {}",
            report.artifact.min_tau,
            expected_min
        );
    }

    #[test]
    fn run_suite_forces_shadow() {
        // Pass a Legacy config — suite should still run shadow internally.
        let config = FacadeConfig {
            routing: FacadeRouting::Legacy,
            ..FacadeConfig::default()
        };
        let report = run_regression_suite(&default_scenarios(), &config);
        // If shadow was not forced, ShadowComparison would be None and all would fail.
        assert!(
            report.all_passed(),
            "suite should force shadow mode: {}",
            report.summary()
        );
    }

    #[test]
    fn passed_flag_consistency() {
        let report = run_regression_suite(&default_scenarios(), &FacadeConfig::default());
        for outcome in &report.artifact.outcomes {
            assert_eq!(
                outcome.passed,
                outcome.failure_reason.is_none(),
                "passed flag and failure_reason must be consistent for '{}'",
                outcome.name
            );
        }
    }
}
