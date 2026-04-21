//! Property tests for release_readiness_gates serde roundtrips and evaluation invariants.

use proptest::prelude::*;

use frankenterm_core::release_readiness_gates::{
    LeakEvidenceStatus, ReleaseDecision, ReleaseGateCheck, ReleaseGateInputs, ReleaseGatePolicy,
    ReleaseGateReport, SoakEvidenceStatus,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn release_decision_strategy() -> impl Strategy<Value = ReleaseDecision> {
    prop_oneof![Just(ReleaseDecision::Ready), Just(ReleaseDecision::Blocked),]
}

fn gate_check_strategy() -> impl Strategy<Value = ReleaseGateCheck> {
    (
        "[a-zA-Z0-9_-]{1,30}",
        "[a-zA-Z0-9 ]{1,50}",
        any::<bool>(),
        any::<bool>(),
        "[a-zA-Z0-9 ]{1,50}",
        "[a-zA-Z0-9 ]{1,50}",
        "[a-zA-Z0-9 ]{1,50}",
    )
        .prop_map(
            |(gate_id, description, passed, blocking, observed, required, action)| {
                ReleaseGateCheck {
                    gate_id,
                    description,
                    passed,
                    blocking,
                    observed,
                    required,
                    action,
                }
            },
        )
}

fn gate_report_strategy() -> impl Strategy<Value = ReleaseGateReport> {
    (
        release_decision_strategy(),
        prop::collection::vec(gate_check_strategy(), 0..8),
    )
        .prop_map(|(decision, checks)| ReleaseGateReport { decision, checks })
}

fn positive_f64() -> impl Strategy<Value = f64> {
    (1u32..=10_000).prop_map(|n| n as f64 / 100.0)
}

fn gate_policy_strategy() -> impl Strategy<Value = ReleaseGatePolicy> {
    (
        prop::collection::vec(0u64..1000, 0..8),
        0usize..20,
        0usize..10,
        positive_f64(),
        positive_f64(),
    )
        .prop_map(
            |(scales, metric_count, min_cycles, max_rss, max_dur)| ReleaseGatePolicy {
                required_pane_scales: scales,
                required_metric_count: metric_count,
                min_release_cycles: min_cycles,
                max_peak_rss_mb: max_rss,
                max_duration_s: max_dur,
            },
        )
}

fn leak_evidence_strategy() -> impl Strategy<Value = LeakEvidenceStatus> {
    (any::<bool>(), any::<bool>(), "[a-zA-Z0-9/_.-]{0,80}").prop_map(|(present, passed, path)| {
        LeakEvidenceStatus {
            summary_present: present,
            summary_passed: passed,
            summary_path: path,
        }
    })
}

fn soak_evidence_strategy() -> impl Strategy<Value = SoakEvidenceStatus> {
    (
        any::<bool>(),
        any::<bool>(),
        0usize..10,
        0usize..10,
        any::<bool>(),
        prop::collection::vec(0u64..1000, 0..8),
        0usize..20,
        positive_f64(),
        positive_f64(),
        any::<bool>(),
    )
        .prop_map(
            |(
                wrapper_present,
                wrapper_passed,
                smoke,
                release,
                consistent,
                scales,
                metric_count,
                rss,
                dur,
                bp,
            )| {
                SoakEvidenceStatus {
                    wrapper_present,
                    wrapper_passed,
                    smoke_cycles: smoke,
                    release_cycles: release,
                    release_consistent: consistent,
                    pane_scales: scales,
                    metric_count,
                    peak_rss_mb: rss,
                    max_duration_s: dur,
                    backpressure_exercised: bp,
                }
            },
        )
}

fn gate_inputs_strategy() -> impl Strategy<Value = ReleaseGateInputs> {
    (
        leak_evidence_strategy(),
        soak_evidence_strategy(),
        any::<bool>(),
    )
        .prop_map(|(leak, soak, guard)| ReleaseGateInputs {
            leak,
            soak,
            guard_contract_passed: guard,
        })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[allow(clippy::float_cmp)]
fn f64_close(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    (a - b).abs() < 1e-10
}

// ---------------------------------------------------------------------------
// Serde roundtrip tests
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn release_decision_serde_roundtrip(d in release_decision_strategy()) {
        let json = serde_json::to_string(&d).unwrap();
        let back: ReleaseDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d, back);
    }

    #[test]
    fn gate_check_serde_roundtrip(gc in gate_check_strategy()) {
        let json = serde_json::to_string(&gc).unwrap();
        let back: ReleaseGateCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(gc, back);
    }

    #[test]
    fn gate_report_serde_roundtrip(report in gate_report_strategy()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: ReleaseGateReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report.decision, back.decision);
        prop_assert_eq!(report.checks.len(), back.checks.len());
        for (a, b) in report.checks.iter().zip(back.checks.iter()) {
            prop_assert_eq!(a, b);
        }
    }

    #[test]
    fn gate_policy_serde_roundtrip(policy in gate_policy_strategy()) {
        let json = serde_json::to_string(&policy).unwrap();
        let back: ReleaseGatePolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&policy.required_pane_scales, &back.required_pane_scales);
        prop_assert_eq!(policy.required_metric_count, back.required_metric_count);
        prop_assert_eq!(policy.min_release_cycles, back.min_release_cycles);
        let close_rss = f64_close(policy.max_peak_rss_mb, back.max_peak_rss_mb);
        prop_assert!(close_rss, "max_peak_rss_mb mismatch");
        let close_dur = f64_close(policy.max_duration_s, back.max_duration_s);
        prop_assert!(close_dur, "max_duration_s mismatch");
    }

    #[test]
    fn leak_evidence_serde_roundtrip(le in leak_evidence_strategy()) {
        let json = serde_json::to_string(&le).unwrap();
        let back: LeakEvidenceStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(le, back);
    }

    #[test]
    fn soak_evidence_serde_roundtrip(se in soak_evidence_strategy()) {
        let json = serde_json::to_string(&se).unwrap();
        let back: SoakEvidenceStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(se.wrapper_present, back.wrapper_present);
        prop_assert_eq!(se.wrapper_passed, back.wrapper_passed);
        prop_assert_eq!(se.smoke_cycles, back.smoke_cycles);
        prop_assert_eq!(se.release_cycles, back.release_cycles);
        prop_assert_eq!(se.release_consistent, back.release_consistent);
        prop_assert_eq!(&se.pane_scales, &back.pane_scales);
        prop_assert_eq!(se.metric_count, back.metric_count);
        let close_rss = f64_close(se.peak_rss_mb, back.peak_rss_mb);
        prop_assert!(close_rss, "peak_rss_mb mismatch");
        let close_dur = f64_close(se.max_duration_s, back.max_duration_s);
        prop_assert!(close_dur, "max_duration_s mismatch");
        prop_assert_eq!(se.backpressure_exercised, back.backpressure_exercised);
    }

    #[test]
    fn gate_inputs_serde_roundtrip(gi in gate_inputs_strategy()) {
        let json = serde_json::to_string(&gi).unwrap();
        let back: ReleaseGateInputs = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(gi.leak, back.leak);
        prop_assert_eq!(gi.guard_contract_passed, back.guard_contract_passed);
        prop_assert_eq!(gi.soak.wrapper_present, back.soak.wrapper_present);
        prop_assert_eq!(gi.soak.smoke_cycles, back.soak.smoke_cycles);
    }
}

// ---------------------------------------------------------------------------
// Evaluation invariant tests
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn evaluate_always_produces_four_checks(
        policy in gate_policy_strategy(),
        inputs in gate_inputs_strategy(),
    ) {
        let report = policy.evaluate(&inputs);
        prop_assert_eq!(report.checks.len(), 4, "should always produce 4 gate checks");
    }

    #[test]
    fn evaluate_all_checks_blocking(
        policy in gate_policy_strategy(),
        inputs in gate_inputs_strategy(),
    ) {
        let report = policy.evaluate(&inputs);
        for check in &report.checks {
            prop_assert!(check.blocking, "all gates should be blocking: {}", check.gate_id);
        }
    }

    #[test]
    fn evaluate_ready_means_all_passed(
        policy in gate_policy_strategy(),
        inputs in gate_inputs_strategy(),
    ) {
        let report = policy.evaluate(&inputs);
        if report.decision == ReleaseDecision::Ready {
            for check in &report.checks {
                prop_assert!(check.passed, "Ready decision but gate {} failed", check.gate_id);
            }
        }
    }

    #[test]
    fn evaluate_blocked_means_at_least_one_failed(
        policy in gate_policy_strategy(),
        inputs in gate_inputs_strategy(),
    ) {
        let report = policy.evaluate(&inputs);
        if report.decision == ReleaseDecision::Blocked {
            let any_failed = report.checks.iter().any(|c| !c.passed);
            prop_assert!(any_failed, "Blocked but no gates failed");
        }
    }

    #[test]
    fn evaluate_failed_count_consistent(
        policy in gate_policy_strategy(),
        inputs in gate_inputs_strategy(),
    ) {
        let report = policy.evaluate(&inputs);
        let manual_count = report.checks.iter().filter(|c| !c.passed).count();
        prop_assert_eq!(report.failed_count(), manual_count);
    }

    #[test]
    fn evaluate_gate_ids_are_stable(
        policy in gate_policy_strategy(),
        inputs in gate_inputs_strategy(),
    ) {
        let report = policy.evaluate(&inputs);
        let ids: Vec<&str> = report.checks.iter().map(|c| c.gate_id.as_str()).collect();
        prop_assert_eq!(ids, vec![
            "REL-01-leak-oracle",
            "REL-02-guard-surface",
            "REL-03-soak-confidence",
            "REL-04-performance-budget",
        ]);
    }

    #[test]
    fn render_summary_contains_decision(
        policy in gate_policy_strategy(),
        inputs in gate_inputs_strategy(),
    ) {
        let report = policy.evaluate(&inputs);
        let summary = report.render_summary();
        let has_decision = summary.contains("Ready") || summary.contains("Blocked");
        prop_assert!(has_decision, "summary should contain decision");
    }

    #[test]
    fn render_summary_shows_pass_fail_icons(
        policy in gate_policy_strategy(),
        inputs in gate_inputs_strategy(),
    ) {
        let report = policy.evaluate(&inputs);
        let summary = report.render_summary();
        for check in &report.checks {
            if check.passed {
                let has_pass = summary.contains("[PASS]");
                prop_assert!(has_pass, "summary should contain [PASS]");
            } else {
                let has_fail = summary.contains("[FAIL]");
                prop_assert!(has_fail, "summary should contain [FAIL]");
            }
        }
    }

    #[test]
    fn finish_line_policy_is_deterministic(_seed in 0u32..100) {
        let a = ReleaseGatePolicy::finish_line();
        let b = ReleaseGatePolicy::finish_line();
        prop_assert_eq!(&a.required_pane_scales, &b.required_pane_scales);
        prop_assert_eq!(a.required_metric_count, b.required_metric_count);
        prop_assert_eq!(a.min_release_cycles, b.min_release_cycles);
    }
}
