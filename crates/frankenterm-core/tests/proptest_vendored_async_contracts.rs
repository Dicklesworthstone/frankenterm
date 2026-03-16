//! Property tests for vendored_async_contracts module.

use proptest::prelude::*;

use frankenterm_core::vendored_async_contracts::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_boundary_direction() -> impl Strategy<Value = BoundaryDirection> {
    prop_oneof![
        Just(BoundaryDirection::CoreToVendored),
        Just(BoundaryDirection::VendoredToCore),
        Just(BoundaryDirection::Bidirectional),
    ]
}

fn arb_contract_category() -> impl Strategy<Value = ContractCategory> {
    prop_oneof![
        Just(ContractCategory::Ownership),
        Just(ContractCategory::Cancellation),
        Just(ContractCategory::Channeling),
        Just(ContractCategory::ErrorMapping),
        Just(ContractCategory::Backpressure),
        Just(ContractCategory::Timeout),
        Just(ContractCategory::TaskLifecycle),
    ]
}

fn arb_evidence_type() -> impl Strategy<Value = EvidenceType> {
    prop_oneof![
        Just(EvidenceType::UnitTest),
        Just(EvidenceType::IntegrationTest),
        Just(EvidenceType::StaticAnalysis),
        Just(EvidenceType::CodeReview),
        Just(EvidenceType::RuntimeAssertion),
    ]
}

#[allow(dead_code)]
fn arb_contract_evidence(contract_id: String) -> impl Strategy<Value = ContractEvidence> {
    (any::<bool>(), arb_evidence_type()).prop_map(move |(passed, evidence_type)| ContractEvidence {
        contract_id: contract_id.clone(),
        test_name: "prop_test".into(),
        passed,
        evidence_type,
        detail: if passed {
            "passed".into()
        } else {
            "failed".into()
        },
    })
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_boundary_direction(d in arb_boundary_direction()) {
        let json = serde_json::to_string(&d).unwrap();
        let back: BoundaryDirection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d, back);
    }

    #[test]
    fn serde_roundtrip_contract_category(c in arb_contract_category()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: ContractCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(c, back);
    }

    #[test]
    fn serde_roundtrip_evidence_type(e in arb_evidence_type()) {
        let json = serde_json::to_string(&e).unwrap();
        let back: EvidenceType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(e, back);
    }

    #[test]
    fn serde_roundtrip_contract_audit_report(_dummy in Just(())) {
        let report = make_all_compliant_report();
        let json = serde_json::to_string(&report).unwrap();
        let back: ContractAuditReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report.audit_id, back.audit_id);
        prop_assert_eq!(report.contracts.len(), back.contracts.len());
        prop_assert_eq!(report.overall_compliant, back.overall_compliant);
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn make_evidence(contract_id: &str, test_name: &str, passed: bool) -> ContractEvidence {
    ContractEvidence {
        contract_id: contract_id.to_owned(),
        test_name: test_name.to_owned(),
        passed,
        evidence_type: EvidenceType::UnitTest,
        detail: if passed {
            "assertion passed".to_owned()
        } else {
            "assertion failed".to_owned()
        },
    }
}

fn make_all_compliant_report() -> ContractAuditReport {
    let mut report = ContractAuditReport::new("prop-audit-001", 1_700_000_000_000);
    for contract in standard_contracts() {
        let id = contract.contract_id.clone();
        let evidence = vec![make_evidence(&id, "auto_test", true)];
        report.add_compliance(ContractCompliance::from_evidence(contract, evidence));
    }
    report.finalize();
    report
}

// =============================================================================
// standard_contracts property tests
// =============================================================================

#[test]
fn standard_contracts_ids_unique() {
    let contracts = standard_contracts();
    let mut ids: Vec<String> = contracts.iter().map(|c| c.contract_id.clone()).collect();
    let original_len = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), original_len, "duplicate contract IDs found");
}

#[test]
fn standard_contracts_all_categories_represented() {
    let contracts = standard_contracts();
    let categories: std::collections::HashSet<String> = contracts
        .iter()
        .map(|c| format!("{:?}", c.category))
        .collect();
    for expected in &[
        "Ownership",
        "Cancellation",
        "Channeling",
        "ErrorMapping",
        "Backpressure",
        "Timeout",
        "TaskLifecycle",
    ] {
        assert!(
            categories.contains(*expected),
            "missing category: {}",
            expected
        );
    }
}

#[test]
fn compliance_mismatched_contract_id_not_compliant_but_matching_coverage_full() {
    let contract = standard_contracts().into_iter().next().unwrap();
    let id = contract.contract_id.clone();
    let evidence = vec![
        make_evidence(&id, "match", true),
        make_evidence("ABC-CAN-001", "mismatch", true),
    ];
    let compliance = ContractCompliance::from_evidence(contract, evidence);
    // Not compliant because not all evidence targets this contract.
    assert!(!compliance.compliant);
    // But coverage is 1.0 because all MATCHING evidence (1/1) passed.
    assert!((compliance.coverage - 1.0).abs() < f64::EPSILON);
}

proptest! {
    #[test]
    fn standard_contracts_invariants_non_empty(_dummy in Just(())) {
        let contracts = standard_contracts();
        for c in &contracts {
            prop_assert!(!c.invariant.is_empty(), "contract {} has empty invariant", c.contract_id);
            prop_assert!(!c.description.is_empty(), "contract {} has empty description", c.contract_id);
            prop_assert!(!c.violation_impact.is_empty(), "contract {} has empty violation_impact", c.contract_id);
        }
    }

    #[test]
    fn standard_contracts_count_at_least_12(_dummy in Just(())) {
        let contracts = standard_contracts();
        prop_assert!(contracts.len() >= 12);
    }
}

// =============================================================================
// ContractCompliance property tests
// =============================================================================

proptest! {
    #[test]
    fn compliance_empty_evidence_not_compliant(_dummy in Just(())) {
        let contract = standard_contracts().into_iter().next().unwrap();
        let compliance = ContractCompliance::from_evidence(contract, vec![]);
        prop_assert!(!compliance.compliant);
        prop_assert!((compliance.coverage - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compliance_all_passing_is_compliant(n in 1usize..=10) {
        let contract = standard_contracts().into_iter().next().unwrap();
        let id = contract.contract_id.clone();
        let evidence: Vec<ContractEvidence> = (0..n)
            .map(|i| make_evidence(&id, &format!("test_{}", i), true))
            .collect();
        let compliance = ContractCompliance::from_evidence(contract, evidence);
        prop_assert!(compliance.compliant);
        prop_assert!((compliance.coverage - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compliance_one_failing_not_compliant(n in 2usize..=10) {
        let contract = standard_contracts().into_iter().next().unwrap();
        let id = contract.contract_id.clone();
        let mut evidence: Vec<ContractEvidence> = (0..n)
            .map(|i| make_evidence(&id, &format!("test_{}", i), true))
            .collect();
        // Make the last one fail
        evidence.last_mut().unwrap().passed = false;
        let compliance = ContractCompliance::from_evidence(contract, evidence);
        prop_assert!(!compliance.compliant);
        let expected_coverage = (n - 1) as f64 / n as f64;
        prop_assert!((compliance.coverage - expected_coverage).abs() < 0.01);
    }

    #[test]
    fn compliance_coverage_between_zero_and_one(
        pass_count in 0usize..=5,
        fail_count in 0usize..=5,
    ) {
        let contract = standard_contracts().into_iter().next().unwrap();
        let id = contract.contract_id.clone();
        let mut evidence = Vec::new();
        for i in 0..pass_count {
            evidence.push(make_evidence(&id, &format!("pass_{}", i), true));
        }
        for i in 0..fail_count {
            evidence.push(make_evidence(&id, &format!("fail_{}", i), false));
        }
        let compliance = ContractCompliance::from_evidence(contract, evidence);
        prop_assert!(compliance.coverage >= 0.0);
        prop_assert!(compliance.coverage <= 1.0);
    }
}

// =============================================================================
// ContractAuditReport property tests
// =============================================================================

proptest! {
    #[test]
    fn audit_report_finalize_consistency(_dummy in Just(())) {
        let report = make_all_compliant_report();
        // All compliant: overall_compliant=true, compliance_rate=1.0, no uncovered
        prop_assert!(report.overall_compliant);
        prop_assert!((report.compliance_rate - 1.0).abs() < f64::EPSILON);
        prop_assert!(report.uncovered_contracts.is_empty());
    }

    #[test]
    fn audit_report_failing_contracts_consistency(_dummy in Just(())) {
        let mut report = ContractAuditReport::new("fail-test", 0);
        let contracts = standard_contracts();
        for (i, contract) in contracts.into_iter().enumerate() {
            let id = contract.contract_id.clone();
            let passed = i % 2 == 0;
            let evidence = vec![make_evidence(&id, "test", passed)];
            report.add_compliance(ContractCompliance::from_evidence(contract, evidence));
        }
        report.finalize();

        let failing = report.failing_contracts();
        let failing_count = failing.len();
        let compliant_count = report.contracts.iter().filter(|c| c.compliant).count();
        prop_assert_eq!(failing_count + compliant_count, report.contracts.len());
        prop_assert!(!report.overall_compliant);
    }

    #[test]
    fn audit_report_by_category_exhaustive(_dummy in Just(())) {
        let report = make_all_compliant_report();
        let by_cat = report.by_category();
        // All 7 categories should appear
        prop_assert_eq!(by_cat.len(), 7);
        let total: usize = by_cat.values().map(|v| v.len()).sum();
        prop_assert_eq!(total, report.contracts.len());
    }

    #[test]
    fn audit_report_compliance_rate_bounded(_dummy in Just(())) {
        let report = make_all_compliant_report();
        prop_assert!(report.compliance_rate >= 0.0);
        prop_assert!(report.compliance_rate <= 1.0);
    }

    #[test]
    fn audit_report_uncovered_tracks_empty_evidence(_dummy in Just(())) {
        let mut report = ContractAuditReport::new("uncovered-test", 0);
        let contracts = standard_contracts();
        let total = contracts.len();
        let half = total / 2;
        for (i, contract) in contracts.into_iter().enumerate() {
            let evidence = if i < half {
                let id = contract.contract_id.clone();
                vec![make_evidence(&id, "test", true)]
            } else {
                vec![]
            };
            report.add_compliance(ContractCompliance::from_evidence(contract, evidence));
        }
        report.finalize();
        prop_assert_eq!(report.uncovered_contracts.len(), total - half);
    }
}

// =============================================================================
// summary() tests
// =============================================================================

proptest! {
    #[test]
    fn summary_contains_audit_id(_dummy in Just(())) {
        let report = make_all_compliant_report();
        let summary = report.summary();
        prop_assert!(summary.contains("prop-audit-001"));
    }

    #[test]
    fn summary_compliant_report_shows_compliant(_dummy in Just(())) {
        let report = make_all_compliant_report();
        let summary = report.summary();
        prop_assert!(summary.contains("COMPLIANT"));
    }

    #[test]
    fn summary_non_compliant_shows_non_compliant(_dummy in Just(())) {
        let mut report = ContractAuditReport::new("fail", 0);
        let contract = standard_contracts().into_iter().next().unwrap();
        let id = contract.contract_id.clone();
        let evidence = vec![make_evidence(&id, "test", false)];
        report.add_compliance(ContractCompliance::from_evidence(contract, evidence));
        report.finalize();
        let summary = report.summary();
        prop_assert!(summary.contains("NON-COMPLIANT"));
    }

    #[test]
    fn summary_contains_surface_status(_dummy in Just(())) {
        let report = make_all_compliant_report();
        let summary = report.summary();
        prop_assert!(summary.contains("keep="));
    }
}

// =============================================================================
// CompatibilityMapping tests
// =============================================================================

#[test]
fn compatibility_mappings_unique_apis() {
    let mappings = standard_compatibility_mappings();
    let mut apis: Vec<String> = mappings.iter().map(|m| m.compat_api.clone()).collect();
    let original_len = apis.len();
    apis.sort();
    apis.dedup();
    assert_eq!(apis.len(), original_len, "duplicate compat_apis found");
}

#[test]
fn compatibility_mappings_count() {
    let mappings = standard_compatibility_mappings();
    assert_eq!(mappings.len(), 15, "expected 15 mappings");
}

proptest! {
    #[test]
    fn compatibility_mapping_serde_roundtrip(_dummy in Just(())) {
        let mappings = standard_compatibility_mappings();
        let json = serde_json::to_string(&mappings).unwrap();
        let back: Vec<CompatibilityMapping> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(mappings.len(), back.len());
        for (orig, restored) in mappings.iter().zip(back.iter()) {
            prop_assert_eq!(&orig.compat_api, &restored.compat_api);
            prop_assert_eq!(orig.satisfies_contracts.len(), restored.satisfies_contracts.len());
            prop_assert_eq!(orig.disposition_aligned, restored.disposition_aligned);
        }
    }

    #[test]
    fn compatibility_mapping_contract_ids_valid(_dummy in Just(())) {
        let mappings = standard_compatibility_mappings();
        let contracts = standard_contracts();
        let valid_ids: std::collections::HashSet<String> = contracts.iter().map(|c| c.contract_id.clone()).collect();
        for m in &mappings {
            for cid in &m.satisfies_contracts {
                prop_assert!(valid_ids.contains(cid), "mapping {} references unknown contract {}", m.compat_api, cid);
            }
        }
    }
}
