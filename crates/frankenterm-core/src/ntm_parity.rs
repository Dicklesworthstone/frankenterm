//! NTM parity corpus and shadow comparator evaluation.
//!
//! This module wires the migration artifacts from `ft-3681t.8.*` into a
//! machine-checkable evaluator:
//! - load the canonical NTM parity corpus and acceptance matrix
//! - evaluate captured `ft` command outputs against assertion contracts
//! - summarize cutover gate status and divergence budgets
//!
//! Command execution and artifact writing live in the CLI crate; this module
//! owns the schemas and deterministic evaluation logic.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// Root document for the canonical parity corpus fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityCorpus {
    pub schema_version: String,
    pub bead_id: String,
    pub title: String,
    pub updated_at: String,
    #[serde(default)]
    pub notes: String,
    pub scenarios: Vec<NtmParityScenario>,
}

impl NtmParityCorpus {
    pub fn from_json_str(input: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(input)
    }
}

/// One parity scenario from the canonical corpus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityScenario {
    pub id: String,
    pub domain: String,
    pub priority: NtmParityPriority,
    pub ntm_equivalent: String,
    pub ft_command: String,
    pub success_assertions: Vec<NtmParityAssertion>,
    #[serde(default)]
    pub failure_assertions: Vec<NtmParityAssertion>,
    pub artifact_key: String,
}

/// Scenario priority class as defined by the acceptance matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NtmParityPriority {
    Blocking,
    High,
}

impl NtmParityPriority {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::High => "high",
        }
    }
}

/// Supported assertion operators from the corpus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NtmParityAssertionOp {
    Eq,
    IsArray,
    HasAny,
    In,
    Contains,
}

impl NtmParityAssertionOp {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::IsArray => "is_array",
            Self::HasAny => "has_any",
            Self::In => "in",
            Self::Contains => "contains",
        }
    }
}

/// One assertion from a parity scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityAssertion {
    pub path: String,
    pub op: NtmParityAssertionOp,
    #[serde(default)]
    pub value: Option<Value>,
}

/// Root document for the acceptance matrix fixture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityAcceptanceMatrix {
    pub schema_version: String,
    pub bead_id: String,
    pub title: String,
    pub gates: NtmParityGates,
    pub result_schema: BTreeMap<String, Value>,
    pub artifacts_contract: NtmParityArtifactsContract,
}

impl NtmParityAcceptanceMatrix {
    pub fn from_json_str(input: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(input)
    }
}

/// Top-level gate configuration from the acceptance matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityGates {
    pub blocking_scenarios: NtmParityBlockingGate,
    pub high_priority_scenarios: NtmParityHighPriorityGate,
    pub envelope_contract: NtmParityEnvelopeContract,
    pub divergence_budget: NtmParityDivergenceBudget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityBlockingGate {
    pub required_ids: Vec<String>,
    pub rule: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityHighPriorityGate {
    pub required_pass_rate: f64,
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityEnvelopeContract {
    pub rule: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityDivergenceBudget {
    pub max_blocking_divergence: usize,
    pub max_high_priority_divergence: usize,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityArtifactsContract {
    pub required_files: Vec<String>,
    pub artifact_root: String,
}

/// Raw captured command output for one scenario execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityCommandOutput {
    pub scenario_id: String,
    pub command: String,
    pub expanded_command: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_error: Option<String>,
}

/// Detailed assertion result for a scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityAssertionResult {
    pub branch: String,
    pub path: String,
    pub op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<Value>,
    pub passed: bool,
    pub message: String,
}

/// Scenario result status recorded in `assertion_results.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NtmParityScenarioStatus {
    Pass,
    Fail,
    IntentionalDelta,
    Untested,
}

impl NtmParityScenarioStatus {
    #[must_use]
    pub const fn is_pass(self) -> bool {
        matches!(self, Self::Pass)
    }
}

/// Scenario-level evaluation record persisted for cutover gates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityScenarioResult {
    pub scenario_id: String,
    pub status: NtmParityScenarioStatus,
    pub artifacts: Vec<String>,
    pub notes: String,
    pub domain: String,
    pub priority: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub envelope_valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_branch: Option<String>,
    pub assertion_results: Vec<NtmParityAssertionResult>,
}

/// Run-level summary written to `summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityRunSummary {
    pub run_id: String,
    pub scenario_count: usize,
    pub pass_count: usize,
    pub fail_count: usize,
    pub intentional_delta_count: usize,
    pub untested_count: usize,
    pub divergence_count: usize,
    pub overall_passed: bool,
    pub blocking_failures: Vec<String>,
    pub high_priority_failures: Vec<String>,
    pub high_priority_intentional_deltas: Vec<String>,
    pub envelope_violations: Vec<String>,
    pub artifact_contract_failures: Vec<String>,
    pub gate_results: Vec<NtmParityGateResult>,
}

/// One gate decision for the summary bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityGateResult {
    pub gate_id: String,
    pub passed: bool,
    pub detail: String,
}

/// Focused divergence report written to `shadow_divergence_report.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityDivergenceReport {
    pub run_id: String,
    pub total_divergences: usize,
    pub blocking_divergence_count: usize,
    pub high_priority_divergence_count: usize,
    pub envelope_violation_count: usize,
    pub divergences: Vec<NtmParityScenarioDivergence>,
}

/// Scenario divergence entry for cutover gates and remediation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtmParityScenarioDivergence {
    pub scenario_id: String,
    pub status: NtmParityScenarioStatus,
    pub priority: String,
    pub reason: String,
}

/// Evaluate one scenario against captured command output.
#[must_use]
pub fn evaluate_scenario(
    scenario: &NtmParityScenario,
    output: &NtmParityCommandOutput,
    artifacts: Vec<String>,
    intentional_delta_note: Option<&str>,
) -> NtmParityScenarioResult {
    let stdout_json = serde_json::from_str::<Value>(&output.stdout).ok();

    let success_results = scenario
        .success_assertions
        .iter()
        .map(|assertion| evaluate_assertion("success", assertion, stdout_json.as_ref(), output))
        .collect::<Vec<_>>();
    let failure_results = scenario
        .failure_assertions
        .iter()
        .map(|assertion| evaluate_assertion("failure", assertion, stdout_json.as_ref(), output))
        .collect::<Vec<_>>();

    let success_passed = scenario.success_assertions.is_empty()
        || success_results.iter().all(|result| result.passed);
    let failure_passed = !scenario.failure_assertions.is_empty()
        && failure_results.iter().all(|result| result.passed);

    let matched_branch = if success_passed {
        Some("success".to_string())
    } else if failure_passed {
        Some("failure".to_string())
    } else if validate_envelope(&scenario.ft_command, stdout_json.as_ref(), &output.stdout) {
        Some("envelope".to_string())
    } else {
        None
    };

    let status = if matched_branch.is_some() {
        NtmParityScenarioStatus::Pass
    } else if intentional_delta_note.is_some() {
        NtmParityScenarioStatus::IntentionalDelta
    } else if output.execution_error.is_some() {
        NtmParityScenarioStatus::Untested
    } else {
        NtmParityScenarioStatus::Fail
    };

    let mut notes = Vec::new();
    if let Some(error) = &output.execution_error {
        notes.push(format!("execution error: {error}"));
    }
    if matched_branch.is_none() {
        notes.push("assertion contract did not match success or failure branches".to_string());
    }
    if !validate_envelope(&scenario.ft_command, stdout_json.as_ref(), &output.stdout) {
        notes.push("output did not satisfy the envelope contract".to_string());
    }
    if let Some(note) = intentional_delta_note {
        notes.push(format!("intentional delta: {note}"));
    }

    let mut assertion_results = success_results;
    assertion_results.extend(failure_results);

    NtmParityScenarioResult {
        scenario_id: scenario.id.clone(),
        status,
        artifacts,
        notes: notes.join("; "),
        domain: scenario.domain.clone(),
        priority: scenario.priority.as_str().to_string(),
        command: output.expanded_command.clone(),
        exit_code: output.exit_code,
        duration_ms: output.duration_ms,
        envelope_valid: validate_envelope(
            &scenario.ft_command,
            stdout_json.as_ref(),
            &output.stdout,
        ),
        matched_branch,
        assertion_results,
    }
}

/// Build the cutover summary for a completed parity run.
#[must_use]
pub fn build_run_summary(
    run_id: &str,
    matrix: &NtmParityAcceptanceMatrix,
    results: &[NtmParityScenarioResult],
) -> NtmParityRunSummary {
    let blocking_ids: HashSet<&str> = matrix
        .gates
        .blocking_scenarios
        .required_ids
        .iter()
        .map(String::as_str)
        .collect();
    let high_priority_ids: HashSet<&str> = matrix
        .gates
        .high_priority_scenarios
        .ids
        .iter()
        .map(String::as_str)
        .collect();

    let pass_count = results
        .iter()
        .filter(|result| result.status.is_pass())
        .count();
    let fail_count = results
        .iter()
        .filter(|result| matches!(result.status, NtmParityScenarioStatus::Fail))
        .count();
    let intentional_delta_count = results
        .iter()
        .filter(|result| matches!(result.status, NtmParityScenarioStatus::IntentionalDelta))
        .count();
    let untested_count = results
        .iter()
        .filter(|result| matches!(result.status, NtmParityScenarioStatus::Untested))
        .count();

    let blocking_failures = results
        .iter()
        .filter(|result| {
            blocking_ids.contains(result.scenario_id.as_str()) && !result.status.is_pass()
        })
        .map(|result| result.scenario_id.clone())
        .collect::<Vec<_>>();

    let high_priority_failures = results
        .iter()
        .filter(|result| {
            high_priority_ids.contains(result.scenario_id.as_str())
                && matches!(
                    result.status,
                    NtmParityScenarioStatus::Fail | NtmParityScenarioStatus::Untested
                )
        })
        .map(|result| result.scenario_id.clone())
        .collect::<Vec<_>>();

    let high_priority_intentional_deltas = results
        .iter()
        .filter(|result| {
            high_priority_ids.contains(result.scenario_id.as_str())
                && matches!(result.status, NtmParityScenarioStatus::IntentionalDelta)
        })
        .map(|result| result.scenario_id.clone())
        .collect::<Vec<_>>();

    let envelope_violations = results
        .iter()
        .filter(|result| !result.envelope_valid)
        .map(|result| result.scenario_id.clone())
        .collect::<Vec<_>>();

    let artifact_contract_failures = results
        .iter()
        .filter(|result| result.artifacts.is_empty())
        .map(|result| result.scenario_id.clone())
        .collect::<Vec<_>>();

    let high_priority_passes = results
        .iter()
        .filter(|result| {
            high_priority_ids.contains(result.scenario_id.as_str()) && result.status.is_pass()
        })
        .count();
    let high_priority_total = high_priority_ids.len();
    let high_priority_pass_rate = if high_priority_total == 0 {
        1.0
    } else {
        high_priority_passes as f64 / high_priority_total as f64
    };

    let blocking_gate_passed =
        blocking_failures.len() <= matrix.gates.divergence_budget.max_blocking_divergence;
    let high_priority_gate_passed = high_priority_failures.is_empty()
        && high_priority_pass_rate >= matrix.gates.high_priority_scenarios.required_pass_rate
        && high_priority_intentional_deltas.len()
            <= matrix.gates.divergence_budget.max_high_priority_divergence;
    let envelope_gate_passed = envelope_violations.is_empty();
    let artifacts_gate_passed = artifact_contract_failures.is_empty();

    let gate_results = vec![
        NtmParityGateResult {
            gate_id: "G-01".to_string(),
            passed: blocking_gate_passed,
            detail: format!(
                "blocking divergence count={} (budget={})",
                blocking_failures.len(),
                matrix.gates.divergence_budget.max_blocking_divergence
            ),
        },
        NtmParityGateResult {
            gate_id: "G-02".to_string(),
            passed: high_priority_gate_passed,
            detail: format!(
                "high-priority pass_rate={high_priority_pass_rate:.2}, intentional_deltas={}, unexplained_failures={}",
                high_priority_intentional_deltas.len(),
                high_priority_failures.len()
            ),
        },
        NtmParityGateResult {
            gate_id: "G-03".to_string(),
            passed: envelope_gate_passed,
            detail: format!("envelope violations={}", envelope_violations.len()),
        },
        NtmParityGateResult {
            gate_id: "ARTIFACTS".to_string(),
            passed: artifacts_gate_passed,
            detail: format!(
                "scenario result objects missing artifacts={}",
                artifact_contract_failures.len()
            ),
        },
    ];

    let overall_passed = gate_results.iter().all(|gate| gate.passed);

    NtmParityRunSummary {
        run_id: run_id.to_string(),
        scenario_count: results.len(),
        pass_count,
        fail_count,
        intentional_delta_count,
        untested_count,
        divergence_count: results
            .iter()
            .filter(|result| !result.status.is_pass())
            .count(),
        overall_passed,
        blocking_failures,
        high_priority_failures,
        high_priority_intentional_deltas,
        envelope_violations,
        artifact_contract_failures,
        gate_results,
    }
}

/// Build the divergence report for a completed parity run.
#[must_use]
pub fn build_divergence_report(
    run_id: &str,
    matrix: &NtmParityAcceptanceMatrix,
    results: &[NtmParityScenarioResult],
) -> NtmParityDivergenceReport {
    let blocking_ids: HashSet<&str> = matrix
        .gates
        .blocking_scenarios
        .required_ids
        .iter()
        .map(String::as_str)
        .collect();
    let high_priority_ids: HashSet<&str> = matrix
        .gates
        .high_priority_scenarios
        .ids
        .iter()
        .map(String::as_str)
        .collect();

    let divergences = results
        .iter()
        .filter(|result| !result.status.is_pass() || !result.envelope_valid)
        .map(|result| NtmParityScenarioDivergence {
            scenario_id: result.scenario_id.clone(),
            status: result.status,
            priority: result.priority.clone(),
            reason: if result.notes.is_empty() {
                "divergence detected".to_string()
            } else {
                result.notes.clone()
            },
        })
        .collect::<Vec<_>>();

    let blocking_divergence_count = results
        .iter()
        .filter(|result| {
            blocking_ids.contains(result.scenario_id.as_str()) && !result.status.is_pass()
        })
        .count();
    let high_priority_divergence_count = results
        .iter()
        .filter(|result| {
            high_priority_ids.contains(result.scenario_id.as_str()) && !result.status.is_pass()
        })
        .count();
    let envelope_violation_count = results
        .iter()
        .filter(|result| !result.envelope_valid)
        .count();

    NtmParityDivergenceReport {
        run_id: run_id.to_string(),
        total_divergences: divergences.len(),
        blocking_divergence_count,
        high_priority_divergence_count,
        envelope_violation_count,
        divergences,
    }
}

fn evaluate_assertion(
    branch: &str,
    assertion: &NtmParityAssertion,
    stdout_json: Option<&Value>,
    output: &NtmParityCommandOutput,
) -> NtmParityAssertionResult {
    let actual = resolve_assertion_target(assertion.path.as_str(), stdout_json, output);
    let (passed, message) = match assertion.op {
        NtmParityAssertionOp::Eq => {
            let expected = assertion.value.clone();
            (actual == expected, "equality assertion".to_string())
        }
        NtmParityAssertionOp::IsArray => (
            actual.as_ref().is_some_and(Value::is_array),
            "target must be an array".to_string(),
        ),
        NtmParityAssertionOp::HasAny => match (&actual, &assertion.value) {
            (Some(Value::Object(map)), Some(Value::Array(keys))) => {
                let expected_keys = keys.iter().filter_map(Value::as_str).collect::<Vec<_>>();
                let matched = expected_keys.iter().any(|key| map.contains_key(*key));
                (
                    matched,
                    format!("object must contain any of [{}]", expected_keys.join(", ")),
                )
            }
            _ => (
                false,
                "has_any requires an object target and array-of-strings value".to_string(),
            ),
        },
        NtmParityAssertionOp::In => match (&actual, &assertion.value) {
            (Some(actual_value), Some(Value::Array(options))) => (
                options.iter().any(|candidate| candidate == actual_value),
                "actual value must exist in expected array".to_string(),
            ),
            _ => (
                false,
                "in requires an actual value and array-of-values expectation".to_string(),
            ),
        },
        NtmParityAssertionOp::Contains => match (&actual, &assertion.value) {
            (Some(Value::String(actual_value)), Some(Value::String(expected_substring))) => (
                actual_value.contains(expected_substring),
                format!("string must contain substring '{expected_substring}'"),
            ),
            _ => (
                false,
                "contains requires string actual/expected values".to_string(),
            ),
        },
    };

    NtmParityAssertionResult {
        branch: branch.to_string(),
        path: assertion.path.clone(),
        op: assertion.op.as_str().to_string(),
        expected: assertion.value.clone(),
        actual,
        passed,
        message,
    }
}

fn resolve_assertion_target(
    path: &str,
    stdout_json: Option<&Value>,
    output: &NtmParityCommandOutput,
) -> Option<Value> {
    match path {
        "$stdout" => Some(Value::String(output.stdout.clone())),
        "$stderr" => Some(Value::String(output.stderr.clone())),
        "$" => stdout_json.cloned(),
        _ => resolve_json_path(stdout_json?, path).cloned(),
    }
}

fn resolve_json_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    let trimmed = path.strip_prefix("$.")?;

    for segment in trimmed.split('.') {
        match current {
            Value::Object(map) => current = map.get(segment)?,
            _ => return None,
        }
    }

    Some(current)
}

fn validate_envelope(command: &str, stdout_json: Option<&Value>, stdout: &str) -> bool {
    if command.contains("--format toon") {
        return !stdout.trim().is_empty();
    }

    let Some(json) = stdout_json else {
        return false;
    };

    let Some(object) = json.as_object() else {
        return false;
    };

    object
        .get("ok")
        .and_then(Value::as_bool)
        .is_some_and(|ok| ok)
        || object
            .get("error")
            .and_then(Value::as_object)
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_scenario() -> NtmParityScenario {
        NtmParityScenario {
            id: "NTM-PARITY-001".to_string(),
            domain: "state_discovery".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "ntm state".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![
                NtmParityAssertion {
                    path: "$.ok".to_string(),
                    op: NtmParityAssertionOp::Eq,
                    value: Some(json!(true)),
                },
                NtmParityAssertion {
                    path: "$.data.panes".to_string(),
                    op: NtmParityAssertionOp::IsArray,
                    value: None,
                },
            ],
            failure_assertions: Vec::new(),
            artifact_key: "state_snapshot".to_string(),
        }
    }

    fn sample_output(stdout: Value) -> NtmParityCommandOutput {
        NtmParityCommandOutput {
            scenario_id: "NTM-PARITY-001".to_string(),
            command: "ft robot --format json state".to_string(),
            expanded_command: "ft robot --format json state".to_string(),
            exit_code: Some(0),
            duration_ms: 12,
            stdout: stdout.to_string(),
            stderr: String::new(),
            execution_error: None,
        }
    }

    #[test]
    fn success_branch_passes_when_assertions_match() {
        let scenario = sample_scenario();
        let output = sample_output(json!({
            "ok": true,
            "data": { "panes": [] }
        }));

        let result = evaluate_scenario(
            &scenario,
            &output,
            vec!["scenarios/001.json".to_string()],
            None,
        );

        assert_eq!(result.status, NtmParityScenarioStatus::Pass);
        assert_eq!(result.matched_branch.as_deref(), Some("success"));
        assert!(result.envelope_valid);
        assert!(
            result
                .assertion_results
                .iter()
                .all(|assertion| assertion.passed)
        );
    }

    #[test]
    fn failure_branch_can_satisfy_policy_error_contract() {
        let scenario = NtmParityScenario {
            id: "NTM-PARITY-011".to_string(),
            domain: "policy_guard".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "ntm policy envelope".to_string(),
            ft_command: "ft robot --format json send <pane_id> \"dangerous\"".to_string(),
            success_assertions: vec![NtmParityAssertion {
                path: "$.ok".to_string(),
                op: NtmParityAssertionOp::Eq,
                value: Some(json!(true)),
            }],
            failure_assertions: vec![NtmParityAssertion {
                path: "$.error.code".to_string(),
                op: NtmParityAssertionOp::In,
                value: Some(json!(["robot.policy_denied", "robot.require_approval"])),
            }],
            artifact_key: "policy_decision_trace".to_string(),
        };
        let output = NtmParityCommandOutput {
            scenario_id: scenario.id.clone(),
            command: scenario.ft_command.clone(),
            expanded_command: "ft robot --format json send 0 \"dangerous\"".to_string(),
            exit_code: Some(1),
            duration_ms: 9,
            stdout: json!({
                "ok": false,
                "error": { "code": "robot.require_approval" }
            })
            .to_string(),
            stderr: String::new(),
            execution_error: None,
        };

        let result = evaluate_scenario(
            &scenario,
            &output,
            vec!["scenarios/011.json".to_string()],
            None,
        );

        assert_eq!(result.status, NtmParityScenarioStatus::Pass);
        assert_eq!(result.matched_branch.as_deref(), Some("failure"));
        assert!(result.envelope_valid);
    }

    #[test]
    fn toon_output_uses_non_empty_stdout_as_envelope() {
        let scenario = NtmParityScenario {
            id: "NTM-PARITY-012".to_string(),
            domain: "output_efficiency".to_string(),
            priority: NtmParityPriority::High,
            ntm_equivalent: "toon stats".to_string(),
            ft_command: "ft robot --format toon --stats state".to_string(),
            success_assertions: vec![NtmParityAssertion {
                path: "$stderr".to_string(),
                op: NtmParityAssertionOp::Contains,
                value: Some(json!("tokens")),
            }],
            failure_assertions: Vec::new(),
            artifact_key: "toon_stats_capture".to_string(),
        };
        let output = NtmParityCommandOutput {
            scenario_id: scenario.id.clone(),
            command: scenario.ft_command.clone(),
            expanded_command: scenario.ft_command.clone(),
            exit_code: Some(0),
            duration_ms: 7,
            stdout: "ok=true data={panes=[]}".to_string(),
            stderr: "saved tokens: 42".to_string(),
            execution_error: None,
        };

        let result = evaluate_scenario(
            &scenario,
            &output,
            vec!["scenarios/012.json".to_string()],
            None,
        );

        assert_eq!(result.status, NtmParityScenarioStatus::Pass);
        assert!(result.envelope_valid);
    }

    #[test]
    fn summary_tracks_gates_and_intentional_deltas() {
        let matrix = NtmParityAcceptanceMatrix::from_json_str(
            r#"{
              "schema_version":"1.0",
              "bead_id":"ft-3681t.8.1",
              "title":"matrix",
              "gates":{
                "blocking_scenarios":{"required_ids":["A"],"rule":"all must pass"},
                "high_priority_scenarios":{"required_pass_rate":0.9,"ids":["B"]},
                "envelope_contract":{"rule":"envelope"},
                "divergence_budget":{"max_blocking_divergence":0,"max_high_priority_divergence":1,"notes":""}
              },
              "result_schema":{"scenario_id":"string"},
              "artifacts_contract":{"required_files":["summary.json"],"artifact_root":"artifacts/e2e/ntm_parity/<run_id>/"}
            }"#,
        )
        .expect("matrix fixture should parse");

        let results = vec![
            NtmParityScenarioResult {
                scenario_id: "A".to_string(),
                status: NtmParityScenarioStatus::Fail,
                artifacts: vec!["a.json".to_string()],
                notes: "blocking failure".to_string(),
                domain: "state".to_string(),
                priority: "blocking".to_string(),
                command: "ft robot state".to_string(),
                exit_code: Some(1),
                duration_ms: 10,
                envelope_valid: true,
                matched_branch: None,
                assertion_results: Vec::new(),
            },
            NtmParityScenarioResult {
                scenario_id: "B".to_string(),
                status: NtmParityScenarioStatus::IntentionalDelta,
                artifacts: vec!["b.json".to_string()],
                notes: "documented delta".to_string(),
                domain: "rules".to_string(),
                priority: "high".to_string(),
                command: "ft robot rules".to_string(),
                exit_code: Some(0),
                duration_ms: 5,
                envelope_valid: true,
                matched_branch: None,
                assertion_results: Vec::new(),
            },
        ];

        let summary = build_run_summary("run-1", &matrix, &results);
        let divergence = build_divergence_report("run-1", &matrix, &results);

        assert!(!summary.overall_passed);
        assert_eq!(summary.blocking_failures, vec!["A".to_string()]);
        assert_eq!(
            summary.high_priority_intentional_deltas,
            vec!["B".to_string()]
        );
        assert_eq!(divergence.total_divergences, 2);
        assert_eq!(divergence.blocking_divergence_count, 1);
        assert_eq!(divergence.high_priority_divergence_count, 1);
    }
}
