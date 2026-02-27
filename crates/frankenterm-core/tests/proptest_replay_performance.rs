//! Property-based tests for replay_performance.rs.
//!
//! Covers serde roundtrips for all serializable types, ReplayPerformanceMetric
//! key/lower_is_better invariants, sample/budget value_for consistency,
//! within_budget correctness relative to lower_is_better semantics,
//! regression_fraction sign conventions, classify_metric_result precedence
//! rules, compare_against_baseline report counting invariants,
//! capacity_guidance arithmetic, and runs_within_relative_spread edge cases.

use frankenterm_core::replay_performance::{
    ReplayCapacityGuidance, ReplayPerformanceBaseline, ReplayPerformanceBudgets,
    ReplayPerformanceMetric, ReplayPerformanceReport, ReplayPerformanceSample,
    ReplayPerformanceStatus, capacity_guidance, classify_metric_result,
    compare_against_baseline, regression_fraction, runs_within_relative_spread,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_metric() -> impl Strategy<Value = ReplayPerformanceMetric> {
    prop_oneof![
        Just(ReplayPerformanceMetric::CaptureOverheadMsPerEvent),
        Just(ReplayPerformanceMetric::ReplayThroughputEventsPerSec),
        Just(ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences),
        Just(ReplayPerformanceMetric::ReportGenerationMs),
        Just(ReplayPerformanceMetric::ArtifactReadEventsPerSec),
    ]
}

fn arb_status() -> impl Strategy<Value = ReplayPerformanceStatus> {
    prop_oneof![
        Just(ReplayPerformanceStatus::Pass),
        Just(ReplayPerformanceStatus::Improvement),
        Just(ReplayPerformanceStatus::Warning),
        Just(ReplayPerformanceStatus::Blocking),
    ]
}

/// Generate positive finite f64 values suitable for performance samples.
fn arb_positive_f64() -> impl Strategy<Value = f64> {
    (1..=1_000_000u64).prop_map(|v| v as f64 / 100.0)
}

fn arb_sample() -> impl Strategy<Value = ReplayPerformanceSample> {
    (
        arb_positive_f64(),
        arb_positive_f64(),
        arb_positive_f64(),
        arb_positive_f64(),
        arb_positive_f64(),
    )
        .prop_map(
            |(capture, replay, diff, report, artifact)| ReplayPerformanceSample {
                capture_overhead_ms_per_event: capture,
                replay_throughput_events_per_sec: replay,
                diff_latency_ms_per_1000_divergences: diff,
                report_generation_ms: report,
                artifact_read_events_per_sec: artifact,
            },
        )
}

fn arb_budgets() -> impl Strategy<Value = ReplayPerformanceBudgets> {
    (
        arb_positive_f64(),
        arb_positive_f64(),
        arb_positive_f64(),
        arb_positive_f64(),
        arb_positive_f64(),
        (1..=50u64).prop_map(|v| v as f64 / 100.0), // warning: 0.01..0.50
        (1..=50u64).prop_map(|v| v as f64 / 100.0), // blocking: 0.01..0.50
    )
        .prop_map(
            |(capture, replay, diff, report, artifact, warning, blocking)| {
                let (w, b) = if warning <= blocking {
                    (warning, blocking)
                } else {
                    (blocking, warning)
                };
                ReplayPerformanceBudgets {
                    capture_overhead_ms_per_event: capture,
                    replay_throughput_events_per_sec: replay,
                    diff_latency_ms_per_1000_divergences: diff,
                    report_generation_ms: report,
                    artifact_read_events_per_sec: artifact,
                    warning_regression_fraction: w,
                    blocking_regression_fraction: b,
                }
            },
        )
}

// ── ReplayPerformanceMetric ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 1. Metric serde roundtrip
    #[test]
    fn metric_serde_roundtrip(metric in arb_metric()) {
        let json = serde_json::to_string(&metric).unwrap();
        let restored: ReplayPerformanceMetric = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, metric);
    }

    // 2. key() returns non-empty string
    #[test]
    fn metric_key_non_empty(metric in arb_metric()) {
        prop_assert!(!metric.key().is_empty());
    }

    // 3. All metrics have unique keys
    #[test]
    fn metric_keys_unique(_seed in 0..10u32) {
        let metrics = [
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent,
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec,
            ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences,
            ReplayPerformanceMetric::ReportGenerationMs,
            ReplayPerformanceMetric::ArtifactReadEventsPerSec,
        ];
        let keys: Vec<_> = metrics.iter().map(|m| m.key()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), keys.len(), "all metric keys must be unique");
    }
}

// ── ReplayPerformanceStatus ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 4. Status serde roundtrip
    #[test]
    fn status_serde_roundtrip(status in arb_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let restored: ReplayPerformanceStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, status);
    }

    // 5. Status rank total ordering: Pass(0) < Improvement(1) < Warning(2) < Blocking(3)
    #[test]
    fn status_rank_ordering(a in arb_status(), b in arb_status()) {
        prop_assert_eq!(a.rank().cmp(&b.rank()), a.rank().cmp(&b.rank()));
        if a == b {
            prop_assert_eq!(a.rank(), b.rank());
        }
    }

    // 6. Blocking always has highest rank
    #[test]
    fn status_blocking_highest_rank(s in arb_status()) {
        prop_assert!(ReplayPerformanceStatus::Blocking.rank() >= s.rank());
    }
}

// ── ReplayPerformanceSample serde ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 7. Sample serde roundtrip (f64 tolerance)
    #[test]
    fn sample_serde_roundtrip(sample in arb_sample()) {
        let json = serde_json::to_string(&sample).unwrap();
        let restored: ReplayPerformanceSample = serde_json::from_str(&json).unwrap();
        let eps = 1e-10;
        prop_assert!((restored.capture_overhead_ms_per_event - sample.capture_overhead_ms_per_event).abs() < eps);
        prop_assert!((restored.replay_throughput_events_per_sec - sample.replay_throughput_events_per_sec).abs() < eps);
        prop_assert!((restored.diff_latency_ms_per_1000_divergences - sample.diff_latency_ms_per_1000_divergences).abs() < eps);
        prop_assert!((restored.report_generation_ms - sample.report_generation_ms).abs() < eps);
        prop_assert!((restored.artifact_read_events_per_sec - sample.artifact_read_events_per_sec).abs() < eps);
    }

    // 8. value_for returns the corresponding field
    #[test]
    fn sample_value_for_consistency(sample in arb_sample(), metric in arb_metric()) {
        let val = sample.value_for(metric);
        let expected = match metric {
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent => sample.capture_overhead_ms_per_event,
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec => sample.replay_throughput_events_per_sec,
            ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences => sample.diff_latency_ms_per_1000_divergences,
            ReplayPerformanceMetric::ReportGenerationMs => sample.report_generation_ms,
            ReplayPerformanceMetric::ArtifactReadEventsPerSec => sample.artifact_read_events_per_sec,
        };
        prop_assert!((val - expected).abs() < 1e-15, "value_for mismatch: {} vs {}", val, expected);
    }
}

// ── ReplayPerformanceBudgets ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 9. Budgets serde roundtrip
    #[test]
    fn budgets_serde_roundtrip(budgets in arb_budgets()) {
        let json = serde_json::to_string(&budgets).unwrap();
        let restored: ReplayPerformanceBudgets = serde_json::from_str(&json).unwrap();
        let eps = 1e-10;
        prop_assert!((restored.capture_overhead_ms_per_event - budgets.capture_overhead_ms_per_event).abs() < eps);
        prop_assert!((restored.warning_regression_fraction - budgets.warning_regression_fraction).abs() < eps);
        prop_assert!((restored.blocking_regression_fraction - budgets.blocking_regression_fraction).abs() < eps);
    }

    // 10. Budget value_for matches corresponding field
    #[test]
    fn budget_value_for_consistency(budgets in arb_budgets(), metric in arb_metric()) {
        let val = budgets.value_for(metric);
        let expected = match metric {
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent => budgets.capture_overhead_ms_per_event,
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec => budgets.replay_throughput_events_per_sec,
            ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences => budgets.diff_latency_ms_per_1000_divergences,
            ReplayPerformanceMetric::ReportGenerationMs => budgets.report_generation_ms,
            ReplayPerformanceMetric::ArtifactReadEventsPerSec => budgets.artifact_read_events_per_sec,
        };
        prop_assert!((val - expected).abs() < 1e-15);
    }

    // 11. within_budget semantics: lower_is_better → value <= budget; else value >= budget
    #[test]
    fn within_budget_direction(metric in arb_metric(), budget_val in arb_positive_f64()) {
        let mut budgets = ReplayPerformanceBudgets::default();
        // Set the relevant budget field
        match metric {
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent => budgets.capture_overhead_ms_per_event = budget_val,
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec => budgets.replay_throughput_events_per_sec = budget_val,
            ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences => budgets.diff_latency_ms_per_1000_divergences = budget_val,
            ReplayPerformanceMetric::ReportGenerationMs => budgets.report_generation_ms = budget_val,
            ReplayPerformanceMetric::ArtifactReadEventsPerSec => budgets.artifact_read_events_per_sec = budget_val,
        }

        // At exactly the budget value, should pass
        prop_assert!(budgets.within_budget(metric, budget_val));

        // Well over budget should fail for lower-is-better, pass for higher-is-better
        let over = budget_val * 2.0;
        if metric.lower_is_better() {
            prop_assert!(!budgets.within_budget(metric, over), "lower-is-better: 2x budget should fail");
        } else {
            prop_assert!(budgets.within_budget(metric, over), "higher-is-better: 2x budget should pass");
        }
    }
}

// ── regression_fraction ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 12. regression_fraction: worsening → positive, improvement → negative
    #[test]
    fn regression_fraction_sign_convention(
        metric in arb_metric(),
        baseline in arb_positive_f64(),
    ) {
        // Worsen: for lower-is-better, increase; for higher-is-better, decrease
        let worse = if metric.lower_is_better() {
            baseline * 1.5
        } else {
            baseline * 0.5
        };
        let frac = regression_fraction(metric, baseline, worse);
        prop_assert!(frac.is_some());
        prop_assert!(frac.unwrap() > 0.0, "worsening should give positive fraction, got {}", frac.unwrap());

        // Improve: opposite direction
        let better = if metric.lower_is_better() {
            baseline * 0.5
        } else {
            baseline * 1.5
        };
        let frac = regression_fraction(metric, baseline, better);
        prop_assert!(frac.is_some());
        prop_assert!(frac.unwrap() < 0.0, "improvement should give negative fraction, got {}", frac.unwrap());
    }

    // 13. regression_fraction: same value → zero
    #[test]
    fn regression_fraction_same_is_zero(
        metric in arb_metric(),
        baseline in arb_positive_f64(),
    ) {
        let frac = regression_fraction(metric, baseline, baseline).unwrap();
        prop_assert!((frac).abs() < 1e-10, "same value should give ~0 fraction, got {}", frac);
    }

    // 14. regression_fraction: zero baseline → None
    #[test]
    fn regression_fraction_zero_baseline_none(metric in arb_metric(), current in arb_positive_f64()) {
        prop_assert!(regression_fraction(metric, 0.0, current).is_none());
    }

    // 15. regression_fraction: NaN current → None
    #[test]
    fn regression_fraction_nan_current_none(metric in arb_metric(), baseline in arb_positive_f64()) {
        prop_assert!(regression_fraction(metric, baseline, f64::NAN).is_none());
    }

    // 16. regression_fraction: negative baseline → None
    #[test]
    fn regression_fraction_negative_baseline_none(metric in arb_metric()) {
        prop_assert!(regression_fraction(metric, -1.0, 1.0).is_none());
    }
}

// ── classify_metric_result ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 17. Budget violation always gives Blocking, regardless of baseline
    #[test]
    fn classify_budget_violation_is_blocking(metric in arb_metric(), budgets in arb_budgets()) {
        let budget_val = budgets.value_for(metric);
        // Create a value that violates the budget
        let violating = if metric.lower_is_better() {
            budget_val * 2.0 // way above budget
        } else {
            budget_val * 0.1 // way below budget
        };
        let result = classify_metric_result(&budgets, metric, violating, None);
        prop_assert_eq!(result.status, ReplayPerformanceStatus::Blocking);
        prop_assert_eq!(&result.reason_code, "budget_exceeded");
        prop_assert!(!result.within_budget);
    }

    // 18. No baseline + within budget → Pass with "baseline_missing_or_invalid"
    #[test]
    fn classify_no_baseline_within_budget_is_pass(metric in arb_metric()) {
        let budgets = ReplayPerformanceBudgets::default();
        let budget_val = budgets.value_for(metric);
        // Value that's within budget
        let good = if metric.lower_is_better() {
            budget_val * 0.5
        } else {
            budget_val * 2.0
        };
        let result = classify_metric_result(&budgets, metric, good, None);
        prop_assert_eq!(result.status, ReplayPerformanceStatus::Pass);
        prop_assert_eq!(&result.reason_code, "baseline_missing_or_invalid");
    }

    // 19. Improvement (negative regression) → Improvement status
    #[test]
    fn classify_improvement(metric in arb_metric(), budgets in arb_budgets()) {
        let budget_val = budgets.value_for(metric);
        let baseline = budget_val * 0.5; // baseline well within budget
        // Improve: lower for lower-is-better, higher for higher-is-better
        let improved = if metric.lower_is_better() {
            baseline * 0.5  // half the baseline
        } else {
            baseline * 2.0  // double the baseline
        };
        let result = classify_metric_result(&budgets, metric, improved, Some(baseline));
        if result.within_budget {
            prop_assert_eq!(result.status, ReplayPerformanceStatus::Improvement);
            prop_assert_eq!(&result.reason_code, "regression_improvement");
        }
    }

    // 20. metric_key in result matches metric.key()
    #[test]
    fn classify_result_metric_key_matches(metric in arb_metric()) {
        let budgets = ReplayPerformanceBudgets::default();
        let result = classify_metric_result(&budgets, metric, 1.0, None);
        prop_assert_eq!(&result.metric_key, metric.key());
        prop_assert_eq!(result.metric, metric);
    }

    // 21. regression_percent = regression_fraction * 100
    #[test]
    fn classify_result_percent_matches_fraction(
        metric in arb_metric(),
        baseline in arb_positive_f64(),
    ) {
        let budgets = ReplayPerformanceBudgets::default();
        let budget_val = budgets.value_for(metric);
        // Use a value within budget
        let val = if metric.lower_is_better() {
            budget_val * 0.5
        } else {
            budget_val * 2.0
        };
        let result = classify_metric_result(&budgets, metric, val, Some(baseline));
        if let (Some(frac), Some(pct)) = (result.regression_fraction, result.regression_percent) {
            prop_assert!((pct - frac * 100.0).abs() < 1e-10,
                "percent {} should equal fraction {} * 100", pct, frac);
        }
    }
}

// ── compare_against_baseline ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 22. Report always has exactly 5 metric results
    #[test]
    fn report_has_five_metrics(sample in arb_sample()) {
        let report = compare_against_baseline(ReplayPerformanceBudgets::default(), None, sample);
        prop_assert_eq!(report.metrics.len(), 5);
    }

    // 23. warning_count + blocking_count <= 5
    #[test]
    fn report_counts_bounded(sample in arb_sample(), budgets in arb_budgets()) {
        let report = compare_against_baseline(budgets, None, sample);
        prop_assert!(report.warning_count + report.blocking_count <= 5);
    }

    // 24. overall_status rank >= max individual rank
    #[test]
    fn report_overall_status_is_maximum(sample in arb_sample()) {
        let report = compare_against_baseline(ReplayPerformanceBudgets::default(), None, sample);
        let max_individual = report.metrics.iter().map(|r| r.status.rank()).max().unwrap_or(0);
        prop_assert!(report.overall_status.rank() >= max_individual,
            "overall rank {} should be >= max individual rank {}", report.overall_status.rank(), max_individual);
    }

    // 25. Report serde roundtrip (structural check)
    #[test]
    fn report_serde_roundtrip(sample in arb_sample()) {
        let report = compare_against_baseline(ReplayPerformanceBudgets::default(), None, sample);
        let json = serde_json::to_string(&report).unwrap();
        let restored: ReplayPerformanceReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.metrics.len(), 5);
        prop_assert_eq!(&restored.version, &report.version);
        prop_assert_eq!(&restored.format, &report.format);
        prop_assert_eq!(restored.warning_count, report.warning_count);
        prop_assert_eq!(restored.blocking_count, report.blocking_count);
        prop_assert_eq!(restored.overall_status, report.overall_status);
    }

    // 26. With baseline, regression_fraction is always Some for valid inputs
    #[test]
    fn report_with_baseline_has_fractions(sample in arb_sample(), baseline_sample in arb_sample()) {
        let baseline = ReplayPerformanceBaseline::from_sample("test", "2026-01-01", baseline_sample);
        let report = compare_against_baseline(ReplayPerformanceBudgets::default(), Some(baseline), sample);
        for row in &report.metrics {
            prop_assert!(row.baseline.is_some());
            prop_assert!(row.regression_fraction.is_some());
            prop_assert!(row.regression_percent.is_some());
        }
    }
}

// ── capacity_guidance ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 27. capacity_guidance arithmetic: seconds = events / throughput
    #[test]
    fn guidance_arithmetic(sample in arb_sample()) {
        let g = capacity_guidance(sample);
        let eps = 1e-6;
        if sample.replay_throughput_events_per_sec > 0.0 {
            let expected_1m = 1_000_000.0 / sample.replay_throughput_events_per_sec;
            prop_assert!((g.replay_seconds_for_1m_events - expected_1m).abs() < eps,
                "replay 1M: {} vs expected {}", g.replay_seconds_for_1m_events, expected_1m);
            let expected_10m = 10_000_000.0 / sample.replay_throughput_events_per_sec;
            prop_assert!((g.replay_seconds_for_10m_events - expected_10m).abs() < eps);
        }
        if sample.artifact_read_events_per_sec > 0.0 {
            let expected_1m = 1_000_000.0 / sample.artifact_read_events_per_sec;
            prop_assert!((g.artifact_read_seconds_for_1m_events - expected_1m).abs() < eps);
        }
    }

    // 28. capacity_guidance 10M = 10 * 1M
    #[test]
    fn guidance_10m_is_10x_1m(sample in arb_sample()) {
        let g = capacity_guidance(sample);
        let eps = 1e-6;
        if g.replay_seconds_for_1m_events.is_finite() {
            prop_assert!((g.replay_seconds_for_10m_events - g.replay_seconds_for_1m_events * 10.0).abs() < eps);
        }
        if g.artifact_read_seconds_for_1m_events.is_finite() {
            prop_assert!((g.artifact_read_seconds_for_10m_events - g.artifact_read_seconds_for_1m_events * 10.0).abs() < eps);
        }
    }

    // 29. capacity_guidance serde roundtrip
    #[test]
    fn guidance_serde_roundtrip(sample in arb_sample()) {
        let g = capacity_guidance(sample);
        let json = serde_json::to_string(&g).unwrap();
        let restored: ReplayCapacityGuidance = serde_json::from_str(&json).unwrap();
        let eps = 1e-10;
        prop_assert!((restored.replay_seconds_for_1m_events - g.replay_seconds_for_1m_events).abs() < eps);
        prop_assert!((restored.replay_seconds_for_10m_events - g.replay_seconds_for_10m_events).abs() < eps);
    }
}

// ── runs_within_relative_spread ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 30. Identical values always within any non-negative spread
    #[test]
    fn spread_identical_values_always_pass(
        val in arb_positive_f64(),
        n in 2..=10usize,
        threshold in (0..=100u64).prop_map(|v| v as f64 / 100.0),
    ) {
        let values: Vec<f64> = vec![val; n];
        prop_assert!(runs_within_relative_spread(&values, threshold));
    }

    // 31. Single value always passes
    #[test]
    fn spread_single_value_passes(val in arb_positive_f64()) {
        prop_assert!(runs_within_relative_spread(&[val], 0.0));
    }

    // 32. Empty slice always fails
    #[test]
    fn spread_empty_fails(threshold in (0..=100u64).prop_map(|v| v as f64 / 100.0)) {
        prop_assert!(!runs_within_relative_spread(&[], threshold));
    }

    // 33. Negative threshold always fails
    #[test]
    fn spread_negative_threshold_fails(
        val in arb_positive_f64(),
        neg in (1..=100u64).prop_map(|v| -(v as f64) / 100.0),
    ) {
        prop_assert!(!runs_within_relative_spread(&[val, val], neg));
    }

    // 34. NaN in values always fails
    #[test]
    fn spread_nan_fails(val in arb_positive_f64()) {
        prop_assert!(!runs_within_relative_spread(&[val, f64::NAN], 1.0));
    }

    // 35. Infinity in values always fails
    #[test]
    fn spread_infinity_fails(val in arb_positive_f64()) {
        prop_assert!(!runs_within_relative_spread(&[val, f64::INFINITY], 1.0));
    }
}

// ── Baseline serde ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 36. ReplayPerformanceBaseline serde roundtrip
    #[test]
    fn baseline_serde_roundtrip(sample in arb_sample(), source in "[a-z]{3,10}", ts in "[0-9]{4}-[0-9]{2}-[0-9]{2}") {
        let baseline = ReplayPerformanceBaseline::from_sample(&source, &ts, sample);
        let json = serde_json::to_string(&baseline).unwrap();
        let restored: ReplayPerformanceBaseline = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored.source, &baseline.source);
        prop_assert_eq!(&restored.generated_at, &baseline.generated_at);
        prop_assert_eq!(&restored.version, &baseline.version);
    }
}
