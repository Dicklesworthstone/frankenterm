//! Property tests for slo_conformance module (ft-3681t.7.5).
//!
//! Covers serde roundtrips, SloComparison evaluation logic, SloSeverity ordering,
//! SloEvaluator registration/recording/evaluation invariants, error budget math,
//! TelemetryAuditReport consistency, AlertFidelityCheck precision/recall/F1 math,
//! SloAuditReport composition, and window-based sample filtering.

use frankenterm_core::slo_conformance::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_slo_comparison() -> impl Strategy<Value = SloComparison> {
    prop_oneof![
        Just(SloComparison::LessOrEqual),
        Just(SloComparison::GreaterOrEqual),
        Just(SloComparison::LessThan),
        Just(SloComparison::GreaterThan),
    ]
}

fn arb_slo_severity() -> impl Strategy<Value = SloSeverity> {
    prop_oneof![
        Just(SloSeverity::Info),
        Just(SloSeverity::Warning),
        Just(SloSeverity::Critical),
        Just(SloSeverity::Page),
    ]
}

fn arb_slo_metric() -> impl Strategy<Value = SloMetric> {
    prop_oneof![
        (1..100u8).prop_map(|p| SloMetric::LatencyMs { percentile: p }),
        Just(SloMetric::ErrorRate),
        Just(SloMetric::Availability),
        Just(SloMetric::Throughput),
        Just(SloMetric::QueueDepth),
        "[a-z]{1,10}".prop_map(|l| SloMetric::Custom { label: l }),
    ]
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_slo_comparison(cmp in arb_slo_comparison()) {
        let json = serde_json::to_string(&cmp).unwrap();
        let back: SloComparison = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cmp, back);
    }

    #[test]
    fn serde_roundtrip_slo_severity(sev in arb_slo_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let back: SloSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, back);
    }

    #[test]
    fn serde_roundtrip_slo_metric(metric in arb_slo_metric()) {
        let json = serde_json::to_string(&metric).unwrap();
        let back: SloMetric = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(metric, back);
    }

    #[test]
    fn serde_roundtrip_slo_definition(_dummy in 0..1u32) {
        let slo = SloDefinition::latency("test", "Test SLO", "sys", 99, 100.0, 60_000);
        let json = serde_json::to_string(&slo).unwrap();
        let back: SloDefinition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, "test");
        prop_assert_eq!(back.target, 100.0);
        prop_assert_eq!(back.window_ms, 60_000);
    }

    #[test]
    fn serde_roundtrip_metric_sample(
        value in 0.0..1000.0f64,
        ts in 0..1_000_000u64,
        good in any::<bool>(),
    ) {
        let sample = MetricSample {
            slo_id: "test".into(),
            value,
            timestamp_ms: ts,
            good,
        };
        let json = serde_json::to_string(&sample).unwrap();
        let back: MetricSample = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.slo_id, "test");
        prop_assert_eq!(back.good, good);
        prop_assert_eq!(back.timestamp_ms, ts);
    }

    #[test]
    fn serde_roundtrip_slo_evaluation(
        conforming in any::<bool>(),
        measured in 0.0..1.0f64,
    ) {
        let eval = SloEvaluation {
            slo_id: "test".into(),
            conforming,
            measured_value: measured,
            target_value: 0.99,
            budget_remaining: 0.5,
            sample_count: 100,
            good_count: 95,
            bad_count: 5,
            window_start_ms: 0,
            window_end_ms: 60_000,
            breach_severity: SloSeverity::Critical,
        };
        let json = serde_json::to_string(&eval).unwrap();
        let back: SloEvaluation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.conforming, conforming);
        prop_assert_eq!(back.sample_count, 100);
    }

    #[test]
    fn serde_roundtrip_telemetry_audit_check(_dummy in 0..1u32) {
        let check = TelemetryAuditCheck {
            id: "c1".into(),
            description: "check".into(),
            passed: true,
            measured: Some(1.0),
            expected: Some(1.0),
            message: "ok".into(),
            checked_at_ms: 1000,
        };
        let json = serde_json::to_string(&check).unwrap();
        let back: TelemetryAuditCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, "c1");
        prop_assert!(back.passed);
    }

    #[test]
    fn serde_roundtrip_alert_fidelity(
        tp in 0..100u64,
        fp in 0..50u64,
        fn_ in 0..50u64,
    ) {
        let check = AlertFidelityCheck::new("alert.test", tp, fp, fn_);
        let json = serde_json::to_string(&check).unwrap();
        let back: AlertFidelityCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.true_positives, tp);
        prop_assert_eq!(back.false_positives, fp);
        prop_assert_eq!(back.false_negatives, fn_);
    }
}

// =============================================================================
// SloComparison evaluation logic
// =============================================================================

proptest! {
    #[test]
    fn less_or_equal_correct(measured in -100.0..100.0f64, target in -100.0..100.0f64) {
        let result = SloComparison::LessOrEqual.evaluate(measured, target);
        prop_assert_eq!(result, measured <= target);
    }

    #[test]
    fn greater_or_equal_correct(measured in -100.0..100.0f64, target in -100.0..100.0f64) {
        let result = SloComparison::GreaterOrEqual.evaluate(measured, target);
        prop_assert_eq!(result, measured >= target);
    }

    #[test]
    fn less_than_correct(measured in -100.0..100.0f64, target in -100.0..100.0f64) {
        let result = SloComparison::LessThan.evaluate(measured, target);
        prop_assert_eq!(result, measured < target);
    }

    #[test]
    fn greater_than_correct(measured in -100.0..100.0f64, target in -100.0..100.0f64) {
        let result = SloComparison::GreaterThan.evaluate(measured, target);
        prop_assert_eq!(result, measured > target);
    }

    #[test]
    fn equal_values_satisfy_or_equal_variants(value in -100.0..100.0f64) {
        prop_assert!(SloComparison::LessOrEqual.evaluate(value, value));
        prop_assert!(SloComparison::GreaterOrEqual.evaluate(value, value));
        prop_assert!(!SloComparison::LessThan.evaluate(value, value));
        prop_assert!(!SloComparison::GreaterThan.evaluate(value, value));
    }
}

// =============================================================================
// SloSeverity ordering
// =============================================================================

proptest! {
    #[test]
    fn severity_total_order(a in arb_slo_severity(), b in arb_slo_severity()) {
        prop_assert!(a <= b || a > b);
    }

    #[test]
    fn info_is_minimum(sev in arb_slo_severity()) {
        prop_assert!(sev >= SloSeverity::Info);
    }

    #[test]
    fn page_is_maximum(sev in arb_slo_severity()) {
        prop_assert!(sev <= SloSeverity::Page);
    }
}

// =============================================================================
// SloEvaluator invariants
// =============================================================================

proptest! {
    #[test]
    fn evaluator_registration_increments_count(n in 1..10usize) {
        let mut eval = SloEvaluator::new(1000);
        for i in 0..n {
            eval.register(SloDefinition::error_rate(
                &format!("slo-{i}"), "Test", "sys", 0.05, 60_000,
            ));
        }
        prop_assert_eq!(eval.slo_count(), n);
    }

    #[test]
    fn evaluator_sample_count_bounded(
        max_samples in 5..50usize,
        n_records in 1..100usize,
    ) {
        let mut eval = SloEvaluator::new(max_samples);
        eval.register(SloDefinition::error_rate("test", "Test", "sys", 0.05, 60_000));
        for i in 0..n_records {
            eval.record(MetricSample {
                slo_id: "test".into(),
                value: 0.0,
                timestamp_ms: i as u64 * 100,
                good: true,
            });
        }
        prop_assert!(eval.sample_count("test") <= max_samples);
    }

    #[test]
    fn evaluator_unknown_slo_returns_none(_dummy in 0..1u32) {
        let eval = SloEvaluator::new(1000);
        prop_assert!(eval.evaluate("nonexistent", 1000).is_none());
    }

    #[test]
    fn evaluator_good_bad_count_sum_equals_sample_count(
        n_good in 1..50usize,
        n_bad in 0..20usize,
    ) {
        let mut eval = SloEvaluator::new(1000);
        eval.register(SloDefinition::error_rate("test", "Test", "sys", 0.5, 100_000));

        let total = n_good + n_bad;
        for i in 0..total {
            eval.record(MetricSample {
                slo_id: "test".into(),
                value: if i < n_bad { 1.0 } else { 0.0 },
                timestamp_ms: i as u64 * 100,
                good: i >= n_bad,
            });
        }

        let result = eval.evaluate("test", total as u64 * 100).unwrap();
        prop_assert_eq!(result.good_count + result.bad_count, result.sample_count);
    }

    #[test]
    fn evaluator_all_good_samples_conform(n in 1..50usize) {
        let mut eval = SloEvaluator::new(1000);
        eval.register(SloDefinition::error_rate("test", "Test", "sys", 0.05, 100_000));

        for i in 0..n {
            eval.record(MetricSample {
                slo_id: "test".into(),
                value: 0.0,
                timestamp_ms: i as u64 * 100,
                good: true,
            });
        }

        let result = eval.evaluate("test", n as u64 * 100).unwrap();
        prop_assert!(result.conforming);
        prop_assert_eq!(result.bad_count, 0);
    }

    #[test]
    fn evaluate_all_covers_all_registered_slos(n in 1..8usize) {
        let mut eval = SloEvaluator::new(1000);
        for i in 0..n {
            eval.register(SloDefinition::error_rate(
                &format!("slo-{i}"), "Test", "sys", 0.05, 60_000,
            ));
            eval.record(MetricSample {
                slo_id: format!("slo-{i}"),
                value: 0.0,
                timestamp_ms: 1000,
                good: true,
            });
        }
        let results = eval.evaluate_all(2000);
        prop_assert_eq!(results.len(), n);
    }
}

// =============================================================================
// SloEvaluation helper methods
// =============================================================================

proptest! {
    #[test]
    fn good_fraction_no_samples_is_one(_dummy in 0..1u32) {
        let eval = SloEvaluation {
            slo_id: "test".into(),
            conforming: true,
            measured_value: 0.0,
            target_value: 0.99,
            budget_remaining: 1.0,
            sample_count: 0,
            good_count: 0,
            bad_count: 0,
            window_start_ms: 0,
            window_end_ms: 1000,
            breach_severity: SloSeverity::Critical,
        };
        prop_assert!((eval.good_fraction() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn good_fraction_consistent(
        good in 0..100usize,
        bad in 0..100usize,
    ) {
        let total = good + bad;
        if total == 0 {
            return Ok(());
        }
        let eval = SloEvaluation {
            slo_id: "test".into(),
            conforming: true,
            measured_value: 0.0,
            target_value: 0.99,
            budget_remaining: 1.0,
            sample_count: total,
            good_count: good,
            bad_count: bad,
            window_start_ms: 0,
            window_end_ms: 1000,
            breach_severity: SloSeverity::Critical,
        };
        let expected = good as f64 / total as f64;
        prop_assert!((eval.good_fraction() - expected).abs() < 1e-10);
    }

    #[test]
    fn budget_exhausted_when_zero_or_negative(
        remaining in -1.0..0.0f64,
    ) {
        let eval = SloEvaluation {
            slo_id: "test".into(),
            conforming: false,
            measured_value: 0.1,
            target_value: 0.01,
            budget_remaining: remaining,
            sample_count: 100,
            good_count: 90,
            bad_count: 10,
            window_start_ms: 0,
            window_end_ms: 1000,
            breach_severity: SloSeverity::Critical,
        };
        prop_assert!(eval.budget_exhausted());
    }

    #[test]
    fn budget_not_exhausted_when_positive(
        remaining in 0.01..1.0f64,
    ) {
        let eval = SloEvaluation {
            slo_id: "test".into(),
            conforming: true,
            measured_value: 0.001,
            target_value: 0.01,
            budget_remaining: remaining,
            sample_count: 100,
            good_count: 99,
            bad_count: 1,
            window_start_ms: 0,
            window_end_ms: 1000,
            breach_severity: SloSeverity::Critical,
        };
        prop_assert!(!eval.budget_exhausted());
    }
}

// =============================================================================
// TelemetryAuditReport consistency
// =============================================================================

proptest! {
    #[test]
    fn audit_report_pass_fail_counts_sum(
        n_pass in 0..10usize,
        n_fail in 0..10usize,
    ) {
        let mut checks = Vec::new();
        for i in 0..n_pass {
            checks.push(TelemetryAuditCheck {
                id: format!("pass-{i}"),
                description: "ok".into(),
                passed: true,
                measured: None,
                expected: None,
                message: "ok".into(),
                checked_at_ms: 1000,
            });
        }
        for i in 0..n_fail {
            checks.push(TelemetryAuditCheck {
                id: format!("fail-{i}"),
                description: "bad".into(),
                passed: false,
                measured: None,
                expected: None,
                message: "bad".into(),
                checked_at_ms: 1000,
            });
        }
        let report = TelemetryAuditReport::from_checks(checks, 1000);
        prop_assert_eq!(report.pass_count, n_pass);
        prop_assert_eq!(report.fail_count, n_fail);
        prop_assert_eq!(report.pass_count + report.fail_count, report.checks.len());
        prop_assert_eq!(report.all_passed, n_fail == 0);
    }

    #[test]
    fn audit_report_failures_only_failing(
        n_pass in 1..5usize,
        n_fail in 1..5usize,
    ) {
        let mut checks = Vec::new();
        for i in 0..n_pass {
            checks.push(TelemetryAuditCheck {
                id: format!("pass-{i}"),
                description: "ok".into(),
                passed: true,
                measured: None,
                expected: None,
                message: "ok".into(),
                checked_at_ms: 1000,
            });
        }
        for i in 0..n_fail {
            checks.push(TelemetryAuditCheck {
                id: format!("fail-{i}"),
                description: "bad".into(),
                passed: false,
                measured: None,
                expected: None,
                message: "bad".into(),
                checked_at_ms: 1000,
            });
        }
        let report = TelemetryAuditReport::from_checks(checks, 1000);
        let failures = report.failures();
        prop_assert_eq!(failures.len(), n_fail);
        for f in &failures {
            prop_assert!(!f.passed);
        }
    }
}

// =============================================================================
// AlertFidelityCheck math
// =============================================================================

proptest! {
    #[test]
    fn precision_in_zero_one_range(
        tp in 0..100u64,
        fp in 0..100u64,
        fn_ in 0..100u64,
    ) {
        let check = AlertFidelityCheck::new("test", tp, fp, fn_);
        prop_assert!(check.precision >= 0.0 && check.precision <= 1.0);
    }

    #[test]
    fn recall_in_zero_one_range(
        tp in 0..100u64,
        fp in 0..100u64,
        fn_ in 0..100u64,
    ) {
        let check = AlertFidelityCheck::new("test", tp, fp, fn_);
        prop_assert!(check.recall >= 0.0 && check.recall <= 1.0);
    }

    #[test]
    fn f1_in_zero_one_range(
        tp in 0..100u64,
        fp in 0..100u64,
        fn_ in 0..100u64,
    ) {
        let check = AlertFidelityCheck::new("test", tp, fp, fn_);
        prop_assert!(check.f1_score() >= 0.0 && check.f1_score() <= 1.0);
    }

    #[test]
    fn alerts_fired_equals_tp_plus_fp(
        tp in 0..100u64,
        fp in 0..100u64,
    ) {
        let check = AlertFidelityCheck::new("test", tp, fp, 0);
        prop_assert_eq!(check.alerts_fired, tp + fp);
    }

    #[test]
    fn perfect_fidelity_has_f1_one(tp in 1..100u64) {
        let check = AlertFidelityCheck::new("test", tp, 0, 0);
        prop_assert!((check.precision - 1.0).abs() < f64::EPSILON);
        prop_assert!((check.recall - 1.0).abs() < f64::EPSILON);
        prop_assert!((check.f1_score() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn no_true_positives_zero_precision(fp in 1..100u64) {
        let check = AlertFidelityCheck::new("test", 0, fp, 0);
        prop_assert!((check.precision - 0.0).abs() < f64::EPSILON);
    }
}

// =============================================================================
// SloAuditReport composition
// =============================================================================

proptest! {
    #[test]
    fn audit_report_conforming_breached_sum(
        n_conform in 0..5usize,
        n_breach in 0..5usize,
    ) {
        let mut evals = Vec::new();
        for i in 0..n_conform {
            evals.push(SloEvaluation {
                slo_id: format!("good-{i}"),
                conforming: true,
                measured_value: 0.001,
                target_value: 0.01,
                budget_remaining: 0.9,
                sample_count: 100,
                good_count: 99,
                bad_count: 1,
                window_start_ms: 0,
                window_end_ms: 1000,
                breach_severity: SloSeverity::Critical,
            });
        }
        for i in 0..n_breach {
            evals.push(SloEvaluation {
                slo_id: format!("bad-{i}"),
                conforming: false,
                measured_value: 0.1,
                target_value: 0.01,
                budget_remaining: 0.0,
                sample_count: 100,
                good_count: 90,
                bad_count: 10,
                window_start_ms: 0,
                window_end_ms: 1000,
                breach_severity: SloSeverity::Critical,
            });
        }
        let telemetry = TelemetryAuditReport::from_checks(Vec::new(), 1000);
        let report = SloAuditReport::build(evals, telemetry, Vec::new(), 1000);
        prop_assert_eq!(report.slos_conforming, n_conform);
        prop_assert_eq!(report.slos_breached, n_breach);
        prop_assert_eq!(report.slos_conforming + report.slos_breached,
            report.slo_evaluations.len());
        prop_assert_eq!(report.overall_pass, n_breach == 0);
    }

    #[test]
    fn audit_report_breached_slos_list(n_breach in 1..5usize) {
        let mut evals = Vec::new();
        for i in 0..n_breach {
            evals.push(SloEvaluation {
                slo_id: format!("bad-{i}"),
                conforming: false,
                measured_value: 0.1,
                target_value: 0.01,
                budget_remaining: 0.0,
                sample_count: 100,
                good_count: 90,
                bad_count: 10,
                window_start_ms: 0,
                window_end_ms: 1000,
                breach_severity: SloSeverity::Critical,
            });
        }
        let telemetry = TelemetryAuditReport::from_checks(Vec::new(), 1000);
        let report = SloAuditReport::build(evals, telemetry, Vec::new(), 1000);
        prop_assert_eq!(report.breached_slos().len(), n_breach);
    }

    #[test]
    fn audit_report_render_summary_not_empty(_dummy in 0..1u32) {
        let evals = vec![SloEvaluation {
            slo_id: "test".into(),
            conforming: true,
            measured_value: 50.0,
            target_value: 100.0,
            budget_remaining: 0.95,
            sample_count: 1000,
            good_count: 999,
            bad_count: 1,
            window_start_ms: 0,
            window_end_ms: 60_000,
            breach_severity: SloSeverity::Critical,
        }];
        let telemetry = TelemetryAuditReport::from_checks(Vec::new(), 1000);
        let report = SloAuditReport::build(evals, telemetry, Vec::new(), 1000);
        let summary = report.render_summary();
        prop_assert!(!summary.is_empty());
        prop_assert!(summary.contains("PASS") || summary.contains("FAIL"));
    }
}

// =============================================================================
// SLO Definition factory invariants
// =============================================================================

#[test]
fn latency_slo_uses_less_or_equal() {
    let slo = SloDefinition::latency("test", "Test", "sys", 99, 100.0, 60_000);
    assert_eq!(slo.comparison, SloComparison::LessOrEqual);
    if let SloMetric::LatencyMs { percentile } = slo.metric {
        assert_eq!(percentile, 99);
    } else {
        panic!("expected LatencyMs");
    }
}

#[test]
fn error_rate_slo_uses_less_or_equal() {
    let slo = SloDefinition::error_rate("test", "Test", "sys", 0.05, 60_000);
    assert_eq!(slo.comparison, SloComparison::LessOrEqual);
    assert_eq!(slo.metric, SloMetric::ErrorRate);
}

#[test]
fn availability_slo_uses_greater_or_equal() {
    let slo = SloDefinition::availability("test", "Test", "sys", 0.999, 60_000);
    assert_eq!(slo.comparison, SloComparison::GreaterOrEqual);
    assert_eq!(slo.metric, SloMetric::Availability);
    assert!((slo.error_budget - 0.001).abs() < 1e-9);
}

#[test]
fn severity_ordering_correct() {
    assert!(SloSeverity::Info < SloSeverity::Warning);
    assert!(SloSeverity::Warning < SloSeverity::Critical);
    assert!(SloSeverity::Critical < SloSeverity::Page);
}
