//! Finish-line release-readiness gate evaluation for the `ft-xbnl0` program.
//!
//! This module turns finish-line evidence into explicit go/no-go checks so
//! release work can consume a stable gate contract instead of re-deriving
//! thresholds from chat history or ad hoc spreadsheets.

use serde::{Deserialize, Serialize};

const GIB: u64 = 1024 * 1024 * 1024;

/// Explicit release decision produced from the gate set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseDecision {
    /// All blocking gates passed.
    Ready,
    /// One or more blocking gates failed.
    Blocked,
}

/// A single gate check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Pane scales that must appear in scale-lab artifact evidence.
    pub required_scale_lab_pane_scales: Vec<u64>,
    /// Minimum logical cores for high-scale release evidence.
    pub min_scale_lab_logical_cores: u64,
    /// Minimum memory bytes for high-scale release evidence.
    pub min_scale_lab_memory_bytes: u64,
    /// Required release-claim truth tier for graduating high-scale claims.
    pub required_scale_lab_release_claim_status: String,
    /// Required proof-manifest truth status for graduating high-scale claims.
    pub required_scale_lab_manifest_status: String,
    /// Required evidence mode for high-scale release evidence.
    pub required_scale_lab_evidence_mode: String,
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
            required_scale_lab_pane_scales: vec![50, 200, 1_000],
            min_scale_lab_logical_cores: 64,
            min_scale_lab_memory_bytes: 256 * GIB,
            required_scale_lab_release_claim_status: "real-hardware-proven".into(),
            required_scale_lab_manifest_status: "proven".into(),
            required_scale_lab_evidence_mode: "real_hardware_run".into(),
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
            ReleaseGateCheck {
                gate_id: "REL-05-scale-lab-evidence".into(),
                description: "Scale-lab artifact evidence supports high-scale release claims".into(),
                passed: inputs.scale_lab.satisfies_policy(self),
                blocking: true,
                observed: inputs.scale_lab.render_observed(),
                required: format!(
                    "fresh valid artifact with status={} manifest_status={} evidence_mode={} live_mux=true panes {:?} and host >= {} cores / {} GiB",
                    self.required_scale_lab_release_claim_status,
                    self.required_scale_lab_manifest_status,
                    self.required_scale_lab_evidence_mode,
                    self.required_scale_lab_pane_scales,
                    self.min_scale_lab_logical_cores,
                    self.min_scale_lab_memory_bytes / GIB,
                ),
                action: "Attach a fresh scale-lab staged proof artifact from live 64-core / 256 GiB hardware, or keep the high-scale claim ungraduated.".into(),
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Scale-lab artifact evidence status for high-scale release claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleLabEvidenceStatus {
    /// Whether the expected scale-lab artifact was found.
    pub artifact_present: bool,
    /// Whether the artifact parsed and validated against its schema.
    pub artifact_valid: bool,
    /// Whether the artifact is too old for release claim graduation.
    pub artifact_stale: bool,
    /// Path used for the evaluation.
    pub artifact_path: String,
    /// Artifact schema version.
    pub schema_version: String,
    /// Release claim status reported by the artifact.
    pub release_claim_status: String,
    /// Proof-manifest status reported by the artifact.
    pub manifest_status: String,
    /// Evidence mode reported by the artifact.
    pub evidence_mode: String,
    /// Pane scales represented by the artifact.
    pub pane_scales: Vec<u64>,
    /// Maximum requested logical cores represented by the artifact.
    pub max_requested_logical_cores: u64,
    /// Maximum requested memory represented by the artifact.
    pub max_requested_memory_bytes: u64,
    /// Whether the artifact came from a live mux substrate.
    pub live_mux_available: bool,
}

impl ScaleLabEvidenceStatus {
    #[must_use]
    fn satisfies_policy(&self, policy: &ReleaseGatePolicy) -> bool {
        self.artifact_present
            && self.artifact_valid
            && !self.artifact_stale
            && self.release_claim_status == policy.required_scale_lab_release_claim_status
            && self.manifest_status == policy.required_scale_lab_manifest_status
            && self.evidence_mode == policy.required_scale_lab_evidence_mode
            && self.live_mux_available
            && self.max_requested_logical_cores >= policy.min_scale_lab_logical_cores
            && self.max_requested_memory_bytes >= policy.min_scale_lab_memory_bytes
            && self.contains_required_pane_scales(&policy.required_scale_lab_pane_scales)
    }

    #[must_use]
    fn contains_required_pane_scales(&self, required_scales: &[u64]) -> bool {
        required_scales
            .iter()
            .all(|required| self.pane_scales.contains(required))
    }

    #[must_use]
    fn render_observed(&self) -> String {
        if !self.artifact_present {
            return "no scale-lab artifact".into();
        }

        format!(
            "valid={} stale={} path={} schema={} release_claim_status={} manifest_status={} evidence_mode={} live_mux={} pane_scales={:?} max_cores={} max_memory_gib={}",
            self.artifact_valid,
            self.artifact_stale,
            self.artifact_path,
            self.schema_version,
            self.release_claim_status,
            self.manifest_status,
            self.evidence_mode,
            self.live_mux_available,
            self.pane_scales,
            self.max_requested_logical_cores,
            self.max_requested_memory_bytes / GIB,
        )
    }
}

/// Inputs needed for a release readiness evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseGateInputs {
    /// Deterministic leak evidence.
    pub leak: LeakEvidenceStatus,
    /// Soak confidence and performance evidence.
    pub soak: SoakEvidenceStatus,
    /// Scale-lab evidence for high-scale swarm claims.
    pub scale_lab: ScaleLabEvidenceStatus,
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
            scale_lab: ScaleLabEvidenceStatus {
                artifact_present: true,
                artifact_valid: true,
                artifact_stale: false,
                artifact_path: "tests/e2e/artifacts/scale-lab/scale-lab-staged-proof.v1.json"
                    .into(),
                schema_version: "ft.scale_lab.staged_proof.v1".into(),
                release_claim_status: "real-hardware-proven".into(),
                manifest_status: "proven".into(),
                evidence_mode: "real_hardware_run".into(),
                pane_scales: vec![50, 200, 1_000],
                max_requested_logical_cores: 64,
                max_requested_memory_bytes: 256 * GIB,
                live_mux_available: true,
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

    #[test]
    fn render_summary_includes_pass_fail_icons_and_actions() {
        let mut inputs = passing_inputs();
        inputs.soak.release_cycles = 1; // force one gate to fail
        let report = ReleaseGatePolicy::finish_line().evaluate(&inputs);
        let summary = report.render_summary();
        assert!(
            summary.contains("[PASS]"),
            "summary must include [PASS] for passing gates"
        );
        assert!(
            summary.contains("[FAIL]"),
            "summary must include [FAIL] for failing gates"
        );
        assert!(
            summary.contains("action:"),
            "summary must include action for failing gates"
        );
        assert!(
            summary.contains("Blocked"),
            "summary must show Blocked decision"
        );
    }

    #[test]
    fn render_summary_ready_has_no_actions() {
        let report = ReleaseGatePolicy::finish_line().evaluate(&passing_inputs());
        let summary = report.render_summary();
        assert!(summary.contains("Ready"));
        assert!(
            !summary.contains("action:"),
            "ready report should have no action lines"
        );
    }

    #[test]
    fn wrong_pane_scales_blocks_soak_confidence() {
        let mut inputs = passing_inputs();
        inputs.soak.pane_scales = vec![1, 50, 100]; // missing 200
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
    fn backpressure_not_exercised_blocks_performance_budget() {
        let mut inputs = passing_inputs();
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
    fn release_gate_report_serde_roundtrip() {
        let report = ReleaseGatePolicy::finish_line().evaluate(&passing_inputs());
        let json = serde_json::to_string(&report).unwrap();
        let restored: ReleaseGateReport = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.decision, report.decision);
        assert_eq!(restored.checks.len(), report.checks.len());
        for (orig, rest) in report.checks.iter().zip(restored.checks.iter()) {
            assert_eq!(orig.gate_id, rest.gate_id);
            assert_eq!(orig.passed, rest.passed);
            assert_eq!(orig.blocking, rest.blocking);
        }
    }

    #[test]
    fn release_gate_inputs_serde_roundtrip() {
        let inputs = passing_inputs();
        let json = serde_json::to_string(&inputs).unwrap();
        let restored: ReleaseGateInputs = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.leak.summary_present, inputs.leak.summary_present);
        assert_eq!(restored.soak.release_cycles, inputs.soak.release_cycles);
        assert_eq!(
            restored.scale_lab.release_claim_status,
            inputs.scale_lab.release_claim_status
        );
        assert_eq!(restored.guard_contract_passed, inputs.guard_contract_passed);
    }

    #[test]
    fn release_gate_policy_serde_roundtrip() {
        let policy = ReleaseGatePolicy::finish_line();
        let json = serde_json::to_string(&policy).unwrap();
        let restored: ReleaseGatePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.required_pane_scales, policy.required_pane_scales);
        assert_eq!(restored.required_metric_count, policy.required_metric_count);
        assert_eq!(restored.min_release_cycles, policy.min_release_cycles);
    }

    #[test]
    fn failed_count_matches_actual_failures() {
        let mut inputs = passing_inputs();
        // Fail all five gates.
        inputs.leak.summary_present = false;
        inputs.guard_contract_passed = false;
        inputs.soak.release_cycles = 0;
        inputs.soak.metric_count = 0;
        inputs.scale_lab.artifact_present = false;
        let report = ReleaseGatePolicy::finish_line().evaluate(&inputs);
        assert_eq!(report.decision, ReleaseDecision::Blocked);
        assert_eq!(report.failed_count(), 5);
        assert_eq!(report.checks.len(), 5);
    }

    #[test]
    fn finish_line_policy_defaults() {
        let policy = ReleaseGatePolicy::finish_line();
        assert_eq!(policy.required_pane_scales, vec![1, 50, 100, 200]);
        assert_eq!(policy.required_metric_count, 8);
        assert_eq!(policy.min_release_cycles, 3);
        assert!((policy.max_peak_rss_mb - 32.0).abs() < f64::EPSILON);
        assert!((policy.max_duration_s - 3.0).abs() < f64::EPSILON);
        assert_eq!(policy.required_scale_lab_pane_scales, vec![50, 200, 1_000]);
        assert_eq!(policy.min_scale_lab_logical_cores, 64);
        assert_eq!(policy.min_scale_lab_memory_bytes, 256 * GIB);
    }

    #[test]
    fn scale_lab_local_smoke_blocks_release_claim_graduation() {
        let mut inputs = passing_inputs();
        inputs.scale_lab.release_claim_status = "local-smoke".into();
        inputs.scale_lab.manifest_status = "skipped_not_proven".into();
        inputs.scale_lab.evidence_mode = "synthetic_smoke".into();
        inputs.scale_lab.live_mux_available = false;

        let report = ReleaseGatePolicy::finish_line().evaluate(&inputs);
        assert_eq!(report.decision, ReleaseDecision::Blocked);
        let check = report
            .checks
            .iter()
            .find(|check| check.gate_id == "REL-05-scale-lab-evidence")
            .expect("scale-lab gate exists");
        assert!(!check.passed);
        assert!(
            check.observed.contains("release_claim_status=local-smoke"),
            "observed summary must expose the non-proven truth tier"
        );
    }

    #[test]
    fn scale_lab_missing_artifact_blocks_release_gate() {
        let mut inputs = passing_inputs();
        inputs.scale_lab.artifact_present = false;
        inputs.scale_lab.artifact_path.clear();

        let report = ReleaseGatePolicy::finish_line().evaluate(&inputs);
        assert_eq!(report.decision, ReleaseDecision::Blocked);
        assert!(report.checks.iter().any(|check| {
            check.gate_id == "REL-05-scale-lab-evidence"
                && !check.passed
                && check.observed == "no scale-lab artifact"
        }));
    }
}
