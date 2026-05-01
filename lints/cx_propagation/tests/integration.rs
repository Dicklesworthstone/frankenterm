//! Integration tests for the cx-propagation analyzer.
//!
//! Each fixture under `tests/fixtures/` exercises one shape of the
//! lint. The tests run [`cx_propagation_lint::audit_dir`] against
//! the fixtures directory and assert on the produced
//! `AuditReport` shape.
//!
//! **Bead:** ft-t9a6q.1.

use cx_propagation_lint::{FindingReason, audit_dir};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn audit_runs_against_fixture_corpus() {
    let report = audit_dir(&fixtures_dir()).expect("audit_dir succeeds");
    assert!(report.total_pub_async_sites > 0);
}

#[test]
fn covered_only_fixture_produces_no_findings() {
    let report = audit_dir(&fixtures_dir()).unwrap();
    let covered_only_findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| {
            f.rel_path
                .file_name()
                .map(|n| n == "covered_only.rs")
                .unwrap_or(false)
        })
        .collect();
    assert!(
        covered_only_findings.is_empty(),
        "covered_only.rs must produce no findings, got {covered_only_findings:?}"
    );
}

#[test]
fn uncovered_only_fixture_produces_three_findings() {
    let report = audit_dir(&fixtures_dir()).unwrap();
    let uncovered_only_findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| {
            f.rel_path
                .file_name()
                .map(|n| n == "uncovered_only.rs")
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        uncovered_only_findings.len(),
        3,
        "uncovered_only.rs must produce 3 findings, got {uncovered_only_findings:#?}"
    );
    let names: Vec<&str> = uncovered_only_findings
        .iter()
        .map(|f| f.fn_name.as_str())
        .collect();
    assert!(names.contains(&"no_params"));
    assert!(names.contains(&"unrelated"));
    assert!(names.contains(&"shadowed_name"));
}

#[test]
fn mixed_fixture_filters_covered_and_skips_test_module() {
    let report = audit_dir(&fixtures_dir()).unwrap();
    let mixed_findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| {
            f.rel_path
                .file_name()
                .map(|n| n == "mixed.rs")
                .unwrap_or(false)
        })
        .collect();

    let names: Vec<&str> = mixed_findings.iter().map(|f| f.fn_name.as_str()).collect();
    assert!(names.contains(&"bad"), "free uncovered fn flagged");
    assert!(
        names.contains(&"inherent_bad"),
        "impl uncovered method flagged"
    );
    assert!(
        !names.contains(&"test_only_uncovered"),
        "#[cfg(test)] mod must be skipped"
    );
    assert!(
        !names.contains(&"private_uncovered"),
        "private async fn must be skipped"
    );
    assert!(!names.contains(&"ok"), "covered fn must not be flagged");
    assert!(
        !names.contains(&"inherent_ok"),
        "covered impl method must not be flagged"
    );
}

#[test]
fn exempt_file_produces_no_findings_even_when_uncovered() {
    let report = audit_dir(&fixtures_dir()).unwrap();
    let runtime_findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| {
            f.rel_path
                .file_name()
                .map(|n| n == "runtime_async.rs")
                .unwrap_or(false)
        })
        .collect();
    assert!(
        runtime_findings.is_empty(),
        "runtime_async.rs is in EXEMPT_FILES — must not report findings, got {runtime_findings:?}"
    );
    assert!(report.exempt_file_sites >= 2);
}

#[test]
fn report_summary_arithmetic_is_consistent() {
    let report = audit_dir(&fixtures_dir()).unwrap();
    let accounted = report.exempt_file_sites
        + report.wrapper_exempt_sites
        + report.covered_sites
        + report.uncovered_count();
    assert_eq!(
        report.total_pub_async_sites, accounted,
        "every pub async fn site must land in exactly one bucket"
    );
}

#[test]
fn finding_reason_tags_match_finding_kind() {
    let report = audit_dir(&fixtures_dir()).unwrap();
    for f in &report.findings {
        assert!(
            matches!(
                f.reason,
                FindingReason::MissingCxOrRuntimeProof | FindingReason::StaleWrapperExemption
            ),
            "every finding has a known reason"
        );
    }
}

#[test]
fn audit_against_real_core_src_returns_a_report() {
    // Smoke-test: the analyzer runs against the actual frankenterm-core
    // src tree without panicking. Doesn't assert on the count (the
    // count is what the burn-down dashboard tracks under ft-t9a6q.2).
    let core_src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/frankenterm-core/src");
    if !core_src.is_dir() {
        return;
    }
    let report = audit_dir(&core_src).expect("audit_dir runs against real core");
    assert!(report.total_pub_async_sites > 0);
}
