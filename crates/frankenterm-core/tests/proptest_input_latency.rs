// Property-based tests for input_latency module (ft-1memj.25).
//
// Covers: serde roundtrips for InputLatencyStage, Percentile, InputLatencyMeasurement,
// InputLatencyCollector, StageBudget, InputLatencyBudget, BudgetCheckResult,
// BudgetCheckDetail, InputLatencyReport. Also tests behavioral invariants:
// percentile monotonicity, collector capacity, budget evaluation correctness.
#![allow(clippy::ignored_unit_patterns)]

use proptest::prelude::*;

use frankenterm_core::input_latency::{
    BudgetCheckDetail, BudgetCheckResult, InputLatencyBudget, InputLatencyCollector,
    InputLatencyMeasurement, InputLatencyReport, InputLatencyStage, Percentile, StageBudget,
    evaluate_budget, generate_report, percentile_nearest_rank,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_stage() -> impl Strategy<Value = InputLatencyStage> {
    prop_oneof![
        Just(InputLatencyStage::KeyEvent),
        Just(InputLatencyStage::PtyWrite),
        Just(InputLatencyStage::PtyRead),
        Just(InputLatencyStage::TermUpdate),
        Just(InputLatencyStage::RenderSubmit),
        Just(InputLatencyStage::GpuPresent),
    ]
}

fn arb_percentile() -> impl Strategy<Value = Percentile> {
    prop_oneof![
        Just(Percentile::P50),
        Just(Percentile::P95),
        Just(Percentile::P99),
        Just(Percentile::P999),
    ]
}

fn arb_measurement() -> impl Strategy<Value = InputLatencyMeasurement> {
    (
        0..10_000u64,
        prop::collection::btree_map(arb_stage(), 0..1_000_000u64, 0..6),
    )
        .prop_map(|(id, stages)| InputLatencyMeasurement { id, stages })
}

fn arb_stage_budget() -> impl Strategy<Value = StageBudget> {
    (
        arb_stage(),
        prop::collection::btree_map(arb_percentile(), 100..100_000u64, 0..4),
    )
        .prop_map(|(stage, targets)| StageBudget { stage, targets })
}

fn arb_budget() -> impl Strategy<Value = InputLatencyBudget> {
    (
        prop::collection::vec(arb_stage_budget(), 0..4),
        prop::collection::btree_map(arb_percentile(), 500..50_000u64, 0..4),
        0.5f64..2.0,
    )
        .prop_map(
            |(stages, aggregate, regression_threshold)| InputLatencyBudget {
                stages,
                aggregate,
                regression_threshold,
            },
        )
}

fn arb_budget_check_detail() -> impl Strategy<Value = BudgetCheckDetail> {
    (
        arb_percentile(),
        100..100_000u64,
        0..200_000u64,
        any::<bool>(),
        0.0f64..5.0,
        "[A-Z_]{5,20}",
    )
        .prop_map(
            |(percentile, budget_us, measured_us, passed, ratio, reason_code)| BudgetCheckDetail {
                percentile,
                budget_us,
                measured_us,
                passed,
                ratio,
                reason_code,
            },
        )
}

fn arb_budget_check_result() -> impl Strategy<Value = BudgetCheckResult> {
    (
        any::<bool>(),
        prop::collection::vec(arb_budget_check_detail(), 0..5),
        "[A-Z_]{5,20}",
    )
        .prop_map(|(passed, details, reason_code)| BudgetCheckResult {
            passed,
            details,
            reason_code,
        })
}

fn arb_report() -> impl Strategy<Value = InputLatencyReport> {
    (
        0..500usize,
        prop::collection::btree_map(arb_percentile(), 100..100_000u64, 0..4),
        prop::collection::btree_map("[a-z_]{3,15}", 0..100_000u64, 0..6),
        prop::option::of(arb_budget_check_result()),
    )
        .prop_map(
            |(sample_count, percentiles, stage_breakdown_p50, budget_check)| InputLatencyReport {
                sample_count,
                percentiles,
                stage_breakdown_p50,
                budget_check,
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn input_latency_stage_serde_roundtrip(stage in arb_stage()) {
        let json = serde_json::to_string(&stage).unwrap();
        let back: InputLatencyStage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stage, back);
    }

    #[test]
    fn percentile_serde_roundtrip(p in arb_percentile()) {
        let json = serde_json::to_string(&p).unwrap();
        let back: Percentile = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(p, back);
    }

    #[test]
    fn measurement_serde_roundtrip(m in arb_measurement()) {
        let json = serde_json::to_string(&m).unwrap();
        let back: InputLatencyMeasurement = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(m.id, back.id);
        prop_assert_eq!(m.stages.len(), back.stages.len());
        for (k, v) in &m.stages {
            prop_assert_eq!(back.stages.get(k), Some(v));
        }
    }

    #[test]
    fn stage_budget_serde_roundtrip(sb in arb_stage_budget()) {
        let json = serde_json::to_string(&sb).unwrap();
        let back: StageBudget = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sb.stage, back.stage);
        prop_assert_eq!(sb.targets.len(), back.targets.len());
    }

    #[test]
    fn budget_serde_roundtrip(b in arb_budget()) {
        let json = serde_json::to_string(&b).unwrap();
        let back: InputLatencyBudget = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(b.stages.len(), back.stages.len());
        prop_assert_eq!(b.aggregate.len(), back.aggregate.len());
    }

    #[test]
    fn budget_check_detail_serde_roundtrip(d in arb_budget_check_detail()) {
        let json = serde_json::to_string(&d).unwrap();
        let back: BudgetCheckDetail = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d.percentile, back.percentile);
        prop_assert_eq!(d.budget_us, back.budget_us);
        prop_assert_eq!(d.measured_us, back.measured_us);
        prop_assert_eq!(d.passed, back.passed);
        prop_assert_eq!(d.reason_code, back.reason_code);
    }

    #[test]
    fn budget_check_result_serde_roundtrip(r in arb_budget_check_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: BudgetCheckResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r.passed, back.passed);
        prop_assert_eq!(r.details.len(), back.details.len());
        prop_assert_eq!(r.reason_code, back.reason_code);
    }

    #[test]
    fn report_serde_roundtrip(r in arb_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: InputLatencyReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r.sample_count, back.sample_count);
        prop_assert_eq!(r.percentiles.len(), back.percentiles.len());
        prop_assert_eq!(r.stage_breakdown_p50.len(), back.stage_breakdown_p50.len());
        let has_check = r.budget_check.is_some();
        let back_has = back.budget_check.is_some();
        prop_assert_eq!(has_check, back_has);
    }
}

// =============================================================================
// Behavioral invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn stage_all_covers_all_variants(_dummy in 0u8..1) {
        prop_assert_eq!(InputLatencyStage::ALL.len(), 6);
    }

    #[test]
    fn percentile_all_covers_all_variants(_dummy in 0u8..1) {
        prop_assert_eq!(Percentile::ALL.len(), 4);
    }

    #[test]
    fn percentile_fraction_monotonic(_dummy in 0u8..1) {
        let fracs: Vec<f64> = Percentile::ALL.iter().map(|p| p.fraction()).collect();
        for w in fracs.windows(2) {
            prop_assert!(w[0] <= w[1], "fractions must be monotonically increasing");
        }
    }

    #[test]
    fn stage_label_nonempty(stage in arb_stage()) {
        prop_assert!(!stage.label().is_empty());
    }

    #[test]
    fn percentile_display_nonempty(p in arb_percentile()) {
        let display = format!("{p}");
        prop_assert!(!display.is_empty());
        prop_assert!(display.starts_with('p'));
    }

    #[test]
    fn measurement_new_has_no_stages(id in 0..10_000u64) {
        let m = InputLatencyMeasurement::new(id);
        prop_assert_eq!(m.id, id);
        prop_assert_eq!(m.stage_count(), 0);
        prop_assert!(m.total_latency_us().is_none());
    }

    #[test]
    fn measurement_total_latency_needs_two_stages(
        id in 0..10_000u64,
        ts in 100..1_000_000u64
    ) {
        let mut m = InputLatencyMeasurement::new(id);
        m.record_stage(InputLatencyStage::KeyEvent, ts);
        // Single stage: total_latency should be None (can't compute range)
        let total = m.total_latency_us();
        // With one stage, first==last, so last > first is false → None
        prop_assert!(total.is_none());
    }

    #[test]
    fn measurement_total_latency_correct(
        id in 0..10_000u64,
        start in 100..500_000u64,
        delta in 1..500_000u64
    ) {
        let mut m = InputLatencyMeasurement::new(id);
        m.record_stage(InputLatencyStage::KeyEvent, start);
        m.record_stage(InputLatencyStage::GpuPresent, start + delta);
        let total = m.total_latency_us();
        prop_assert_eq!(total, Some(delta));
    }

    #[test]
    fn collector_respects_capacity(capacity in 1..50usize, count in 0..100usize) {
        let mut collector = InputLatencyCollector::new(capacity);
        for _ in 0..count {
            let m = collector.begin_measurement();
            collector.record(m);
        }
        prop_assert!(collector.count() <= capacity);
    }

    #[test]
    fn collector_clear_resets(count in 1..20usize) {
        let mut collector = InputLatencyCollector::new(100);
        for _ in 0..count {
            let m = collector.begin_measurement();
            collector.record(m);
        }
        prop_assert!(collector.count() > 0);
        collector.clear();
        prop_assert_eq!(collector.count(), 0);
    }

    #[test]
    fn percentile_nearest_rank_empty_returns_none(p in arb_percentile()) {
        let empty: Vec<u64> = vec![];
        prop_assert!(percentile_nearest_rank(&empty, p).is_none());
    }

    #[test]
    fn percentile_nearest_rank_single_value(val in 0..1_000_000u64, p in arb_percentile()) {
        let single = vec![val];
        prop_assert_eq!(percentile_nearest_rank(&single, p), Some(val));
    }

    #[test]
    fn percentile_nearest_rank_monotonic(vals in prop::collection::vec(0..1_000_000u64, 2..50)) {
        let mut sorted = vals;
        sorted.sort_unstable();
        // p50 <= p95 <= p99 <= p999
        let p50 = percentile_nearest_rank(&sorted, Percentile::P50).unwrap();
        let p95 = percentile_nearest_rank(&sorted, Percentile::P95).unwrap();
        let p99 = percentile_nearest_rank(&sorted, Percentile::P99).unwrap();
        let p999 = percentile_nearest_rank(&sorted, Percentile::P999).unwrap();
        prop_assert!(p50 <= p95, "p50 ({p50}) > p95 ({p95})");
        prop_assert!(p95 <= p99, "p95 ({p95}) > p99 ({p99})");
        prop_assert!(p99 <= p999, "p99 ({p99}) > p999 ({p999})");
    }

    #[test]
    fn budget_default_has_aggregate_targets(_dummy in 0u8..1) {
        let budget = InputLatencyBudget::default();
        prop_assert!(!budget.aggregate.is_empty());
        prop_assert!((budget.regression_threshold - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn evaluate_budget_empty_collector_passes(_dummy in 0u8..1) {
        let collector = InputLatencyCollector::new(100);
        let budget = InputLatencyBudget::default();
        let result = evaluate_budget(&collector, &budget);
        // Empty collector: all measured values are 0, which is <= budget
        prop_assert!(result.passed);
    }

    #[test]
    fn generate_report_sample_count_matches(count in 0..20usize) {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..count {
            let mut m = collector.begin_measurement();
            m.record_stage(InputLatencyStage::KeyEvent, (i as u64) * 100);
            m.record_stage(InputLatencyStage::GpuPresent, (i as u64) * 100 + 500);
            collector.record(m);
        }
        let report = generate_report(&collector, None);
        prop_assert_eq!(report.sample_count, count);
        prop_assert!(report.budget_check.is_none());
    }

    #[test]
    fn generate_report_with_budget_includes_check(count in 1..10usize) {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..count {
            let mut m = collector.begin_measurement();
            m.record_stage(InputLatencyStage::KeyEvent, (i as u64) * 100);
            m.record_stage(InputLatencyStage::GpuPresent, (i as u64) * 100 + 500);
            collector.record(m);
        }
        let budget = InputLatencyBudget::default();
        let report = generate_report(&collector, Some(&budget));
        prop_assert!(report.budget_check.is_some());
    }

    #[test]
    fn stage_display_matches_label(stage in arb_stage()) {
        let label = stage.label();
        let display = format!("{stage}");
        prop_assert_eq!(label, display.as_str());
    }

    #[test]
    fn collector_serde_roundtrip(capacity in 1..20usize, count in 0..15usize) {
        let mut collector = InputLatencyCollector::new(capacity);
        for _ in 0..count {
            let m = collector.begin_measurement();
            collector.record(m);
        }
        let json = serde_json::to_string(&collector).unwrap();
        let back: InputLatencyCollector = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(collector.count(), back.count());
    }
}
