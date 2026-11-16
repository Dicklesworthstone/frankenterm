//! Property tests for cross_crate_integration module.

use proptest::prelude::*;

use frankenterm_core::cross_crate_integration::*;

// =============================================================================
// Strategy helpers
// =============================================================================

fn arb_scenario_category() -> impl Strategy<Value = ScenarioCategory> {
    prop_oneof![
        Just(ScenarioCategory::UserCli),
        Just(ScenarioCategory::RobotMode),
        Just(ScenarioCategory::WatchPipeline),
        Just(ScenarioCategory::SearchStack),
        Just(ScenarioCategory::SessionLifecycle),
        Just(ScenarioCategory::DegradedPath),
        Just(ScenarioCategory::CancellationPath),
        Just(ScenarioCategory::RestartRecovery),
    ]
}

fn arb_crate_boundary() -> impl Strategy<Value = CrateBoundary> {
    prop_oneof![
        Just(CrateBoundary::CoreToVendored),
        Just(CrateBoundary::VendoredToCore),
        Just(CrateBoundary::CoreToSearch),
        Just(CrateBoundary::CoreToRuntime),
        Just(CrateBoundary::BinaryToCore),
    ]
}

fn arb_contract_type() -> impl Strategy<Value = ContractType> {
    prop_oneof![
        Just(ContractType::SemanticParity),
        Just(ContractType::LatencyBudget),
        Just(ContractType::RecoverySafety),
        Just(ContractType::ErrorPropagation),
        Just(ContractType::ResourceCleanup),
        Just(ContractType::StateConsistency),
    ]
}

fn arb_scenario_step() -> impl Strategy<Value = ScenarioStep> {
    (
        1..100u32,
        "[a-z ]{5,30}",
        prop::option::of(arb_crate_boundary()),
        "[a-z ]{5,30}",
        any::<bool>(),
    )
        .prop_map(|(step, desc, boundary, outcome, fault)| ScenarioStep {
            step,
            description: desc,
            crate_boundary: boundary,
            expected_outcome: outcome,
            fault_injection: fault,
        })
}

fn arb_contract_assertion() -> impl Strategy<Value = ContractAssertion> {
    (
        "[A-Z]{3}-[0-9]{3}-A[0-9]",
        "[a-z ]{5,30}",
        arb_contract_type(),
        any::<bool>(),
        "[a-z ]{5,30}",
    )
        .prop_map(|(id, desc, ct, passed, evidence)| ContractAssertion {
            assertion_id: id,
            description: desc,
            contract_type: ct,
            passed,
            evidence,
        })
}

fn arb_integration_scenario() -> impl Strategy<Value = IntegrationScenario> {
    (
        "[A-Z]{3}-[A-Z]{3}-[0-9]{3}",
        arb_scenario_category(),
        "[a-z ]{5,20}",
        "[a-z ]{10,40}",
        prop::collection::vec(arb_scenario_step(), 1..5),
        prop::collection::vec(arb_contract_assertion(), 1..4),
        prop::collection::vec(arb_crate_boundary(), 0..4),
        any::<bool>(),
    )
        .prop_map(
            |(id, cat, title, desc, steps, assertions, boundaries, fault)| IntegrationScenario {
                scenario_id: id,
                category: cat,
                title,
                description: desc,
                steps,
                assertions,
                boundaries_exercised: boundaries,
                includes_fault_injection: fault,
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_scenario_category(cat in arb_scenario_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let restored: ScenarioCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, restored);
    }

    #[test]
    fn serde_roundtrip_crate_boundary(b in arb_crate_boundary()) {
        let json = serde_json::to_string(&b).unwrap();
        let restored: CrateBoundary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(b, restored);
    }

    #[test]
    fn serde_roundtrip_contract_type(ct in arb_contract_type()) {
        let json = serde_json::to_string(&ct).unwrap();
        let restored: ContractType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ct, restored);
    }

    #[test]
    fn serde_roundtrip_scenario_step(step in arb_scenario_step()) {
        let json = serde_json::to_string(&step).unwrap();
        let restored: ScenarioStep = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(step.step, restored.step);
        prop_assert_eq!(step.crate_boundary, restored.crate_boundary);
        prop_assert_eq!(step.fault_injection, restored.fault_injection);
    }

    #[test]
    fn serde_roundtrip_contract_assertion(a in arb_contract_assertion()) {
        let json = serde_json::to_string(&a).unwrap();
        let restored: ContractAssertion = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&a.assertion_id, &restored.assertion_id);
        prop_assert_eq!(a.contract_type, restored.contract_type);
        prop_assert_eq!(a.passed, restored.passed);
    }

    #[test]
    fn serde_roundtrip_integration_scenario(s in arb_integration_scenario()) {
        let json = serde_json::to_string(&s).unwrap();
        let restored: IntegrationScenario = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&s.scenario_id, &restored.scenario_id);
        prop_assert_eq!(s.category, restored.category);
        prop_assert_eq!(s.steps.len(), restored.steps.len());
        prop_assert_eq!(s.assertions.len(), restored.assertions.len());
    }

    #[test]
    fn serde_roundtrip_suite_report(scenarios in prop::collection::vec(arb_integration_scenario(), 0..5)) {
        let report = SuiteReport::from_scenarios(&scenarios);
        let json = serde_json::to_string(&report).unwrap();
        let restored: SuiteReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report.total_scenarios, restored.total_scenarios);
        prop_assert_eq!(report.overall_pass, restored.overall_pass);
        prop_assert_eq!(report.fault_injection_exercised, restored.fault_injection_exercised);
    }
}

// =============================================================================
// ScenarioCategory invariants
// =============================================================================

proptest! {
    #[test]
    fn category_label_not_empty(cat in arb_scenario_category()) {
        prop_assert!(!cat.label().is_empty());
    }

    #[test]
    fn category_label_stable(cat in arb_scenario_category()) {
        prop_assert_eq!(cat.label(), cat.label());
    }

    #[test]
    fn category_self_equality(cat in arb_scenario_category()) {
        prop_assert_eq!(cat, cat);
    }
}

// =============================================================================
// CrateBoundary invariants
// =============================================================================

proptest! {
    #[test]
    fn boundary_label_not_empty(b in arb_crate_boundary()) {
        prop_assert!(!b.label().is_empty());
    }

    #[test]
    fn boundary_label_contains_arrow(b in arb_crate_boundary()) {
        prop_assert!(b.label().contains('→'));
    }
}

// =============================================================================
// SuiteReport from_scenarios properties
// =============================================================================

proptest! {
    #[test]
    fn report_counts_consistent(scenarios in prop::collection::vec(arb_integration_scenario(), 0..8)) {
        let n = scenarios.len();
        let report = SuiteReport::from_scenarios(&scenarios);
        prop_assert_eq!(report.total_scenarios, n);
        prop_assert_eq!(report.passed_scenarios + report.failed_scenarios, n);
    }

    #[test]
    fn report_overall_pass_iff_no_failures(scenarios in prop::collection::vec(arb_integration_scenario(), 0..6)) {
        let report = SuiteReport::from_scenarios(&scenarios);
        prop_assert_eq!(report.overall_pass, report.failed_scenarios == 0);
    }

    #[test]
    fn report_scenario_passed_iff_all_assertions_pass(scenario in arb_integration_scenario()) {
        let report = SuiteReport::from_scenarios(std::slice::from_ref(&scenario));
        let expected_pass = scenario.assertions.iter().all(|a| a.passed);
        prop_assert_eq!(report.results[0].passed, expected_pass);
    }

    #[test]
    fn report_steps_counted(scenario in arb_integration_scenario()) {
        let expected_steps = scenario.steps.len() as u32;
        let report = SuiteReport::from_scenarios(&[scenario]);
        prop_assert_eq!(report.results[0].steps_completed, expected_steps);
        prop_assert_eq!(report.results[0].total_steps, expected_steps);
    }

    #[test]
    fn report_assertions_passed_count(scenario in arb_integration_scenario()) {
        let expected = scenario.assertions.iter().filter(|a| a.passed).count();
        let report = SuiteReport::from_scenarios(&[scenario]);
        prop_assert_eq!(report.results[0].assertions_passed, expected);
    }

    #[test]
    fn report_fault_injection_flag_matches(scenarios in prop::collection::vec(arb_integration_scenario(), 1..5)) {
        let expected = scenarios.iter().any(|s| s.includes_fault_injection);
        let report = SuiteReport::from_scenarios(&scenarios);
        prop_assert_eq!(report.fault_injection_exercised, expected);
    }

    #[test]
    fn report_boundaries_are_subset_of_scenario_boundaries(scenarios in prop::collection::vec(arb_integration_scenario(), 0..5)) {
        let report = SuiteReport::from_scenarios(&scenarios);
        for b in &report.boundaries_covered {
            let found = scenarios.iter().any(|s| s.boundaries_exercised.contains(b));
            prop_assert!(found);
        }
    }

    #[test]
    fn report_categories_are_subset_of_scenario_categories(scenarios in prop::collection::vec(arb_integration_scenario(), 0..5)) {
        let report = SuiteReport::from_scenarios(&scenarios);
        for c in &report.categories_covered {
            let found = scenarios.iter().any(|s| s.category == *c);
            prop_assert!(found);
        }
    }

    #[test]
    fn empty_scenarios_pass(scenarios in Just(Vec::<IntegrationScenario>::new())) {
        let report = SuiteReport::from_scenarios(&scenarios);
        prop_assert!(report.overall_pass);
        prop_assert_eq!(report.total_scenarios, 0);
    }
}

// =============================================================================
// Coverage gap properties
// =============================================================================

proptest! {
    #[test]
    fn coverage_gaps_disjoint_from_covered(scenarios in prop::collection::vec(arb_integration_scenario(), 0..5)) {
        let report = SuiteReport::from_scenarios(&scenarios);
        let gaps = report.coverage_gaps();
        for gap in &gaps {
            prop_assert!(!report.categories_covered.contains(gap));
        }
    }

    #[test]
    fn gaps_plus_covered_equals_all(scenarios in prop::collection::vec(arb_integration_scenario(), 0..5)) {
        let report = SuiteReport::from_scenarios(&scenarios);
        let gaps = report.coverage_gaps();
        let total = gaps.len() + report.categories_covered.len();
        prop_assert_eq!(total, ScenarioCategory::all().len());
    }
}

// =============================================================================
// Render summary properties
// =============================================================================

proptest! {
    #[test]
    fn render_summary_not_empty(scenarios in prop::collection::vec(arb_integration_scenario(), 0..4)) {
        let report = SuiteReport::from_scenarios(&scenarios);
        let summary = report.render_summary();
        prop_assert!(!summary.is_empty());
        prop_assert!(summary.contains("Cross-Crate Integration Suite"));
    }

    #[test]
    fn render_summary_reflects_pass_fail(scenarios in prop::collection::vec(arb_integration_scenario(), 1..4)) {
        let report = SuiteReport::from_scenarios(&scenarios);
        let summary = report.render_summary();
        if report.overall_pass {
            prop_assert!(summary.contains("ALL PASS"));
        } else {
            prop_assert!(summary.contains("FAILURES"));
        }
    }
}

// =============================================================================
// Standard data tests
// =============================================================================

proptest! {
    #[test]
    fn standard_scenarios_ids_unique(_dummy in Just(())) {
        let scenarios = standard_scenarios();
        for (i, a) in scenarios.iter().enumerate() {
            for (j, b) in scenarios.iter().enumerate() {
                if i != j {
                    prop_assert_ne!(&a.scenario_id, &b.scenario_id);
                }
            }
        }
    }

    #[test]
    fn standard_scenarios_all_have_steps_and_assertions(_dummy in Just(())) {
        let scenarios = standard_scenarios();
        for s in &scenarios {
            prop_assert!(!s.steps.is_empty());
            prop_assert!(!s.assertions.is_empty());
        }
    }

    #[test]
    fn standard_scenarios_include_fault_injection(_dummy in Just(())) {
        let scenarios = standard_scenarios();
        let has_fault = scenarios.iter().any(|s| s.includes_fault_injection);
        prop_assert!(has_fault);
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn scenario_category_all_length() {
    assert_eq!(ScenarioCategory::all().len(), 8);
}

#[test]
fn category_labels_all_unique() {
    let all = ScenarioCategory::all();
    let labels: Vec<&str> = all.iter().map(|c| c.label()).collect();
    for (i, a) in labels.iter().enumerate() {
        for (j, b) in labels.iter().enumerate() {
            if i != j {
                assert_ne!(a, b);
            }
        }
    }
}

#[test]
fn standard_scenarios_all_pass_by_default() {
    let scenarios = standard_scenarios();
    let report = SuiteReport::from_scenarios(&scenarios);
    assert!(report.overall_pass);
}
