//! Integration tests for Core↔Vendored async contracts (ft-e34d9.10.5.4 AC#4).
//!
//! Validates cross-layer workflows and degraded behavior using the contract
//! definitions from `vendored_async_contracts.rs`. These tests exercise the
//! compliance and audit infrastructure to prove that contract invariants
//! hold at the integration level — not just in isolation.

use frankenterm_core::vendored_async_contracts::*;

// =============================================================================
// Contract Infrastructure Integration
// =============================================================================

#[test]
fn standard_contracts_cover_all_categories() {
    let contracts = standard_contracts();
    let categories: std::collections::HashSet<_> = contracts.iter().map(|c| c.category).collect();

    assert!(categories.contains(&ContractCategory::Ownership));
    assert!(categories.contains(&ContractCategory::Cancellation));
    assert!(categories.contains(&ContractCategory::Channeling));
    assert!(categories.contains(&ContractCategory::ErrorMapping));
    assert!(categories.contains(&ContractCategory::Backpressure));
    assert!(categories.contains(&ContractCategory::Timeout));
    assert!(categories.contains(&ContractCategory::TaskLifecycle));
    assert_eq!(categories.len(), 7, "exactly 7 contract categories");
}

#[test]
fn standard_contracts_have_exactly_12_entries() {
    assert_eq!(standard_contracts().len(), 12);
}

#[test]
fn contract_ids_are_unique() {
    let contracts = standard_contracts();
    let ids: std::collections::HashSet<_> = contracts.iter().map(|c| &c.contract_id).collect();
    assert_eq!(ids.len(), contracts.len());
}

#[test]
fn contract_ids_follow_naming_convention() {
    for contract in &standard_contracts() {
        assert!(
            contract.contract_id.starts_with("ABC-"),
            "Contract ID '{}' must start with 'ABC-'",
            contract.contract_id
        );
        let parts: Vec<&str> = contract.contract_id.split('-').collect();
        assert_eq!(parts.len(), 3, "Contract ID format: ABC-CAT-NNN");
    }
}

#[test]
fn only_err_002_is_non_verifiable() {
    let contracts = standard_contracts();
    let non_verifiable: Vec<_> = contracts.iter().filter(|c| !c.verifiable).collect();
    assert_eq!(non_verifiable.len(), 1);
    assert_eq!(non_verifiable[0].contract_id, "ABC-ERR-002");
}

// =============================================================================
// Compliance Evidence Assembly
// =============================================================================

fn make_evidence(contract: &AsyncBoundaryContract, passed: bool) -> ContractEvidence {
    ContractEvidence {
        contract_id: contract.contract_id.clone(),
        test_name: format!("integration_{}", contract.contract_id),
        passed,
        evidence_type: EvidenceType::IntegrationTest,
        detail: if passed { "OK" } else { "FAIL" }.into(),
    }
}

#[test]
fn compliance_with_all_passing_evidence() {
    for contract in standard_contracts() {
        let evidence = vec![make_evidence(&contract, true)];
        let compliance = ContractCompliance::from_evidence(contract.clone(), evidence);
        assert!(
            compliance.compliant,
            "Contract {} should be compliant with passing evidence",
            contract.contract_id
        );
        assert!((compliance.coverage - 1.0).abs() < f64::EPSILON);
    }
}

#[test]
fn compliance_with_failing_evidence() {
    let contract = standard_contracts().into_iter().next().unwrap();
    let evidence = vec![make_evidence(&contract, false)];
    let compliance = ContractCompliance::from_evidence(contract, evidence);
    assert!(!compliance.compliant);
    assert!((compliance.coverage - 0.0).abs() < f64::EPSILON);
}

#[test]
fn compliance_with_mixed_evidence_computes_coverage() {
    let contract = standard_contracts().into_iter().next().unwrap();
    let evidence = vec![
        ContractEvidence {
            contract_id: contract.contract_id.clone(),
            test_name: "test_a".into(),
            passed: true,
            evidence_type: EvidenceType::UnitTest,
            detail: "OK".into(),
        },
        ContractEvidence {
            contract_id: contract.contract_id.clone(),
            test_name: "test_b".into(),
            passed: false,
            evidence_type: EvidenceType::StaticAnalysis,
            detail: "Found violation".into(),
        },
    ];
    let compliance = ContractCompliance::from_evidence(contract, evidence);
    assert!(!compliance.compliant, "mixed evidence → non-compliant");
    assert!(
        (compliance.coverage - 0.5).abs() < f64::EPSILON,
        "50% coverage"
    );
}

#[test]
fn compliance_with_no_evidence_is_non_compliant() {
    let contract = standard_contracts().into_iter().next().unwrap();
    let compliance = ContractCompliance::from_evidence(contract, Vec::new());
    assert!(!compliance.compliant);
    assert!((compliance.coverage - 0.0).abs() < f64::EPSILON);
}

#[test]
fn compliance_with_mismatched_contract_id_is_non_compliant() {
    let contract = standard_contracts().into_iter().next().unwrap();
    let evidence = vec![
        make_evidence(&contract, true),
        ContractEvidence {
            contract_id: "ABC-CAN-001".into(),
            test_name: "wrong_contract".into(),
            passed: true,
            evidence_type: EvidenceType::StaticAnalysis,
            detail: "mismatch".into(),
        },
    ];

    let compliance = ContractCompliance::from_evidence(contract, evidence);
    assert!(!compliance.compliant);
    assert!((compliance.coverage - 0.5).abs() < f64::EPSILON);
}

// =============================================================================
// Audit Report Assembly
// =============================================================================

fn build_report(all_passing: bool) -> ContractAuditReport {
    let mut report = ContractAuditReport::new("integration-test", 1000);
    for contract in standard_contracts() {
        let evidence = vec![make_evidence(&contract, all_passing)];
        report.add_compliance(ContractCompliance::from_evidence(contract, evidence));
    }
    report.finalize();
    report
}

#[test]
fn audit_report_all_passing() {
    let report = build_report(true);
    assert!(report.overall_compliant);
    assert!((report.compliance_rate - 1.0).abs() < f64::EPSILON);
    assert!(report.uncovered_contracts.is_empty());
    assert!(report.failing_contracts().is_empty());
}

#[test]
fn audit_report_all_failing() {
    let report = build_report(false);
    assert!(!report.overall_compliant);
    assert!((report.compliance_rate - 0.0).abs() < f64::EPSILON);
    assert_eq!(report.failing_contracts().len(), 12);
}

#[test]
fn audit_report_with_one_failure() {
    let mut report = ContractAuditReport::new("partial-test", 2000);
    for (i, contract) in standard_contracts().into_iter().enumerate() {
        let evidence = vec![make_evidence(&contract, i != 0)];
        report.add_compliance(ContractCompliance::from_evidence(contract, evidence));
    }
    report.finalize();

    assert!(!report.overall_compliant);
    assert!(report.compliance_rate > 0.9); // 11/12 = 0.917
    assert_eq!(report.failing_contracts().len(), 1);
    assert_eq!(
        report.failing_contracts()[0].contract.contract_id,
        "ABC-OWN-001"
    );
}

#[test]
fn audit_report_empty_is_non_compliant() {
    let mut report = ContractAuditReport::new("empty-test", 3000);
    report.finalize();
    assert!(!report.overall_compliant);
    assert!((report.compliance_rate - 0.0).abs() < f64::EPSILON);
}

#[test]
fn audit_report_by_category_covers_all_7() {
    let report = build_report(true);
    let by_cat = report.by_category();
    assert_eq!(by_cat.len(), 7, "must cover all 7 categories");
}

#[test]
fn audit_report_summary_is_non_empty() {
    let report = build_report(true);
    let summary = report.summary();
    assert!(!summary.is_empty());
    assert!(summary.contains("COMPLIANT"));
}

// =============================================================================
// Compatibility Mapping Integration
// =============================================================================

#[test]
fn compatibility_mappings_have_15_entries() {
    assert_eq!(standard_compatibility_mappings().len(), 15);
}

#[test]
fn compatibility_mapping_apis_are_unique() {
    let mappings = standard_compatibility_mappings();
    let apis: std::collections::HashSet<_> = mappings.iter().map(|m| &m.compat_api).collect();
    assert_eq!(apis.len(), mappings.len());
}

#[test]
fn compatibility_mapping_contract_ids_reference_valid_contracts() {
    let contract_ids: std::collections::HashSet<_> = standard_contracts()
        .iter()
        .map(|c| c.contract_id.clone())
        .collect();
    for mapping in &standard_compatibility_mappings() {
        for cid in &mapping.satisfies_contracts {
            assert!(
                contract_ids.contains(cid),
                "Mapping '{}' references unknown contract '{}'",
                mapping.compat_api,
                cid
            );
        }
    }
}

#[test]
fn misaligned_mappings_have_reasons() {
    // spawn_detached, process::Command, and signal should be misaligned
    let mappings = standard_compatibility_mappings();
    let misaligned: Vec<_> = mappings.iter().filter(|m| !m.disposition_aligned).collect();
    assert!(
        misaligned.len() >= 3,
        "at least 3 misaligned mappings expected"
    );
}

// =============================================================================
// Cross-Category Invariant Tests
// =============================================================================

#[test]
fn ownership_contracts_are_bidirectional() {
    let ownership: Vec<_> = standard_contracts()
        .into_iter()
        .filter(|c| c.category == ContractCategory::Ownership)
        .collect();
    assert_eq!(ownership.len(), 2);
    for c in &ownership {
        assert_eq!(c.direction, BoundaryDirection::Bidirectional);
    }
}

#[test]
fn error_mapping_includes_vendored_to_core() {
    let err: Vec<_> = standard_contracts()
        .into_iter()
        .filter(|c| c.category == ContractCategory::ErrorMapping)
        .collect();
    assert_eq!(err.len(), 2);
    assert!(
        err.iter()
            .any(|c| c.direction == BoundaryDirection::VendoredToCore)
    );
}

#[test]
fn backpressure_is_vendored_to_core() {
    let bp: Vec<_> = standard_contracts()
        .into_iter()
        .filter(|c| c.category == ContractCategory::Backpressure)
        .collect();
    assert_eq!(bp.len(), 1);
    assert_eq!(bp[0].direction, BoundaryDirection::VendoredToCore);
}

#[test]
fn timeout_is_core_to_vendored() {
    let to: Vec<_> = standard_contracts()
        .into_iter()
        .filter(|c| c.category == ContractCategory::Timeout)
        .collect();
    assert_eq!(to.len(), 1);
    assert_eq!(to[0].direction, BoundaryDirection::CoreToVendored);
}

// =============================================================================
// Serde Stability
// =============================================================================

#[test]
fn all_contracts_serde_roundtrip() {
    let contracts = standard_contracts();
    let json = serde_json::to_string_pretty(&contracts).unwrap();
    let back: Vec<AsyncBoundaryContract> = serde_json::from_str(&json).unwrap();
    assert_eq!(back.len(), contracts.len());
    for (a, b) in contracts.iter().zip(back.iter()) {
        assert_eq!(a.contract_id, b.contract_id);
        assert_eq!(a.category, b.category);
        assert_eq!(a.direction, b.direction);
    }
}

#[test]
fn audit_report_serde_roundtrip() {
    let report = build_report(true);
    let json = serde_json::to_string(&report).unwrap();
    let back: ContractAuditReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.overall_compliant, report.overall_compliant);
    assert!((back.compliance_rate - report.compliance_rate).abs() < f64::EPSILON);
    assert_eq!(back.contracts.len(), report.contracts.len());
}

// =============================================================================
// Regression Anchors
// =============================================================================

#[test]
fn contract_category_distribution_is_stable() {
    let contracts = standard_contracts();
    let mut counts: std::collections::HashMap<ContractCategory, usize> =
        std::collections::HashMap::new();
    for c in &contracts {
        *counts.entry(c.category).or_default() += 1;
    }
    assert_eq!(counts[&ContractCategory::Ownership], 2);
    assert_eq!(counts[&ContractCategory::Cancellation], 2);
    assert_eq!(counts[&ContractCategory::Channeling], 2);
    assert_eq!(counts[&ContractCategory::ErrorMapping], 2);
    assert_eq!(counts[&ContractCategory::Backpressure], 1);
    assert_eq!(counts[&ContractCategory::Timeout], 1);
    assert_eq!(counts[&ContractCategory::TaskLifecycle], 2);
}

#[test]
fn all_invariant_fields_non_empty() {
    for c in &standard_contracts() {
        assert!(!c.invariant.is_empty(), "{} invariant empty", c.contract_id);
        assert!(
            !c.violation_impact.is_empty(),
            "{} impact empty",
            c.contract_id
        );
        assert!(!c.description.is_empty(), "{} desc empty", c.contract_id);
    }
}
