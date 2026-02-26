//! Property-based tests for replay_ci_gate (ft-og6q6.7.4).
//!
//! Invariants tested:
//! - CG-1: GateId as_str/from_str_id roundtrip
//! - CG-2: GateId gate_number is 1, 2, 3
//! - CG-3: GateReport pass_count + fail_count == total_count
//! - CG-4: GateReport status is Pass when fail_count == 0
//! - CG-5: GateReport status is Fail when fail_count > 0
//! - CG-6: GateReport serde roundtrip
//! - CG-7: GateCheck serde roundtrip
//! - CG-8: Waiver serde roundtrip
//! - CG-9: Waiver matches_check with wildcard
//! - CG-10: Waiver expired when current > expires
//! - CG-11: Waiver not expired when no expiry
//! - CG-12: EvidenceBundle overall_status Fail when any gate fails
//! - CG-13: EvidenceBundle overall_status Pass when all gates pass
//! - CG-14: EvidenceBundle is_promotable iff Pass or Waived
//! - CG-15: Gate 1 smoke: schema fail always fails gate
//! - CG-16: Gate 2 test suite: proptest cases below min fails gate
//! - CG-17: Gate 3 regression: blocking metrics fail gate
//! - CG-18: parse_waivers returns empty for no markers
//! - CG-19: matches_replay_path positive for replay sources
//! - CG-20: GateId timeout_seconds ascending order

use proptest::prelude::*;

use frankenterm_core::replay_ci_gate::{
    ALL_GATES, EvidenceBundle, GateCheck, GateId, GateReport, GateStatus, MIN_PROPTEST_CASES,
    RegressionResults, TestSuiteResults, Waiver, evaluate_gate1_smoke, evaluate_gate2_test_suite,
    evaluate_gate3_regression, matches_replay_path, parse_waivers,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── CG-1: GateId str roundtrip ───────────────────────────────────────

    #[test]
    fn cg01_gate_id_roundtrip(idx in 0usize..3) {
        let gate = ALL_GATES[idx];
        let s = gate.as_str();
        let parsed = GateId::from_str_id(s);
        prop_assert_eq!(parsed, Some(gate));
    }

    // ── CG-2: Gate numbers are 1, 2, 3 ──────────────────────────────────

    #[test]
    fn cg02_gate_numbers(idx in 0usize..3) {
        let gate = ALL_GATES[idx];
        let num = gate.gate_number();
        prop_assert!((1..=3).contains(&num));
        prop_assert_eq!(num as usize, idx + 1);
    }

    // ── CG-3: pass + fail == total ───────────────────────────────────────

    #[test]
    fn cg03_counts_consistent(
        pass_count in 0usize..20,
        fail_count in 0usize..20
    ) {
        let mut checks = Vec::new();
        for i in 0..pass_count {
            checks.push(GateCheck {
                name: format!("pass_{}", i),
                passed: true,
                message: "ok".into(),
                duration_ms: None,
                artifact_path: None,
            });
        }
        for i in 0..fail_count {
            checks.push(GateCheck {
                name: format!("fail_{}", i),
                passed: false,
                message: "bad".into(),
                duration_ms: None,
                artifact_path: None,
            });
        }
        let report = GateReport::new(GateId::Smoke, checks, 100, "now".into());
        prop_assert_eq!(report.pass_count + report.fail_count, report.total_count);
        prop_assert_eq!(report.pass_count, pass_count);
        prop_assert_eq!(report.fail_count, fail_count);
    }

    // ── CG-4: No failures → Pass ────────────────────────────────────────

    #[test]
    fn cg04_no_failures_pass(count in 0usize..10) {
        let checks: Vec<GateCheck> = (0..count).map(|i| GateCheck {
            name: format!("check_{}", i),
            passed: true,
            message: "ok".into(),
            duration_ms: None,
            artifact_path: None,
        }).collect();
        let report = GateReport::new(GateId::TestSuite, checks, 50, "now".into());
        prop_assert_eq!(report.status, GateStatus::Pass);
    }

    // ── CG-5: Any failure → Fail ─────────────────────────────────────────

    #[test]
    fn cg05_any_failure_fails(
        pass_count in 0usize..10,
        fail_count in 1usize..10
    ) {
        let mut checks = Vec::new();
        for i in 0..pass_count {
            checks.push(GateCheck {
                name: format!("p_{}", i),
                passed: true,
                message: "ok".into(),
                duration_ms: None,
                artifact_path: None,
            });
        }
        for i in 0..fail_count {
            checks.push(GateCheck {
                name: format!("f_{}", i),
                passed: false,
                message: "bad".into(),
                duration_ms: None,
                artifact_path: None,
            });
        }
        let report = GateReport::new(GateId::Regression, checks, 50, "now".into());
        prop_assert_eq!(report.status, GateStatus::Fail);
    }

    // ── CG-6: GateReport serde roundtrip ─────────────────────────────────

    #[test]
    fn cg06_report_serde(idx in 0usize..3, dur in 0u64..100000) {
        let gate = ALL_GATES[idx];
        let checks = vec![GateCheck {
            name: "test".into(),
            passed: true,
            message: "ok".into(),
            duration_ms: Some(dur),
            artifact_path: None,
        }];
        let report = GateReport::new(gate, checks, dur, "2026-01-01T00:00:00Z".into());
        let json = serde_json::to_string(&report).unwrap();
        let restored: GateReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, report);
    }

    // ── CG-7: GateCheck serde roundtrip ──────────────────────────────────

    #[test]
    fn cg07_check_serde(dur in 0u64..10000) {
        let check = GateCheck {
            name: "check".into(),
            passed: true,
            message: "ok".into(),
            duration_ms: Some(dur),
            artifact_path: Some("/path".into()),
        };
        let json = serde_json::to_string(&check).unwrap();
        let restored: GateCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, check);
    }

    // ── CG-8: Waiver serde roundtrip ─────────────────────────────────────

    #[test]
    fn cg08_waiver_serde(idx in 0usize..3) {
        let gate = ALL_GATES[idx];
        let waiver = Waiver {
            gate,
            check_name: "test_check".into(),
            reason: "test reason".into(),
            author: "dev".into(),
            expires_at: Some("2027-01-01T00:00:00Z".into()),
            pr_reference: Some("#100".into()),
        };
        let json = serde_json::to_string(&waiver).unwrap();
        let restored: Waiver = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, waiver);
    }

    // ── CG-9: Wildcard waiver matches any check ──────────────────────────

    #[test]
    fn cg09_wildcard_waiver(idx in 0usize..3) {
        let gate = ALL_GATES[idx];
        let waiver = Waiver {
            gate,
            check_name: "*".into(),
            reason: "blanket".into(),
            author: "ops".into(),
            expires_at: None,
            pr_reference: None,
        };
        prop_assert!(waiver.matches_check(gate, "any_check_name"));
        prop_assert!(waiver.matches_check(gate, "another_check"));
        // Different gate should not match
        let other_gate = ALL_GATES[(idx + 1) % 3];
        let matches_other = waiver.matches_check(other_gate, "any_check_name");
        prop_assert!(!matches_other);
    }

    // ── CG-10: Waiver expired when past deadline ─────────────────────────

    #[test]
    fn cg10_waiver_expired(year in 2020u32..2025) {
        let waiver = Waiver {
            gate: GateId::Smoke,
            check_name: "*".into(),
            reason: "old".into(),
            author: "dev".into(),
            expires_at: Some(format!("{}-01-01T00:00:00Z", year)),
            pr_reference: None,
        };
        let is_expired = waiver.is_expired_at("2026-01-01T00:00:00Z");
        prop_assert!(is_expired);
    }

    // ── CG-11: No expiry → never expired ─────────────────────────────────

    #[test]
    fn cg11_no_expiry(year in 2020u32..2099) {
        let waiver = Waiver {
            gate: GateId::Smoke,
            check_name: "*".into(),
            reason: "permanent".into(),
            author: "dev".into(),
            expires_at: None,
            pr_reference: None,
        };
        let is_expired = waiver.is_expired_at(&format!("{}-12-31T23:59:59Z", year));
        prop_assert!(!is_expired);
    }

    // ── CG-12: Bundle fails when any gate fails ──────────────────────────

    #[test]
    fn cg12_bundle_fail_propagates(idx in 0usize..3) {
        let fail_gate = ALL_GATES[idx];
        let reports: Vec<GateReport> = ALL_GATES.iter().map(|g| {
            if *g == fail_gate {
                let checks = vec![GateCheck {
                    name: "x".into(),
                    passed: false,
                    message: "bad".into(),
                    duration_ms: None,
                    artifact_path: None,
                }];
                GateReport::new(*g, checks, 10, "now".into())
            } else {
                GateReport::new(*g, vec![], 10, "now".into())
            }
        }).collect();
        let bundle = EvidenceBundle::new(reports, "now".into());
        prop_assert_eq!(bundle.overall_status, GateStatus::Fail);
        let is_promotable = bundle.is_promotable();
        prop_assert!(!is_promotable);
    }

    // ── CG-13: Bundle passes when all gates pass ─────────────────────────

    #[test]
    fn cg13_bundle_all_pass(_dummy in 0u8..1) {
        let reports: Vec<GateReport> = ALL_GATES.iter().map(|g| {
            GateReport::new(*g, vec![], 10, "now".into())
        }).collect();
        let bundle = EvidenceBundle::new(reports, "now".into());
        prop_assert_eq!(bundle.overall_status, GateStatus::Pass);
        prop_assert!(bundle.is_promotable());
    }

    // ── CG-14: Promotable iff Pass or Waived ─────────────────────────────

    #[test]
    fn cg14_promotable_semantics(_dummy in 0u8..1) {
        // Pass → promotable
        let pass_bundle = EvidenceBundle::new(vec![], "now".into());
        prop_assert!(pass_bundle.is_promotable());

        // Fail → not promotable
        let fail_checks = vec![GateCheck {
            name: "x".into(),
            passed: false,
            message: "bad".into(),
            duration_ms: None,
            artifact_path: None,
        }];
        let fail_report = GateReport::new(GateId::Smoke, fail_checks, 10, "now".into());
        let fail_bundle = EvidenceBundle::new(vec![fail_report], "now".into());
        let is_promotable = fail_bundle.is_promotable();
        prop_assert!(!is_promotable);
    }

    // ── CG-15: Gate 1 schema fail blocks ─────────────────────────────────

    #[test]
    fn cg15_gate1_schema_fail(dur in 0u64..1000) {
        let report = evaluate_gate1_smoke(false, &[], dur, "now");
        prop_assert_eq!(report.status, GateStatus::Fail);
        prop_assert_eq!(report.gate, GateId::Smoke);
    }

    // ── CG-16: Gate 2 proptest below min fails ───────────────────────────

    #[test]
    fn cg16_gate2_low_proptest(cases in 0usize..MIN_PROPTEST_CASES) {
        let results = TestSuiteResults {
            unit_tests_passed: 50,
            unit_tests_total: 50,
            proptest_cases: cases,
            proptest_passed: true,
            integration_tests_passed: 10,
            integration_tests_total: 10,
        };
        let report = evaluate_gate2_test_suite(&results, 5000, "now");
        prop_assert_eq!(report.status, GateStatus::Fail);
    }

    // ── CG-17: Gate 3 blocking metric fails ──────────────────────────────

    #[test]
    fn cg17_gate3_blocking_metric(count in 1usize..10) {
        let results = RegressionResults {
            e2e_passed: true,
            e2e_scenario_count: 5,
            regression_suite_passed: true,
            regression_divergence_count: 0,
            blocking_metric_count: count,
            warning_metric_count: 0,
            evidence_bundle_path: None,
        };
        let report = evaluate_gate3_regression(&results, 15000, "now");
        prop_assert_eq!(report.status, GateStatus::Fail);
    }

    // ── CG-18: No waiver markers → empty ─────────────────────────────────

    #[test]
    fn cg18_no_markers_empty(text in "[a-z ]{0,100}") {
        let waivers = parse_waivers(&text);
        prop_assert!(waivers.is_empty());
    }

    // ── CG-19: Replay source paths match ─────────────────────────────────

    #[test]
    fn cg19_replay_paths_match(suffix in "[a-z_]{3,15}") {
        let path = format!("crates/frankenterm-core/src/replay_{}.rs", suffix);
        prop_assert!(matches_replay_path(&path));
    }

    // ── CG-20: Timeouts ascending ────────────────────────────────────────

    #[test]
    fn cg20_timeouts_ascending(_dummy in 0u8..1) {
        let t1 = GateId::Smoke.timeout_seconds();
        let t2 = GateId::TestSuite.timeout_seconds();
        let t3 = GateId::Regression.timeout_seconds();
        prop_assert!(t1 < t2);
        prop_assert!(t2 < t3);
    }
}
