//! Property tests for disaster_recovery_drills module.
//!
//! Covers serde roundtrips, RTO/RPO target evaluation, DrillVerdict
//! algebra, DrillMetrics arithmetic, DrillRunner execution invariants,
//! and ContinuityReport health aggregation.

use frankenterm_core::disaster_recovery_drills::*;
use proptest::prelude::*;
use std::collections::HashMap;
use std::time::Duration;

// =============================================================================
// Strategies
// =============================================================================

fn arb_drill_kind() -> impl Strategy<Value = DrillKind> {
    prop_oneof![
        Just(DrillKind::ColdStart),
        (0..1000u32).prop_map(|f| DrillKind::PartialFailure { failure_fraction: f }),
        Just(DrillKind::CascadingFailure),
        (1..3600u64).prop_map(|s| DrillKind::TimeTravel { lookback: Duration::from_secs(s) }),
        (1..500u64).prop_map(|c| DrillKind::ScaleRecovery { pane_count: c }),
        "[a-z-]{3,15}".prop_map(DrillKind::Custom),
    ]
}

fn arb_drill_verdict() -> impl Strategy<Value = DrillVerdict> {
    prop_oneof![
        Just(DrillVerdict::Pass),
        Just(DrillVerdict::Degraded),
        Just(DrillVerdict::Fail),
        Just(DrillVerdict::Skipped),
    ]
}

fn arb_continuity_status() -> impl Strategy<Value = ContinuityStatus> {
    prop_oneof![
        Just(ContinuityStatus::Healthy),
        "[a-z ]{3,20}".prop_map(ContinuityStatus::Warning),
        "[a-z ]{3,20}".prop_map(ContinuityStatus::Unhealthy),
        Just(ContinuityStatus::Unknown),
    ]
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_drill_kind(kind in arb_drill_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: DrillKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }

    #[test]
    fn serde_roundtrip_drill_verdict(v in arb_drill_verdict()) {
        let json = serde_json::to_string(&v).unwrap();
        let back: DrillVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, back);
    }

    #[test]
    fn serde_roundtrip_drill_metrics(
        recovery in 0..1_000_000u64,
        data_loss in 0..1_000_000u64,
        completeness in 0..1000u32,
    ) {
        let m = DrillMetrics {
            recovery_time_ms: recovery,
            data_loss_window_ms: data_loss,
            completeness_permille: completeness,
            panes_attempted: 10,
            panes_restored: 8,
            panes_failed: 2,
            scrollback_lines_recovered: 800,
            scrollback_lines_expected: 1000,
            backup_size_bytes: 1024,
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: DrillMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.recovery_time_ms, recovery);
        prop_assert_eq!(back.completeness_permille, completeness);
    }

    #[test]
    fn serde_roundtrip_drill_scenario(_dummy in 0..1u32) {
        let s = DrillScenario::cold_start();
        let json = serde_json::to_string(&s).unwrap();
        let back: DrillScenario = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, s.id);
        prop_assert_eq!(back.kind, s.kind);
    }

    #[test]
    fn serde_roundtrip_continuity_status(st in arb_continuity_status()) {
        let json = serde_json::to_string(&st).unwrap();
        let back: ContinuityStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(st, back);
    }

    #[test]
    fn serde_roundtrip_continuity_health_check(_dummy in 0..1u32) {
        let check = ContinuityHealthCheck {
            subsystem: "backup".into(),
            status: ContinuityStatus::Healthy,
            last_checked_ms: 1000,
            last_backup_ms: Some(900),
            restore_points: 5,
        };
        let json = serde_json::to_string(&check).unwrap();
        let back: ContinuityHealthCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.subsystem, "backup");
        prop_assert_eq!(back.restore_points, 5);
    }

    #[test]
    fn serde_roundtrip_drill_report(_dummy in 0..1u32) {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_scenario(DrillScenario::cold_start());
        let report = runner.execute_all();
        let json = serde_json::to_string(&report).unwrap();
        let back: DrillReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.results.len(), 1);
        prop_assert_eq!(back.overall_verdict, report.overall_verdict);
    }
}

// =============================================================================
// RTO/RPO target evaluation
// =============================================================================

proptest! {
    #[test]
    fn rto_met_when_under_target(
        target_secs in 1..3600u64,
        actual_secs in 0..3600u64,
    ) {
        let target = RtoTarget::new(Duration::from_secs(target_secs), "test");
        let actual = Duration::from_secs(actual_secs);
        let met = target.is_met(actual);
        prop_assert_eq!(met, actual_secs <= target_secs);
    }

    #[test]
    fn rto_delta_sign(
        target_secs in 1..3600u64,
        actual_secs in 0..3600u64,
    ) {
        let target = RtoTarget::new(Duration::from_secs(target_secs), "test");
        let actual = Duration::from_secs(actual_secs);
        let delta = target.delta(actual);
        if actual_secs > target_secs {
            prop_assert!(delta > 0);
        } else if actual_secs < target_secs {
            prop_assert!(delta < 0);
        } else {
            prop_assert_eq!(delta, 0);
        }
    }

    #[test]
    fn rto_target_self_exact_is_met(target_secs in 1..3600u64) {
        let target = RtoTarget::new(Duration::from_secs(target_secs), "test");
        prop_assert!(target.is_met(Duration::from_secs(target_secs)));
        prop_assert_eq!(target.delta(Duration::from_secs(target_secs)), 0);
    }

    #[test]
    fn rpo_met_when_under_target(
        target_secs in 1..3600u64,
        actual_secs in 0..3600u64,
    ) {
        let target = RpoTarget::new(Duration::from_secs(target_secs), "test");
        let actual = Duration::from_secs(actual_secs);
        prop_assert_eq!(target.is_met(actual), actual_secs <= target_secs);
    }
}

// =============================================================================
// DrillVerdict algebra
// =============================================================================

proptest! {
    #[test]
    fn verdict_combine_commutative(a in arb_drill_verdict(), b in arb_drill_verdict()) {
        prop_assert_eq!(a.combine(b), b.combine(a));
    }

    #[test]
    fn verdict_fail_dominates(other in arb_drill_verdict()) {
        prop_assert_eq!(DrillVerdict::Fail.combine(other), DrillVerdict::Fail);
        prop_assert_eq!(other.combine(DrillVerdict::Fail), DrillVerdict::Fail);
    }

    #[test]
    fn verdict_pass_identity_with_pass(_dummy in 0..1u32) {
        prop_assert_eq!(DrillVerdict::Pass.combine(DrillVerdict::Pass), DrillVerdict::Pass);
    }

    #[test]
    fn verdict_is_pass_iff_pass(v in arb_drill_verdict()) {
        prop_assert_eq!(v.is_pass(), v == DrillVerdict::Pass);
    }

    #[test]
    fn verdict_is_fail_iff_fail(v in arb_drill_verdict()) {
        prop_assert_eq!(v.is_fail(), v == DrillVerdict::Fail);
    }

    #[test]
    fn verdict_display_not_empty(v in arb_drill_verdict()) {
        prop_assert!(!v.to_string().is_empty());
    }
}

// =============================================================================
// DrillKind Display
// =============================================================================

proptest! {
    #[test]
    fn drill_kind_display_not_empty(kind in arb_drill_kind()) {
        prop_assert!(!kind.to_string().is_empty());
    }

    #[test]
    fn cold_start_display_stable(_dummy in 0..1u32) {
        prop_assert_eq!(DrillKind::ColdStart.to_string(), "cold-start");
    }
}

// =============================================================================
// DrillMetrics arithmetic
// =============================================================================

proptest! {
    #[test]
    fn completeness_fraction_in_range(permille in 0..1001u32) {
        let m = DrillMetrics {
            completeness_permille: permille,
            ..Default::default()
        };
        let frac = m.completeness_fraction();
        prop_assert!(frac >= 0.0);
        prop_assert!(frac <= 1.001); // allow small float rounding
    }

    #[test]
    fn completeness_fraction_formula(permille in 0..1001u32) {
        let m = DrillMetrics {
            completeness_permille: permille,
            ..Default::default()
        };
        let expected = permille as f64 / 1000.0;
        prop_assert!((m.completeness_fraction() - expected).abs() < 0.0001);
    }

    #[test]
    fn scrollback_ratio_zero_expected_returns_one(_dummy in 0..1u32) {
        let m = DrillMetrics {
            scrollback_lines_expected: 0,
            scrollback_lines_recovered: 0,
            ..Default::default()
        };
        prop_assert!((m.scrollback_ratio() - 1.0).abs() < 0.001);
    }

    #[test]
    fn scrollback_ratio_formula(
        recovered in 0..1000u64,
        expected in 1..1000u64,
    ) {
        let m = DrillMetrics {
            scrollback_lines_recovered: recovered,
            scrollback_lines_expected: expected,
            ..Default::default()
        };
        let ratio = m.scrollback_ratio();
        let expected_ratio = recovered as f64 / expected as f64;
        prop_assert!((ratio - expected_ratio).abs() < 0.001);
    }
}

// =============================================================================
// DrillScenario constructors
// =============================================================================

proptest! {
    #[test]
    fn partial_failure_completeness_target(fraction in 0..1000u32) {
        let s = DrillScenario::partial_failure(fraction);
        prop_assert_eq!(s.min_completeness_permille, 1000 - fraction);
    }

    #[test]
    fn cold_start_has_rto_and_rpo(_dummy in 0..1u32) {
        let s = DrillScenario::cold_start();
        prop_assert!(s.rto.is_some());
        prop_assert!(s.rpo.is_some());
        prop_assert_eq!(s.min_completeness_permille, 950);
    }

    #[test]
    fn scale_recovery_pane_count_preserved(count in 1..1000u64) {
        let s = DrillScenario::scale_recovery(count);
        let is_scale = matches!(s.kind, DrillKind::ScaleRecovery { pane_count } if pane_count == count);
        prop_assert!(is_scale);
    }

    #[test]
    fn time_travel_lookback_preserved(secs in 1..3600u64) {
        let s = DrillScenario::time_travel(Duration::from_secs(secs));
        if let DrillKind::TimeTravel { lookback } = s.kind {
            prop_assert_eq!(lookback.as_secs(), secs);
        } else {
            prop_assert!(false, "wrong kind");
        }
    }
}

// =============================================================================
// DrillRunner execution
// =============================================================================

proptest! {
    #[test]
    fn baseline_suite_all_pass(_dummy in 0..1u32) {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_baseline_suite();
        let report = runner.execute_all();
        prop_assert!(report.overall_verdict.is_pass());
        prop_assert_eq!(report.summary.total_drills, 3);
        prop_assert_eq!(report.summary.passed, 3);
    }

    #[test]
    fn scale_suite_all_pass(_dummy in 0..1u32) {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_scale_suite();
        let report = runner.execute_all();
        prop_assert!(report.overall_verdict.is_pass());
        prop_assert_eq!(report.summary.total_drills, 4);
    }

    #[test]
    fn empty_runner_pass(_dummy in 0..1u32) {
        let mut runner = DrillRunner::new(DrillConfig::default());
        let report = runner.execute_all();
        prop_assert!(report.overall_verdict.is_pass());
        prop_assert_eq!(report.summary.total_drills, 0);
    }

    #[test]
    fn scenario_count_tracks_additions(count in 1..10usize) {
        let mut runner = DrillRunner::new(DrillConfig::default());
        for _ in 0..count {
            runner.add_scenario(DrillScenario::cold_start());
        }
        prop_assert_eq!(runner.scenario_count(), count);
    }

    #[test]
    fn summary_stats_consistency(_dummy in 0..1u32) {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_baseline_suite();
        let report = runner.execute_all();
        let s = &report.summary;
        prop_assert_eq!(s.total_drills, s.passed + s.degraded + s.failed + s.skipped);
    }

    #[test]
    fn failing_metrics_produce_fail(_dummy in 0..1u32) {
        let mut runner = DrillRunner::new(DrillConfig::default());
        runner.add_scenario(DrillScenario::cold_start());
        let mut m = HashMap::new();
        m.insert("dr-cold-start".to_string(), DrillMetrics {
            recovery_time_ms: 999_999,
            data_loss_window_ms: 999_999,
            completeness_permille: 0,
            ..Default::default()
        });
        let report = runner.execute_with_metrics(&m);
        prop_assert!(report.overall_verdict.is_fail());
    }

    #[test]
    fn stop_on_failure_limits_results(_dummy in 0..1u32) {
        let config = DrillConfig {
            continue_on_failure: false,
            ..Default::default()
        };
        let mut runner = DrillRunner::new(config);
        runner.add_baseline_suite();
        let mut m = HashMap::new();
        m.insert("dr-cold-start".to_string(), DrillMetrics {
            recovery_time_ms: 999_999,
            data_loss_window_ms: 999_999,
            completeness_permille: 0,
            ..Default::default()
        });
        let report = runner.execute_with_metrics(&m);
        prop_assert_eq!(report.results.len(), 1);
    }
}

// =============================================================================
// ContinuityStatus
// =============================================================================

proptest! {
    #[test]
    fn continuity_healthy_predicate(st in arb_continuity_status()) {
        let expected = matches!(st, ContinuityStatus::Healthy);
        prop_assert_eq!(st.is_healthy(), expected);
    }
}

// =============================================================================
// ContinuityReport aggregation
// =============================================================================

proptest! {
    #[test]
    fn all_healthy_means_drill_ready(count in 1..5usize) {
        let checks: Vec<ContinuityHealthCheck> = (0..count)
            .map(|i| ContinuityHealthCheck {
                subsystem: format!("sub-{i}"),
                status: ContinuityStatus::Healthy,
                last_checked_ms: 1000,
                last_backup_ms: Some(900),
                restore_points: 3,
            })
            .collect();
        let report = ContinuityReport::from_checks(checks);
        prop_assert!(report.drill_ready);
        prop_assert!(report.overall.is_healthy());
        prop_assert_eq!(report.healthy_count(), count);
    }

    #[test]
    fn any_unhealthy_means_not_drill_ready(count in 1..5usize) {
        let mut checks: Vec<ContinuityHealthCheck> = (0..count)
            .map(|i| ContinuityHealthCheck {
                subsystem: format!("sub-{i}"),
                status: ContinuityStatus::Healthy,
                last_checked_ms: 1000,
                last_backup_ms: None,
                restore_points: 1,
            })
            .collect();
        checks.push(ContinuityHealthCheck {
            subsystem: "broken".into(),
            status: ContinuityStatus::Unhealthy("dead".into()),
            last_checked_ms: 1000,
            last_backup_ms: None,
            restore_points: 0,
        });
        let report = ContinuityReport::from_checks(checks);
        prop_assert!(!report.drill_ready);
        let is_unhealthy = matches!(report.overall, ContinuityStatus::Unhealthy(_));
        prop_assert!(is_unhealthy);
    }

    #[test]
    fn healthy_count_bounded(count in 1..10usize) {
        let checks: Vec<ContinuityHealthCheck> = (0..count)
            .map(|i| ContinuityHealthCheck {
                subsystem: format!("sub-{i}"),
                status: if i % 2 == 0 {
                    ContinuityStatus::Healthy
                } else {
                    ContinuityStatus::Warning("warn".into())
                },
                last_checked_ms: 1000,
                last_backup_ms: None,
                restore_points: 1,
            })
            .collect();
        let report = ContinuityReport::from_checks(checks);
        prop_assert!(report.healthy_count() <= count);
    }

    #[test]
    fn total_restore_points_is_sum(points in proptest::collection::vec(0..100u32, 1..5)) {
        let checks: Vec<ContinuityHealthCheck> = points
            .iter()
            .enumerate()
            .map(|(i, &p)| ContinuityHealthCheck {
                subsystem: format!("sub-{i}"),
                status: ContinuityStatus::Healthy,
                last_checked_ms: 1000,
                last_backup_ms: None,
                restore_points: p,
            })
            .collect();
        let expected: u32 = points.iter().sum();
        let report = ContinuityReport::from_checks(checks);
        prop_assert_eq!(report.total_restore_points(), expected);
    }

    #[test]
    fn all_unknown_status(_dummy in 0..1u32) {
        let checks = vec![ContinuityHealthCheck {
            subsystem: "test".into(),
            status: ContinuityStatus::Unknown,
            last_checked_ms: 0,
            last_backup_ms: None,
            restore_points: 0,
        }];
        let report = ContinuityReport::from_checks(checks);
        prop_assert!(!report.drill_ready);
        let is_unknown = matches!(report.overall, ContinuityStatus::Unknown);
        prop_assert!(is_unknown);
    }
}
