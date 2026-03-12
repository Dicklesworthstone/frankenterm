//! Property tests for migration_artifact_contracts module.

use proptest::prelude::*;
use frankenterm_core::migration_artifact_contracts::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_artifact_type() -> impl Strategy<Value = ArtifactType> {
    prop_oneof![
        Just(ArtifactType::BenchmarkResult),
        Just(ArtifactType::TestReport),
        Just(ArtifactType::SloSnapshot),
        Just(ArtifactType::DependencyScan),
        Just(ArtifactType::RegressionGuard),
        Just(ArtifactType::SoakResult),
        Just(ArtifactType::RecoveryResult),
        Just(ArtifactType::DiagnosticCertification),
        Just(ArtifactType::IntegrationSuiteReport),
        Just(ArtifactType::LogBundle),
    ]
}

fn arb_verification_verdict() -> impl Strategy<Value = VerificationVerdict> {
    prop_oneof![
        Just(VerificationVerdict::Complete),
        Just(VerificationVerdict::PartiallyComplete),
        Just(VerificationVerdict::Incomplete),
    ]
}

fn arb_collected_artifact() -> impl Strategy<Value = CollectedArtifact> {
    (arb_artifact_type(), any::<bool>(), any::<bool>()).prop_map(
        |(artifact_type, schema_valid, content_pass)| CollectedArtifact {
            artifact_type,
            location: format!("artifacts/{}.json", artifact_type.label()),
            schema_valid,
            content_pass,
        },
    )
}

fn make_full_artifacts(schema_valid: bool, content_pass: bool) -> Vec<CollectedArtifact> {
    ArtifactType::all()
        .iter()
        .map(|t| CollectedArtifact {
            artifact_type: *t,
            location: format!("artifacts/{}.json", t.label()),
            schema_valid,
            content_pass,
        })
        .collect()
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_artifact_type(t in arb_artifact_type()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: ArtifactType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t, back);
    }

    #[test]
    fn serde_roundtrip_verification_verdict(v in arb_verification_verdict()) {
        let json = serde_json::to_string(&v).unwrap();
        let back: VerificationVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, back);
    }

    #[test]
    fn serde_roundtrip_collected_artifact(a in arb_collected_artifact()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: CollectedArtifact = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a.artifact_type, back.artifact_type);
        prop_assert_eq!(a.schema_valid, back.schema_valid);
        prop_assert_eq!(a.content_pass, back.content_pass);
    }

    #[test]
    fn serde_roundtrip_migration_manifest_standard(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let json = serde_json::to_string(&manifest).unwrap();
        let back: MigrationManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(manifest.steps.len(), back.steps.len());
        prop_assert_eq!(manifest.contracts.len(), back.contracts.len());
        prop_assert_eq!(manifest.manifest_id, back.manifest_id);
    }
}

// =============================================================================
// ArtifactType property tests
// =============================================================================

proptest! {
    #[test]
    fn artifact_type_label_non_empty(t in arb_artifact_type()) {
        prop_assert!(!t.label().is_empty());
    }

    #[test]
    fn artifact_type_label_no_whitespace(t in arb_artifact_type()) {
        prop_assert!(!t.label().contains(' '));
    }

    #[test]
    fn artifact_type_label_lowercase(t in arb_artifact_type()) {
        prop_assert_eq!(t.label(), t.label().to_lowercase());
    }
}

#[test]
fn artifact_type_all_labels_unique() {
    let all = ArtifactType::all();
    let mut labels: Vec<&str> = all.iter().map(|t| t.label()).collect();
    let original_len = labels.len();
    labels.sort();
    labels.dedup();
    assert_eq!(labels.len(), original_len, "duplicate labels found");
}

#[test]
fn artifact_type_all_covers_every_variant() {
    let all = ArtifactType::all();
    assert_eq!(all.len(), 10, "expected 10 artifact types");
}

// =============================================================================
// ArtifactContract tests
// =============================================================================

#[test]
fn standard_contracts_cover_all_types() {
    let contracts = standard_artifact_contracts();
    let types: Vec<ArtifactType> = contracts.iter().map(|c| c.artifact_type).collect();
    for t in ArtifactType::all() {
        assert!(types.contains(t), "missing contract for {:?}", t);
    }
}

#[test]
fn standard_contracts_all_have_required_fields() {
    let contracts = standard_artifact_contracts();
    for c in &contracts {
        assert!(
            !c.required_fields.is_empty(),
            "{:?} has no required fields",
            c.artifact_type
        );
    }
}

#[test]
fn standard_contracts_all_have_proof_statement() {
    let contracts = standard_artifact_contracts();
    for c in &contracts {
        assert!(
            !c.proof_statement.is_empty(),
            "{:?} has empty proof statement",
            c.artifact_type
        );
    }
}

#[test]
fn standard_contracts_mandatory_count() {
    let contracts = standard_artifact_contracts();
    let mandatory_count = contracts.iter().filter(|c| c.mandatory).count();
    // At least 6 mandatory artifacts
    assert!(
        mandatory_count >= 6,
        "expected at least 6 mandatory, got {}",
        mandatory_count
    );
}

// =============================================================================
// MigrationManifest tests
// =============================================================================

proptest! {
    #[test]
    fn manifest_steps_order_sequential(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        for (i, step) in manifest.steps.iter().enumerate() {
            prop_assert_eq!(step.order as usize, i + 1);
        }
    }

    #[test]
    fn manifest_all_steps_produce_artifacts(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        for step in &manifest.steps {
            prop_assert!(!step.produces.is_empty(), "step {} has no artifacts", step.step_id);
        }
    }

    #[test]
    fn manifest_gate_steps_are_subset_of_steps(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let gates = manifest.gate_steps();
        let step_ids: Vec<&str> = manifest.steps.iter().map(|s| s.step_id.as_str()).collect();
        for gate in &gates {
            prop_assert!(step_ids.contains(&gate.step_id.as_str()));
        }
    }

    #[test]
    fn manifest_mandatory_subset_of_contracts(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let mandatory = manifest.mandatory_artifacts();
        let contract_types: Vec<ArtifactType> = manifest.contracts.iter().map(|c| c.artifact_type).collect();
        for m in &mandatory {
            prop_assert!(contract_types.contains(m));
        }
    }

    #[test]
    fn manifest_step_ids_unique(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let mut ids: Vec<&str> = manifest.steps.iter().map(|s| s.step_id.as_str()).collect();
        let original_len = ids.len();
        ids.sort();
        ids.dedup();
        prop_assert_eq!(ids.len(), original_len);
    }
}

// =============================================================================
// VerificationReport tests
// =============================================================================

proptest! {
    #[test]
    fn verify_full_valid_artifacts_is_complete(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let artifacts = make_full_artifacts(true, true);
        let report = VerificationReport::verify(&manifest, &artifacts);
        prop_assert_eq!(report.verdict, VerificationVerdict::Complete);
        prop_assert!(report.mandatory_missing.is_empty());
        prop_assert!(report.step_results.iter().all(|s| s.passed));
    }

    #[test]
    fn verify_empty_artifacts_is_incomplete(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let report = VerificationReport::verify(&manifest, &[]);
        prop_assert_eq!(report.verdict, VerificationVerdict::Incomplete);
        prop_assert!(!report.mandatory_missing.is_empty());
        prop_assert_eq!(report.artifacts_collected, 0);
    }

    #[test]
    fn verify_schema_invalid_fails_step(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let mut artifacts = make_full_artifacts(true, true);
        artifacts[0].schema_valid = false;
        let report = VerificationReport::verify(&manifest, &artifacts);
        prop_assert!(report.step_results.iter().any(|s| !s.passed));
    }

    #[test]
    fn verify_content_fail_fails_step(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let mut artifacts = make_full_artifacts(true, true);
        artifacts[0].content_pass = false;
        let report = VerificationReport::verify(&manifest, &artifacts);
        prop_assert!(report.step_results.iter().any(|s| !s.passed));
    }

    #[test]
    fn verify_report_id_matches_manifest(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let artifacts = make_full_artifacts(true, true);
        let report = VerificationReport::verify(&manifest, &artifacts);
        prop_assert!(report.report_id.contains(&manifest.manifest_id));
        prop_assert_eq!(report.manifest_id, manifest.manifest_id);
    }

    #[test]
    fn verify_artifacts_collected_equals_input_len(count in 0usize..=10) {
        let manifest = MigrationManifest::standard();
        let all = make_full_artifacts(true, true);
        let subset: Vec<CollectedArtifact> = all.into_iter().take(count).collect();
        let report = VerificationReport::verify(&manifest, &subset);
        prop_assert_eq!(report.artifacts_collected, subset.len());
    }

    #[test]
    fn verify_step_results_count_matches_manifest(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let step_count = manifest.steps.len();
        let artifacts = make_full_artifacts(true, true);
        let report = VerificationReport::verify(&manifest, &artifacts);
        prop_assert_eq!(report.step_results.len(), step_count);
    }
}

// =============================================================================
// render_summary tests
// =============================================================================

proptest! {
    #[test]
    fn render_summary_contains_report_id(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let artifacts = make_full_artifacts(true, true);
        let report = VerificationReport::verify(&manifest, &artifacts);
        let summary = report.render_summary();
        prop_assert!(summary.contains(&report.report_id));
    }

    #[test]
    fn render_summary_contains_verdict(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let report = VerificationReport::verify(&manifest, &[]);
        let summary = report.render_summary();
        prop_assert!(summary.contains("Incomplete"));
    }

    #[test]
    fn render_summary_complete_contains_pass(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let artifacts = make_full_artifacts(true, true);
        let report = VerificationReport::verify(&manifest, &artifacts);
        let summary = report.render_summary();
        prop_assert!(summary.contains("PASS"));
    }

    #[test]
    fn render_summary_missing_shows_missing(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let report = VerificationReport::verify(&manifest, &[]);
        let summary = report.render_summary();
        prop_assert!(summary.contains("missing"));
    }
}

// =============================================================================
// StepVerification property tests
// =============================================================================

proptest! {
    #[test]
    fn step_verification_missing_implies_not_complete(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let report = VerificationReport::verify(&manifest, &[]);
        for step in &report.step_results {
            if !step.missing_artifacts.is_empty() {
                prop_assert!(!step.artifacts_complete);
                prop_assert!(!step.passed);
            }
        }
    }

    #[test]
    fn step_verification_passed_implies_all_conditions(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let artifacts = make_full_artifacts(true, true);
        let report = VerificationReport::verify(&manifest, &artifacts);
        for step in &report.step_results {
            if step.passed {
                prop_assert!(step.artifacts_complete);
                prop_assert!(step.schemas_valid);
                prop_assert!(step.content_passing);
            }
        }
    }
}

// =============================================================================
// Serde roundtrip of VerificationReport
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_verification_report(_dummy in Just(())) {
        let manifest = MigrationManifest::standard();
        let artifacts = make_full_artifacts(true, true);
        let report = VerificationReport::verify(&manifest, &artifacts);
        let json = serde_json::to_string(&report).unwrap();
        let back: VerificationReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report.verdict, back.verdict);
        prop_assert_eq!(report.step_results.len(), back.step_results.len());
        prop_assert_eq!(report.mandatory_missing.len(), back.mandatory_missing.len());
    }

    #[test]
    fn serde_roundtrip_artifact_contract(_dummy in Just(())) {
        let contracts = standard_artifact_contracts();
        for c in &contracts {
            let json = serde_json::to_string(c).unwrap();
            let back: ArtifactContract = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(c.artifact_type, back.artifact_type);
            prop_assert_eq!(c.mandatory, back.mandatory);
            prop_assert_eq!(c.required_fields.len(), back.required_fields.len());
        }
    }
}
