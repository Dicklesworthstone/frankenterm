//! Replay CI gate evaluator and promotion pipeline integration.
//!
//! Bead: ft-og6q6.7.4
//!
//! This module orchestrates the 3-gate CI architecture for replay operations:
//! - Gate 1 (Smoke, <30s): schema check + smoke tests S-01..S-05
//! - Gate 2 (Test Suite, <10min): unit + property + integration tests
//! - Gate 3 (Regression, <30min): E2E scenarios, regression suite, evidence bundle
//!
//! It also provides waiver parsing (from PR descriptions) and gate report
//! generation for CI status checks.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Constants ────────────────────────────────────────────────────────────────

pub const GATE_REPORT_VERSION: &str = "1";
pub const GATE_REPORT_FORMAT: &str = "ft-replay-gate-report";

/// Path triggers: only run gates on replay-related changes.
pub const REPLAY_PATH_TRIGGERS: &[&str] = &[
    "crates/frankenterm-core/src/replay_*.rs",
    "crates/frankenterm-core/tests/*replay*.rs",
    "crates/frankenterm-core/benches/replay_*.rs",
    "tests/e2e/test_replay_*.sh",
    "scripts/check_replay_*.sh",
    ".github/workflows/replay-gates.yml",
];

/// Nightly cron schedule (4 AM UTC).
pub const NIGHTLY_CRON: &str = "0 4 * * *";

// ── Gate Identifiers ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateId {
    Smoke,
    TestSuite,
    Regression,
}

impl GateId {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::TestSuite => "test_suite",
            Self::Regression => "regression",
        }
    }

    #[must_use]
    pub fn from_str_id(s: &str) -> Option<Self> {
        match s {
            "smoke" => Some(Self::Smoke),
            "test_suite" => Some(Self::TestSuite),
            "regression" => Some(Self::Regression),
            _ => None,
        }
    }

    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Smoke => "Gate 1: Smoke",
            Self::TestSuite => "Gate 2: Test Suite",
            Self::Regression => "Gate 3: Regression",
        }
    }

    #[must_use]
    pub fn timeout_seconds(self) -> u64 {
        match self {
            Self::Smoke => 30,
            Self::TestSuite => 600,
            Self::Regression => 1800,
        }
    }

    #[must_use]
    pub fn gate_number(self) -> u8 {
        match self {
            Self::Smoke => 1,
            Self::TestSuite => 2,
            Self::Regression => 3,
        }
    }
}

pub const ALL_GATES: [GateId; 3] = [GateId::Smoke, GateId::TestSuite, GateId::Regression];

// ── Gate Status ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pass,
    Fail,
    Pending,
    Skipped,
    Waived,
}

impl GateStatus {
    #[must_use]
    pub fn is_blocking(self) -> bool {
        self == Self::Fail
    }

    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Pass | Self::Fail | Self::Skipped | Self::Waived)
    }
}

// ── Gate Check ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateCheck {
    pub name: String,
    pub passed: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
}

// ── Gate Report ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateReport {
    pub version: String,
    pub format: String,
    pub gate: GateId,
    pub status: GateStatus,
    pub evaluated_at: String,
    pub duration_ms: u64,
    pub checks: Vec<GateCheck>,
    pub pass_count: usize,
    pub fail_count: usize,
    pub total_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiver: Option<Waiver>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_bundle_path: Option<String>,
    pub summary: String,
}

impl GateReport {
    #[must_use]
    pub fn is_pass(&self) -> bool {
        self.status == GateStatus::Pass || self.status == GateStatus::Waived
    }

    #[must_use]
    pub fn new(
        gate: GateId,
        checks: Vec<GateCheck>,
        duration_ms: u64,
        evaluated_at: String,
    ) -> Self {
        let pass_count = checks.iter().filter(|c| c.passed).count();
        let fail_count = checks.iter().filter(|c| !c.passed).count();
        let total_count = checks.len();
        let status = if fail_count == 0 {
            GateStatus::Pass
        } else {
            GateStatus::Fail
        };
        let summary = format!(
            "{}: {}/{} checks passed",
            gate.display_name(),
            pass_count,
            total_count
        );

        Self {
            version: GATE_REPORT_VERSION.into(),
            format: GATE_REPORT_FORMAT.into(),
            gate,
            status,
            evaluated_at,
            duration_ms,
            checks,
            pass_count,
            fail_count,
            total_count,
            waiver: None,
            evidence_bundle_path: None,
            summary,
        }
    }

    /// Apply a waiver if the gate failed and a matching waiver exists.
    pub fn apply_waiver(&mut self, waiver: Waiver) {
        if self.status == GateStatus::Fail && !waiver.is_expired_at(&self.evaluated_at) {
            self.status = GateStatus::Waived;
            self.summary = format!("{} (waived: {})", self.summary, waiver.reason);
            self.waiver = Some(waiver);
        }
    }
}

// ── Waiver ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiver {
    pub gate: GateId,
    pub check_name: String,
    pub reason: String,
    pub author: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_reference: Option<String>,
}

impl Waiver {
    #[must_use]
    pub fn is_expired_at(&self, current_time: &str) -> bool {
        match &self.expires_at {
            Some(exp) => current_time > exp.as_str(),
            None => false, // no expiry = never expires
        }
    }

    #[must_use]
    pub fn matches_check(&self, gate: GateId, check_name: &str) -> bool {
        self.gate == gate && (self.check_name == "*" || self.check_name == check_name)
    }
}

/// Parse waivers from a PR description body.
///
/// Format:
/// ```text
/// <!-- replay-waiver
/// gate: smoke
/// check: schema_validation
/// reason: Known issue #123, fix in next sprint
/// author: user@example.com
/// expires: 2026-03-01T00:00:00Z
/// -->
/// ```
#[must_use]
pub fn parse_waivers(pr_body: &str) -> Vec<Waiver> {
    let mut waivers = Vec::new();
    let marker_start = "<!-- replay-waiver";
    let marker_end = "-->";

    let mut remaining = pr_body;
    while let Some(start_idx) = remaining.find(marker_start) {
        let after_start = &remaining[start_idx + marker_start.len()..];
        if let Some(end_idx) = after_start.find(marker_end) {
            let block = &after_start[..end_idx];
            if let Some(waiver) = parse_single_waiver(block) {
                waivers.push(waiver);
            }
            remaining = &after_start[end_idx + marker_end.len()..];
        } else {
            break;
        }
    }
    waivers
}

fn parse_single_waiver(block: &str) -> Option<Waiver> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for line in block.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_lowercase();
            let value = value.trim().to_string();
            if !value.is_empty() {
                fields.insert(key, value);
            }
        }
    }

    let gate_str = fields.get("gate")?;
    let gate = GateId::from_str_id(gate_str)?;
    let check_name = fields.get("check").cloned().unwrap_or_else(|| "*".into());
    let reason = fields.get("reason").cloned()?;
    let author = fields
        .get("author")
        .cloned()
        .unwrap_or_else(|| "unknown".into());
    let expires_at = fields.get("expires").cloned();

    Some(Waiver {
        gate,
        check_name,
        reason,
        author,
        expires_at,
        pr_reference: None,
    })
}

// ── Evidence Bundle ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundle {
    pub version: String,
    pub generated_at: String,
    pub gate_reports: Vec<GateReport>,
    pub overall_status: GateStatus,
    pub artifact_paths: Vec<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl EvidenceBundle {
    #[must_use]
    pub fn new(gate_reports: Vec<GateReport>, generated_at: String) -> Self {
        let overall_status = if gate_reports.iter().any(|r| r.status == GateStatus::Fail) {
            GateStatus::Fail
        } else if gate_reports.iter().any(|r| r.status == GateStatus::Pending) {
            GateStatus::Pending
        } else if gate_reports.iter().any(|r| r.status == GateStatus::Waived) {
            GateStatus::Waived
        } else {
            GateStatus::Pass
        };

        let artifact_paths = gate_reports
            .iter()
            .filter_map(|r| r.evidence_bundle_path.clone())
            .collect();

        Self {
            version: GATE_REPORT_VERSION.into(),
            generated_at,
            gate_reports,
            overall_status,
            artifact_paths,
            metadata: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn is_promotable(&self) -> bool {
        self.overall_status == GateStatus::Pass || self.overall_status == GateStatus::Waived
    }
}

// ── Gate 1: Smoke Checks ─────────────────────────────────────────────────────

/// Evaluate Gate 1 smoke checks.
#[must_use]
pub fn evaluate_gate1_smoke(
    schema_valid: bool,
    smoke_results: &[(String, bool)],
    duration_ms: u64,
    evaluated_at: &str,
) -> GateReport {
    let mut checks = Vec::new();

    checks.push(GateCheck {
        name: "schema_validation".into(),
        passed: schema_valid,
        message: if schema_valid {
            "MCP tool schemas valid".into()
        } else {
            "MCP tool schema validation failed".into()
        },
        duration_ms: None,
        artifact_path: None,
    });

    for (name, passed) in smoke_results {
        checks.push(GateCheck {
            name: name.clone(),
            passed: *passed,
            message: format!(
                "Smoke test {}: {}",
                name,
                if *passed { "passed" } else { "failed" }
            ),
            duration_ms: None,
            artifact_path: None,
        });
    }

    GateReport::new(GateId::Smoke, checks, duration_ms, evaluated_at.into())
}

// ── Gate 2: Test Suite Checks ────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSuiteResults {
    pub unit_tests_passed: usize,
    pub unit_tests_total: usize,
    pub proptest_cases: usize,
    pub proptest_passed: bool,
    pub integration_tests_passed: usize,
    pub integration_tests_total: usize,
}

/// Minimum property test cases required for Gate 2.
pub const MIN_PROPTEST_CASES: usize = 100;

/// Evaluate Gate 2 test suite checks.
#[must_use]
pub fn evaluate_gate2_test_suite(
    results: &TestSuiteResults,
    duration_ms: u64,
    evaluated_at: &str,
) -> GateReport {
    let mut checks = Vec::new();

    let unit_all_pass = results.unit_tests_passed == results.unit_tests_total;
    checks.push(GateCheck {
        name: "unit_tests".into(),
        passed: unit_all_pass,
        message: format!(
            "Unit tests: {}/{} passed",
            results.unit_tests_passed, results.unit_tests_total
        ),
        duration_ms: None,
        artifact_path: None,
    });

    let proptest_enough_cases = results.proptest_cases >= MIN_PROPTEST_CASES;
    checks.push(GateCheck {
        name: "proptest_case_count".into(),
        passed: proptest_enough_cases,
        message: format!(
            "Property tests: {} cases (min: {})",
            results.proptest_cases, MIN_PROPTEST_CASES
        ),
        duration_ms: None,
        artifact_path: None,
    });

    checks.push(GateCheck {
        name: "proptest_pass".into(),
        passed: results.proptest_passed,
        message: format!(
            "Property tests: {}",
            if results.proptest_passed {
                "all passed"
            } else {
                "failures detected"
            }
        ),
        duration_ms: None,
        artifact_path: None,
    });

    let integration_all_pass = results.integration_tests_passed == results.integration_tests_total;
    checks.push(GateCheck {
        name: "integration_tests".into(),
        passed: integration_all_pass,
        message: format!(
            "Integration tests: {}/{} passed",
            results.integration_tests_passed, results.integration_tests_total
        ),
        duration_ms: None,
        artifact_path: None,
    });

    GateReport::new(GateId::TestSuite, checks, duration_ms, evaluated_at.into())
}

// ── Gate 3: Regression Checks ────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegressionResults {
    pub e2e_passed: bool,
    pub e2e_scenario_count: usize,
    pub regression_suite_passed: bool,
    pub regression_divergence_count: u64,
    pub blocking_metric_count: usize,
    pub warning_metric_count: usize,
    pub evidence_bundle_path: Option<String>,
}

/// Evaluate Gate 3 regression checks.
#[must_use]
pub fn evaluate_gate3_regression(
    results: &RegressionResults,
    duration_ms: u64,
    evaluated_at: &str,
) -> GateReport {
    let mut checks = Vec::new();

    checks.push(GateCheck {
        name: "e2e_scenarios".into(),
        passed: results.e2e_passed,
        message: format!(
            "E2E scenarios: {} total, {}",
            results.e2e_scenario_count,
            if results.e2e_passed {
                "all passed"
            } else {
                "failures detected"
            }
        ),
        duration_ms: None,
        artifact_path: None,
    });

    checks.push(GateCheck {
        name: "regression_suite".into(),
        passed: results.regression_suite_passed,
        message: format!(
            "Regression suite: {} divergences, {}",
            results.regression_divergence_count,
            if results.regression_suite_passed {
                "within budget"
            } else {
                "budget exceeded"
            }
        ),
        duration_ms: None,
        artifact_path: None,
    });

    let no_blocking = results.blocking_metric_count == 0;
    checks.push(GateCheck {
        name: "performance_budgets".into(),
        passed: no_blocking,
        message: format!(
            "Performance: {} blocking, {} warning",
            results.blocking_metric_count, results.warning_metric_count
        ),
        duration_ms: None,
        artifact_path: results.evidence_bundle_path.clone(),
    });

    let mut report = GateReport::new(GateId::Regression, checks, duration_ms, evaluated_at.into());
    report.evidence_bundle_path = results.evidence_bundle_path.clone();
    report
}

// ── Path Trigger Matching ────────────────────────────────────────────────────

/// Check if a file path matches any replay path trigger.
#[must_use]
pub fn matches_replay_path(path: &str) -> bool {
    for pattern in REPLAY_PATH_TRIGGERS {
        if glob_match(pattern, path) {
            return true;
        }
    }
    false
}

/// Simple glob matching supporting * and ** patterns.
fn glob_match(pattern: &str, path: &str) -> bool {
    if pattern.contains("**") {
        let parts: Vec<&str> = pattern.split("**").collect();
        if parts.len() != 2 {
            return false;
        }
        let prefix = parts[0].trim_end_matches('/');
        let suffix_pattern = parts[1].trim_start_matches('/');
        if !path.starts_with(prefix) {
            return false;
        }
        let remaining = &path[prefix.len()..].trim_start_matches('/');
        simple_glob(suffix_pattern, remaining)
    } else {
        simple_glob(pattern, path)
    }
}

fn simple_glob(pattern: &str, text: &str) -> bool {
    let pat_parts: Vec<&str> = pattern.split('*').collect();
    if pat_parts.len() == 1 {
        return pattern == text;
    }

    let mut pos = 0;
    for (i, part) in pat_parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if let Some(found) = text[pos..].find(part) {
            if i == 0 && found != 0 {
                return false;
            }
            pos += found + part.len();
        } else {
            return false;
        }
    }
    if let Some(last) = pat_parts.last() {
        if !last.is_empty() {
            return text.ends_with(last);
        }
    }
    true
}

// ── PR Status Summary ────────────────────────────────────────────────────────

/// Generate a PR status check summary from gate reports.
#[must_use]
pub fn pr_status_summary(bundle: &EvidenceBundle) -> String {
    let mut lines = Vec::new();
    for report in &bundle.gate_reports {
        let icon = match report.status {
            GateStatus::Pass => "✅",
            GateStatus::Fail => "❌",
            GateStatus::Pending => "⏳",
            GateStatus::Skipped => "⏭️",
            GateStatus::Waived => "⚠️",
        };
        lines.push(format!("{} {}", icon, report.summary));
    }
    let overall = match bundle.overall_status {
        GateStatus::Pass => "All replay gates passed",
        GateStatus::Fail => "Replay gate failure — PR blocked",
        GateStatus::Pending => "Replay gates pending",
        GateStatus::Skipped => "Replay gates skipped",
        GateStatus::Waived => "Replay gates passed with waivers",
    };
    lines.push(String::new());
    lines.push(overall.into());
    lines.join("\n")
}

// ── MCP Schema ───────────────────────────────────────────────────────────────

/// Generate MCP tool schema for replay CI gate operations.
#[must_use]
pub fn gate_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "wa.replay.ci_gate",
        "description": "Evaluate replay CI gates and generate evidence bundles for promotion decisions.",
        "tags": ["replay", "ci", "gate"],
        "input_schema": {
            "type": "object",
            "properties": {
                "gate": {
                    "type": "string",
                    "enum": ["smoke", "test_suite", "regression"],
                    "description": "Which gate to evaluate"
                },
                "pr_body": {
                    "type": "string",
                    "description": "PR description body for waiver extraction"
                }
            },
            "required": ["gate"],
            "additionalProperties": false
        }
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Gate ID ──────────────────────────────────────────────────────────

    #[test]
    fn gate_id_str_roundtrip() {
        for gate in &ALL_GATES {
            let s = gate.as_str();
            let parsed = GateId::from_str_id(s);
            assert_eq!(parsed, Some(*gate));
        }
    }

    #[test]
    fn gate_id_unknown_returns_none() {
        assert_eq!(GateId::from_str_id("unknown"), None);
    }

    #[test]
    fn gate_numbers_sequential() {
        assert_eq!(GateId::Smoke.gate_number(), 1);
        assert_eq!(GateId::TestSuite.gate_number(), 2);
        assert_eq!(GateId::Regression.gate_number(), 3);
    }

    #[test]
    fn gate_timeouts_ascending() {
        assert!(GateId::Smoke.timeout_seconds() < GateId::TestSuite.timeout_seconds());
        assert!(GateId::TestSuite.timeout_seconds() < GateId::Regression.timeout_seconds());
    }

    #[test]
    fn gate_display_names_contain_gate() {
        for gate in &ALL_GATES {
            assert!(gate.display_name().contains("Gate"));
        }
    }

    // ── Gate Status ──────────────────────────────────────────────────────

    #[test]
    fn gate_status_blocking() {
        assert!(GateStatus::Fail.is_blocking());
        assert!(!GateStatus::Pass.is_blocking());
        assert!(!GateStatus::Waived.is_blocking());
        assert!(!GateStatus::Pending.is_blocking());
        assert!(!GateStatus::Skipped.is_blocking());
    }

    #[test]
    fn gate_status_terminal() {
        assert!(GateStatus::Pass.is_terminal());
        assert!(GateStatus::Fail.is_terminal());
        assert!(GateStatus::Skipped.is_terminal());
        assert!(GateStatus::Waived.is_terminal());
        assert!(!GateStatus::Pending.is_terminal());
    }

    // ── Gate Report ──────────────────────────────────────────────────────

    #[test]
    fn gate_report_all_pass() {
        let checks = vec![
            GateCheck {
                name: "a".into(),
                passed: true,
                message: "ok".into(),
                duration_ms: None,
                artifact_path: None,
            },
            GateCheck {
                name: "b".into(),
                passed: true,
                message: "ok".into(),
                duration_ms: None,
                artifact_path: None,
            },
        ];
        let report = GateReport::new(GateId::Smoke, checks, 100, "2026-01-01T00:00:00Z".into());
        assert_eq!(report.status, GateStatus::Pass);
        assert_eq!(report.pass_count, 2);
        assert_eq!(report.fail_count, 0);
        assert!(report.is_pass());
    }

    #[test]
    fn gate_report_one_failure() {
        let checks = vec![
            GateCheck {
                name: "a".into(),
                passed: true,
                message: "ok".into(),
                duration_ms: None,
                artifact_path: None,
            },
            GateCheck {
                name: "b".into(),
                passed: false,
                message: "fail".into(),
                duration_ms: None,
                artifact_path: None,
            },
        ];
        let report = GateReport::new(GateId::Smoke, checks, 100, "2026-01-01T00:00:00Z".into());
        assert_eq!(report.status, GateStatus::Fail);
        assert_eq!(report.pass_count, 1);
        assert_eq!(report.fail_count, 1);
        assert!(!report.is_pass());
    }

    #[test]
    fn gate_report_serde_roundtrip() {
        let checks = vec![GateCheck {
            name: "test".into(),
            passed: true,
            message: "ok".into(),
            duration_ms: Some(50),
            artifact_path: Some("/path".into()),
        }];
        let report = GateReport::new(
            GateId::TestSuite,
            checks,
            500,
            "2026-01-01T00:00:00Z".into(),
        );
        let json = serde_json::to_string(&report).unwrap();
        let restored: GateReport = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, report);
    }

    #[test]
    fn gate_report_version_and_format() {
        let report = GateReport::new(GateId::Smoke, vec![], 0, "now".into());
        assert_eq!(report.version, GATE_REPORT_VERSION);
        assert_eq!(report.format, GATE_REPORT_FORMAT);
    }

    #[test]
    fn gate_report_empty_checks_is_pass() {
        let report = GateReport::new(GateId::Smoke, vec![], 0, "now".into());
        assert_eq!(report.status, GateStatus::Pass);
        assert_eq!(report.total_count, 0);
    }

    // ── Waiver ───────────────────────────────────────────────────────────

    #[test]
    fn parse_single_waiver_from_pr() {
        let body = r#"Some PR text.

<!-- replay-waiver
gate: smoke
check: schema_validation
reason: Known issue #123
author: dev@example.com
expires: 2026-03-01T00:00:00Z
-->

More text."#;
        let waivers = parse_waivers(body);
        assert_eq!(waivers.len(), 1);
        assert_eq!(waivers[0].gate, GateId::Smoke);
        assert_eq!(waivers[0].check_name, "schema_validation");
        assert_eq!(waivers[0].reason, "Known issue #123");
        assert_eq!(waivers[0].author, "dev@example.com");
        assert_eq!(
            waivers[0].expires_at.as_deref(),
            Some("2026-03-01T00:00:00Z")
        );
    }

    #[test]
    fn parse_multiple_waivers() {
        let body = r#"<!-- replay-waiver
gate: smoke
check: *
reason: Emergency hotfix
author: ops
-->
Some text
<!-- replay-waiver
gate: regression
check: performance_budgets
reason: Known perf regression in new feature
author: dev
expires: 2026-04-01T00:00:00Z
-->"#;
        let waivers = parse_waivers(body);
        assert_eq!(waivers.len(), 2);
        assert_eq!(waivers[0].gate, GateId::Smoke);
        assert_eq!(waivers[0].check_name, "*");
        assert_eq!(waivers[1].gate, GateId::Regression);
        assert_eq!(waivers[1].check_name, "performance_budgets");
    }

    #[test]
    fn parse_waiver_missing_reason_skipped() {
        let body = r#"<!-- replay-waiver
gate: smoke
check: test
-->"#;
        let waivers = parse_waivers(body);
        assert!(waivers.is_empty());
    }

    #[test]
    fn parse_waiver_invalid_gate_skipped() {
        let body = r#"<!-- replay-waiver
gate: invalid_gate
reason: something
-->"#;
        let waivers = parse_waivers(body);
        assert!(waivers.is_empty());
    }

    #[test]
    fn parse_waiver_no_markers_returns_empty() {
        let waivers = parse_waivers("Just a normal PR description");
        assert!(waivers.is_empty());
    }

    #[test]
    fn waiver_not_expired() {
        let waiver = Waiver {
            gate: GateId::Smoke,
            check_name: "*".into(),
            reason: "test".into(),
            author: "dev".into(),
            expires_at: Some("2027-01-01T00:00:00Z".into()),
            pr_reference: None,
        };
        assert!(!waiver.is_expired_at("2026-06-01T00:00:00Z"));
    }

    #[test]
    fn waiver_expired() {
        let waiver = Waiver {
            gate: GateId::Smoke,
            check_name: "*".into(),
            reason: "test".into(),
            author: "dev".into(),
            expires_at: Some("2026-01-01T00:00:00Z".into()),
            pr_reference: None,
        };
        assert!(waiver.is_expired_at("2026-06-01T00:00:00Z"));
    }

    #[test]
    fn waiver_no_expiry_never_expires() {
        let waiver = Waiver {
            gate: GateId::Smoke,
            check_name: "*".into(),
            reason: "permanent".into(),
            author: "dev".into(),
            expires_at: None,
            pr_reference: None,
        };
        assert!(!waiver.is_expired_at("2099-12-31T23:59:59Z"));
    }

    #[test]
    fn waiver_matches_wildcard_check() {
        let waiver = Waiver {
            gate: GateId::Smoke,
            check_name: "*".into(),
            reason: "test".into(),
            author: "dev".into(),
            expires_at: None,
            pr_reference: None,
        };
        assert!(waiver.matches_check(GateId::Smoke, "schema_validation"));
        assert!(waiver.matches_check(GateId::Smoke, "any_check"));
        assert!(!waiver.matches_check(GateId::Regression, "any_check"));
    }

    #[test]
    fn waiver_matches_specific_check() {
        let waiver = Waiver {
            gate: GateId::Regression,
            check_name: "performance_budgets".into(),
            reason: "test".into(),
            author: "dev".into(),
            expires_at: None,
            pr_reference: None,
        };
        assert!(waiver.matches_check(GateId::Regression, "performance_budgets"));
        assert!(!waiver.matches_check(GateId::Regression, "e2e_scenarios"));
    }

    #[test]
    fn apply_waiver_changes_status() {
        let checks = vec![GateCheck {
            name: "test".into(),
            passed: false,
            message: "fail".into(),
            duration_ms: None,
            artifact_path: None,
        }];
        let mut report = GateReport::new(GateId::Smoke, checks, 100, "2026-01-01T00:00:00Z".into());
        assert_eq!(report.status, GateStatus::Fail);

        let waiver = Waiver {
            gate: GateId::Smoke,
            check_name: "*".into(),
            reason: "approved hotfix".into(),
            author: "ops".into(),
            expires_at: Some("2026-12-01T00:00:00Z".into()),
            pr_reference: None,
        };
        report.apply_waiver(waiver);
        assert_eq!(report.status, GateStatus::Waived);
        assert!(report.is_pass());
        assert!(report.waiver.is_some());
    }

    #[test]
    fn apply_expired_waiver_no_change() {
        let checks = vec![GateCheck {
            name: "test".into(),
            passed: false,
            message: "fail".into(),
            duration_ms: None,
            artifact_path: None,
        }];
        let mut report = GateReport::new(GateId::Smoke, checks, 100, "2026-06-01T00:00:00Z".into());
        assert_eq!(report.status, GateStatus::Fail);

        let waiver = Waiver {
            gate: GateId::Smoke,
            check_name: "*".into(),
            reason: "old waiver".into(),
            author: "ops".into(),
            expires_at: Some("2026-01-01T00:00:00Z".into()),
            pr_reference: None,
        };
        report.apply_waiver(waiver);
        assert_eq!(report.status, GateStatus::Fail);
        assert!(report.waiver.is_none());
    }

    #[test]
    fn apply_waiver_on_pass_no_change() {
        let checks = vec![GateCheck {
            name: "test".into(),
            passed: true,
            message: "ok".into(),
            duration_ms: None,
            artifact_path: None,
        }];
        let mut report = GateReport::new(GateId::Smoke, checks, 100, "2026-01-01T00:00:00Z".into());
        assert_eq!(report.status, GateStatus::Pass);

        let waiver = Waiver {
            gate: GateId::Smoke,
            check_name: "*".into(),
            reason: "not needed".into(),
            author: "ops".into(),
            expires_at: None,
            pr_reference: None,
        };
        report.apply_waiver(waiver);
        assert_eq!(report.status, GateStatus::Pass); // stays Pass, not Waived
    }

    // ── Evidence Bundle ──────────────────────────────────────────────────

    #[test]
    fn evidence_bundle_all_pass() {
        let r1 = GateReport::new(GateId::Smoke, vec![], 10, "now".into());
        let r2 = GateReport::new(GateId::TestSuite, vec![], 20, "now".into());
        let bundle = EvidenceBundle::new(vec![r1, r2], "now".into());
        assert_eq!(bundle.overall_status, GateStatus::Pass);
        assert!(bundle.is_promotable());
    }

    #[test]
    fn evidence_bundle_one_fail() {
        let r1 = GateReport::new(GateId::Smoke, vec![], 10, "now".into());
        let checks = vec![GateCheck {
            name: "x".into(),
            passed: false,
            message: "bad".into(),
            duration_ms: None,
            artifact_path: None,
        }];
        let r2 = GateReport::new(GateId::TestSuite, checks, 20, "now".into());
        let bundle = EvidenceBundle::new(vec![r1, r2], "now".into());
        assert_eq!(bundle.overall_status, GateStatus::Fail);
        assert!(!bundle.is_promotable());
    }

    #[test]
    fn evidence_bundle_waived_is_promotable() {
        let checks = vec![GateCheck {
            name: "x".into(),
            passed: false,
            message: "fail".into(),
            duration_ms: None,
            artifact_path: None,
        }];
        let mut r1 = GateReport::new(GateId::Smoke, checks, 10, "2026-01-01T00:00:00Z".into());
        r1.apply_waiver(Waiver {
            gate: GateId::Smoke,
            check_name: "*".into(),
            reason: "hotfix".into(),
            author: "ops".into(),
            expires_at: None,
            pr_reference: None,
        });
        let bundle = EvidenceBundle::new(vec![r1], "now".into());
        assert_eq!(bundle.overall_status, GateStatus::Waived);
        assert!(bundle.is_promotable());
    }

    #[test]
    fn evidence_bundle_serde_roundtrip() {
        let r1 = GateReport::new(GateId::Smoke, vec![], 10, "now".into());
        let bundle = EvidenceBundle::new(vec![r1], "now".into());
        let json = serde_json::to_string(&bundle).unwrap();
        let restored: EvidenceBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, bundle);
    }

    #[test]
    fn evidence_bundle_collects_artifact_paths() {
        let mut r1 = GateReport::new(GateId::Regression, vec![], 10, "now".into());
        r1.evidence_bundle_path = Some("/path/to/evidence".into());
        let bundle = EvidenceBundle::new(vec![r1], "now".into());
        assert_eq!(bundle.artifact_paths, vec!["/path/to/evidence"]);
    }

    // ── Gate 1 Evaluation ────────────────────────────────────────────────

    #[test]
    fn gate1_all_smoke_pass() {
        let smokes = vec![
            ("S-01".into(), true),
            ("S-02".into(), true),
            ("S-03".into(), true),
        ];
        let report = evaluate_gate1_smoke(true, &smokes, 500, "now");
        assert_eq!(report.status, GateStatus::Pass);
        assert_eq!(report.total_count, 4); // schema + 3 smokes
    }

    #[test]
    fn gate1_schema_fail_blocks() {
        let report = evaluate_gate1_smoke(false, &[], 100, "now");
        assert_eq!(report.status, GateStatus::Fail);
        assert_eq!(report.fail_count, 1);
    }

    #[test]
    fn gate1_smoke_failure_blocks() {
        let smokes = vec![("S-01".into(), true), ("S-02".into(), false)];
        let report = evaluate_gate1_smoke(true, &smokes, 200, "now");
        assert_eq!(report.status, GateStatus::Fail);
        assert_eq!(report.pass_count, 2);
        assert_eq!(report.fail_count, 1);
    }

    // ── Gate 2 Evaluation ────────────────────────────────────────────────

    #[test]
    fn gate2_all_pass() {
        let results = TestSuiteResults {
            unit_tests_passed: 100,
            unit_tests_total: 100,
            proptest_cases: 200,
            proptest_passed: true,
            integration_tests_passed: 20,
            integration_tests_total: 20,
        };
        let report = evaluate_gate2_test_suite(&results, 5000, "now");
        assert_eq!(report.status, GateStatus::Pass);
        assert_eq!(report.pass_count, 4);
    }

    #[test]
    fn gate2_unit_test_failure() {
        let results = TestSuiteResults {
            unit_tests_passed: 99,
            unit_tests_total: 100,
            proptest_cases: 200,
            proptest_passed: true,
            integration_tests_passed: 20,
            integration_tests_total: 20,
        };
        let report = evaluate_gate2_test_suite(&results, 5000, "now");
        assert_eq!(report.status, GateStatus::Fail);
        assert_eq!(report.fail_count, 1);
    }

    #[test]
    fn gate2_insufficient_proptest_cases() {
        let results = TestSuiteResults {
            unit_tests_passed: 50,
            unit_tests_total: 50,
            proptest_cases: 50, // below MIN_PROPTEST_CASES
            proptest_passed: true,
            integration_tests_passed: 10,
            integration_tests_total: 10,
        };
        let report = evaluate_gate2_test_suite(&results, 3000, "now");
        assert_eq!(report.status, GateStatus::Fail);
    }

    #[test]
    fn gate2_proptest_failure() {
        let results = TestSuiteResults {
            unit_tests_passed: 50,
            unit_tests_total: 50,
            proptest_cases: 200,
            proptest_passed: false,
            integration_tests_passed: 10,
            integration_tests_total: 10,
        };
        let report = evaluate_gate2_test_suite(&results, 3000, "now");
        assert_eq!(report.status, GateStatus::Fail);
    }

    // ── Gate 3 Evaluation ────────────────────────────────────────────────

    #[test]
    fn gate3_all_pass() {
        let results = RegressionResults {
            e2e_passed: true,
            e2e_scenario_count: 5,
            regression_suite_passed: true,
            regression_divergence_count: 0,
            blocking_metric_count: 0,
            warning_metric_count: 0,
            evidence_bundle_path: Some("/evidence".into()),
        };
        let report = evaluate_gate3_regression(&results, 10000, "now");
        assert_eq!(report.status, GateStatus::Pass);
        assert_eq!(report.evidence_bundle_path.as_deref(), Some("/evidence"));
    }

    #[test]
    fn gate3_regression_failure() {
        let results = RegressionResults {
            e2e_passed: true,
            e2e_scenario_count: 5,
            regression_suite_passed: false,
            regression_divergence_count: 10,
            blocking_metric_count: 0,
            warning_metric_count: 2,
            evidence_bundle_path: None,
        };
        let report = evaluate_gate3_regression(&results, 15000, "now");
        assert_eq!(report.status, GateStatus::Fail);
    }

    #[test]
    fn gate3_blocking_metric_fails() {
        let results = RegressionResults {
            e2e_passed: true,
            e2e_scenario_count: 5,
            regression_suite_passed: true,
            regression_divergence_count: 0,
            blocking_metric_count: 1,
            warning_metric_count: 0,
            evidence_bundle_path: None,
        };
        let report = evaluate_gate3_regression(&results, 15000, "now");
        assert_eq!(report.status, GateStatus::Fail);
    }

    // ── Path Trigger ─────────────────────────────────────────────────────

    #[test]
    fn path_trigger_matches_replay_source() {
        assert!(matches_replay_path(
            "crates/frankenterm-core/src/replay_mcp.rs"
        ));
        assert!(matches_replay_path(
            "crates/frankenterm-core/src/replay_guide.rs"
        ));
    }

    #[test]
    fn path_trigger_matches_replay_tests() {
        assert!(matches_replay_path(
            "crates/frankenterm-core/tests/proptest_replay_mcp.rs"
        ));
    }

    #[test]
    fn path_trigger_matches_replay_benches() {
        assert!(matches_replay_path(
            "crates/frankenterm-core/benches/replay_capture.rs"
        ));
    }

    #[test]
    fn path_trigger_matches_e2e_scripts() {
        assert!(matches_replay_path("tests/e2e/test_replay_performance.sh"));
    }

    #[test]
    fn path_trigger_matches_gate_scripts() {
        assert!(matches_replay_path(
            "scripts/check_replay_performance_gates.sh"
        ));
    }

    #[test]
    fn path_trigger_matches_workflow() {
        assert!(matches_replay_path(".github/workflows/replay-gates.yml"));
    }

    #[test]
    fn path_trigger_excludes_non_replay() {
        assert!(!matches_replay_path(
            "crates/frankenterm-core/src/storage.rs"
        ));
        assert!(!matches_replay_path("src/main.rs"));
        assert!(!matches_replay_path("Cargo.toml"));
    }

    // ── PR Status Summary ────────────────────────────────────────────────

    #[test]
    fn pr_status_summary_all_pass() {
        let r1 = GateReport::new(GateId::Smoke, vec![], 10, "now".into());
        let r2 = GateReport::new(GateId::TestSuite, vec![], 20, "now".into());
        let bundle = EvidenceBundle::new(vec![r1, r2], "now".into());
        let summary = pr_status_summary(&bundle);
        assert!(summary.contains("All replay gates passed"));
    }

    #[test]
    fn pr_status_summary_with_failure() {
        let checks = vec![GateCheck {
            name: "x".into(),
            passed: false,
            message: "bad".into(),
            duration_ms: None,
            artifact_path: None,
        }];
        let r1 = GateReport::new(GateId::Smoke, checks, 10, "now".into());
        let bundle = EvidenceBundle::new(vec![r1], "now".into());
        let summary = pr_status_summary(&bundle);
        assert!(summary.contains("PR blocked"));
    }

    // ── MCP Schema ───────────────────────────────────────────────────────

    #[test]
    fn schema_has_additional_properties_false() {
        let schema = gate_tool_schema();
        let addl = schema["input_schema"]["additionalProperties"].as_bool();
        assert_eq!(addl, Some(false));
    }

    #[test]
    fn schema_gate_field_required() {
        let schema = gate_tool_schema();
        let required = schema["input_schema"]["required"].as_array().unwrap();
        let has_gate = required.iter().any(|v| v.as_str() == Some("gate"));
        assert!(has_gate);
    }

    #[test]
    fn schema_tagged_replay() {
        let schema = gate_tool_schema();
        let tags = schema["tags"].as_array().unwrap();
        let has_replay = tags.iter().any(|v| v.as_str() == Some("replay"));
        assert!(has_replay);
    }

    // ── Waiver serde ─────────────────────────────────────────────────────

    #[test]
    fn waiver_serde_roundtrip() {
        let waiver = Waiver {
            gate: GateId::Regression,
            check_name: "performance_budgets".into(),
            reason: "Known regression in feature X".into(),
            author: "dev@example.com".into(),
            expires_at: Some("2026-03-01T00:00:00Z".into()),
            pr_reference: Some("#456".into()),
        };
        let json = serde_json::to_string(&waiver).unwrap();
        let restored: Waiver = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, waiver);
    }

    // ── Nightly cron ─────────────────────────────────────────────────────

    #[test]
    fn nightly_cron_is_valid_format() {
        let parts: Vec<&str> = NIGHTLY_CRON.split_whitespace().collect();
        assert_eq!(parts.len(), 5, "cron expression should have 5 fields");
        assert_eq!(parts[0], "0", "minute should be 0");
        assert_eq!(parts[1], "4", "hour should be 4 (UTC)");
    }

    // ── GateCheck serde ──────────────────────────────────────────────────

    #[test]
    fn gate_check_serde_roundtrip() {
        let check = GateCheck {
            name: "test_check".into(),
            passed: true,
            message: "All good".into(),
            duration_ms: Some(42),
            artifact_path: Some("/tmp/result.json".into()),
        };
        let json = serde_json::to_string(&check).unwrap();
        let restored: GateCheck = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, check);
    }

    #[test]
    fn gate_check_optional_fields_omitted() {
        let check = GateCheck {
            name: "basic".into(),
            passed: true,
            message: "ok".into(),
            duration_ms: None,
            artifact_path: None,
        };
        let json = serde_json::to_string(&check).unwrap();
        assert!(!json.contains("duration_ms"));
        assert!(!json.contains("artifact_path"));
    }
}
