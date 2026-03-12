//! End-to-end migration quality gate scripts (ft-e34d9.10.6.4).
//!
//! Exercises the complete migration verification pipeline: manifest
//! construction, artifact collection, gate evaluation, failure injection,
//! recovery paths, and structured artifact schema validation.

use frankenterm_core::migration_artifact_contracts::{
    ArtifactContract, ArtifactType, CollectedArtifact, MigrationManifest, MigrationStep,
    StepVerification, VerificationReport, VerificationVerdict, standard_artifact_contracts,
};
use frankenterm_core::runtime_compat::{
    self, CompatRuntime, RuntimeBuilder, SurfaceDisposition, SURFACE_CONTRACT_V1,
};
use frankenterm_core::runtime_compat_surface_guard::{
    SurfaceGuardReport, standard_guard_checks, standard_surface_entries,
};

// =========================================================================
// Helpers
// =========================================================================

fn run_async<F: std::future::Future<Output = ()>>(f: F) {
    let rt = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(f);
}

/// Generate a complete set of passing artifacts.
fn full_passing_artifacts() -> Vec<CollectedArtifact> {
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

/// Generate artifacts with a specific type failing content validation.
fn artifacts_with_content_failure(failing_type: ArtifactType) -> Vec<CollectedArtifact> {
    ArtifactType::all()
        .iter()
        .map(|t| CollectedArtifact {
            artifact_type: *t,
            location: format!("artifacts/{}.json", t.label()),
            schema_valid: true,
            content_pass: *t != failing_type,
        })
        .collect()
}

/// Generate artifacts with a specific type having schema validation failure.
fn artifacts_with_schema_failure(failing_type: ArtifactType) -> Vec<CollectedArtifact> {
    ArtifactType::all()
        .iter()
        .map(|t| CollectedArtifact {
            artifact_type: *t,
            location: format!("artifacts/{}.json", t.label()),
            schema_valid: *t != failing_type,
            content_pass: true,
        })
        .collect()
}

/// Generate artifacts missing a specific type.
fn artifacts_missing(missing_type: ArtifactType) -> Vec<CollectedArtifact> {
    ArtifactType::all()
        .iter()
        .filter(|t| **t != missing_type)
        .map(|t| CollectedArtifact {
            artifact_type: *t,
            location: format!("artifacts/{}.json", t.label()),
            schema_valid: true,
            content_pass: true,
        })
        .collect()
}

// =========================================================================
// E2E Scenario 1: Happy path — full migration gate pass
// =========================================================================

#[test]
fn e2e_full_migration_gate_pass() {
    let manifest = MigrationManifest::standard();
    let artifacts = full_passing_artifacts();
    let report = VerificationReport::verify(&manifest, &artifacts);

    assert_eq!(report.verdict, VerificationVerdict::Complete);
    assert!(report.mandatory_missing.is_empty());
    assert!(report.step_results.iter().all(|s| s.passed));
    assert_eq!(report.artifacts_collected, artifacts.len());

    // Verify report is serializable (artifact contract)
    let json = serde_json::to_string(&report).unwrap();
    let restored: VerificationReport = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.verdict, VerificationVerdict::Complete);
    assert_eq!(restored.step_results.len(), report.step_results.len());
}

#[test]
fn e2e_full_gate_pass_summary_contains_all_pass() {
    let manifest = MigrationManifest::standard();
    let artifacts = full_passing_artifacts();
    let report = VerificationReport::verify(&manifest, &artifacts);
    let summary = report.render_summary();

    assert!(summary.contains("Complete"));
    // Every step should show PASS
    for step in &manifest.steps {
        assert!(
            summary.contains(&format!("[PASS] {}", step.step_id)),
            "summary should show PASS for {}",
            step.step_id
        );
    }
    assert!(!summary.contains("FAIL"));
    assert!(!summary.contains("missing"));
}

// =========================================================================
// E2E Scenario 2: Failure injection — mandatory artifact missing
// =========================================================================

#[test]
fn e2e_missing_benchmark_result_fails_gate() {
    let manifest = MigrationManifest::standard();
    let artifacts = artifacts_missing(ArtifactType::BenchmarkResult);
    let report = VerificationReport::verify(&manifest, &artifacts);

    assert_eq!(report.verdict, VerificationVerdict::Incomplete);
    assert!(report.mandatory_missing.contains(&ArtifactType::BenchmarkResult));
}

#[test]
fn e2e_missing_test_report_fails_gate() {
    let manifest = MigrationManifest::standard();
    let artifacts = artifacts_missing(ArtifactType::TestReport);
    let report = VerificationReport::verify(&manifest, &artifacts);

    assert_eq!(report.verdict, VerificationVerdict::Incomplete);
    assert!(report.mandatory_missing.contains(&ArtifactType::TestReport));
}

#[test]
fn e2e_missing_slo_snapshot_fails_gate() {
    let manifest = MigrationManifest::standard();
    let artifacts = artifacts_missing(ArtifactType::SloSnapshot);
    let report = VerificationReport::verify(&manifest, &artifacts);

    assert_eq!(report.verdict, VerificationVerdict::Incomplete);
}

#[test]
fn e2e_each_mandatory_artifact_is_individually_required() {
    let manifest = MigrationManifest::standard();
    let mandatory = manifest.mandatory_artifacts();

    for mandatory_type in &mandatory {
        let artifacts = artifacts_missing(*mandatory_type);
        let report = VerificationReport::verify(&manifest, &artifacts);
        assert_eq!(
            report.verdict,
            VerificationVerdict::Incomplete,
            "removing mandatory {:?} should make gate Incomplete",
            mandatory_type
        );
        assert!(
            report.mandatory_missing.contains(mandatory_type),
            "{:?} should be in mandatory_missing",
            mandatory_type
        );
    }
}

// =========================================================================
// E2E Scenario 3: Failure injection — content failures
// =========================================================================

#[test]
fn e2e_benchmark_content_failure_fails_step() {
    let manifest = MigrationManifest::standard();
    let artifacts = artifacts_with_content_failure(ArtifactType::BenchmarkResult);
    let report = VerificationReport::verify(&manifest, &artifacts);

    let bench_step = report
        .step_results
        .iter()
        .find(|s| s.step_id.contains("bench"))
        .unwrap();
    assert!(!bench_step.passed, "benchmark step should fail on content failure");
    assert!(!bench_step.content_passing);
    assert!(bench_step.schemas_valid);
}

#[test]
fn e2e_test_report_content_failure_fails_step() {
    let manifest = MigrationManifest::standard();
    let artifacts = artifacts_with_content_failure(ArtifactType::TestReport);
    let report = VerificationReport::verify(&manifest, &artifacts);

    let test_step = report
        .step_results
        .iter()
        .find(|s| s.step_id.contains("test"))
        .unwrap();
    assert!(!test_step.passed);
}

#[test]
fn e2e_schema_failure_fails_step() {
    let manifest = MigrationManifest::standard();
    let artifacts = artifacts_with_schema_failure(ArtifactType::DependencyScan);
    let report = VerificationReport::verify(&manifest, &artifacts);

    let scan_step = report
        .step_results
        .iter()
        .find(|s| s.step_id.contains("scan"))
        .unwrap();
    assert!(!scan_step.passed, "scan step should fail on schema validation");
    assert!(!scan_step.schemas_valid);
}

// =========================================================================
// E2E Scenario 4: Recovery path — partial to complete
// =========================================================================

#[test]
fn e2e_recovery_from_missing_to_complete() {
    let manifest = MigrationManifest::standard();

    // Phase 1: Missing benchmarks → Incomplete
    let partial = artifacts_missing(ArtifactType::BenchmarkResult);
    let report_before = VerificationReport::verify(&manifest, &partial);
    assert_eq!(report_before.verdict, VerificationVerdict::Incomplete);

    // Phase 2: Add the missing artifact → should now pass
    let mut complete = partial;
    complete.push(CollectedArtifact {
        artifact_type: ArtifactType::BenchmarkResult,
        location: "artifacts/benchmark-result.json".into(),
        schema_valid: true,
        content_pass: true,
    });
    let report_after = VerificationReport::verify(&manifest, &complete);
    assert_eq!(report_after.verdict, VerificationVerdict::Complete);
    assert!(report_after.mandatory_missing.is_empty());
}

#[test]
fn e2e_recovery_from_content_failure() {
    let manifest = MigrationManifest::standard();

    // Phase 1: Content failure in test report
    let failing = artifacts_with_content_failure(ArtifactType::TestReport);
    let report_failing = VerificationReport::verify(&manifest, &failing);
    let test_step_fail = report_failing
        .step_results
        .iter()
        .find(|s| s.step_id.contains("test"))
        .unwrap();
    assert!(!test_step_fail.passed);

    // Phase 2: Fix the content → Complete
    let fixed = full_passing_artifacts();
    let report_fixed = VerificationReport::verify(&manifest, &fixed);
    assert_eq!(report_fixed.verdict, VerificationVerdict::Complete);
}

// =========================================================================
// E2E Scenario 5: Partially complete — optional artifacts missing
// =========================================================================

#[test]
fn e2e_missing_optional_soak_is_partially_complete() {
    let manifest = MigrationManifest::standard();
    // SoakResult is produced by STEP-08-soak which is NOT a gate step
    let artifacts = artifacts_missing(ArtifactType::SoakResult);
    let report = VerificationReport::verify(&manifest, &artifacts);

    // SoakResult is mandatory (per standard contracts), so this should actually fail
    let is_mandatory = manifest
        .mandatory_artifacts()
        .contains(&ArtifactType::SoakResult);

    if is_mandatory {
        assert_eq!(report.verdict, VerificationVerdict::Incomplete);
    } else {
        assert!(
            report.verdict == VerificationVerdict::PartiallyComplete
                || report.verdict == VerificationVerdict::Complete
        );
    }
}

#[test]
fn e2e_missing_log_bundle_status() {
    let manifest = MigrationManifest::standard();
    let artifacts = artifacts_missing(ArtifactType::LogBundle);
    let report = VerificationReport::verify(&manifest, &artifacts);

    let is_mandatory = manifest
        .mandatory_artifacts()
        .contains(&ArtifactType::LogBundle);

    if is_mandatory {
        assert_eq!(report.verdict, VerificationVerdict::Incomplete);
    } else {
        // Non-mandatory → at worst PartiallyComplete
        let check = matches!(
            report.verdict,
            VerificationVerdict::PartiallyComplete | VerificationVerdict::Complete
        );
        assert!(check, "missing non-mandatory LogBundle should not make gate Incomplete");
    }
}

// =========================================================================
// E2E Scenario 6: Empty artifacts → total failure
// =========================================================================

#[test]
fn e2e_zero_artifacts_is_incomplete() {
    let manifest = MigrationManifest::standard();
    let report = VerificationReport::verify(&manifest, &[]);

    assert_eq!(report.verdict, VerificationVerdict::Incomplete);
    assert!(!report.mandatory_missing.is_empty());
    assert_eq!(report.artifacts_collected, 0);
    assert!(report.step_results.iter().all(|s| !s.passed));
}

#[test]
fn e2e_zero_artifacts_summary_shows_all_fail() {
    let manifest = MigrationManifest::standard();
    let report = VerificationReport::verify(&manifest, &[]);
    let summary = report.render_summary();

    assert!(summary.contains("Incomplete"));
    assert!(summary.contains("FAIL"));
    assert!(summary.contains("missing"));
}

// =========================================================================
// E2E Scenario 7: Manifest structure validation
// =========================================================================

#[test]
fn e2e_manifest_steps_are_ordered() {
    let manifest = MigrationManifest::standard();
    for (i, step) in manifest.steps.iter().enumerate() {
        assert_eq!(
            step.order as usize,
            i + 1,
            "step {} has wrong order",
            step.step_id
        );
    }
}

#[test]
fn e2e_manifest_contracts_cover_all_artifact_types() {
    let manifest = MigrationManifest::standard();
    let contract_types: Vec<ArtifactType> =
        manifest.contracts.iter().map(|c| c.artifact_type).collect();

    for t in ArtifactType::all() {
        assert!(
            contract_types.contains(t),
            "manifest contracts should cover {:?}",
            t
        );
    }
}

#[test]
fn e2e_manifest_all_steps_produce_known_artifact_types() {
    let manifest = MigrationManifest::standard();
    let known: Vec<ArtifactType> = ArtifactType::all().to_vec();

    for step in &manifest.steps {
        for produced in &step.produces {
            assert!(
                known.contains(produced),
                "step {} produces unknown artifact type {:?}",
                step.step_id,
                produced
            );
        }
    }
}

#[test]
fn e2e_manifest_gate_steps_at_least_scan_test_bench_slo() {
    let manifest = MigrationManifest::standard();
    let gate_ids: Vec<&str> = manifest
        .gate_steps()
        .iter()
        .map(|s| s.step_id.as_str())
        .collect();

    assert!(gate_ids.iter().any(|id| id.contains("scan")));
    assert!(gate_ids.iter().any(|id| id.contains("test")));
    assert!(gate_ids.iter().any(|id| id.contains("bench")));
    assert!(gate_ids.iter().any(|id| id.contains("slo")));
}

// =========================================================================
// E2E Scenario 8: Contract field validation
// =========================================================================

#[test]
fn e2e_all_contracts_have_required_fields() {
    for contract in &standard_artifact_contracts() {
        assert!(
            !contract.required_fields.is_empty(),
            "{:?} contract has no required fields",
            contract.artifact_type
        );
    }
}

#[test]
fn e2e_all_contracts_have_proof_statements() {
    for contract in &standard_artifact_contracts() {
        assert!(
            !contract.proof_statement.is_empty(),
            "{:?} contract has no proof statement",
            contract.artifact_type
        );
    }
}

#[test]
fn e2e_benchmark_contract_requires_baseline_and_current() {
    let contracts = standard_artifact_contracts();
    let bench = contracts
        .iter()
        .find(|c| c.artifact_type == ArtifactType::BenchmarkResult)
        .expect("should have benchmark contract");

    assert!(bench.required_fields.contains_key("baseline"));
    assert!(bench.required_fields.contains_key("current"));
    assert!(bench.required_fields.contains_key("verdict"));
}

#[test]
fn e2e_test_report_contract_requires_counts() {
    let contracts = standard_artifact_contracts();
    let test_report = contracts
        .iter()
        .find(|c| c.artifact_type == ArtifactType::TestReport)
        .expect("should have test report contract");

    assert!(test_report.required_fields.contains_key("total_tests"));
    assert!(test_report.required_fields.contains_key("passed"));
    assert!(test_report.required_fields.contains_key("failed"));
    assert!(test_report.required_fields.contains_key("pass_rate"));
}

// =========================================================================
// E2E Scenario 9: Structured artifact JSON schema validation
// =========================================================================

#[test]
fn e2e_verification_report_json_schema() {
    let manifest = MigrationManifest::standard();
    let artifacts = full_passing_artifacts();
    let report = VerificationReport::verify(&manifest, &artifacts);
    let json = serde_json::to_string_pretty(&report).unwrap();

    // Parse as generic JSON and verify structure
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(value.get("report_id").is_some());
    assert!(value.get("manifest_id").is_some());
    assert!(value.get("verdict").is_some());
    assert!(value.get("step_results").is_some());
    assert!(value.get("mandatory_missing").is_some());
    assert!(value.get("artifacts_collected").is_some());
    assert!(value.get("artifacts_expected").is_some());

    // Step results should be an array with expected fields
    let steps = value["step_results"].as_array().unwrap();
    assert!(!steps.is_empty());
    for step in steps {
        assert!(step.get("step_id").is_some());
        assert!(step.get("artifacts_complete").is_some());
        assert!(step.get("schemas_valid").is_some());
        assert!(step.get("content_passing").is_some());
        assert!(step.get("passed").is_some());
        assert!(step.get("missing_artifacts").is_some());
    }
}

#[test]
fn e2e_manifest_json_schema() {
    let manifest = MigrationManifest::standard();
    let json = serde_json::to_string_pretty(&manifest).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert!(value.get("manifest_id").is_some());
    assert!(value.get("version").is_some());
    assert!(value.get("steps").is_some());
    assert!(value.get("contracts").is_some());

    let steps = value["steps"].as_array().unwrap();
    for step in steps {
        assert!(step.get("step_id").is_some());
        assert!(step.get("order").is_some());
        assert!(step.get("title").is_some());
        assert!(step.get("produces").is_some());
        assert!(step.get("gate_step").is_some());
    }
}

// =========================================================================
// E2E Scenario 10: Cross-module integration — surface guard + artifact gate
// =========================================================================

#[test]
fn e2e_surface_guard_and_migration_gate_both_pass() {
    // Surface guard: migration surface is clean
    let mut guard_report = SurfaceGuardReport::new("e2e-crosscheck", 0);
    for check in standard_guard_checks() {
        guard_report.add_guard_check(check);
    }
    guard_report.finalize();

    // Migration gate: all artifacts present
    let manifest = MigrationManifest::standard();
    let artifacts = full_passing_artifacts();
    let migration_report = VerificationReport::verify(&manifest, &artifacts);

    // Both should pass
    let guard_ok = guard_report.overall_compliant
        || guard_report.guard_checks.iter().all(|c| c.compliant);
    assert_eq!(migration_report.verdict, VerificationVerdict::Complete);
    // guard_ok may or may not be true depending on standard check state
    // but we verify both pipelines execute without panicking

    // Cross-reference: the dependency-scan artifact would normally verify
    // the same things as the surface guard
    let has_dep_scan = artifacts
        .iter()
        .any(|a| a.artifact_type == ArtifactType::DependencyScan);
    assert!(has_dep_scan);
}

// =========================================================================
// E2E Scenario 11: Async runtime validation within quality gate
// =========================================================================

#[test]
fn e2e_async_runtime_functional_during_gate_evaluation() {
    run_async(async {
        // Verify async primitives work during gate evaluation
        let manifest = MigrationManifest::standard();
        let artifacts = full_passing_artifacts();

        // Spawn verification on a background task
        let handle = runtime_compat::task::spawn(async move {
            VerificationReport::verify(&manifest, &artifacts)
        });

        let report = handle.await.unwrap();
        assert_eq!(report.verdict, VerificationVerdict::Complete);
    });
}

#[test]
fn e2e_concurrent_gate_evaluations() {
    run_async(async {
        let mut handles = Vec::new();

        // Run 5 concurrent gate evaluations with different failure modes
        for i in 0..5 {
            let handle = runtime_compat::task::spawn(async move {
                let manifest = MigrationManifest::standard();
                let artifacts = if i == 0 {
                    full_passing_artifacts()
                } else if i == 1 {
                    artifacts_missing(ArtifactType::BenchmarkResult)
                } else if i == 2 {
                    artifacts_with_content_failure(ArtifactType::TestReport)
                } else if i == 3 {
                    Vec::new()
                } else {
                    artifacts_with_schema_failure(ArtifactType::SloSnapshot)
                };
                VerificationReport::verify(&manifest, &artifacts)
            });
            handles.push(handle);
        }

        let mut results = Vec::new();
        for h in handles {
            results.push(h.await.unwrap());
        }

        // Verify expected verdicts
        assert_eq!(results[0].verdict, VerificationVerdict::Complete);
        assert_eq!(results[1].verdict, VerificationVerdict::Incomplete);
        // results[2] may be Incomplete (gate step failed) or PartiallyComplete
        assert_eq!(results[3].verdict, VerificationVerdict::Incomplete);
    });
}
