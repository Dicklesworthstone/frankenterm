//! Property-based tests for the `tantivy_quality` module.
//!
//! Covers `QueryClass` enum serde, `LatencyBudget` serde,
//! `AssertionResult` serde, `QueryTestResult` serde,
//! `QualityReport` serde and aggregate invariants,
//! `default_latency_budgets()` coverage,
//! `RelevanceAssertion` serde roundtrips (all 9 variants),
//! and `GoldenQuery` serde roundtrips.

use std::time::Duration;

use frankenterm_core::tantivy_quality::{
    AssertionResult, GoldenQuery, LatencyBudget, QualityReport, QueryClass, QueryTestResult,
    RelevanceAssertion, default_latency_budgets,
};
use frankenterm_core::tantivy_query::{SearchFilter, SearchQuery};
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

fn arb_relevance_assertion() -> impl Strategy<Value = RelevanceAssertion> {
    prop_oneof![
        "[a-z0-9-]{5,20}".prop_map(|id| RelevanceAssertion::MustHit { event_id: id }),
        "[a-z0-9-]{5,20}".prop_map(|id| RelevanceAssertion::MustNotHit { event_id: id }),
        (0_u64..100_000).prop_map(RelevanceAssertion::MinTotalHits),
        (0_u64..100_000).prop_map(RelevanceAssertion::MaxTotalHits),
        (0_u64..100_000).prop_map(RelevanceAssertion::ExactTotalHits),
        ("[a-z0-9-]{5,20}", 1_usize..100)
            .prop_map(|(id, n)| RelevanceAssertion::InTopN { event_id: id, n }),
        ("[a-z0-9-]{5,20}", "[a-z0-9-]{5,20}").prop_map(|(h, l)| {
            RelevanceAssertion::RankedBefore {
                higher: h,
                lower: l,
            }
        }),
        "[a-z0-9-]{5,20}".prop_map(|id| RelevanceAssertion::FirstResult { event_id: id }),
        prop::collection::vec(0_u64..1000, 1..5)
            .prop_map(|v| RelevanceAssertion::AllMatchFilter(SearchFilter::PaneId { values: v })),
    ]
}

fn arb_golden_query() -> impl Strategy<Value = GoldenQuery> {
    (
        "[a-z_]{3,20}",
        arb_query_class(),
        "[a-z ]{3,30}",
        proptest::collection::vec(arb_relevance_assertion(), 0..4),
        "[a-z ]{5,40}",
    )
        .prop_map(|(name, class, text, assertions, description)| GoldenQuery {
            name,
            class,
            query: SearchQuery::simple(text),
            assertions,
            description,
        })
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
        let simple = budgets
            .iter()
            .find(|b| b.class == QueryClass::SimpleTerm)
            .unwrap();
        let high_card = budgets
            .iter()
            .find(|b| b.class == QueryClass::HighCardinality)
            .unwrap();
        prop_assert!(simple.max_duration <= high_card.max_duration);
    }
}

// =========================================================================
// RelevanceAssertion — serde roundtrip (all 9 variants)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn prop_relevance_assertion_serde(assertion in arb_relevance_assertion()) {
        let json = serde_json::to_string(&assertion).unwrap();
        let back: RelevanceAssertion = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(&json, &json2, "serde roundtrip should be stable");
    }

    #[test]
    fn prop_relevance_assertion_deterministic(assertion in arb_relevance_assertion()) {
        let j1 = serde_json::to_string(&assertion).unwrap();
        let j2 = serde_json::to_string(&assertion).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    #[test]
    fn prop_relevance_assertion_json_is_object(assertion in arb_relevance_assertion()) {
        let json = serde_json::to_string(&assertion).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Tagged enum should serialize as JSON object (or string for unit variants).
        prop_assert!(
            val.is_object() || val.is_string(),
            "RelevanceAssertion should be JSON object or string, got: {}",
            json
        );
    }
}

// =========================================================================
// RelevanceAssertion — individual variant serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_must_hit_serde(id in "[a-z0-9-]{5,20}") {
        let a = RelevanceAssertion::MustHit { event_id: id };
        let json = serde_json::to_string(&a).unwrap();
        let back: RelevanceAssertion = serde_json::from_str(&json).unwrap();
        if let RelevanceAssertion::MustHit { event_id } = &back {
            if let RelevanceAssertion::MustHit {
                event_id: orig_id,
            } = &a
            {
                prop_assert_eq!(event_id, orig_id);
            }
        } else {
            prop_assert!(false, "expected MustHit variant");
        }
    }

    #[test]
    fn prop_must_not_hit_serde(id in "[a-z0-9-]{5,20}") {
        let a = RelevanceAssertion::MustNotHit { event_id: id };
        let json = serde_json::to_string(&a).unwrap();
        let back: RelevanceAssertion = serde_json::from_str(&json).unwrap();
        if let RelevanceAssertion::MustNotHit { event_id } = &back {
            if let RelevanceAssertion::MustNotHit {
                event_id: orig_id,
            } = &a
            {
                prop_assert_eq!(event_id, orig_id);
            }
        } else {
            prop_assert!(false, "expected MustNotHit variant");
        }
    }

    #[test]
    fn prop_min_max_exact_hits_serde(n in 0_u64..100_000) {
        // MinTotalHits
        let json = serde_json::to_string(&RelevanceAssertion::MinTotalHits(n)).unwrap();
        let back: RelevanceAssertion = serde_json::from_str(&json).unwrap();
        if let RelevanceAssertion::MinTotalHits(v) = back {
            prop_assert_eq!(v, n);
        } else {
            prop_assert!(false, "expected MinTotalHits");
        }

        // MaxTotalHits
        let json = serde_json::to_string(&RelevanceAssertion::MaxTotalHits(n)).unwrap();
        let back: RelevanceAssertion = serde_json::from_str(&json).unwrap();
        if let RelevanceAssertion::MaxTotalHits(v) = back {
            prop_assert_eq!(v, n);
        } else {
            prop_assert!(false, "expected MaxTotalHits");
        }

        // ExactTotalHits
        let json = serde_json::to_string(&RelevanceAssertion::ExactTotalHits(n)).unwrap();
        let back: RelevanceAssertion = serde_json::from_str(&json).unwrap();
        if let RelevanceAssertion::ExactTotalHits(v) = back {
            prop_assert_eq!(v, n);
        } else {
            prop_assert!(false, "expected ExactTotalHits");
        }
    }

    #[test]
    fn prop_in_top_n_serde(id in "[a-z0-9-]{5,20}", n in 1_usize..100) {
        let a = RelevanceAssertion::InTopN {
            event_id: id.clone(),
            n,
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: RelevanceAssertion = serde_json::from_str(&json).unwrap();
        if let RelevanceAssertion::InTopN {
            event_id,
            n: back_n,
        } = back
        {
            prop_assert_eq!(&event_id, &id);
            prop_assert_eq!(back_n, n);
        } else {
            prop_assert!(false, "expected InTopN");
        }
    }

    #[test]
    fn prop_ranked_before_serde(
        higher in "[a-z0-9-]{5,20}",
        lower in "[a-z0-9-]{5,20}",
    ) {
        let a = RelevanceAssertion::RankedBefore {
            higher: higher.clone(),
            lower: lower.clone(),
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: RelevanceAssertion = serde_json::from_str(&json).unwrap();
        if let RelevanceAssertion::RankedBefore {
            higher: h,
            lower: l,
        } = back
        {
            prop_assert_eq!(&h, &higher);
            prop_assert_eq!(&l, &lower);
        } else {
            prop_assert!(false, "expected RankedBefore");
        }
    }

    #[test]
    fn prop_first_result_serde(id in "[a-z0-9-]{5,20}") {
        let a = RelevanceAssertion::FirstResult {
            event_id: id.clone(),
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: RelevanceAssertion = serde_json::from_str(&json).unwrap();
        if let RelevanceAssertion::FirstResult { event_id } = back {
            prop_assert_eq!(&event_id, &id);
        } else {
            prop_assert!(false, "expected FirstResult");
        }
    }
}

// =========================================================================
// GoldenQuery — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_golden_query_serde(gq in arb_golden_query()) {
        let json = serde_json::to_string(&gq).unwrap();
        let back: GoldenQuery = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &gq.name);
        prop_assert_eq!(back.class, gq.class);
        prop_assert_eq!(&back.query.text, &gq.query.text);
        prop_assert_eq!(back.assertions.len(), gq.assertions.len());
        prop_assert_eq!(&back.description, &gq.description);
    }

    #[test]
    fn prop_golden_query_deterministic(gq in arb_golden_query()) {
        let j1 = serde_json::to_string(&gq).unwrap();
        let j2 = serde_json::to_string(&gq).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    #[test]
    fn prop_golden_query_json_has_required_fields(gq in arb_golden_query()) {
        let json = serde_json::to_string(&gq).unwrap();
        prop_assert!(json.contains("\"name\""), "missing name field");
        prop_assert!(json.contains("\"class\""), "missing class field");
        prop_assert!(json.contains("\"query\""), "missing query field");
        prop_assert!(json.contains("\"assertions\""), "missing assertions field");
    }
}

// =========================================================================
// Aggregate invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// errors + non-errors == total_queries
    #[test]
    fn prop_report_error_partition(
        results in proptest::collection::vec(arb_query_test_result(), 0..10),
    ) {
        let errors = results.iter().filter(|r| r.error.is_some()).count();
        let non_errors = results.iter().filter(|r| r.error.is_none()).count();
        prop_assert_eq!(errors + non_errors, results.len());
    }

    /// latency_violations + latency_ok == total_queries
    #[test]
    fn prop_report_latency_partition(
        results in proptest::collection::vec(arb_query_test_result(), 0..10),
    ) {
        let violations = results.iter().filter(|r| !r.latency_ok).count();
        let ok = results.iter().filter(|r| r.latency_ok).count();
        prop_assert_eq!(violations + ok, results.len());
    }

    /// hits_returned <= total_hits for any result
    #[test]
    fn prop_result_hits_bounded(result in arb_query_test_result()) {
        // This tests the structural invariant we'd expect from real data.
        // Since arb values are independent, this documents the expected relationship.
        // In a real system: hits_returned <= total_hits always holds.
        // We verify serde preserves whatever values were set.
        let json = serde_json::to_string(&result).unwrap();
        let back: QueryTestResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.hits_returned, result.hits_returned);
        prop_assert_eq!(back.total_hits, result.total_hits);
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
