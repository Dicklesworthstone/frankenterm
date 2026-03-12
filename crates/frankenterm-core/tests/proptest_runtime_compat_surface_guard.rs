//! Property-based tests for the `runtime_compat_surface_guard` module.
//!
//! Covers serde roundtrips and structural invariants for `SurfaceApiEntry`,
//! `UnwrappedCallSite`, `SurfaceGuardCheck`, `RegressionType`,
//! `SurfaceRegression`, and `SurfaceGuardReport`.

use frankenterm_core::runtime_compat_surface_guard::{
    RegressionType, SurfaceApiEntry, SurfaceGuardCheck, SurfaceGuardReport, SurfaceRegression,
    UnwrappedCallSite, standard_guard_checks, standard_surface_entries,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_surface_api_entry() -> impl Strategy<Value = SurfaceApiEntry> {
    (
        "[A-Za-z_:]{3,30}",
        prop_oneof![Just("Keep"), Just("Replace"), Just("Retire")],
        "[a-z ]{5,40}",
        proptest::option::of("[a-z ]{5,30}"),
    )
        .prop_map(
            |(api_name, disposition, rationale, replacement)| SurfaceApiEntry {
                api_name,
                disposition: disposition.to_string(),
                rationale,
                replacement,
            },
        )
}

fn arb_unwrapped_call_site() -> impl Strategy<Value = UnwrappedCallSite> {
    (
        "[a-z_/]{5,30}\\.rs",
        "[a-z_:]{5,25}",
        "[a-z_:]{5,30}",
        any::<bool>(),
    )
        .prop_map(
            |(file_path, api_used, wrapper_available, in_allowed_file)| UnwrappedCallSite {
                file_path,
                api_used,
                wrapper_available,
                in_allowed_file,
            },
        )
}

fn arb_surface_guard_check() -> impl Strategy<Value = SurfaceGuardCheck> {
    (
        "SGC-[0-9]{2}-[A-Za-z]{3,15}",
        "[A-Za-z_:]{3,20}",
        prop_oneof![Just("Keep"), Just("Replace"), Just("Retire")],
        any::<bool>(),
        0..100usize,
        0..100usize,
        any::<bool>(),
    )
        .prop_map(
            |(check_id, api_name, disposition, wrapper_exists, wrapped, unwrapped, compliant)| {
                SurfaceGuardCheck {
                    check_id,
                    api_name,
                    disposition: disposition.to_string(),
                    wrapper_exists,
                    call_sites_wrapped: wrapped,
                    call_sites_unwrapped: unwrapped,
                    compliant,
                }
            },
        )
}

fn arb_regression_type() -> impl Strategy<Value = RegressionType> {
    prop_oneof![
        Just(RegressionType::DirectRuntimeImport),
        Just(RegressionType::UnwrappedApiCall),
        Just(RegressionType::DispositionViolation),
        Just(RegressionType::ShimBypass),
    ]
}

fn arb_surface_regression() -> impl Strategy<Value = SurfaceRegression> {
    (
        "SR-[0-9]{3}",
        arb_regression_type(),
        "[a-z_/]{5,25}\\.rs",
        "[a-z ]{5,40}",
        prop_oneof![Just("warning"), Just("error"), Just("critical")],
    )
        .prop_map(
            |(regression_id, regression_type, file_path, description, severity)| {
                SurfaceRegression {
                    regression_id,
                    regression_type,
                    file_path,
                    description,
                    severity: severity.to_string(),
                }
            },
        )
}

fn arb_surface_guard_report() -> impl Strategy<Value = SurfaceGuardReport> {
    (
        "[a-z-]{3,15}",
        any::<u64>(),
        proptest::collection::vec(arb_surface_api_entry(), 0..5),
        proptest::collection::vec(arb_surface_guard_check(), 0..5),
        proptest::collection::vec(arb_surface_regression(), 0..3),
        proptest::collection::vec(arb_unwrapped_call_site(), 0..3),
    )
        .prop_map(|(report_id, ts, entries, checks, regressions, sites)| {
            let total = checks.len();
            let compliant_count = checks.iter().filter(|c| c.compliant).count();
            let compliance_rate = if total == 0 {
                1.0
            } else {
                compliant_count as f64 / total as f64
            };
            let overall_compliant = regressions.is_empty() && checks.iter().all(|c| c.compliant);
            SurfaceGuardReport {
                report_id,
                generated_at_ms: ts,
                surface_entries: entries,
                guard_checks: checks,
                regressions,
                unwrapped_call_sites: sites,
                overall_compliant,
                compliance_rate,
            }
        })
}

// =========================================================================
// SurfaceApiEntry serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn surface_api_entry_serde_roundtrip(entry in arb_surface_api_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let back: SurfaceApiEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.api_name, &entry.api_name);
        prop_assert_eq!(&back.disposition, &entry.disposition);
        prop_assert_eq!(&back.rationale, &entry.rationale);
        prop_assert_eq!(&back.replacement, &entry.replacement);
    }
}

// =========================================================================
// UnwrappedCallSite serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn unwrapped_call_site_serde_roundtrip(site in arb_unwrapped_call_site()) {
        let json = serde_json::to_string(&site).unwrap();
        let back: UnwrappedCallSite = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.file_path, &site.file_path);
        prop_assert_eq!(&back.api_used, &site.api_used);
        prop_assert_eq!(&back.wrapper_available, &site.wrapper_available);
        prop_assert_eq!(back.in_allowed_file, site.in_allowed_file);
    }
}

// =========================================================================
// SurfaceGuardCheck serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn surface_guard_check_serde_roundtrip(check in arb_surface_guard_check()) {
        let json = serde_json::to_string(&check).unwrap();
        let back: SurfaceGuardCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.check_id, &check.check_id);
        prop_assert_eq!(&back.api_name, &check.api_name);
        prop_assert_eq!(&back.disposition, &check.disposition);
        prop_assert_eq!(back.wrapper_exists, check.wrapper_exists);
        prop_assert_eq!(back.call_sites_wrapped, check.call_sites_wrapped);
        prop_assert_eq!(back.call_sites_unwrapped, check.call_sites_unwrapped);
        prop_assert_eq!(back.compliant, check.compliant);
    }
}

// =========================================================================
// RegressionType serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn regression_type_serde_roundtrip(rt in arb_regression_type()) {
        let json = serde_json::to_string(&rt).unwrap();
        let back: RegressionType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, rt);
    }
}

// =========================================================================
// SurfaceRegression serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn surface_regression_serde_roundtrip(r in arb_surface_regression()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: SurfaceRegression = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.regression_id, &r.regression_id);
        prop_assert_eq!(back.regression_type, r.regression_type);
        prop_assert_eq!(&back.file_path, &r.file_path);
        prop_assert_eq!(&back.description, &r.description);
        prop_assert_eq!(&back.severity, &r.severity);
    }
}

// =========================================================================
// SurfaceGuardReport serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn surface_guard_report_serde_roundtrip(report in arb_surface_guard_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: SurfaceGuardReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.report_id, &report.report_id);
        prop_assert_eq!(back.generated_at_ms, report.generated_at_ms);
        prop_assert_eq!(back.surface_entries.len(), report.surface_entries.len());
        prop_assert_eq!(back.guard_checks.len(), report.guard_checks.len());
        prop_assert_eq!(back.regressions.len(), report.regressions.len());
        prop_assert_eq!(back.unwrapped_call_sites.len(), report.unwrapped_call_sites.len());
        prop_assert_eq!(back.overall_compliant, report.overall_compliant);
        // f64 compliance_rate: use tolerance
        let diff = (back.compliance_rate - report.compliance_rate).abs();
        prop_assert!(diff < 0.001, "compliance_rate drift: {}", diff);
    }
}

// =========================================================================
// Standard surface entries structural invariants
// =========================================================================

#[test]
fn standard_surface_entries_has_15() {
    let entries = standard_surface_entries();
    assert_eq!(entries.len(), 15);
}

#[test]
fn standard_surface_entries_all_have_known_dispositions() {
    for entry in &standard_surface_entries() {
        assert!(
            ["Keep", "Replace", "Retire"].contains(&entry.disposition.as_str()),
            "unexpected disposition: {}",
            entry.disposition
        );
    }
}

#[test]
fn standard_guard_checks_has_15() {
    let checks = standard_guard_checks();
    assert_eq!(checks.len(), 15);
}

// =========================================================================
// Finalize invariants
// =========================================================================

proptest! {
    #[test]
    fn finalize_compliance_rate_in_range(
        checks in proptest::collection::vec(arb_surface_guard_check(), 0..10),
        regressions in proptest::collection::vec(arb_surface_regression(), 0..5),
    ) {
        let mut report = SurfaceGuardReport::new("test", 0);
        for c in checks {
            report.add_guard_check(c);
        }
        for r in regressions {
            report.add_regression(r);
        }
        report.finalize();
        prop_assert!(report.compliance_rate >= 0.0);
        prop_assert!(report.compliance_rate <= 1.0);
    }

    #[test]
    fn finalize_overall_compliant_iff_no_regressions_and_all_compliant(
        checks in proptest::collection::vec(arb_surface_guard_check(), 1..5),
    ) {
        let mut report = SurfaceGuardReport::new("test", 0);
        for c in checks {
            report.add_guard_check(c);
        }
        // No regressions added
        report.finalize();
        let all_compliant = report.guard_checks.iter().all(|c| c.compliant);
        prop_assert_eq!(report.overall_compliant, all_compliant);
    }
}

// =========================================================================
// Summary rendering
// =========================================================================

proptest! {
    #[test]
    fn summary_contains_report_id(report in arb_surface_guard_report()) {
        let summary = report.summary();
        prop_assert!(summary.contains(&report.report_id));
    }

    #[test]
    fn summary_contains_compliance_status(report in arb_surface_guard_report()) {
        let summary = report.summary();
        let has_status = summary.contains("COMPLIANT") || summary.contains("NON-COMPLIANT");
        prop_assert!(has_status, "summary should contain compliance status");
    }
}

// =========================================================================
// Regressions-by-type grouping
// =========================================================================

proptest! {
    #[test]
    fn regressions_by_type_total_matches(
        regressions in proptest::collection::vec(arb_surface_regression(), 0..10),
    ) {
        let mut report = SurfaceGuardReport::new("test", 0);
        for r in &regressions {
            report.add_regression(r.clone());
        }
        let grouped = report.regressions_by_type();
        let total: usize = grouped.values().map(|v| v.len()).sum();
        prop_assert_eq!(total, regressions.len());
    }
}
