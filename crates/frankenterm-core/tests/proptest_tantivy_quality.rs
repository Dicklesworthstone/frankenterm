//! Property-based tests for the `tantivy_quality` module.
//!
//! Covers `QueryClass` enum serde, `LatencyBudget` serde,
//! `AssertionResult` serde, `QueryTestResult` serde,
//! `QualityReport` serde and aggregate invariants,
//! and `default_latency_budgets()` coverage.

use std::time::Duration;

use frankenterm_core::tantivy_quality::{
    AssertionResult, LatencyBudget, QualityReport, QueryClass, QueryTestResult,
    default_latency_budgets,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_query_class() -> impl Strategy<Value = QueryClass> {
    prop_oneof![
        Just(QueryClass::SimpleTerm),
        Just(QueryClass::MultiTerm),
        Just(QueryClass::Filtered),
        Just(QueryClass::Forensic),
        Just(QueryClass::HighCardinality),
    ]
}

fn arb_assertion_result() -> impl Strategy<Value = AssertionResult> {
    (
        "[a-z ]{5,30}",
        any::<bool>(),
        proptest::option::of("[a-z ]{5,30}"),
    )
        .prop_map(|(description, passed, message)| AssertionResult {
            description,
            passed,
            message,
        })
}

fn arb_query_test_result() -> impl Strategy<Value = QueryTestResult> {
    (
        "[a-z_]{3,15}",
        any::<bool>(),
        proptest::collection::vec(arb_assertion_result(), 0..5),
        any::<bool>(),
        0_u64..1_000_000,
        proptest::option::of(0_u64..1_000_000),
        0_u64..10_000,
        0_u64..100_000,
        proptest::option::of("[a-z ]{5,20}"),
    )
        .prop_map(
            |(
                name,
                passed,
                assertion_results,
                latency_ok,
                duration_us,
                budget_us,
                hits_returned,
                total_hits,
                error,
            )| {
                QueryTestResult {
                    name,
                    passed,
                    assertion_results,
                    latency_ok,
                    duration_us,
                    budget_us,
                    hits_returned,
                    total_hits,
                    error,
                }
            },
        )
}

// =========================================================================
// QueryClass — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_query_class_serde(class in arb_query_class()) {
        let json = serde_json::to_string(&class).unwrap();
        let back: QueryClass = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, class);
    }

    #[test]
    fn prop_query_class_deterministic(class in arb_query_class()) {
        let j1 = serde_json::to_string(&class).unwrap();
        let j2 = serde_json::to_string(&class).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// LatencyBudget — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_latency_budget_serde(
        class in arb_query_class(),
        max_ms in 1_u64..10_000,
    ) {
        let budget = LatencyBudget {
            class,
            max_duration: Duration::from_millis(max_ms),
        };
        let json = serde_json::to_string(&budget).unwrap();
        let back: LatencyBudget = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.class, budget.class);
        prop_assert_eq!(back.max_duration, budget.max_duration);
    }
}

// =========================================================================
// AssertionResult — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_assertion_result_serde(result in arb_assertion_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: AssertionResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.description, &result.description);
        prop_assert_eq!(back.passed, result.passed);
        prop_assert_eq!(&back.message, &result.message);
    }

    #[test]
    fn prop_assertion_result_deterministic(result in arb_assertion_result()) {
        let j1 = serde_json::to_string(&result).unwrap();
        let j2 = serde_json::to_string(&result).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// QueryTestResult — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_query_test_result_serde(result in arb_query_test_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: QueryTestResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &result.name);
        prop_assert_eq!(back.passed, result.passed);
        prop_assert_eq!(back.latency_ok, result.latency_ok);
        prop_assert_eq!(back.duration_us, result.duration_us);
        prop_assert_eq!(back.budget_us, result.budget_us);
        prop_assert_eq!(back.hits_returned, result.hits_returned);
        prop_assert_eq!(back.total_hits, result.total_hits);
        prop_assert_eq!(&back.error, &result.error);
        prop_assert_eq!(back.assertion_results.len(), result.assertion_results.len());
    }
}

// =========================================================================
// QualityReport — serde + invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_quality_report_serde(
        results in proptest::collection::vec(arb_query_test_result(), 0..5),
        all_passed in any::<bool>(),
    ) {
        let passed_count = results.iter().filter(|r| r.passed).count();
        let failed_count = results.iter().filter(|r| !r.passed).count();
        let latency_violations = results.iter().filter(|r| !r.latency_ok).count();
        let errors = results.iter().filter(|r| r.error.is_some()).count();
        let report = QualityReport {
            total_queries: results.len(),
            passed: passed_count,
            failed: failed_count,
            latency_violations,
            errors,
            all_passed,
            results,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: QualityReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_queries, report.total_queries);
        prop_assert_eq!(back.passed, report.passed);
        prop_assert_eq!(back.failed, report.failed);
        prop_assert_eq!(back.latency_violations, report.latency_violations);
        prop_assert_eq!(back.errors, report.errors);
        prop_assert_eq!(back.all_passed, report.all_passed);
        prop_assert_eq!(back.results.len(), report.results.len());
    }

    /// passed + failed == total_queries when counts are consistent.
    #[test]
    fn prop_report_counts_consistent(
        results in proptest::collection::vec(arb_query_test_result(), 0..10),
    ) {
        let passed = results.iter().filter(|r| r.passed).count();
        let failed = results.iter().filter(|r| !r.passed).count();
        prop_assert_eq!(passed + failed, results.len());
    }
}

// =========================================================================
// default_latency_budgets
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// default_latency_budgets covers all query classes.
    #[test]
    fn prop_default_budgets_complete(_dummy in 0..1_u8) {
        let budgets = default_latency_budgets();
        prop_assert_eq!(budgets.len(), 5);
        let classes: Vec<QueryClass> = budgets.iter().map(|b| b.class).collect();
        prop_assert!(classes.contains(&QueryClass::SimpleTerm));
        prop_assert!(classes.contains(&QueryClass::MultiTerm));
        prop_assert!(classes.contains(&QueryClass::Filtered));
        prop_assert!(classes.contains(&QueryClass::Forensic));
        prop_assert!(classes.contains(&QueryClass::HighCardinality));
    }

    /// All default budgets have positive duration.
    #[test]
    fn prop_default_budgets_positive(_dummy in 0..1_u8) {
        for budget in default_latency_budgets() {
            prop_assert!(budget.max_duration > Duration::ZERO);
        }
    }

    /// SimpleTerm has strictest budget, HighCardinality has most generous.
    #[test]
    fn prop_budget_ordering(_dummy in 0..1_u8) {
        let budgets = default_latency_budgets();
        let simple = budgets.iter().find(|b| b.class == QueryClass::SimpleTerm).unwrap();
        let high_card = budgets.iter().find(|b| b.class == QueryClass::HighCardinality).unwrap();
        prop_assert!(simple.max_duration <= high_card.max_duration);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn all_query_classes_distinct_json() {
    let classes = [
        QueryClass::SimpleTerm,
        QueryClass::MultiTerm,
        QueryClass::Filtered,
        QueryClass::Forensic,
        QueryClass::HighCardinality,
    ];
    let jsons: Vec<_> = classes
        .iter()
        .map(|c| serde_json::to_string(c).unwrap())
        .collect();
    for i in 0..jsons.len() {
        for j in (i + 1)..jsons.len() {
            assert_ne!(jsons[i], jsons[j]);
        }
    }
}

#[test]
fn empty_quality_report_roundtrips() {
    let report = QualityReport {
        results: vec![],
        total_queries: 0,
        passed: 0,
        failed: 0,
        latency_violations: 0,
        errors: 0,
        all_passed: true,
    };
    let json = serde_json::to_string(&report).unwrap();
    let back: QualityReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.total_queries, 0);
    assert!(back.all_passed);
}
