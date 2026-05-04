//! E2E: onboarding stress capsule dry-run on synthetic environment
//! fixtures (ft-1650n.17).
//!
//! The substrate (`crate::onboarding_stress_capsule`) ships:
//! - 8 OnboardingCheck variants (HooksInstalled, RuntimePolicy,
//!   CargoWrapperSane, RobotOutputAvailable, StoragePathWritable,
//!   ForbiddenSurfaces, MachineProfile, SafeProbeBudget).
//! - CheckOutcome enum (Pass / Fail { remediation, issue_class } /
//!   Skipped { reason }).
//! - IssueClass (MachineLocal / RepoCode).
//! - sanitize_remediation: strips forbidden-command patterns and
//!   replaces with [REDACTED-FORBIDDEN].
//! - compile_report: pure-function entry point.
//!
//! This E2E suite covers the bead's
//! "E2E dry-run on synthetic environment fixtures" acceptance with
//! 5 distinct synthetic profiles, each asserting the full
//! OnboardingReport shape against an expected outcome.

use frankenterm_core::onboarding_stress_capsule::{
    CheckOutcome, IssueClass, OnboardingCheck, OnboardingProbeResults, compile_report,
    sanitize_remediation,
};

// ── Fixture helpers ────────────────────────────────────────────────────

/// Build a probe-results fixture where every required check passes.
/// MachineProfile is optionally skipped per the substrate's contract
/// (the only check that may Skip without blocking eligibility).
fn fixture_all_pass(machine_profile: bool) -> OnboardingProbeResults {
    let mut r = OnboardingProbeResults::new();
    r.record(OnboardingCheck::HooksInstalled, CheckOutcome::Pass)
        .record(OnboardingCheck::RuntimePolicy, CheckOutcome::Pass)
        .record(OnboardingCheck::CargoWrapperSane, CheckOutcome::Pass)
        .record(OnboardingCheck::RobotOutputAvailable, CheckOutcome::Pass)
        .record(OnboardingCheck::StoragePathWritable, CheckOutcome::Pass)
        .record(OnboardingCheck::ForbiddenSurfaces, CheckOutcome::Pass)
        .record(OnboardingCheck::SafeProbeBudget, CheckOutcome::Pass);
    if machine_profile {
        r.record(OnboardingCheck::MachineProfile, CheckOutcome::Pass);
    } else {
        r.record(
            OnboardingCheck::MachineProfile,
            CheckOutcome::Skipped {
                reason: "low-end machine, profile check skipped".into(),
            },
        );
    }
    r
}

/// Build a fixture where multiple machine-local checks fail
/// (operator can fix on this box) and one repo-code check fails
/// (operator should open a PR).
fn fixture_mixed_failures() -> OnboardingProbeResults {
    let mut r = OnboardingProbeResults::new();
    // Machine-local failures (fixable on this box):
    r.record(
        OnboardingCheck::HooksInstalled,
        CheckOutcome::Fail {
            remediation: "run `bash scripts/install-hooks.sh` to install pre-commit + pre-push"
                .into(),
            issue_class: IssueClass::MachineLocal,
        },
    );
    r.record(
        OnboardingCheck::CargoWrapperSane,
        CheckOutcome::Fail {
            remediation:
                "ensure CARGO_TARGET_DIR points to a writable path; ours is /tmp/ft-cc1-target"
                    .into(),
            issue_class: IssueClass::MachineLocal,
        },
    );
    r.record(
        OnboardingCheck::StoragePathWritable,
        CheckOutcome::Fail {
            remediation: "create writable storage dir at ~/.frankenterm/storage".into(),
            issue_class: IssueClass::MachineLocal,
        },
    );
    // Repo-code failure (operator should open a PR):
    r.record(
        OnboardingCheck::ForbiddenSurfaces,
        CheckOutcome::Fail {
            remediation: "remove `#[tokio::test]` from crates/foo/tests/bar.rs and migrate to asupersync_test_macro".into(),
            issue_class: IssueClass::RepoCode,
        },
    );
    // Remaining checks pass.
    r.record(OnboardingCheck::RuntimePolicy, CheckOutcome::Pass)
        .record(OnboardingCheck::RobotOutputAvailable, CheckOutcome::Pass)
        .record(OnboardingCheck::SafeProbeBudget, CheckOutcome::Pass)
        .record(
            OnboardingCheck::MachineProfile,
            CheckOutcome::Skipped {
                reason: "low-end".into(),
            },
        );
    r
}

// ── E2E assertions ─────────────────────────────────────────────────────

/// All-pass synthetic profile yields eligible-for-high-risk-tasks.
/// Even when MachineProfile is Skipped (operator running on a low-
/// end machine), eligibility is unaffected — only Failures block.
#[test]
fn all_pass_profile_yields_eligibility_with_optional_machine_profile_skipped() {
    let probes = fixture_all_pass(false);
    let report = compile_report(&probes);
    assert!(
        report.eligible_for_high_risk_tasks,
        "all-pass with MachineProfile skipped should still be eligible"
    );
    assert!(report.machine_local_failures.is_empty());
    assert!(report.repo_code_failures.is_empty());
    // 7 required checks passed; MachineProfile in skipped.
    assert_eq!(report.passed.len(), 7);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].check, OnboardingCheck::MachineProfile);
    assert!(report.skipped[0].reason.contains("low-end"));
}

#[test]
fn all_pass_profile_with_machine_profile_passing_yields_full_eligibility() {
    let probes = fixture_all_pass(true);
    let report = compile_report(&probes);
    assert!(report.eligible_for_high_risk_tasks);
    // 8 checks passed; nothing skipped.
    assert_eq!(report.passed.len(), 8);
    assert!(report.skipped.is_empty());
}

/// Mixed failures — machine-local + repo-code — block eligibility
/// AND the report cleanly separates the two so the operator knows
/// what to fix locally vs what to PR.
#[test]
fn mixed_failures_block_eligibility_and_separate_classes_ft_1650n_17() {
    let probes = fixture_mixed_failures();
    let report = compile_report(&probes);
    assert!(
        !report.eligible_for_high_risk_tasks,
        "any failure must block eligibility"
    );
    // 3 machine-local failures (HooksInstalled, CargoWrapperSane,
    // StoragePathWritable).
    assert_eq!(report.machine_local_failures.len(), 3);
    let machine_checks: Vec<_> = report
        .machine_local_failures
        .iter()
        .map(|f| f.check)
        .collect();
    assert!(machine_checks.contains(&OnboardingCheck::HooksInstalled));
    assert!(machine_checks.contains(&OnboardingCheck::CargoWrapperSane));
    assert!(machine_checks.contains(&OnboardingCheck::StoragePathWritable));
    // 1 repo-code failure (ForbiddenSurfaces).
    assert_eq!(report.repo_code_failures.len(), 1);
    assert_eq!(
        report.repo_code_failures[0].check,
        OnboardingCheck::ForbiddenSurfaces
    );
    // The 3 unrelated passes survived.
    assert_eq!(report.passed.len(), 3);
}

/// Forbidden-command suppression end-to-end: a probe that returns
/// remediation containing destructive-command patterns has them
/// stripped before they reach the report. Pre-fix, an operator
/// receiving a hint like "run `rm -rf node_modules`" might paste it
/// into a shell; post-fix every forbidden pattern lands as
/// [REDACTED-FORBIDDEN].
#[test]
fn forbidden_remediation_patterns_are_redacted_in_report_ft_1650n_17() {
    let mut probes = OnboardingProbeResults::new();
    // All other checks pass.
    probes
        .record(OnboardingCheck::HooksInstalled, CheckOutcome::Pass)
        .record(OnboardingCheck::RuntimePolicy, CheckOutcome::Pass)
        .record(OnboardingCheck::CargoWrapperSane, CheckOutcome::Pass)
        .record(OnboardingCheck::RobotOutputAvailable, CheckOutcome::Pass)
        .record(OnboardingCheck::ForbiddenSurfaces, CheckOutcome::Pass)
        .record(OnboardingCheck::SafeProbeBudget, CheckOutcome::Pass)
        .record(
            OnboardingCheck::MachineProfile,
            CheckOutcome::Skipped {
                reason: "n/a".into(),
            },
        );
    // Probe supplies a remediation embedding multiple forbidden
    // patterns at once.
    probes.record(
        OnboardingCheck::StoragePathWritable,
        CheckOutcome::Fail {
            remediation:
                "to fix: sudo chmod -R 777 /var/lib/ft && rm -rf old-data && git push --force"
                    .into(),
            issue_class: IssueClass::MachineLocal,
        },
    );
    let report = compile_report(&probes);
    assert!(!report.eligible_for_high_risk_tasks);
    assert_eq!(report.machine_local_failures.len(), 1);
    let r = &report.machine_local_failures[0].remediation;
    // None of the 3 forbidden patterns survive verbatim.
    assert!(!r.contains("sudo "), "sudo prefix leaked: {r}");
    assert!(!r.contains("chmod -R 777"), "chmod -R 777 leaked: {r}");
    assert!(!r.contains("rm -rf"), "rm -rf leaked: {r}");
    assert!(!r.contains("git push --force"), "force-push leaked: {r}");
    // [REDACTED-FORBIDDEN] markers appear.
    assert!(
        r.contains("[REDACTED-FORBIDDEN]"),
        "remediation should carry redaction marker: {r}"
    );
}

/// Empty probe-results — operator hasn't run anything yet — yields
/// a report that is NOT eligible (no positive evidence) but ALSO
/// does not crash. Defends the substrate's "fallback: mark pane
/// ineligible for high-risk tasks until fixed" contract.
#[test]
fn empty_probe_results_yield_ineligible_report_ft_1650n_17() {
    let probes = OnboardingProbeResults::new();
    let report = compile_report(&probes);
    assert!(
        !report.eligible_for_high_risk_tasks,
        "no evidence MUST NOT imply eligibility"
    );
    assert!(report.passed.is_empty());
    assert!(report.machine_local_failures.is_empty());
    assert!(report.repo_code_failures.is_empty());
    // schema_version is set by compile_report regardless.
    assert!(!report.schema_version.is_empty());
}

/// sanitize_remediation called directly produces the same redaction
/// behavior the report path applies. End-to-end test pins the
/// public contract: clean strings unchanged, dirty strings carry
/// [REDACTED-FORBIDDEN] markers.
#[test]
fn sanitize_remediation_pins_redaction_contract_ft_1650n_17() {
    // Clean string passes through unchanged.
    assert_eq!(
        sanitize_remediation("install hooks via scripts/install-hooks.sh"),
        "install hooks via scripts/install-hooks.sh"
    );
    // Each forbidden pattern individually replaced.
    assert!(sanitize_remediation("rm -rf x").contains("[REDACTED-FORBIDDEN]"));
    assert!(sanitize_remediation("rm -fr x").contains("[REDACTED-FORBIDDEN]"));
    assert!(sanitize_remediation("git push --force").contains("[REDACTED-FORBIDDEN]"));
    assert!(sanitize_remediation("git push -f").contains("[REDACTED-FORBIDDEN]"));
    assert!(sanitize_remediation("git commit --no-verify").contains("[REDACTED-FORBIDDEN]"));
    assert!(sanitize_remediation("git reset --hard HEAD").contains("[REDACTED-FORBIDDEN]"));
    assert!(sanitize_remediation("chmod 777 file").contains("[REDACTED-FORBIDDEN]"));
    assert!(sanitize_remediation("chmod -R 777 dir").contains("[REDACTED-FORBIDDEN]"));
    assert!(sanitize_remediation("sudo restart service").contains("[REDACTED-FORBIDDEN]"));
}

/// Report serializes/deserializes cleanly — operators can pass it
/// across an RPC boundary or store as JSONL audit. End-to-end
/// roundtrip preserves every field including the eligibility flag
/// + the per-class failure lists.
#[test]
fn report_serde_roundtrip_preserves_eligibility_and_failure_classes_ft_1650n_17() {
    let probes = fixture_mixed_failures();
    let report = compile_report(&probes);
    let json = serde_json::to_string(&report).expect("serialize");
    let back: frankenterm_core::onboarding_stress_capsule::OnboardingReport =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.eligible_for_high_risk_tasks, false);
    assert_eq!(
        back.machine_local_failures.len(),
        report.machine_local_failures.len()
    );
    assert_eq!(
        back.repo_code_failures.len(),
        report.repo_code_failures.len()
    );
    assert_eq!(back.passed, report.passed);
    assert_eq!(back.skipped, report.skipped);
}
