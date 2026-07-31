// Property-based tests for input_latency module (ft-1memj.25).
//
// Covers: serde roundtrips for InputLatencyStage, Percentile, InputLatencyMeasurement,
// InputLatencyCollector, StageBudget, InputLatencyBudget, BudgetCheckResult,
// BudgetCheckDetail, InputLatencyReport. Also tests behavioral invariants:
// percentile monotonicity, collector capacity, budget evaluation correctness.
#![allow(clippy::ignored_unit_patterns)]

use proptest::prelude::*;

use frankenterm_core::input_latency::{
    BudgetCheckDetail, BudgetCheckResult, INPUT_LATENCY_REPORT_SCHEMA_VERSION,
    InputLatencyBudget, InputLatencyClockDomainId, InputLatencyCollector,
    InputLatencyCollectorError, InputLatencyEvidenceClass, InputLatencyEvidenceStatus,
    InputLatencyMeasurement, InputLatencyMeasurementError, InputLatencyProducerId,
    InputLatencyReport, InputLatencyStage, InputLatencyTimestamp, Percentile, StageBudget,
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

fn arb_timestamp() -> impl Strategy<Value = InputLatencyTimestamp> {
    (0..1_000_000u64, 1..10_000u64, 1..10_000u64).prop_map(
        |(timestamp_us, producer_id, clock_domain_id)| {
            InputLatencyTimestamp::new(
                timestamp_us,
                InputLatencyProducerId::new(producer_id).unwrap(),
                InputLatencyClockDomainId::new(clock_domain_id).unwrap(),
            )
        },
    )
}

fn proxy_timestamp(timestamp_us: u64) -> InputLatencyTimestamp {
    InputLatencyTimestamp::new(
        timestamp_us,
        InputLatencyProducerId::new(1).unwrap(),
        InputLatencyClockDomainId::new(1).unwrap(),
    )
}

fn complete_measurement(id: u64, start: u64, total: u64) -> InputLatencyMeasurement {
    let mut measurement = InputLatencyMeasurement::new(id);
    let final_index = (InputLatencyStage::ALL.len() - 1) as u64;
    for (index, &stage) in InputLatencyStage::ALL.iter().enumerate() {
        let offset = total.saturating_mul(index as u64) / final_index;
        measurement
            .record_stage(stage, proxy_timestamp(start.saturating_add(offset)))
            .unwrap();
    }
    measurement
}

fn arb_measurement() -> impl Strategy<Value = InputLatencyMeasurement> {
    (
        1..10_000u64,
        prop::collection::btree_map(arb_stage(), arb_timestamp(), 0..6),
    )
        .prop_map(|(id, stages)| InputLatencyMeasurement::from_stages(id, stages))
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
        prop::option::of(arb_stage()),
        arb_percentile(),
        100..100_000u64,
        0..200_000u64,
        any::<bool>(),
        prop::option::of(0.0f64..5.0),
        "[A-Z_]{5,20}",
    )
        .prop_map(|(stage, percentile, budget_us, measured_us, passed, ratio, reason_code)| {
            BudgetCheckDetail {
                stage,
                percentile,
                budget_us,
                measured_us,
                passed,
                ratio,
                reason_code,
            }
        })
}

fn arb_budget_check_result() -> impl Strategy<Value = BudgetCheckResult> {
    (
        any::<bool>(),
        prop::collection::vec(arb_budget_check_detail(), 0..5),
        "[A-Z_]{5,20}",
    )
        .prop_map(|(passed, details, reason_code)| BudgetCheckResult {
            evidence_class: InputLatencyEvidenceClass::ProxyOnly,
            sample_count: 1,
            passed,
            details,
            evidence_error: None,
            budget_error: None,
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
                schema_version: INPUT_LATENCY_REPORT_SCHEMA_VERSION,
                evidence_class: InputLatencyEvidenceClass::ProxyOnly,
                evidence_status: InputLatencyEvidenceStatus::ValidProxy,
                evidence_error: None,
                sample_count,
                admitted_sample_count: sample_count,
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
        prop_assert_eq!(m.stages().len(), back.stages().len());
        for (k, v) in m.stages() {
            prop_assert_eq!(back.stages().get(k), Some(v));
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
        prop_assert_eq!(d.stage, back.stage);
        prop_assert_eq!(d.percentile, back.percentile);
        prop_assert_eq!(d.budget_us, back.budget_us);
        prop_assert_eq!(d.measured_us, back.measured_us);
        prop_assert_eq!(d.passed, back.passed);
        prop_assert_eq!(d.ratio, back.ratio);
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
        prop_assert!(m.total_latency_us().is_err());
    }

    #[test]
    fn measurement_total_latency_needs_all_stages(
        id in 0..10_000u64,
        ts in 100..1_000_000u64
    ) {
        let mut m = InputLatencyMeasurement::new(id);
        m.record_stage(InputLatencyStage::KeyEvent, proxy_timestamp(ts)).unwrap();
        let total = m.total_latency_us();
        prop_assert!(total.is_err());
    }

    #[test]
    fn measurement_total_latency_correct(
        id in 0..10_000u64,
        start in 100..500_000u64,
        delta in 1..500_000u64
    ) {
        let m = complete_measurement(id, start, delta);
        let total = m.total_latency_us();
        prop_assert_eq!(total, Ok(delta));
    }

    #[test]
    fn duplicate_stage_write_preserves_original_and_taints_measurement(
        id in 1..10_000u64,
        stage in arb_stage(),
        original_us in 0..1_000_000u64,
        replacement_us in 0..1_000_000u64,
    ) {
        let mut measurement = InputLatencyMeasurement::new(id);
        let original = proxy_timestamp(original_us);
        measurement.record_stage(stage, original).unwrap();

        let error = measurement
            .record_stage(stage, proxy_timestamp(replacement_us))
            .unwrap_err();

        prop_assert_eq!(measurement.stage_timestamp(stage), Some(original));
        prop_assert_eq!(
            error,
            InputLatencyMeasurementError::DuplicateStage { stage }
        );
        prop_assert!(matches!(
            measurement.validate_complete(),
            Err(InputLatencyMeasurementError::DuplicateStage { stage: failed_stage })
                if failed_stage == stage
        ));
    }

    #[test]
    fn one_unrelated_clock_domain_invalidates_complete_measurement(
        id in 1..10_000u64,
        mismatched_stage in arb_stage(),
        start in 0..500_000u64,
        step in 0..10_000u64,
    ) {
        let stages = InputLatencyStage::ALL
            .iter()
            .enumerate()
            .map(|(index, &stage)| {
                let clock_domain_id = if stage == mismatched_stage { 2 } else { 1 };
                (
                    stage,
                    InputLatencyTimestamp::new(
                        start.saturating_add(step.saturating_mul(index as u64)),
                        InputLatencyProducerId::new(index as u64 + 1).unwrap(),
                        InputLatencyClockDomainId::new(clock_domain_id).unwrap(),
                    ),
                )
            })
            .collect();
        let measurement = InputLatencyMeasurement::from_stages(id, stages);

        prop_assert!(matches!(
            measurement.validate_complete(),
            Err(InputLatencyMeasurementError::ClockDomainMismatch { .. })
        ));
    }

    #[test]
    fn one_regressing_stage_invalidates_complete_measurement(
        id in 1..10_000u64,
        regression_index in 1usize..InputLatencyStage::ALL.len(),
        start in 1..500_000u64,
        step in 1..10_000u64,
    ) {
        let stages = InputLatencyStage::ALL
            .iter()
            .enumerate()
            .map(|(index, &stage)| {
                let timestamp_us = if index == regression_index {
                    start
                        .saturating_add(step.saturating_mul((index - 1) as u64))
                        .saturating_sub(1)
                } else {
                    start.saturating_add(step.saturating_mul(index as u64))
                };
                (stage, proxy_timestamp(timestamp_us))
            })
            .collect();
        let measurement = InputLatencyMeasurement::from_stages(id, stages);

        prop_assert!(matches!(
            measurement.validate_complete(),
            Err(InputLatencyMeasurementError::TimestampRegression { .. })
        ));
    }

    #[test]
    fn terminal_measurement_id_boundary_is_sticky_fail_closed(capacity in 1..50usize) {
        let collector = InputLatencyCollector::new(capacity);
        let mut encoded = serde_json::to_value(collector).unwrap();
        encoded["next_id"] = serde_json::json!(u64::MAX);
        let mut exhausted: InputLatencyCollector = serde_json::from_value(encoded).unwrap();

        prop_assert!(matches!(
            exhausted.begin_measurement(),
            Err(InputLatencyCollectorError::MeasurementIdExhausted)
        ));
        let result = evaluate_budget(&exhausted, &InputLatencyBudget::default());
        prop_assert!(!result.passed);
        prop_assert_eq!(result.reason_code, "EVIDENCE_ID_EXHAUSTED");
        prop_assert!(matches!(
            exhausted.begin_measurement(),
            Err(InputLatencyCollectorError::MeasurementIdExhausted)
        ));
    }

    #[test]
    fn collector_respects_capacity(capacity in 1..50usize, count in 0..100usize) {
        let mut collector = InputLatencyCollector::new(capacity);
        for _ in 0..count {
            let id = collector.begin_measurement().unwrap().id;
            collector.record(complete_measurement(id, 100, 500));
        }
        prop_assert!(collector.count() <= capacity);
    }

    #[test]
    fn collector_clear_resets(count in 1..20usize) {
        let mut collector = InputLatencyCollector::new(100);
        for _ in 0..count {
            let id = collector.begin_measurement().unwrap().id;
            collector.record(complete_measurement(id, 100, 500));
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

    /// Nearest-rank never interpolates: the result must be an actual
    /// element of the input slice (true even with duplicate values).
    #[test]
    fn percentile_nearest_rank_result_is_member(
        vals in prop::collection::vec(0..1_000_000u64, 1..50),
        p in arb_percentile(),
    ) {
        let mut sorted = vals;
        sorted.sort_unstable();
        let result = percentile_nearest_rank(&sorted, p).unwrap();
        prop_assert!(
            sorted.contains(&result),
            "nearest-rank result {result} is not a member of the input slice"
        );
    }

    /// The selected value is bounded by the slice min and max for every
    /// percentile — a percentile can never escape the observed range.
    #[test]
    fn percentile_nearest_rank_within_bounds(
        vals in prop::collection::vec(0..1_000_000u64, 1..50),
        p in arb_percentile(),
    ) {
        let mut sorted = vals;
        sorted.sort_unstable();
        let min = *sorted.first().unwrap();
        let max = *sorted.last().unwrap();
        let result = percentile_nearest_rank(&sorted, p).unwrap();
        prop_assert!(min <= result, "result {result} below min {min}");
        prop_assert!(result <= max, "result {result} above max {max}");
    }

    /// A constant slice has the same value at every rank, so every
    /// percentile must return that constant regardless of length.
    #[test]
    fn percentile_nearest_rank_constant_slice(
        val in 0..1_000_000u64,
        len in 1..50usize,
        p in arb_percentile(),
    ) {
        let slice = vec![val; len];
        prop_assert_eq!(percentile_nearest_rank(&slice, p), Some(val));
    }

    /// Metamorphic relation: adding a constant offset to every element
    /// preserves ordering, so it must shift the selected percentile by
    /// exactly that offset (order-preserving translation commutes with
    /// nearest-rank selection). Offsets stay within u64 to avoid overflow.
    #[test]
    fn percentile_nearest_rank_commutes_with_translation(
        vals in prop::collection::vec(0..1_000_000u64, 1..50),
        offset in 0..1_000_000u64,
        p in arb_percentile(),
    ) {
        let mut sorted = vals;
        sorted.sort_unstable();
        let base = percentile_nearest_rank(&sorted, p).unwrap();
        let shifted: Vec<u64> = sorted.iter().map(|v| v + offset).collect();
        let shifted_result = percentile_nearest_rank(&shifted, p).unwrap();
        prop_assert_eq!(
            shifted_result,
            base + offset,
            "translation by {} must shift the percentile by the same amount",
            offset
        );
    }

    #[test]
    fn budget_default_has_aggregate_targets(_dummy in 0u8..1) {
        let budget = InputLatencyBudget::default();
        prop_assert!(!budget.aggregate.is_empty());
        prop_assert!((budget.regression_threshold - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn evaluate_budget_empty_collector_fails_closed(_dummy in 0u8..1) {
        let collector = InputLatencyCollector::new(100);
        let budget = InputLatencyBudget::default();
        let result = evaluate_budget(&collector, &budget);
        prop_assert!(!result.passed);
        prop_assert!(result.evidence_error.is_some());
        prop_assert!(result.details.is_empty());
    }

    #[test]
    fn generate_report_sample_count_matches(count in 0..20usize) {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..count {
            let id = collector.begin_measurement().unwrap().id;
            collector.record(complete_measurement(id, (i as u64) * 100, 500));
        }
        let report = generate_report(&collector, None);
        prop_assert_eq!(report.sample_count, count);
        prop_assert!(report.budget_check.is_none());
    }

    /// Metamorphic relation: loosening a budget (raising any budget_us)
    /// can never turn a passing check into a failing one, because
    /// `passed = measured <= floor(budget_us * threshold)` is monotonic
    /// non-decreasing in budget_us for a fixed measurement and threshold.
    /// Both per-percentile passes and the overall AND must be preserved.
    #[test]
    fn evaluate_budget_loosening_preserves_passes(
        count in 1..10usize,
        base_us in 100..20_000u64,
        delta in 0..20_000u64,
    ) {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..count {
            let id = collector.begin_measurement().unwrap().id;
            collector.record(complete_measurement(id, (i as u64) * 10, base_us));
        }
        let strict = InputLatencyBudget::default();
        let mut loose = strict.clone();
        for v in loose.aggregate.values_mut() {
            *v = v.saturating_add(delta);
        }
        let strict_result = evaluate_budget(&collector, &strict);
        let loose_result = evaluate_budget(&collector, &loose);

        // Per-percentile: a strict pass must remain a pass when loosened.
        for sd in &strict_result.details {
            if sd.passed {
                let ld = loose_result
                    .details
                    .iter()
                    .find(|d| d.percentile == sd.percentile)
                    .expect("loose result must cover the same percentiles");
                prop_assert!(
                    ld.passed,
                    "loosening percentile {:?} (budget +{}) broke a pass",
                    sd.percentile, delta
                );
            }
        }
        // Overall AND is likewise monotonic: strict-pass implies loose-pass.
        if strict_result.passed {
            prop_assert!(
                loose_result.passed,
                "loosening every budget by {} broke the overall pass", delta
            );
        }
    }

    #[test]
    fn generate_report_with_budget_includes_check(count in 1..10usize) {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..count {
            let id = collector.begin_measurement().unwrap().id;
            collector.record(complete_measurement(id, (i as u64) * 100, 500));
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
            let id = collector.begin_measurement().unwrap().id;
            collector.record(complete_measurement(id, 100, 500));
        }
        let json = serde_json::to_string(&collector).unwrap();
        let back: InputLatencyCollector = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(collector.count(), back.count());
    }
}
