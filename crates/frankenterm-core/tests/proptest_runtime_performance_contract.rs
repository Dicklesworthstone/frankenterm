//! Property tests for runtime_performance_contract module.

use proptest::prelude::*;

use frankenterm_core::runtime_performance_contract::*;

// =============================================================================
// Strategy helpers
// =============================================================================

fn arb_operation_category() -> impl Strategy<Value = OperationCategory> {
    prop_oneof![
        Just(OperationCategory::Cli),
        Just(OperationCategory::Robot),
        Just(OperationCategory::Watch),
        Just(OperationCategory::Search),
        Just(OperationCategory::Events),
        Just(OperationCategory::Session),
        Just(OperationCategory::Lifecycle),
    ]
}

fn arb_regression_verdict() -> impl Strategy<Value = RegressionVerdict> {
    prop_oneof![
        Just(RegressionVerdict::Pass),
        Just(RegressionVerdict::ConditionalPass),
        Just(RegressionVerdict::Fail),
    ]
}

fn arb_percentile_thresholds() -> impl Strategy<Value = PercentileThresholds> {
    (1.0..1000.0f64, 1.0..2000.0f64, 1.0..5000.0f64).prop_map(|(p50, p95_add, p99_add)| {
        PercentileThresholds::new(p50, p50 + p95_add, p50 + p95_add + p99_add)
    })
}

fn arb_operation_contract() -> impl Strategy<Value = OperationContract> {
    (
        "[a-z.]{3,15}",
        arb_operation_category(),
        arb_percentile_thresholds(),
        0.0..200.0f64,
        prop::option::of(100..5000u64),
        1.05..1.5f64,
        any::<bool>(),
    )
        .prop_map(
            |(id, cat, latency, throughput, startup, tolerance, critical)| OperationContract {
                operation_id: id,
                category: cat,
                description: "test op".into(),
                latency_target: latency,
                throughput_min_ops_sec: throughput,
                startup_max_ms: startup,
                regression_tolerance: tolerance,
                critical,
            },
        )
}

#[allow(dead_code)]
fn arb_operation_benchmark(op_id: String) -> impl Strategy<Value = OperationBenchmark> {
    (
        arb_percentile_thresholds(),
        0.0..500.0f64,
        100..10_000u64,
        prop::option::of(100..5000u64),
    )
        .prop_map(
            move |(latency, throughput, samples, startup)| OperationBenchmark {
                operation_id: op_id.clone(),
                latency,
                throughput_ops_sec: throughput,
                samples,
                startup_ms: startup,
                label: "test".into(),
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_operation_category(cat in arb_operation_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let restored: OperationCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, restored);
    }

    #[test]
    fn serde_roundtrip_regression_verdict(v in arb_regression_verdict()) {
        let json = serde_json::to_string(&v).unwrap();
        let restored: RegressionVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, restored);
    }

    #[test]
    fn serde_roundtrip_percentile_thresholds(pt in arb_percentile_thresholds()) {
        let json = serde_json::to_string(&pt).unwrap();
        let restored: PercentileThresholds = serde_json::from_str(&json).unwrap();
        prop_assert!((pt.p50_ms - restored.p50_ms).abs() < 1e-6);
        prop_assert!((pt.p95_ms - restored.p95_ms).abs() < 1e-6);
        prop_assert!((pt.p99_ms - restored.p99_ms).abs() < 1e-6);
    }

    #[test]
    fn serde_roundtrip_operation_contract(oc in arb_operation_contract()) {
        let json = serde_json::to_string(&oc).unwrap();
        let restored: OperationContract = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&oc.operation_id, &restored.operation_id);
        prop_assert_eq!(oc.category, restored.category);
        prop_assert_eq!(oc.critical, restored.critical);
    }
}

// =============================================================================
// OperationCategory properties
// =============================================================================

proptest! {
    #[test]
    fn category_label_not_empty(cat in arb_operation_category()) {
        prop_assert!(!cat.label().is_empty());
    }

    #[test]
    fn category_label_stable(cat in arb_operation_category()) {
        prop_assert_eq!(cat.label(), cat.label());
    }

    #[test]
    fn category_self_equality(cat in arb_operation_category()) {
        prop_assert_eq!(cat, cat);
    }
}

// =============================================================================
// PercentileThresholds properties
// =============================================================================

proptest! {
    #[test]
    fn satisfied_by_self(pt in arb_percentile_thresholds()) {
        prop_assert!(pt.satisfied_by(&pt));
    }

    #[test]
    fn satisfied_by_lower(
        target in arb_percentile_thresholds(),
        shrink in 0.01..0.99f64,
    ) {
        let measured = PercentileThresholds::new(
            target.p50_ms * shrink,
            target.p95_ms * shrink,
            target.p99_ms * shrink,
        );
        prop_assert!(target.satisfied_by(&measured));
    }

    #[test]
    fn not_satisfied_by_higher_p50(target in arb_percentile_thresholds(), extra in 0.01..1000.0f64) {
        let measured = PercentileThresholds::new(
            target.p50_ms + extra,
            target.p95_ms * 0.5,
            target.p99_ms * 0.5,
        );
        prop_assert!(!target.satisfied_by(&measured));
    }

    #[test]
    fn headroom_zero_at_target(pt in arb_percentile_thresholds()) {
        let headroom = pt.headroom(&pt);
        prop_assert!((headroom.p50_ms).abs() < 1e-10);
        prop_assert!((headroom.p95_ms).abs() < 1e-10);
        prop_assert!((headroom.p99_ms).abs() < 1e-10);
    }

    #[test]
    fn headroom_positive_when_under(
        target in arb_percentile_thresholds(),
        shrink in 0.01..0.99f64,
    ) {
        let measured = PercentileThresholds::new(
            target.p50_ms * shrink,
            target.p95_ms * shrink,
            target.p99_ms * shrink,
        );
        let headroom = target.headroom(&measured);
        prop_assert!(headroom.p50_ms > 0.0);
        prop_assert!(headroom.p95_ms > 0.0);
        prop_assert!(headroom.p99_ms > 0.0);
    }

    #[test]
    fn headroom_negative_when_over(
        target in arb_percentile_thresholds(),
        extra in 0.01..1000.0f64,
    ) {
        let measured = PercentileThresholds::new(
            target.p50_ms + extra,
            target.p95_ms + extra,
            target.p99_ms + extra,
        );
        let headroom = target.headroom(&measured);
        prop_assert!(headroom.p50_ms < 0.0);
        prop_assert!(headroom.p95_ms < 0.0);
        prop_assert!(headroom.p99_ms < 0.0);
    }
}

// =============================================================================
// evaluate_operation properties
// =============================================================================

proptest! {
    #[test]
    fn evaluate_passes_when_all_within_budget(
        cat in arb_operation_category(),
        shrink in 0.1..0.9f64,
    ) {
        let contract = OperationContract {
            operation_id: "test.op".into(),
            category: cat,
            description: "test".into(),
            latency_target: PercentileThresholds::new(100.0, 200.0, 400.0),
            throughput_min_ops_sec: 10.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        };

        let current = OperationBenchmark {
            operation_id: "test.op".into(),
            latency: PercentileThresholds::new(100.0 * shrink, 200.0 * shrink, 400.0 * shrink),
            throughput_ops_sec: 50.0,
            samples: 1000,
            startup_ms: None,
            label: "current".into(),
        };

        let result = evaluate_operation(&contract, &current, None);
        prop_assert!(result.passed);
        prop_assert!(result.latency_pass);
        prop_assert!(result.throughput_pass);
        prop_assert!(result.startup_pass);
        prop_assert!(result.regression_pass);
        prop_assert!(result.failure_reasons.is_empty());
    }

    #[test]
    fn evaluate_fails_on_latency_breach(
        extra in 1.0..500.0f64,
    ) {
        let contract = OperationContract {
            operation_id: "test.op".into(),
            category: OperationCategory::Cli,
            description: "test".into(),
            latency_target: PercentileThresholds::new(50.0, 100.0, 200.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        };

        let current = OperationBenchmark {
            operation_id: "test.op".into(),
            latency: PercentileThresholds::new(50.0 + extra, 100.0 + extra, 200.0 + extra),
            throughput_ops_sec: 0.0,
            samples: 1000,
            startup_ms: None,
            label: "current".into(),
        };

        let result = evaluate_operation(&contract, &current, None);
        prop_assert!(!result.passed);
        prop_assert!(!result.latency_pass);
    }

    #[test]
    fn evaluate_fails_on_throughput_breach(
        throughput in 0.0..9.9f64,
    ) {
        let contract = OperationContract {
            operation_id: "test.op".into(),
            category: OperationCategory::Robot,
            description: "test".into(),
            latency_target: PercentileThresholds::new(1000.0, 2000.0, 5000.0),
            throughput_min_ops_sec: 10.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        };

        let current = OperationBenchmark {
            operation_id: "test.op".into(),
            latency: PercentileThresholds::new(10.0, 20.0, 50.0),
            throughput_ops_sec: throughput,
            samples: 1000,
            startup_ms: None,
            label: "current".into(),
        };

        let result = evaluate_operation(&contract, &current, None);
        prop_assert!(!result.passed);
        prop_assert!(!result.throughput_pass);
    }

    #[test]
    fn evaluate_zero_throughput_min_always_passes(
        measured in 0.0..1000.0f64,
    ) {
        let contract = OperationContract {
            operation_id: "test.op".into(),
            category: OperationCategory::Cli,
            description: "test".into(),
            latency_target: PercentileThresholds::new(1000.0, 2000.0, 5000.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        };

        let current = OperationBenchmark {
            operation_id: "test.op".into(),
            latency: PercentileThresholds::new(10.0, 20.0, 50.0),
            throughput_ops_sec: measured,
            samples: 1000,
            startup_ms: None,
            label: "current".into(),
        };

        let result = evaluate_operation(&contract, &current, None);
        prop_assert!(result.throughput_pass);
    }

    #[test]
    fn evaluate_regression_detected(
        regression_factor in 1.11..3.0f64,
    ) {
        let contract = OperationContract {
            operation_id: "test.op".into(),
            category: OperationCategory::Robot,
            description: "test".into(),
            latency_target: PercentileThresholds::new(1000.0, 2000.0, 5000.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        };

        let baseline = OperationBenchmark {
            operation_id: "test.op".into(),
            latency: PercentileThresholds::new(50.0, 100.0, 200.0),
            throughput_ops_sec: 0.0,
            samples: 1000,
            startup_ms: None,
            label: "baseline".into(),
        };

        let current = OperationBenchmark {
            operation_id: "test.op".into(),
            latency: PercentileThresholds::new(
                50.0 * regression_factor,
                100.0 * regression_factor,
                200.0 * regression_factor,
            ),
            throughput_ops_sec: 0.0,
            samples: 1000,
            startup_ms: None,
            label: "current".into(),
        };

        let result = evaluate_operation(&contract, &current, Some(&baseline));
        prop_assert!(!result.regression_pass);
        prop_assert!(result.regression_ratio.is_some());
    }

    #[test]
    fn evaluate_no_baseline_regression_passes(
        cat in arb_operation_category(),
    ) {
        let contract = OperationContract {
            operation_id: "test.op".into(),
            category: cat,
            description: "test".into(),
            latency_target: PercentileThresholds::new(1000.0, 2000.0, 5000.0),
            throughput_min_ops_sec: 0.0,
            startup_max_ms: None,
            regression_tolerance: 1.10,
            critical: true,
        };

        let current = OperationBenchmark {
            operation_id: "test.op".into(),
            latency: PercentileThresholds::new(10.0, 20.0, 50.0),
            throughput_ops_sec: 0.0,
            samples: 1000,
            startup_ms: None,
            label: "current".into(),
        };

        let result = evaluate_operation(&contract, &current, None);
        prop_assert!(result.regression_pass);
        prop_assert!(result.regression_ratio.is_none());
    }
}

// =============================================================================
// RegressionReport properties
// =============================================================================

proptest! {
    #[test]
    fn report_all_pass_standard_contract(_dummy in Just(())) {
        let contract = RuntimePerformanceContract::standard();
        let mut suite = BenchmarkSuite::new("test");
        for op in &contract.operations {
            suite.add_current(OperationBenchmark {
                operation_id: op.operation_id.clone(),
                latency: PercentileThresholds::new(
                    op.latency_target.p50_ms * 0.5,
                    op.latency_target.p95_ms * 0.5,
                    op.latency_target.p99_ms * 0.5,
                ),
                throughput_ops_sec: op.throughput_min_ops_sec.max(1.0) * 2.0,
                samples: 1000,
                startup_ms: op.startup_max_ms.map(|m| m / 2),
                label: "current".into(),
            });
        }

        let report = RegressionReport::evaluate(&contract, &suite);
        prop_assert_eq!(report.verdict, RegressionVerdict::Pass);
        prop_assert_eq!(report.failed_operations, 0);
    }

    #[test]
    fn report_counts_consistent(_dummy in Just(())) {
        let contract = RuntimePerformanceContract::standard();
        let mut suite = BenchmarkSuite::new("test");
        for op in &contract.operations {
            suite.add_current(OperationBenchmark {
                operation_id: op.operation_id.clone(),
                latency: PercentileThresholds::new(
                    op.latency_target.p50_ms * 0.5,
                    op.latency_target.p95_ms * 0.5,
                    op.latency_target.p99_ms * 0.5,
                ),
                throughput_ops_sec: op.throughput_min_ops_sec.max(1.0) * 2.0,
                samples: 1000,
                startup_ms: None,
                label: "current".into(),
            });
        }

        let report = RegressionReport::evaluate(&contract, &suite);
        prop_assert_eq!(report.passed_operations + report.failed_operations, report.total_operations);
        prop_assert_eq!(report.critical_passed + report.critical_failed,
            contract.critical_operations().len());
    }

    #[test]
    fn report_missing_benchmarks_excluded(_dummy in Just(())) {
        let contract = RuntimePerformanceContract::standard();
        let suite = BenchmarkSuite::new("empty");
        let report = RegressionReport::evaluate(&contract, &suite);
        prop_assert_eq!(report.total_operations, 0);
    }
}

// =============================================================================
// Standard contract properties
// =============================================================================

proptest! {
    #[test]
    fn standard_contract_has_all_categories(_dummy in Just(())) {
        let contract = RuntimePerformanceContract::standard();
        let cats = contract.by_category();
        prop_assert!(cats.contains_key("cli"));
        prop_assert!(cats.contains_key("robot"));
        prop_assert!(cats.contains_key("watch"));
        prop_assert!(cats.contains_key("search"));
        prop_assert!(cats.contains_key("lifecycle"));
    }

    #[test]
    fn standard_contract_operations_unique(_dummy in Just(())) {
        let contract = RuntimePerformanceContract::standard();
        for (i, a) in contract.operations.iter().enumerate() {
            for (j, b) in contract.operations.iter().enumerate() {
                if i != j {
                    prop_assert_ne!(&a.operation_id, &b.operation_id);
                }
            }
        }
    }

    #[test]
    fn standard_contract_has_critical_ops(_dummy in Just(())) {
        let contract = RuntimePerformanceContract::standard();
        let critical = contract.critical_operations();
        prop_assert!(critical.len() >= 8);
    }

    #[test]
    fn standard_contract_positive_latency_targets(_dummy in Just(())) {
        let contract = RuntimePerformanceContract::standard();
        for op in &contract.operations {
            prop_assert!(op.latency_target.p50_ms > 0.0);
            prop_assert!(op.latency_target.p95_ms > 0.0);
            prop_assert!(op.latency_target.p99_ms > 0.0);
            prop_assert!(op.latency_target.p50_ms <= op.latency_target.p95_ms);
            prop_assert!(op.latency_target.p95_ms <= op.latency_target.p99_ms);
        }
    }

    #[test]
    fn standard_contract_regression_tolerance_above_one(_dummy in Just(())) {
        let contract = RuntimePerformanceContract::standard();
        for op in &contract.operations {
            prop_assert!(op.regression_tolerance > 1.0);
        }
    }
}

// =============================================================================
// Render summary
// =============================================================================

proptest! {
    #[test]
    fn render_summary_not_empty(_dummy in Just(())) {
        let contract = RuntimePerformanceContract::standard();
        let mut suite = BenchmarkSuite::new("test");
        for op in &contract.operations {
            suite.add_current(OperationBenchmark {
                operation_id: op.operation_id.clone(),
                latency: PercentileThresholds::new(
                    op.latency_target.p50_ms * 0.5,
                    op.latency_target.p95_ms * 0.5,
                    op.latency_target.p99_ms * 0.5,
                ),
                throughput_ops_sec: op.throughput_min_ops_sec.max(1.0) * 2.0,
                samples: 1000,
                startup_ms: None,
                label: "current".into(),
            });
        }

        let report = RegressionReport::evaluate(&contract, &suite);
        let summary = report.render_summary();
        prop_assert!(!summary.is_empty());
        prop_assert!(summary.contains("Regression Report"));
        prop_assert!(summary.contains("Pass"));
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn standard_contract_can_lookup_robot_get_text() {
    let contract = RuntimePerformanceContract::standard();
    let op = contract.get("robot.get-text");
    assert!(op.is_some());
    assert!(op.unwrap().critical);
}

#[test]
fn empty_contract_no_critical() {
    let contract = RuntimePerformanceContract::new("empty");
    assert!(contract.critical_operations().is_empty());
    assert!(contract.by_category().is_empty());
}

#[test]
fn benchmark_suite_lookup() {
    let mut suite = BenchmarkSuite::new("test");
    suite.add_baseline(OperationBenchmark {
        operation_id: "op1".into(),
        latency: PercentileThresholds::new(10.0, 20.0, 50.0),
        throughput_ops_sec: 100.0,
        samples: 500,
        startup_ms: None,
        label: "baseline".into(),
    });
    assert!(suite.baseline_for("op1").is_some());
    assert!(suite.baseline_for("op2").is_none());
}
