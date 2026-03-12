//! End-to-end runtime migration scripts and artifact contracts (ft-e34d9.10.6.4).
//!
//! Defines the artifact contracts, migration script manifest, and verification
//! gate for the asupersync migration.  Each migration step produces typed
//! artifacts that are machine-checkable for completeness.
//!
//! # Architecture
//!
//! ```text
//! MigrationManifest
//!   └── steps: Vec<MigrationStep>
//!         ├── artifact_contracts: Vec<ArtifactContract>
//!         └── verification: StepVerification
//!
//! ArtifactContract
//!   ├── artifact_type: ArtifactType
//!   ├── schema_version
//!   └── required_fields
//!
//! MigrationVerificationGate
//!   └── verify(manifest, collected) → VerificationReport
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Artifact types and contracts
// =============================================================================

/// Types of artifacts produced during migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ArtifactType {
    /// Benchmark results (before/after measurements).
    BenchmarkResult,
    /// Test suite execution report.
    TestReport,
    /// SLO evaluation snapshot.
    SloSnapshot,
    /// Dependency scan report.
    DependencyScan,
    /// Regression guard evaluation.
    RegressionGuard,
    /// Soak test execution results.
    SoakResult,
    /// Recovery scenario results.
    RecoveryResult,
    /// Diagnostic certification report.
    DiagnosticCertification,
    /// Integration suite report.
    IntegrationSuiteReport,
    /// Structured log bundle.
    LogBundle,
}

impl ArtifactType {
    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::BenchmarkResult => "benchmark-result",
            Self::TestReport => "test-report",
            Self::SloSnapshot => "slo-snapshot",
            Self::DependencyScan => "dependency-scan",
            Self::RegressionGuard => "regression-guard",
            Self::SoakResult => "soak-result",
            Self::RecoveryResult => "recovery-result",
            Self::DiagnosticCertification => "diagnostic-certification",
            Self::IntegrationSuiteReport => "integration-suite-report",
            Self::LogBundle => "log-bundle",
        }
    }

    /// All artifact types.
    #[must_use]
    pub fn all() -> &'static [ArtifactType] {
        &[
            Self::BenchmarkResult,
            Self::TestReport,
            Self::SloSnapshot,
            Self::DependencyScan,
            Self::RegressionGuard,
            Self::SoakResult,
            Self::RecoveryResult,
            Self::DiagnosticCertification,
            Self::IntegrationSuiteReport,
            Self::LogBundle,
        ]
    }
}

/// Contract for a single artifact: what it must contain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactContract {
    /// Artifact type.
    pub artifact_type: ArtifactType,
    /// Schema version for this contract.
    pub schema_version: String,
    /// Required fields (field name → description).
    pub required_fields: BTreeMap<String, String>,
    /// Whether this artifact is mandatory for gate pass.
    pub mandatory: bool,
    /// Description of what this artifact proves.
    pub proof_statement: String,
}

/// Standard artifact contracts for the migration.
#[must_use]
pub fn standard_artifact_contracts() -> Vec<ArtifactContract> {
    vec![
        ArtifactContract {
            artifact_type: ArtifactType::BenchmarkResult,
            schema_version: "1.0.0".into(),
            required_fields: {
                let mut f = BTreeMap::new();
                f.insert("suite_id".into(), "Unique benchmark suite identifier".into());
                f.insert("baseline".into(), "Pre-migration baseline measurements".into());
                f.insert("current".into(), "Post-migration current measurements".into());
                f.insert("verdict".into(), "Pass/ConditionalPass/Fail regression verdict".into());
                f
            },
            mandatory: true,
            proof_statement: "Performance has not regressed beyond tolerance thresholds".into(),
        },
        ArtifactContract {
            artifact_type: ArtifactType::TestReport,
            schema_version: "1.0.0".into(),
            required_fields: {
                let mut f = BTreeMap::new();
                f.insert("total_tests".into(), "Number of tests executed".into());
                f.insert("passed".into(), "Number of passing tests".into());
                f.insert("failed".into(), "Number of failing tests".into());
                f.insert("pass_rate".into(), "Pass rate as percentage".into());
                f
            },
            mandatory: true,
            proof_statement: "Test suite passes above minimum threshold (99%)".into(),
        },
        ArtifactContract {
            artifact_type: ArtifactType::SloSnapshot,
            schema_version: "1.0.0".into(),
            required_fields: {
                let mut f = BTreeMap::new();
                f.insert("slos_evaluated".into(), "Number of SLOs evaluated".into());
                f.insert("slos_satisfied".into(), "Number satisfied".into());
                f.insert("gate_verdict".into(), "Pass/ConditionalPass/Fail".into());
                f
            },
            mandatory: true,
            proof_statement: "All critical runtime SLOs are satisfied".into(),
        },
        ArtifactContract {
            artifact_type: ArtifactType::DependencyScan,
            schema_version: "1.0.0".into(),
            required_fields: {
                let mut f = BTreeMap::new();
                f.insert("scan_verdict".into(), "Clean/CleanWithNotes/Violations".into());
                f.insert("violations_count".into(), "Number of forbidden dependency matches".into());
                f
            },
            mandatory: true,
            proof_statement: "No forbidden runtime dependencies remain in source tree".into(),
        },
        ArtifactContract {
            artifact_type: ArtifactType::RegressionGuard,
            schema_version: "1.0.0".into(),
            required_fields: {
                let mut f = BTreeMap::new();
                f.insert("guards_total".into(), "Total regression guards".into());
                f.insert("guards_passing".into(), "Number passing".into());
                f.insert("all_pass".into(), "Boolean: all guards pass".into());
                f
            },
            mandatory: true,
            proof_statement: "All regression guards pass, preventing dependency re-introduction".into(),
        },
        ArtifactContract {
            artifact_type: ArtifactType::SoakResult,
            schema_version: "1.0.0".into(),
            required_fields: {
                let mut f = BTreeMap::new();
                f.insert("matrix_cells".into(), "Number of soak matrix cells executed".into());
                f.insert("confidence_verdict".into(), "Confident/ConditionallyConfident/NotConfident".into());
                f
            },
            mandatory: false,
            proof_statement: "System survives sustained workload without invariant violations".into(),
        },
        ArtifactContract {
            artifact_type: ArtifactType::RecoveryResult,
            schema_version: "1.0.0".into(),
            required_fields: {
                let mut f = BTreeMap::new();
                f.insert("scenarios_run".into(), "Number of crash/recovery scenarios".into());
                f.insert("data_loss_detected".into(), "Boolean: any data loss".into());
                f.insert("gate_verdict".into(), "Pass/ConditionalPass/Fail".into());
                f
            },
            mandatory: true,
            proof_statement: "No data loss under crash/restart scenarios".into(),
        },
        ArtifactContract {
            artifact_type: ArtifactType::DiagnosticCertification,
            schema_version: "1.0.0".into(),
            required_fields: {
                let mut f = BTreeMap::new();
                f.insert("templates_certified".into(), "Number of certified diagnostic templates".into());
                f.insert("overall_pass".into(), "Boolean: all failure classes covered".into());
                f
            },
            mandatory: true,
            proof_statement: "All runtime failure classes have actionable diagnostic templates".into(),
        },
        ArtifactContract {
            artifact_type: ArtifactType::IntegrationSuiteReport,
            schema_version: "1.0.0".into(),
            required_fields: {
                let mut f = BTreeMap::new();
                f.insert("scenarios_run".into(), "Number of integration scenarios".into());
                f.insert("boundaries_covered".into(), "Crate boundaries exercised".into());
                f.insert("overall_pass".into(), "Boolean: all scenarios pass".into());
                f
            },
            mandatory: true,
            proof_statement: "Cross-crate behavior is correct across all integration scenarios".into(),
        },
        ArtifactContract {
            artifact_type: ArtifactType::LogBundle,
            schema_version: "1.0.0".into(),
            required_fields: {
                let mut f = BTreeMap::new();
                f.insert("correlation_ids".into(), "Trace correlation IDs for all scenarios".into());
                f.insert("log_format".into(), "Structured log format (JSONL)".into());
                f
            },
            mandatory: false,
            proof_statement: "Complete structured logs available for audit and replay".into(),
        },
    ]
}

// =============================================================================
// Migration steps and manifest
// =============================================================================

/// A single migration step with required artifacts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationStep {
    /// Step identifier.
    pub step_id: String,
    /// Step number (execution order).
    pub order: u32,
    /// Human-readable title.
    pub title: String,
    /// Description.
    pub description: String,
    /// Artifact types produced by this step.
    pub produces: Vec<ArtifactType>,
    /// Whether this step must pass for the next step.
    pub gate_step: bool,
}

/// Complete migration manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationManifest {
    /// Manifest identifier.
    pub manifest_id: String,
    /// Manifest version.
    pub version: String,
    /// Ordered migration steps.
    pub steps: Vec<MigrationStep>,
    /// Artifact contracts.
    pub contracts: Vec<ArtifactContract>,
}

impl MigrationManifest {
    /// Standard migration manifest for asupersync.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            manifest_id: "asupersync-migration-v1".into(),
            version: "1.0.0".into(),
            steps: vec![
                MigrationStep {
                    step_id: "STEP-01-scan".into(),
                    order: 1,
                    title: "Dependency scan".into(),
                    description: "Scan source tree for forbidden runtime dependencies".into(),
                    produces: vec![ArtifactType::DependencyScan, ArtifactType::RegressionGuard],
                    gate_step: true,
                },
                MigrationStep {
                    step_id: "STEP-02-test".into(),
                    order: 2,
                    title: "Test suite execution".into(),
                    description: "Run full test suite including unit, integration, and property tests".into(),
                    produces: vec![ArtifactType::TestReport],
                    gate_step: true,
                },
                MigrationStep {
                    step_id: "STEP-03-bench".into(),
                    order: 3,
                    title: "Performance benchmarks".into(),
                    description: "Run baseline and current benchmarks for all user-facing operations".into(),
                    produces: vec![ArtifactType::BenchmarkResult],
                    gate_step: true,
                },
                MigrationStep {
                    step_id: "STEP-04-slo".into(),
                    order: 4,
                    title: "SLO evaluation".into(),
                    description: "Evaluate runtime SLOs against collected metrics".into(),
                    produces: vec![ArtifactType::SloSnapshot],
                    gate_step: true,
                },
                MigrationStep {
                    step_id: "STEP-05-integration".into(),
                    order: 5,
                    title: "Cross-crate integration".into(),
                    description: "Run integration suite across crate boundaries".into(),
                    produces: vec![ArtifactType::IntegrationSuiteReport],
                    gate_step: true,
                },
                MigrationStep {
                    step_id: "STEP-06-recovery".into(),
                    order: 6,
                    title: "Crash/restart recovery".into(),
                    description: "Execute crash/restart scenarios and verify persistence invariants".into(),
                    produces: vec![ArtifactType::RecoveryResult],
                    gate_step: true,
                },
                MigrationStep {
                    step_id: "STEP-07-diagnostics".into(),
                    order: 7,
                    title: "Diagnostic certification".into(),
                    description: "Certify diagnostic templates for all failure classes".into(),
                    produces: vec![ArtifactType::DiagnosticCertification],
                    gate_step: false,
                },
                MigrationStep {
                    step_id: "STEP-08-soak".into(),
                    order: 8,
                    title: "Soak testing".into(),
                    description: "Run sustained workload soak matrix".into(),
                    produces: vec![ArtifactType::SoakResult, ArtifactType::LogBundle],
                    gate_step: false,
                },
            ],
            contracts: standard_artifact_contracts(),
        }
    }

    /// All mandatory artifact types.
    #[must_use]
    pub fn mandatory_artifacts(&self) -> Vec<ArtifactType> {
        self.contracts
            .iter()
            .filter(|c| c.mandatory)
            .map(|c| c.artifact_type)
            .collect()
    }

    /// Gate steps (must pass in order).
    #[must_use]
    pub fn gate_steps(&self) -> Vec<&MigrationStep> {
        self.steps.iter().filter(|s| s.gate_step).collect()
    }
}

// =============================================================================
// Verification gate
// =============================================================================

/// A collected artifact (evidence that a step produced its output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectedArtifact {
    /// Artifact type.
    pub artifact_type: ArtifactType,
    /// Path or identifier for the artifact.
    pub location: String,
    /// Whether the artifact has all required fields.
    pub schema_valid: bool,
    /// Whether the artifact's content indicates a passing result.
    pub content_pass: bool,
}

/// Result of verifying a single step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepVerification {
    /// Step identifier.
    pub step_id: String,
    /// Whether all required artifacts were collected.
    pub artifacts_complete: bool,
    /// Whether all artifacts are schema-valid.
    pub schemas_valid: bool,
    /// Whether all artifacts indicate passing results.
    pub content_passing: bool,
    /// Overall step pass.
    pub passed: bool,
    /// Missing artifact types.
    pub missing_artifacts: Vec<ArtifactType>,
}

/// Overall verification verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationVerdict {
    /// All steps pass, all mandatory artifacts collected and valid.
    Complete,
    /// Some optional artifacts missing but all mandatory present.
    PartiallyComplete,
    /// Mandatory artifacts missing or failing.
    Incomplete,
}

/// Complete verification report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    /// Report identifier.
    pub report_id: String,
    /// Manifest used.
    pub manifest_id: String,
    /// Per-step verifications.
    pub step_results: Vec<StepVerification>,
    /// Overall verdict.
    pub verdict: VerificationVerdict,
    /// Mandatory artifacts missing.
    pub mandatory_missing: Vec<ArtifactType>,
    /// Total artifacts collected.
    pub artifacts_collected: usize,
    /// Total artifacts expected.
    pub artifacts_expected: usize,
}

impl VerificationReport {
    /// Verify collected artifacts against the manifest.
    #[must_use]
    pub fn verify(manifest: &MigrationManifest, artifacts: &[CollectedArtifact]) -> Self {
        let artifact_map: BTreeMap<String, &CollectedArtifact> = artifacts
            .iter()
            .map(|a| (a.artifact_type.label().to_string(), a))
            .collect();

        let mut step_results = Vec::new();
        let mut all_expected: Vec<ArtifactType> = Vec::new();

        for step in &manifest.steps {
            let mut missing = Vec::new();
            let mut schemas_valid = true;
            let mut content_passing = true;
            let mut artifacts_complete = true;

            for art_type in &step.produces {
                all_expected.push(*art_type);
                let key = art_type.label().to_string();
                match artifact_map.get(&key) {
                    Some(a) => {
                        if !a.schema_valid {
                            schemas_valid = false;
                        }
                        if !a.content_pass {
                            content_passing = false;
                        }
                    }
                    None => {
                        missing.push(*art_type);
                        artifacts_complete = false;
                    }
                }
            }

            let passed = artifacts_complete && schemas_valid && content_passing;
            step_results.push(StepVerification {
                step_id: step.step_id.clone(),
                artifacts_complete,
                schemas_valid,
                content_passing,
                passed,
                missing_artifacts: missing,
            });
        }

        let mandatory = manifest.mandatory_artifacts();
        let mandatory_missing: Vec<ArtifactType> = mandatory
            .iter()
            .filter(|m| !artifact_map.contains_key(m.label()))
            .copied()
            .collect();

        let verdict = if mandatory_missing.is_empty()
            && step_results.iter().filter(|s| {
                manifest.steps.iter().find(|ms| ms.step_id == s.step_id).map(|ms| ms.gate_step).unwrap_or(false)
            }).all(|s| s.passed)
        {
            if step_results.iter().all(|s| s.passed) {
                VerificationVerdict::Complete
            } else {
                VerificationVerdict::PartiallyComplete
            }
        } else {
            VerificationVerdict::Incomplete
        };

        Self {
            report_id: format!("{}-verification", manifest.manifest_id),
            manifest_id: manifest.manifest_id.clone(),
            step_results,
            verdict,
            mandatory_missing,
            artifacts_collected: artifacts.len(),
            artifacts_expected: all_expected.len(),
        }
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("=== Migration Verification: {} ===", self.report_id));
        lines.push(format!("Verdict: {:?}", self.verdict));
        lines.push(format!(
            "Artifacts: {}/{} collected",
            self.artifacts_collected, self.artifacts_expected
        ));

        if !self.mandatory_missing.is_empty() {
            lines.push(format!(
                "Mandatory missing: {}",
                self.mandatory_missing.iter().map(|a| a.label()).collect::<Vec<_>>().join(", ")
            ));
        }

        lines.push(String::new());
        for step in &self.step_results {
            let status = if step.passed { "PASS" } else { "FAIL" };
            let missing = if step.missing_artifacts.is_empty() {
                String::new()
            } else {
                format!(
                    " (missing: {})",
                    step.missing_artifacts.iter().map(|a| a.label()).collect::<Vec<_>>().join(", ")
                )
            };
            lines.push(format!("  [{}] {}{}", status, step.step_id, missing));
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

    fn full_artifacts() -> Vec<CollectedArtifact> {
        ArtifactType::all()
            .iter()
            .map(|t| CollectedArtifact {
                artifact_type: *t,
                location: format!("artifacts/{}.json", t.label()),
                schema_valid: true,
                content_pass: true,
            })
            .collect()
    }

    #[test]
    fn standard_manifest_has_steps() {
        let manifest = MigrationManifest::standard();
        assert!(!manifest.steps.is_empty());
        assert!(manifest.steps.len() >= 6);
    }

    #[test]
    fn standard_manifest_has_contracts_for_all_types() {
        let manifest = MigrationManifest::standard();
        let types: Vec<ArtifactType> = manifest.contracts.iter().map(|c| c.artifact_type).collect();
        for t in ArtifactType::all() {
            assert!(types.contains(t), "missing contract for {:?}", t);
        }
    }

    #[test]
    fn standard_manifest_mandatory_artifacts() {
        let manifest = MigrationManifest::standard();
        let mandatory = manifest.mandatory_artifacts();
        assert!(mandatory.len() >= 6);
    }

    #[test]
    fn standard_manifest_gate_steps() {
        let manifest = MigrationManifest::standard();
        let gates = manifest.gate_steps();
        assert!(gates.len() >= 4);
    }

    #[test]
    fn verification_complete_with_all_artifacts() {
        let manifest = MigrationManifest::standard();
        let artifacts = full_artifacts();
        let report = VerificationReport::verify(&manifest, &artifacts);
        assert_eq!(report.verdict, VerificationVerdict::Complete);
        assert!(report.mandatory_missing.is_empty());
    }

    #[test]
    fn verification_incomplete_when_mandatory_missing() {
        let manifest = MigrationManifest::standard();
        // Only provide non-mandatory artifacts.
        let artifacts = vec![
            CollectedArtifact {
                artifact_type: ArtifactType::SoakResult,
                location: "artifacts/soak.json".into(),
                schema_valid: true,
                content_pass: true,
            },
        ];
        let report = VerificationReport::verify(&manifest, &artifacts);
        assert_eq!(report.verdict, VerificationVerdict::Incomplete);
        assert!(!report.mandatory_missing.is_empty());
    }

    #[test]
    fn verification_partially_complete() {
        let manifest = MigrationManifest::standard();
        // Provide all mandatory but not optional.
        let mandatory = manifest.mandatory_artifacts();
        let artifacts: Vec<CollectedArtifact> = mandatory
            .iter()
            .map(|t| CollectedArtifact {
                artifact_type: *t,
                location: format!("artifacts/{}.json", t.label()),
                schema_valid: true,
                content_pass: true,
            })
            .collect();
        let report = VerificationReport::verify(&manifest, &artifacts);
        // Some optional steps (soak, log-bundle) will be missing.
        assert!(
            report.verdict == VerificationVerdict::PartiallyComplete
                || report.verdict == VerificationVerdict::Complete
        );
    }

    #[test]
    fn verification_fails_on_schema_invalid() {
        let manifest = MigrationManifest::standard();
        let mut artifacts = full_artifacts();
        // Mark first artifact as schema-invalid.
        artifacts[0].schema_valid = false;
        let report = VerificationReport::verify(&manifest, &artifacts);
        // At least one step should fail.
        assert!(report.step_results.iter().any(|s| !s.passed));
    }

    #[test]
    fn verification_fails_on_content_fail() {
        let manifest = MigrationManifest::standard();
        let mut artifacts = full_artifacts();
        artifacts[0].content_pass = false;
        let report = VerificationReport::verify(&manifest, &artifacts);
        assert!(report.step_results.iter().any(|s| !s.passed));
    }

    #[test]
    fn artifact_type_labels_unique() {
        let all = ArtifactType::all();
        let labels: Vec<&str> = all.iter().map(|t| t.label()).collect();
        for (i, a) in labels.iter().enumerate() {
            for (j, b) in labels.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn steps_ordered() {
        let manifest = MigrationManifest::standard();
        for (i, step) in manifest.steps.iter().enumerate() {
            assert_eq!(step.order as usize, i + 1, "step {} out of order", step.step_id);
        }
    }

    #[test]
    fn all_steps_produce_artifacts() {
        let manifest = MigrationManifest::standard();
        for step in &manifest.steps {
            assert!(!step.produces.is_empty(), "{} produces no artifacts", step.step_id);
        }
    }

    #[test]
    fn render_summary_shows_verdict() {
        let manifest = MigrationManifest::standard();
        let artifacts = full_artifacts();
        let report = VerificationReport::verify(&manifest, &artifacts);
        let summary = report.render_summary();
        assert!(summary.contains("Complete"));
        assert!(summary.contains("PASS"));
    }

    #[test]
    fn render_summary_shows_missing() {
        let manifest = MigrationManifest::standard();
        let report = VerificationReport::verify(&manifest, &[]);
        let summary = report.render_summary();
        assert!(summary.contains("Incomplete"));
        assert!(summary.contains("missing"));
    }

    #[test]
    fn serde_roundtrip_manifest() {
        let manifest = MigrationManifest::standard();
        let json = serde_json::to_string(&manifest).expect("serialize");
        let restored: MigrationManifest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.steps.len(), manifest.steps.len());
    }

    #[test]
    fn serde_roundtrip_report() {
        let manifest = MigrationManifest::standard();
        let artifacts = full_artifacts();
        let report = VerificationReport::verify(&manifest, &artifacts);
        let json = serde_json::to_string(&report).expect("serialize");
        let restored: VerificationReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.verdict, report.verdict);
    }

    #[test]
    fn contracts_have_required_fields() {
        let contracts = standard_artifact_contracts();
        for c in &contracts {
            assert!(!c.required_fields.is_empty(), "{:?} has no required fields", c.artifact_type);
            assert!(!c.proof_statement.is_empty());
        }
    }
}
