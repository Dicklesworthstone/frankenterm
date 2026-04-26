//! Unified replay test orchestrator, CI evidence bundle, and log-retention policy.
//!
//! Bead: ft-og6q6.7.7
//!
//! Runs all replay test classes in gate order (Gate 1 → 2 → 3), generates
//! CI evidence bundles with manifests and checksums, and enforces a
//! configurable log retention policy.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::replay_ci_gate::{ALL_GATES, GateId, GateReport, GateStatus};

// ── Constants ────────────────────────────────────────────────────────────────

/// Default retention period for gate reports and evidence (days).
pub const DEFAULT_RETENTION_DAYS: u64 = 90;

/// Default maximum concurrency for parallel test execution within a gate.
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;

/// Evidence directory name.
pub const EVIDENCE_DIR: &str = "evidence";

// ── Orchestrator Config ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    /// Whether to stop at the first gate failure.
    pub fail_fast: bool,
    /// Maximum parallelism within a gate.
    pub max_concurrency: usize,
    /// Which gates to run (None = all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_filter: Option<Vec<GateId>>,
    /// Output format.
    pub format: OrchestratorFormat,
    /// Retention policy for evidence files.
    pub retention_days: u64,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            fail_fast: true,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            gate_filter: None,
            format: OrchestratorFormat::Human,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

impl OrchestratorConfig {
    /// Gates to execute (filtered or all).
    #[must_use]
    pub fn gates_to_run(&self) -> Vec<GateId> {
        match &self.gate_filter {
            Some(filter) => filter.clone(),
            None => ALL_GATES.to_vec(),
        }
    }

    /// Create config for a specific gate only.
    #[must_use]
    pub fn for_gate(gate: GateId) -> Self {
        Self {
            gate_filter: Some(vec![gate]),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorFormat {
    Human,
    Json,
}

// ── Orchestrator Result ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorResult {
    pub gate_results: BTreeMap<String, GateRunResult>,
    pub gates_run: usize,
    pub gates_passed: usize,
    pub gates_failed: usize,
    pub overall_status: GateStatus,
    pub total_duration_ms: u64,
    pub fail_fast_triggered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_path: Option<String>,
}

impl OrchestratorResult {
    #[must_use]
    pub fn is_pass(&self) -> bool {
        self.overall_status == GateStatus::Pass || self.overall_status == GateStatus::Waived
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateRunResult {
    pub gate: GateId,
    pub status: GateStatus,
    pub pass_count: usize,
    pub fail_count: usize,
    pub total_count: usize,
    pub duration_ms: u64,
}

impl From<&GateReport> for GateRunResult {
    fn from(report: &GateReport) -> Self {
        Self {
            gate: report.gate,
            status: report.status,
            pass_count: report.pass_count,
            fail_count: report.fail_count,
            total_count: report.total_count,
            duration_ms: report.duration_ms,
        }
    }
}

// ── Orchestrate ──────────────────────────────────────────────────────────────

/// Run the orchestration pipeline with pre-computed gate reports.
///
/// This is the pure evaluation logic; actual test execution is handled
/// by the caller. The orchestrator sequences gates and applies fail-fast.
#[must_use]
pub fn orchestrate(config: &OrchestratorConfig, reports: &[GateReport]) -> OrchestratorResult {
    let gates = config.gates_to_run();
    let mut gate_results = BTreeMap::new();
    let mut total_duration = 0u64;
    let mut fail_fast_triggered = false;
    let mut gates_run = 0usize;

    for gate in &gates {
        if let Some(report) = reports.iter().find(|r| r.gate == *gate) {
            let run_result = GateRunResult::from(report);
            total_duration = total_duration.saturating_add(report.duration_ms);
            gate_results.insert(gate.as_str().to_string(), run_result);
            gates_run += 1;

            if config.fail_fast && report.status == GateStatus::Fail {
                fail_fast_triggered = true;
                break;
            }
        }
    }

    let gates_passed = gate_results
        .values()
        .filter(|r| r.status == GateStatus::Pass || r.status == GateStatus::Waived)
        .count();
    let gates_failed = gate_results
        .values()
        .filter(|r| r.status == GateStatus::Fail)
        .count();

    let overall_status = if gates_failed > 0 {
        GateStatus::Fail
    } else if gate_results
        .values()
        .any(|r| r.status == GateStatus::Waived)
    {
        GateStatus::Waived
    } else if gate_results
        .values()
        .any(|r| r.status == GateStatus::Pending)
    {
        GateStatus::Pending
    } else {
        GateStatus::Pass
    };

    OrchestratorResult {
        gate_results,
        gates_run,
        gates_passed,
        gates_failed,
        overall_status,
        total_duration_ms: total_duration,
        fail_fast_triggered,
        evidence_path: None,
    }
}

// ── Evidence Manifest ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceManifest {
    pub version: String,
    pub generated_at: String,
    pub files: Vec<ManifestEntry>,
    pub total_size_bytes: u64,
    pub retention_days: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub size_bytes: u64,
    pub checksum: String,
    pub file_type: ManifestFileType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestFileType {
    GateReport,
    RegressionResult,
    DiffReport,
    ProvenanceLog,
    TestOutput,
    Summary,
}

impl EvidenceManifest {
    /// Create a new manifest from entries.
    #[must_use]
    pub fn new(entries: Vec<ManifestEntry>, generated_at: String, retention_days: u64) -> Self {
        let total_size_bytes = entries.iter().map(|e| e.size_bytes).sum();
        Self {
            version: "1".into(),
            generated_at,
            files: entries,
            total_size_bytes,
            retention_days,
        }
    }

    /// Check if manifest contains a file of the given type.
    #[must_use]
    pub fn has_file_type(&self, file_type: ManifestFileType) -> bool {
        self.files.iter().any(|e| e.file_type == file_type)
    }

    /// Count files of a given type.
    #[must_use]
    pub fn count_by_type(&self, file_type: ManifestFileType) -> usize {
        self.files
            .iter()
            .filter(|e| e.file_type == file_type)
            .count()
    }
}

// ── Log Retention ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Gate reports retention (days).
    pub gate_reports_days: u64,
    /// Regression suite logs retention (days).
    pub regression_logs_days: u64,
    /// Test output logs retention (days).
    pub test_output_days: u64,
    /// Waiver records: permanent (stored in PR/git).
    pub waiver_permanent: bool,
    /// Emergency override log: permanent (stored in git).
    pub emergency_override_permanent: bool,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            gate_reports_days: DEFAULT_RETENTION_DAYS,
            regression_logs_days: DEFAULT_RETENTION_DAYS,
            test_output_days: DEFAULT_RETENTION_DAYS,
            waiver_permanent: true,
            emergency_override_permanent: true,
        }
    }
}

/// Determine which files should be pruned based on age.
#[must_use]
pub fn evaluate_retention(
    files: &[RetentionCandidate],
    policy: &RetentionPolicy,
    _current_age_days: u64,
) -> Vec<PruneDecision> {
    files
        .iter()
        .map(|f| {
            let max_age = match f.file_type {
                ManifestFileType::GateReport => policy.gate_reports_days,
                ManifestFileType::RegressionResult => policy.regression_logs_days,
                ManifestFileType::DiffReport => policy.regression_logs_days,
                ManifestFileType::ProvenanceLog => policy.regression_logs_days,
                ManifestFileType::TestOutput => policy.test_output_days,
                ManifestFileType::Summary => policy.gate_reports_days,
            };
            let should_prune = f.age_days > max_age;
            PruneDecision {
                path: f.path.clone(),
                age_days: f.age_days,
                max_age_days: max_age,
                prune: should_prune,
                reason: if should_prune {
                    format!("age {} > limit {}", f.age_days, max_age)
                } else {
                    "within retention period".into()
                },
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionCandidate {
    pub path: String,
    pub age_days: u64,
    pub file_type: ManifestFileType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneDecision {
    pub path: String,
    pub age_days: u64,
    pub max_age_days: u64,
    pub prune: bool,
    pub reason: String,
}

/// Count files that would be pruned.
#[must_use]
pub fn prune_count(decisions: &[PruneDecision]) -> usize {
    decisions.iter().filter(|d| d.prune).count()
}

// ── Summary Report ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryReport {
    pub gates: BTreeMap<String, GateSummary>,
    pub overall: GateStatus,
    pub evidence_path: Option<String>,
    pub generated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateSummary {
    pub pass: usize,
    pub fail: usize,
    pub skip: usize,
    pub duration_ms: u64,
}

impl SummaryReport {
    /// Generate summary from orchestrator result.
    #[must_use]
    pub fn from_result(result: &OrchestratorResult, generated_at: String) -> Self {
        let mut gates = BTreeMap::new();
        for (name, run) in &result.gate_results {
            gates.insert(
                name.clone(),
                GateSummary {
                    pass: run.pass_count,
                    fail: run.fail_count,
                    skip: run
                        .total_count
                        .saturating_sub(run.pass_count + run.fail_count),
                    duration_ms: run.duration_ms,
                },
            );
        }
        Self {
            gates,
            overall: result.overall_status,
            evidence_path: result.evidence_path.clone(),
            generated_at,
        }
    }

    /// Render as a markdown PR comment.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();
        md.push_str("## Replay Test Summary\n\n");
        md.push_str("| Gate | Pass | Fail | Skip | Duration |\n");
        md.push_str("|------|------|------|------|----------|\n");

        for (name, summary) in &self.gates {
            md.push_str(&format!(
                "| {} | {} | {} | {} | {}ms |\n",
                name, summary.pass, summary.fail, summary.skip, summary.duration_ms
            ));
        }
        md.push('\n');

        let status_str = match self.overall {
            GateStatus::Pass => "PASS",
            GateStatus::Fail => "FAIL",
            GateStatus::Pending => "PENDING",
            GateStatus::Skipped => "SKIPPED",
            GateStatus::Waived => "PASS (waived)",
        };
        md.push_str(&format!("**Overall: {}**\n", status_str));

        if let Some(path) = &self.evidence_path {
            md.push_str(&format!("\nEvidence: `{}`\n", path));
        }
        md
    }

    /// Total pass count across all gates.
    #[must_use]
    pub fn total_pass(&self) -> usize {
        self.gates.values().map(|g| g.pass).sum()
    }

    /// Total fail count across all gates.
    #[must_use]
    pub fn total_fail(&self) -> usize {
        self.gates.values().map(|g| g.fail).sum()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay_ci_gate::GateCheck;

    fn make_pass_report(gate: GateId) -> GateReport {
        GateReport::new(
            gate,
            vec![GateCheck {
                name: "ok".into(),
                passed: true,
                message: "pass".into(),
                duration_ms: Some(10),
                artifact_path: None,
            }],
            100,
            "2026-01-01T00:00:00Z".into(),
        )
    }

    fn make_fail_report(gate: GateId) -> GateReport {
        GateReport::new(
            gate,
            vec![GateCheck {
                name: "bad".into(),
                passed: false,
                message: "fail".into(),
                duration_ms: None,
                artifact_path: None,
            }],
            200,
            "2026-01-01T00:00:00Z".into(),
        )
    }

    // ── Orchestrator Config ──────────────────────────────────────────────

    #[test]
    fn default_config_fail_fast() {
        let config = OrchestratorConfig::default();
        assert!(config.fail_fast);
        assert_eq!(config.max_concurrency, DEFAULT_MAX_CONCURRENCY);
        assert_eq!(config.retention_days, DEFAULT_RETENTION_DAYS);
    }

    #[test]
    fn config_gates_to_run_default_all() {
        let config = OrchestratorConfig::default();
        let gates = config.gates_to_run();
        assert_eq!(gates.len(), 3);
    }

    #[test]
    fn config_gates_to_run_filtered() {
        let config = OrchestratorConfig::for_gate(GateId::Smoke);
        let gates = config.gates_to_run();
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0], GateId::Smoke);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = OrchestratorConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let restored: OrchestratorConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, config);
    }

    // ── Orchestrate ──────────────────────────────────────────────────────

    #[test]
    fn orchestrate_all_pass() {
        let config = OrchestratorConfig::default();
        let reports = vec![
            make_pass_report(GateId::Smoke),
            make_pass_report(GateId::TestSuite),
            make_pass_report(GateId::Regression),
        ];
        let result = orchestrate(&config, &reports);
        assert_eq!(result.overall_status, GateStatus::Pass);
        assert_eq!(result.gates_run, 3);
        assert_eq!(result.gates_passed, 3);
        assert_eq!(result.gates_failed, 0);
        assert!(!result.fail_fast_triggered);
        assert!(result.is_pass());
    }

    #[test]
    fn orchestrate_gate1_fail_fast() {
        let config = OrchestratorConfig {
            fail_fast: true,
            ..Default::default()
        };
        let reports = vec![
            make_fail_report(GateId::Smoke),
            make_pass_report(GateId::TestSuite),
            make_pass_report(GateId::Regression),
        ];
        let result = orchestrate(&config, &reports);
        assert_eq!(result.overall_status, GateStatus::Fail);
        assert_eq!(result.gates_run, 1); // stopped after gate 1
        assert!(result.fail_fast_triggered);
    }

    #[test]
    fn orchestrate_no_fail_fast_continues() {
        let config = OrchestratorConfig {
            fail_fast: false,
            ..Default::default()
        };
        let reports = vec![
            make_fail_report(GateId::Smoke),
            make_pass_report(GateId::TestSuite),
            make_pass_report(GateId::Regression),
        ];
        let result = orchestrate(&config, &reports);
        assert_eq!(result.overall_status, GateStatus::Fail);
        assert_eq!(result.gates_run, 3); // ran all
        assert!(!result.fail_fast_triggered);
    }

    #[test]
    fn orchestrate_gate2_fail_fast() {
        let config = OrchestratorConfig {
            fail_fast: true,
            ..Default::default()
        };
        let reports = vec![
            make_pass_report(GateId::Smoke),
            make_fail_report(GateId::TestSuite),
            make_pass_report(GateId::Regression),
        ];
        let result = orchestrate(&config, &reports);
        assert_eq!(result.gates_run, 2);
        assert!(result.fail_fast_triggered);
        assert_eq!(result.gates_passed, 1);
        assert_eq!(result.gates_failed, 1);
    }

    #[test]
    fn orchestrate_single_gate_filter() {
        let config = OrchestratorConfig::for_gate(GateId::Regression);
        let reports = vec![make_pass_report(GateId::Regression)];
        let result = orchestrate(&config, &reports);
        assert_eq!(result.gates_run, 1);
        assert_eq!(result.overall_status, GateStatus::Pass);
    }

    #[test]
    fn orchestrate_empty_reports() {
        let config = OrchestratorConfig::default();
        let result = orchestrate(&config, &[]);
        assert_eq!(result.gates_run, 0);
        assert_eq!(result.overall_status, GateStatus::Pass);
    }

    #[test]
    fn orchestrate_duration_accumulates() {
        let config = OrchestratorConfig::default();
        let reports = vec![
            make_pass_report(GateId::Smoke),      // 100ms
            make_pass_report(GateId::TestSuite),  // 100ms
            make_pass_report(GateId::Regression), // 100ms
        ];
        let result = orchestrate(&config, &reports);
        assert_eq!(result.total_duration_ms, 300);
    }

    #[test]
    fn orchestrate_result_serde_roundtrip() {
        let config = OrchestratorConfig::default();
        let reports = vec![make_pass_report(GateId::Smoke)];
        let result = orchestrate(&config, &reports);
        let json = serde_json::to_string(&result).unwrap();
        let restored: OrchestratorResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, result);
    }

    // ── Evidence Manifest ────────────────────────────────────────────────

    #[test]
    fn manifest_total_size() {
        let entries = vec![
            ManifestEntry {
                path: "gate1-report.json".into(),
                size_bytes: 1000,
                checksum: "abc".into(),
                file_type: ManifestFileType::GateReport,
            },
            ManifestEntry {
                path: "regression.json".into(),
                size_bytes: 5000,
                checksum: "def".into(),
                file_type: ManifestFileType::RegressionResult,
            },
        ];
        let manifest = EvidenceManifest::new(entries, "now".into(), 90);
        assert_eq!(manifest.total_size_bytes, 6000);
        assert_eq!(manifest.files.len(), 2);
    }

    #[test]
    fn manifest_has_file_type() {
        let entries = vec![ManifestEntry {
            path: "gate.json".into(),
            size_bytes: 100,
            checksum: "abc".into(),
            file_type: ManifestFileType::GateReport,
        }];
        let manifest = EvidenceManifest::new(entries, "now".into(), 90);
        assert!(manifest.has_file_type(ManifestFileType::GateReport));
        assert!(!manifest.has_file_type(ManifestFileType::DiffReport));
    }

    #[test]
    fn manifest_count_by_type() {
        let entries = vec![
            ManifestEntry {
                path: "a.json".into(),
                size_bytes: 100,
                checksum: "a".into(),
                file_type: ManifestFileType::GateReport,
            },
            ManifestEntry {
                path: "b.json".into(),
                size_bytes: 200,
                checksum: "b".into(),
                file_type: ManifestFileType::GateReport,
            },
            ManifestEntry {
                path: "c.json".into(),
                size_bytes: 300,
                checksum: "c".into(),
                file_type: ManifestFileType::TestOutput,
            },
        ];
        let manifest = EvidenceManifest::new(entries, "now".into(), 90);
        assert_eq!(manifest.count_by_type(ManifestFileType::GateReport), 2);
        assert_eq!(manifest.count_by_type(ManifestFileType::TestOutput), 1);
        assert_eq!(manifest.count_by_type(ManifestFileType::DiffReport), 0);
    }

    #[test]
    fn manifest_serde_roundtrip() {
        let entries = vec![ManifestEntry {
            path: "test.json".into(),
            size_bytes: 500,
            checksum: "sha256:abc123".into(),
            file_type: ManifestFileType::Summary,
        }];
        let manifest = EvidenceManifest::new(entries, "2026-01-01T00:00:00Z".into(), 90);
        let json = serde_json::to_string(&manifest).unwrap();
        let restored: EvidenceManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, manifest);
    }

    #[test]
    fn manifest_empty_valid() {
        let manifest = EvidenceManifest::new(vec![], "now".into(), 90);
        assert_eq!(manifest.total_size_bytes, 0);
        assert_eq!(manifest.files.len(), 0);
    }

    // ── Retention Policy ─────────────────────────────────────────────────

    #[test]
    fn default_retention_90_days() {
        let policy = RetentionPolicy::default();
        assert_eq!(policy.gate_reports_days, 90);
        assert_eq!(policy.regression_logs_days, 90);
        assert_eq!(policy.test_output_days, 90);
        assert!(policy.waiver_permanent);
        assert!(policy.emergency_override_permanent);
    }

    #[test]
    fn retention_prunes_old_files() {
        let policy = RetentionPolicy::default();
        let files = vec![
            RetentionCandidate {
                path: "old-gate.json".into(),
                age_days: 100,
                file_type: ManifestFileType::GateReport,
            },
            RetentionCandidate {
                path: "fresh-gate.json".into(),
                age_days: 30,
                file_type: ManifestFileType::GateReport,
            },
        ];
        let decisions = evaluate_retention(&files, &policy, 0);
        assert_eq!(decisions.len(), 2);
        assert!(decisions[0].prune);
        assert!(!decisions[1].prune);
    }

    #[test]
    fn retention_boundary_not_pruned() {
        let policy = RetentionPolicy::default();
        let files = vec![RetentionCandidate {
            path: "boundary.json".into(),
            age_days: 90,
            file_type: ManifestFileType::GateReport,
        }];
        let decisions = evaluate_retention(&files, &policy, 0);
        assert!(!decisions[0].prune); // exactly at limit, not pruned
    }

    #[test]
    fn retention_one_day_over_pruned() {
        let policy = RetentionPolicy::default();
        let files = vec![RetentionCandidate {
            path: "over.json".into(),
            age_days: 91,
            file_type: ManifestFileType::GateReport,
        }];
        let decisions = evaluate_retention(&files, &policy, 0);
        assert!(decisions[0].prune);
    }

    #[test]
    fn retention_different_types_different_limits() {
        let policy = RetentionPolicy {
            gate_reports_days: 30,
            regression_logs_days: 60,
            test_output_days: 14,
            ..Default::default()
        };
        let files = vec![
            RetentionCandidate {
                path: "gate.json".into(),
                age_days: 40,
                file_type: ManifestFileType::GateReport,
            },
            RetentionCandidate {
                path: "regression.json".into(),
                age_days: 40,
                file_type: ManifestFileType::RegressionResult,
            },
            RetentionCandidate {
                path: "output.log".into(),
                age_days: 20,
                file_type: ManifestFileType::TestOutput,
            },
        ];
        let decisions = evaluate_retention(&files, &policy, 0);
        assert!(decisions[0].prune); // gate: 40 > 30
        assert!(!decisions[1].prune); // regression: 40 <= 60
        assert!(decisions[2].prune); // output: 20 > 14
    }

    #[test]
    fn prune_count_correct() {
        let decisions = vec![
            PruneDecision {
                path: "a".into(),
                age_days: 100,
                max_age_days: 90,
                prune: true,
                reason: "old".into(),
            },
            PruneDecision {
                path: "b".into(),
                age_days: 50,
                max_age_days: 90,
                prune: false,
                reason: "ok".into(),
            },
            PruneDecision {
                path: "c".into(),
                age_days: 95,
                max_age_days: 90,
                prune: true,
                reason: "old".into(),
            },
        ];
        assert_eq!(prune_count(&decisions), 2);
    }

    #[test]
    fn retention_empty_files() {
        let policy = RetentionPolicy::default();
        let decisions = evaluate_retention(&[], &policy, 0);
        assert!(decisions.is_empty());
    }

    #[test]
    fn retention_policy_serde_roundtrip() {
        let policy = RetentionPolicy::default();
        let json = serde_json::to_string(&policy).unwrap();
        let restored: RetentionPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, policy);
    }

    // ── Summary Report ───────────────────────────────────────────────────

    #[test]
    fn summary_from_result() {
        let config = OrchestratorConfig::default();
        let reports = vec![
            make_pass_report(GateId::Smoke),
            make_pass_report(GateId::TestSuite),
        ];
        let result = orchestrate(&config, &reports);
        let summary = SummaryReport::from_result(&result, "now".into());
        assert_eq!(summary.gates.len(), 2);
        assert_eq!(summary.overall, GateStatus::Pass);
    }

    #[test]
    fn summary_totals() {
        let config = OrchestratorConfig {
            fail_fast: false,
            ..Default::default()
        };
        let reports = vec![
            make_pass_report(GateId::Smoke),
            make_fail_report(GateId::TestSuite),
        ];
        let result = orchestrate(&config, &reports);
        let summary = SummaryReport::from_result(&result, "now".into());
        assert_eq!(summary.total_pass(), 1);
        assert_eq!(summary.total_fail(), 1);
    }

    #[test]
    fn summary_markdown_contains_table() {
        let config = OrchestratorConfig::default();
        let reports = vec![make_pass_report(GateId::Smoke)];
        let result = orchestrate(&config, &reports);
        let summary = SummaryReport::from_result(&result, "now".into());
        let md = summary.to_markdown();
        assert!(md.contains("## Replay Test Summary"));
        assert!(md.contains("| Gate |"));
        assert!(md.contains("**Overall: PASS**"));
    }

    #[test]
    fn summary_markdown_shows_evidence() {
        let config = OrchestratorConfig::default();
        let reports = vec![make_pass_report(GateId::Smoke)];
        let mut result = orchestrate(&config, &reports);
        result.evidence_path = Some("evidence/bundle-123".into());
        let summary = SummaryReport::from_result(&result, "now".into());
        let md = summary.to_markdown();
        assert!(md.contains("evidence/bundle-123"));
    }

    #[test]
    fn summary_serde_roundtrip() {
        let config = OrchestratorConfig::default();
        let reports = vec![make_pass_report(GateId::Smoke)];
        let result = orchestrate(&config, &reports);
        let summary = SummaryReport::from_result(&result, "now".into());
        let json = serde_json::to_string(&summary).unwrap();
        let restored: SummaryReport = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, summary);
    }

    #[test]
    fn summary_fail_status() {
        let config = OrchestratorConfig::default();
        let reports = vec![make_fail_report(GateId::Smoke)];
        let result = orchestrate(&config, &reports);
        let summary = SummaryReport::from_result(&result, "now".into());
        let md = summary.to_markdown();
        assert!(md.contains("**Overall: FAIL**"));
    }

    #[test]
    fn summary_empty_result() {
        let result = OrchestratorResult {
            gate_results: BTreeMap::new(),
            gates_run: 0,
            gates_passed: 0,
            gates_failed: 0,
            overall_status: GateStatus::Pass,
            total_duration_ms: 0,
            fail_fast_triggered: false,
            evidence_path: None,
        };
        let summary = SummaryReport::from_result(&result, "now".into());
        assert_eq!(summary.total_pass(), 0);
        assert_eq!(summary.total_fail(), 0);
    }

    // ── GateRunResult ────────────────────────────────────────────────────

    #[test]
    fn gate_run_result_from_report() {
        let report = make_pass_report(GateId::Smoke);
        let run = GateRunResult::from(&report);
        assert_eq!(run.gate, GateId::Smoke);
        assert_eq!(run.status, GateStatus::Pass);
        assert_eq!(run.pass_count, 1);
        assert_eq!(run.fail_count, 0);
    }
}
