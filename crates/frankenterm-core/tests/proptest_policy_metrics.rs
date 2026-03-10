//! Property tests for policy_metrics module types and behavioral invariants.

use proptest::prelude::*;
use serde_json;
use std::collections::BTreeMap;

use frankenterm_core::policy_metrics::{
    HealthIndicator, HealthStatus, MetricSample, MetricTimeSeries, MetricUnit,
    PolicyMetricsCollector, PolicyMetricsCounters, PolicyMetricsDashboard,
    PolicyMetricsThresholds, PolicySubsystemInput, SubsystemMetricSummary,
};

// =============================================================================
// Arbitrary strategies
// =============================================================================

fn arb_metric_unit() -> impl Strategy<Value = MetricUnit> {
    prop_oneof![
        Just(MetricUnit::Count),
        Just(MetricUnit::Milliseconds),
        Just(MetricUnit::Percentage),
        Just(MetricUnit::BytesPerSecond),
    ]
}

fn arb_health_status() -> impl Strategy<Value = HealthStatus> {
    prop_oneof![
        Just(HealthStatus::Healthy),
        Just(HealthStatus::Warning),
        Just(HealthStatus::Critical),
        Just(HealthStatus::Unknown),
    ]
}

fn arb_metric_sample() -> impl Strategy<Value = MetricSample> {
    (any::<u64>(), any::<u64>()).prop_map(|(timestamp_ms, value)| MetricSample {
        timestamp_ms,
        value,
    })
}

fn arb_health_indicator() -> impl Strategy<Value = HealthIndicator> {
    (
        "[a-z_]{1,20}",
        arb_health_status(),
        "[0-9]{1,5}%?",
        "[0-9]{1,5}%?",
        "[0-9]{1,5}%?",
        "[a-z ]{1,40}",
    )
        .prop_map(
            |(name, status, value, threshold_warning, threshold_critical, description)| {
                HealthIndicator {
                    name,
                    status,
                    value,
                    threshold_warning,
                    threshold_critical,
                    description,
                }
            },
        )
}

fn arb_subsystem_metric_summary() -> impl Strategy<Value = SubsystemMetricSummary> {
    (
        "[a-z_]{1,20}",
        arb_health_status(),
        any::<u64>(),
        any::<u64>(),
        0..=100u32,
        any::<u32>(),
        any::<u32>(),
    )
        .prop_map(
            |(subsystem, health, evaluations, denials, denial_rate_pct, active_quarantines, active_violations)| {
                SubsystemMetricSummary {
                    subsystem,
                    health,
                    evaluations,
                    denials,
                    denial_rate_pct,
                    active_quarantines,
                    active_violations,
                }
            },
        )
}

fn arb_policy_metrics_counters() -> impl Strategy<Value = PolicyMetricsCounters> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u32>(),
        any::<u32>(),
        any::<u64>(),
        any::<bool>(),
        any::<u64>(),
        any::<bool>(),
        any::<u64>(),
    )
        .prop_map(
            |(
                total_evaluations,
                total_denials,
                total_quarantines_active,
                total_violations_active,
                audit_chain_length,
                audit_chain_valid,
                forensic_records_count,
                kill_switch_active,
                snapshots_generated,
            )| {
                PolicyMetricsCounters {
                    total_evaluations,
                    total_denials,
                    total_quarantines_active,
                    total_violations_active,
                    audit_chain_length,
                    audit_chain_valid,
                    forensic_records_count,
                    kill_switch_active,
                    snapshots_generated,
                }
            },
        )
}

fn arb_policy_metrics_thresholds() -> impl Strategy<Value = PolicyMetricsThresholds> {
    (
        0..=100u32,
        0..=100u32,
        0..=1000u32,
        0..=1000u32,
        0..=1000u32,
        0..=1000u32,
    )
        .prop_map(
            |(dw, dc, qw, qc, vw, vc)| PolicyMetricsThresholds {
                denial_rate_warning_pct: dw,
                denial_rate_critical_pct: dc,
                quarantine_warning_count: qw,
                quarantine_critical_count: qc,
                violation_warning_count: vw,
                violation_critical_count: vc,
            },
        )
}

fn arb_policy_metrics_dashboard() -> impl Strategy<Value = PolicyMetricsDashboard> {
    (
        any::<u64>(),
        arb_health_status(),
        prop::collection::vec(arb_health_indicator(), 0..5),
        prop::collection::btree_map("[a-z_]{1,10}", arb_subsystem_metric_summary(), 0..4),
        arb_policy_metrics_counters(),
    )
        .prop_map(
            |(captured_at_ms, overall_health, indicators, subsystem_metrics, counters)| {
                PolicyMetricsDashboard {
                    captured_at_ms,
                    overall_health,
                    indicators,
                    subsystem_metrics,
                    counters,
                }
            },
        )
}

fn arb_subsystem_input() -> impl Strategy<Value = PolicySubsystemInput> {
    (any::<u64>(), any::<u64>(), any::<u32>(), any::<u32>()).prop_map(
        |(evaluations, denials, active_quarantines, active_violations)| PolicySubsystemInput {
            evaluations,
            denials,
            active_quarantines,
            active_violations,
        },
    )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn metric_sample_serde_roundtrip(sample in arb_metric_sample()) {
        let json = serde_json::to_string(&sample).unwrap();
        let back: MetricSample = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sample, back);
    }

    #[test]
    fn metric_unit_serde_roundtrip(unit in arb_metric_unit()) {
        let json = serde_json::to_string(&unit).unwrap();
        let back: MetricUnit = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(unit, back);
    }

    #[test]
    fn health_status_serde_roundtrip(status in arb_health_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: HealthStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back);
    }

    #[test]
    fn health_indicator_serde_roundtrip(ind in arb_health_indicator()) {
        let json = serde_json::to_string(&ind).unwrap();
        let back: HealthIndicator = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ind, back);
    }

    #[test]
    fn subsystem_metric_summary_serde_roundtrip(summary in arb_subsystem_metric_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let back: SubsystemMetricSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(summary, back);
    }

    #[test]
    fn policy_metrics_counters_serde_roundtrip(counters in arb_policy_metrics_counters()) {
        let json = serde_json::to_string(&counters).unwrap();
        let back: PolicyMetricsCounters = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(counters, back);
    }

    #[test]
    fn policy_metrics_thresholds_serde_roundtrip(t in arb_policy_metrics_thresholds()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: PolicyMetricsThresholds = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t.denial_rate_warning_pct, back.denial_rate_warning_pct);
        prop_assert_eq!(t.denial_rate_critical_pct, back.denial_rate_critical_pct);
        prop_assert_eq!(t.quarantine_warning_count, back.quarantine_warning_count);
        prop_assert_eq!(t.quarantine_critical_count, back.quarantine_critical_count);
        prop_assert_eq!(t.violation_warning_count, back.violation_warning_count);
        prop_assert_eq!(t.violation_critical_count, back.violation_critical_count);
    }

    #[test]
    fn policy_metrics_dashboard_serde_roundtrip(dash in arb_policy_metrics_dashboard()) {
        let json = serde_json::to_string(&dash).unwrap();
        let back: PolicyMetricsDashboard = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dash.captured_at_ms, back.captured_at_ms);
        prop_assert_eq!(dash.overall_health, back.overall_health);
        prop_assert_eq!(dash.counters, back.counters);
        prop_assert_eq!(dash.indicators.len(), back.indicators.len());
        prop_assert_eq!(dash.subsystem_metrics.len(), back.subsystem_metrics.len());
    }
}

// =============================================================================
// MetricTimeSeries behavioral invariants
// =============================================================================

proptest! {
    #[test]
    fn time_series_never_exceeds_max(
        max_samples in 1..50usize,
        pushes in prop::collection::vec((any::<u64>(), any::<u64>()), 0..100),
    ) {
        let mut ts = MetricTimeSeries::new("test", MetricUnit::Count, max_samples);
        for (ts_val, value) in &pushes {
            ts.push(*ts_val, *value);
        }
        prop_assert!(ts.len() <= max_samples);
    }

    #[test]
    fn time_series_len_matches_push_count_within_cap(
        max_samples in 1..50usize,
        pushes in prop::collection::vec((any::<u64>(), any::<u64>()), 0..100),
    ) {
        let mut ts = MetricTimeSeries::new("test", MetricUnit::Count, max_samples);
        for (ts_val, value) in &pushes {
            ts.push(*ts_val, *value);
        }
        let expected = pushes.len().min(max_samples);
        prop_assert_eq!(ts.len(), expected);
    }

    #[test]
    fn time_series_latest_is_last_pushed(
        max_samples in 1..50usize,
        pushes in prop::collection::vec((any::<u64>(), 0..1000u64), 1..50),
    ) {
        let mut ts = MetricTimeSeries::new("test", MetricUnit::Count, max_samples);
        let mut last_value = 0u64;
        for (ts_val, value) in &pushes {
            ts.push(*ts_val, *value);
            last_value = *value;
        }
        prop_assert_eq!(ts.latest().unwrap().value, last_value);
    }

    #[test]
    fn time_series_serde_roundtrip(
        max_samples in 1..20usize,
        pushes in prop::collection::vec((0..10000u64, 0..10000u64), 0..30),
    ) {
        let mut ts = MetricTimeSeries::new("test_series", MetricUnit::Milliseconds, max_samples);
        for (ts_val, value) in &pushes {
            ts.push(*ts_val, *value);
        }
        let json = serde_json::to_string(&ts).unwrap();
        let back: MetricTimeSeries = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ts.len(), back.len());
        // Verify latest matches
        if let Some(orig) = ts.latest() {
            let restored = back.latest().unwrap();
            prop_assert_eq!(orig.value, restored.value);
            prop_assert_eq!(orig.timestamp_ms, restored.timestamp_ms);
        }
    }

    #[test]
    fn time_series_min_leq_average_leq_max(
        values in prop::collection::vec(0..10000u64, 1..50),
    ) {
        let mut ts = MetricTimeSeries::new("test", MetricUnit::Count, 100);
        for (i, v) in values.iter().enumerate() {
            ts.push(i as u64 * 1000, *v);
        }
        let min = ts.min().unwrap();
        let avg = ts.average().unwrap();
        let max = ts.max().unwrap();
        prop_assert!(min <= avg, "min {} > avg {}", min, avg);
        prop_assert!(avg <= max, "avg {} > max {}", avg, max);
    }

    #[test]
    fn time_series_empty_stats_are_none(
        max_samples in 1..20usize,
    ) {
        let ts = MetricTimeSeries::new("test", MetricUnit::Count, max_samples);
        prop_assert!(ts.is_empty());
        prop_assert!(ts.average().is_none());
        prop_assert!(ts.min().is_none());
        prop_assert!(ts.max().is_none());
        prop_assert!(ts.latest().is_none());
    }

    #[test]
    fn time_series_range_subset_of_all(
        values in prop::collection::vec((0..10000u64, 0..10000u64), 1..30),
        start_ms in 0..5000u64,
        end_ms in 5000..10000u64,
    ) {
        let mut ts = MetricTimeSeries::new("test", MetricUnit::Count, 100);
        for (t, v) in &values {
            ts.push(*t, *v);
        }
        let in_range = ts.samples_in_range(start_ms, end_ms);
        // All returned samples must actually be within range
        for s in &in_range {
            prop_assert!(s.timestamp_ms >= start_ms);
            prop_assert!(s.timestamp_ms <= end_ms);
        }
        // Count must be <= total
        prop_assert!(in_range.len() <= ts.len());
    }

    #[test]
    fn time_series_max_samples_at_least_one(max_val in 0..5usize) {
        // The constructor clamps max_samples to at least 1
        let ts = MetricTimeSeries::new("test", MetricUnit::Count, max_val);
        // We can always push at least one
        let mut ts = ts;
        ts.push(1000, 42);
        prop_assert!(ts.len() >= 1);
    }
}

// =============================================================================
// HealthStatus ordering invariants
// =============================================================================

proptest! {
    #[test]
    fn health_status_total_order(a in arb_health_status(), b in arb_health_status()) {
        // Total ordering: exactly one of a < b, a == b, a > b
        let lt = a < b;
        let eq = a == b;
        let gt = a > b;
        let count = lt as u8 + eq as u8 + gt as u8;
        prop_assert_eq!(count, 1, "total order violated for {:?} vs {:?}", a, b);
    }

    #[test]
    fn health_status_display_nonempty(status in arb_health_status()) {
        let s = status.to_string();
        prop_assert!(!s.is_empty());
    }
}

// =============================================================================
// MetricUnit display invariants
// =============================================================================

proptest! {
    #[test]
    fn metric_unit_display_nonempty(unit in arb_metric_unit()) {
        let s = unit.to_string();
        prop_assert!(!s.is_empty());
    }

    #[test]
    fn metric_unit_serde_is_snake_case(unit in arb_metric_unit()) {
        let json = serde_json::to_string(&unit).unwrap();
        // Should be a JSON string like "count", "milliseconds", etc.
        let parsed: String = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "expected snake_case, got: {}", parsed);
    }
}

// =============================================================================
// PolicyMetricsCollector behavioral invariants
// =============================================================================

proptest! {
    #[test]
    fn dashboard_counters_sum_subsystem_inputs(
        inputs in prop::collection::vec(
            ("[a-z]{1,8}", 0..1000u64, 0..1000u64, 0..100u32, 0..100u32),
            1..6,
        ),
    ) {
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        let mut expected_evals = 0u64;
        let mut expected_denials = 0u64;
        let mut expected_quarantines = 0u32;
        let mut expected_violations = 0u32;

        for (name, evals, denials, quarantines, violations) in &inputs {
            collector.update_subsystem(name, PolicySubsystemInput {
                evaluations: *evals,
                denials: *denials,
                active_quarantines: *quarantines,
                active_violations: *violations,
            });
            // BTreeMap deduplicates by key, so track latest per name
        }

        // Re-sum from the collector's perspective (deduped by name)
        let mut seen: BTreeMap<String, (u64, u64, u32, u32)> = BTreeMap::new();
        for (name, evals, denials, quarantines, violations) in &inputs {
            seen.insert(name.clone(), (*evals, *denials, *quarantines, *violations));
        }
        for (e, d, q, v) in seen.values() {
            expected_evals += e;
            expected_denials += d;
            expected_quarantines += q;
            expected_violations += v;
        }

        let dash = collector.dashboard(1000);
        prop_assert_eq!(dash.counters.total_evaluations, expected_evals);
        prop_assert_eq!(dash.counters.total_denials, expected_denials);
        prop_assert_eq!(dash.counters.total_quarantines_active, expected_quarantines);
        prop_assert_eq!(dash.counters.total_violations_active, expected_violations);
    }

    #[test]
    fn dashboard_overall_health_is_worst_indicator(
        evals in 0..1000u64,
        denials in 0..1000u64,
        quarantines in 0..50u32,
        violations in 0..50u32,
        chain_valid in any::<bool>(),
        ks_active in any::<bool>(),
    ) {
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: evals,
            denials,
            active_quarantines: quarantines,
            active_violations: violations,
        });
        collector.update_audit_chain(100, chain_valid);
        collector.update_kill_switch(ks_active);

        let dash = collector.dashboard(1000);

        // overall_health should be the max (worst) of all indicator statuses
        let worst = dash.indicators.iter()
            .map(|i| i.status)
            .max()
            .unwrap_or(HealthStatus::Unknown);
        prop_assert_eq!(dash.overall_health, worst,
            "overall_health {:?} != worst indicator {:?}", dash.overall_health, worst);
    }

    #[test]
    fn dashboard_always_has_five_indicators(
        evals in 0..1000u64,
        denials in 0..1000u64,
    ) {
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: evals,
            denials,
            ..Default::default()
        });
        let dash = collector.dashboard(1000);
        // denial_rate, quarantine_density, compliance_violations, audit_chain_integrity, kill_switch
        prop_assert_eq!(dash.indicators.len(), 5);
    }

    #[test]
    fn snapshots_generated_monotonically_increases(n in 1..20usize) {
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        for i in 0..n {
            let dash = collector.dashboard(i as u64 * 1000);
            prop_assert_eq!(dash.counters.snapshots_generated, (i + 1) as u64);
        }
    }

    #[test]
    fn zero_evaluations_always_healthy_denial_rate(
        quarantines in 0..5u32,
        violations in 0..5u32,
    ) {
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: 0,
            denials: 0,
            active_quarantines: quarantines,
            active_violations: violations,
        });
        let dash = collector.dashboard(1000);
        let denial_ind = dash.indicators.iter()
            .find(|i| i.name == "denial_rate")
            .unwrap();
        // Zero evals -> 0% denial rate -> healthy
        prop_assert_eq!(denial_ind.status, HealthStatus::Healthy);
        prop_assert_eq!(&denial_ind.value, "0%");
    }

    #[test]
    fn kill_switch_overrides_to_critical(
        evals in 0..1000u64,
        denials in 0..100u64,
    ) {
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: evals,
            denials,
            ..Default::default()
        });
        collector.update_kill_switch(true);
        let dash = collector.dashboard(1000);
        // Kill switch active -> at least one Critical indicator -> overall Critical or worse
        let check = dash.overall_health >= HealthStatus::Critical;
        prop_assert!(check, "kill switch active but health is {:?}", dash.overall_health);
    }

    #[test]
    fn invalid_chain_overrides_to_critical(
        evals in 0..1000u64,
        denials in 0..100u64,
    ) {
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: evals,
            denials,
            ..Default::default()
        });
        collector.update_audit_chain(50, false);
        let dash = collector.dashboard(1000);
        let check = dash.overall_health >= HealthStatus::Critical;
        prop_assert!(check, "invalid chain but health is {:?}", dash.overall_health);
    }

    #[test]
    fn denial_rate_sampling_matches_manual_calculation(
        evals in 1..10000u64,
        denial_frac in 0..100u32,
    ) {
        let denials = (evals * u64::from(denial_frac)) / 100;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: evals,
            denials,
            ..Default::default()
        });
        collector.sample_denial_rate(1000);
        let sampled = collector.denial_rate_series().latest().unwrap().value;
        let expected = (denials * 100) / evals;
        prop_assert_eq!(sampled, expected);
    }

    #[test]
    fn quarantine_sampling_sums_all_subsystems(
        a_q in 0..100u32,
        b_q in 0..100u32,
    ) {
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem("a", PolicySubsystemInput {
            active_quarantines: a_q,
            ..Default::default()
        });
        collector.update_subsystem("b", PolicySubsystemInput {
            active_quarantines: b_q,
            ..Default::default()
        });
        collector.sample_quarantine_count(1000);
        let sampled = collector.quarantine_series().latest().unwrap().value;
        prop_assert_eq!(sampled, u64::from(a_q) + u64::from(b_q));
    }

    #[test]
    fn subsystem_denial_rate_in_summary_is_correct(
        evals in 1..10000u64,
        denials in 0..10000u64,
    ) {
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: evals,
            denials,
            ..Default::default()
        });
        let dash = collector.dashboard(1000);
        let summary = &dash.subsystem_metrics["test"];
        let expected_rate = ((denials * 100) / evals) as u32;
        prop_assert_eq!(summary.denial_rate_pct, expected_rate);
    }

    #[test]
    fn forensic_count_passes_through(count in any::<u64>()) {
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_forensic_count(count);
        let dash = collector.dashboard(1000);
        prop_assert_eq!(dash.counters.forensic_records_count, count);
    }

    #[test]
    fn audit_chain_state_passes_through(length in any::<u64>(), valid in any::<bool>()) {
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_audit_chain(length, valid);
        let dash = collector.dashboard(1000);
        prop_assert_eq!(dash.counters.audit_chain_length, length);
        prop_assert_eq!(dash.counters.audit_chain_valid, valid);
    }
}

// =============================================================================
// Threshold boundary tests
// =============================================================================

proptest! {
    #[test]
    fn denial_rate_at_warning_threshold_is_warning(
        warning_pct in 1..50u32,
    ) {
        let critical_pct = warning_pct + 10; // ensure critical > warning
        let thresholds = PolicyMetricsThresholds {
            denial_rate_warning_pct: warning_pct,
            denial_rate_critical_pct: critical_pct,
            ..PolicyMetricsThresholds::default()
        };
        let mut collector = PolicyMetricsCollector::new(thresholds);
        // Create exact denial rate matching warning threshold
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: 100,
            denials: u64::from(warning_pct),
            ..Default::default()
        });
        let dash = collector.dashboard(1000);
        let denial_ind = dash.indicators.iter()
            .find(|i| i.name == "denial_rate")
            .unwrap();
        prop_assert_eq!(denial_ind.status, HealthStatus::Warning,
            "denial rate {}% with warning threshold {}% should be Warning",
            warning_pct, warning_pct);
    }

    #[test]
    fn denial_rate_below_warning_is_healthy(
        warning_pct in 2..50u32,
    ) {
        let thresholds = PolicyMetricsThresholds {
            denial_rate_warning_pct: warning_pct,
            denial_rate_critical_pct: warning_pct + 10,
            ..PolicyMetricsThresholds::default()
        };
        let mut collector = PolicyMetricsCollector::new(thresholds);
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: 100,
            denials: u64::from(warning_pct - 1),
            ..Default::default()
        });
        let dash = collector.dashboard(1000);
        let denial_ind = dash.indicators.iter()
            .find(|i| i.name == "denial_rate")
            .unwrap();
        prop_assert_eq!(denial_ind.status, HealthStatus::Healthy);
    }

    #[test]
    fn denial_rate_at_critical_threshold_is_critical(
        critical_pct in 1..50u32,
    ) {
        let thresholds = PolicyMetricsThresholds {
            denial_rate_warning_pct: 0,
            denial_rate_critical_pct: critical_pct,
            ..PolicyMetricsThresholds::default()
        };
        let mut collector = PolicyMetricsCollector::new(thresholds);
        collector.update_subsystem("test", PolicySubsystemInput {
            evaluations: 100,
            denials: u64::from(critical_pct),
            ..Default::default()
        });
        let dash = collector.dashboard(1000);
        let denial_ind = dash.indicators.iter()
            .find(|i| i.name == "denial_rate")
            .unwrap();
        prop_assert_eq!(denial_ind.status, HealthStatus::Critical);
    }
}
