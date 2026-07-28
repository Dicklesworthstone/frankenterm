//! Golden + property tests for the rch_admission operator surface (ft-69gwh.5
//! live collector / .9 `ft doctor --rch-admission`).
//!
//! The collector must populate from REAL admission analysis — a parsed cargo
//! command, not stub/placeholder data — and the doctor report must surface that
//! accurate state. These pin:
//!   1. the analyzer derives real fields from the command (golden + property),
//!   2. the collector observation carries the real analysis explanation (not a
//!      placeholder),
//!   3. the report preserves the analyzed state the operator sees.

use proptest::prelude::*;

use frankenterm_core::rch_admission::{
    RchAdmissionCollectorInput, RchAdmissionCommandDiagnostic, analyze_rch_admission_cargo_command,
    build_rch_admission_report,
};

/// No environment overrides — the analyzer reads only the command.
fn no_env() -> std::iter::Empty<(&'static str, &'static str)> {
    std::iter::empty::<(&'static str, &'static str)>()
}

// ---------------------------------------------------------------------------
// 1. Golden: the analyzer derives real fields from a known command
// ---------------------------------------------------------------------------

#[test]
fn analyzer_derives_real_fields_from_command_golden() {
    let analysis = analyze_rch_admission_cargo_command(
        "cargo test -p frankenterm-core --lib mytest -j 4",
        no_env(),
        None,
    );
    assert_eq!(
        analysis.cargo_subcommand.as_deref(),
        Some("test"),
        "subcommand must be parsed from the command, not stubbed"
    );
    assert_eq!(
        analysis.package_scope,
        vec!["frankenterm-core".to_string()],
        "the -p package must be captured from the command"
    );
    assert_eq!(
        analysis.explicit_jobs,
        Some(4),
        "the -j job count must be captured from the command"
    );
    assert!(
        analysis.would_intercept,
        "a cargo command is intercepted by the rch hook"
    );
    assert!(
        analysis.raw.contains("cargo test -p frankenterm-core"),
        "the analysis preserves the real command text"
    );
    assert!(
        !analysis.explanation.is_empty(),
        "a real analysis produces a non-empty explanation"
    );
}

// ---------------------------------------------------------------------------
// 2. Property: the analyzer reflects arbitrary commands (real parse)
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn prop_analyzer_captures_package_and_jobs(
        pkg in "[a-z][a-z0-9_]{0,15}",
        jobs in 1u32..=64,
    ) {
        let cmd = format!("cargo test -p {pkg} -j {jobs}");
        let a = analyze_rch_admission_cargo_command(&cmd, no_env(), None);
        prop_assert_eq!(a.cargo_subcommand.as_deref(), Some("test"));
        prop_assert!(
            a.package_scope.contains(&pkg),
            "package {:?} must be captured from the command, got {:?}",
            pkg,
            a.package_scope
        );
        prop_assert_eq!(a.explicit_jobs, Some(jobs));
        prop_assert!(a.raw.contains(&pkg), "raw must preserve the command package");
    }

    /// Analysis is deterministic — the same command yields the same observation
    /// summary (no time-varying stub).
    #[test]
    fn prop_analysis_is_deterministic(pkg in "[a-z][a-z0-9_]{0,15}") {
        let cmd = format!("cargo test -p {pkg}");
        let a1 = analyze_rch_admission_cargo_command(&cmd, no_env(), None);
        let a2 = analyze_rch_admission_cargo_command(&cmd, no_env(), None);
        prop_assert_eq!(a1.explanation, a2.explanation);
        prop_assert_eq!(a1.package_scope, a2.package_scope);
    }
}

// ---------------------------------------------------------------------------
// 3. The collector observation carries the real analysis (not stub)
// ---------------------------------------------------------------------------

#[test]
fn collector_observation_carries_real_analysis_not_stub() {
    let analysis =
        analyze_rch_admission_cargo_command("cargo test -p frankenterm-core", no_env(), None);
    let obs = analysis.collector_observation();

    assert_eq!(
        obs.summary, analysis.explanation,
        "the observation summary must be the real analysis explanation"
    );
    assert!(
        !obs.summary.is_empty(),
        "a real observation has a non-empty summary"
    );
    assert_eq!(
        obs.source_id, "rch_admission.cargo_command_analysis",
        "the observation must be attributed to the real analyzer source"
    );
    let lowered = obs.summary.to_ascii_lowercase();
    assert!(
        !lowered.contains("placeholder") && !lowered.contains("stub"),
        "a real-analysis observation must not contain stub/placeholder text: {:?}",
        obs.summary
    );
}

#[test]
fn collector_input_populates_from_real_analysis() {
    let analysis =
        analyze_rch_admission_cargo_command("cargo test -p frankenterm-core -j 4", no_env(), None);
    let input = RchAdmissionCollectorInput::new(
        1_000,
        "doctor",
        RchAdmissionCommandDiagnostic::new("seed"),
    )
    .with_cargo_command_analysis(&analysis);

    // The real observation was pushed (not dropped, not replaced by a stub).
    assert_eq!(
        input.collector_observations.len(),
        1,
        "the analysis observation must be recorded"
    );
    assert_eq!(
        input.collector_observations[0].summary,
        analysis.explanation
    );
    // The collected job count reflects the real analysis.
    assert_eq!(input.cargo_jobs, analysis.explicit_jobs);
    assert_eq!(input.cargo_jobs, Some(4));
}

// ---------------------------------------------------------------------------
// 4. The doctor report surfaces the accurate collected state
// ---------------------------------------------------------------------------

#[test]
fn doctor_report_surfaces_accurate_collected_state() {
    let analysis =
        analyze_rch_admission_cargo_command("cargo test -p frankenterm-core -j 4", no_env(), None);
    let input = RchAdmissionCollectorInput::new(
        1_000,
        "doctor",
        RchAdmissionCommandDiagnostic::new("seed"),
    )
    .with_cargo_command_analysis(&analysis);

    let report = build_rch_admission_report(&input);

    // The doctor surfaces the real analyzed job count, not a stub default.
    assert_eq!(
        report.cargo_jobs,
        Some(4),
        "the doctor report must surface the real analyzed cargo job count"
    );
}

// ---------------------------------------------------------------------------
// 5. Negative & optional-field cases: the analyzer is a real parser, not a
//    blanket "always intercept / always populated" stub.
// ---------------------------------------------------------------------------

#[test]
fn a_non_cargo_command_is_not_intercepted_and_carries_no_cargo_fields() {
    // A real parser must distinguish non-cargo commands rather than
    // blanket-intercepting; the trailing `-j 4` is NOT a cargo job count here.
    let a = analyze_rch_admission_cargo_command("ls -la /tmp -j 4", no_env(), None);
    assert!(
        !a.would_intercept,
        "a non-cargo command must not be intercepted"
    );
    assert_eq!(
        a.cargo_subcommand, None,
        "a non-cargo command has no cargo subcommand"
    );
    assert!(
        a.package_scope.is_empty(),
        "a non-cargo command has no package scope"
    );
    assert_eq!(
        a.explicit_jobs, None,
        "a bare -j on a non-cargo command is not a cargo job count"
    );
    assert!(
        a.raw.contains("ls -la"),
        "the raw command text is still preserved"
    );
}

#[test]
fn a_cargo_command_without_p_or_j_has_absent_optional_fields_not_defaults() {
    // Optional fields must be genuinely absent (None / empty), never stubbed
    // with placeholder defaults.
    let a = analyze_rch_admission_cargo_command("cargo build", no_env(), None);
    assert_eq!(a.cargo_subcommand.as_deref(), Some("build"));
    assert!(
        a.would_intercept,
        "any cargo command is intercepted by the hook"
    );
    assert!(
        a.package_scope.is_empty(),
        "no -p means an empty package scope"
    );
    assert_eq!(a.explicit_jobs, None, "no -j means no explicit job count");
}

#[test]
fn multiple_p_flags_are_all_captured_in_package_scope() {
    let a = analyze_rch_admission_cargo_command("cargo test -p alpha -p beta", no_env(), None);
    assert!(
        a.package_scope.contains(&"alpha".to_string()),
        "first -p package must be captured, got {:?}",
        a.package_scope
    );
    assert!(
        a.package_scope.contains(&"beta".to_string()),
        "second -p package must be captured, got {:?}",
        a.package_scope
    );
}

// ---------------------------------------------------------------------------
// 6. Doctor report reflects ACTUAL collected state (real-not-stub at the
//    operator surface): absent when nothing was collected, present when it was.
// ---------------------------------------------------------------------------

#[test]
fn doctor_report_reports_no_job_count_when_nothing_was_collected() {
    // Contrast with doctor_report_surfaces_accurate_collected_state: with no
    // analysis applied, the report must surface None — not a stubbed default.
    let input = RchAdmissionCollectorInput::new(
        1_000,
        "doctor",
        RchAdmissionCommandDiagnostic::new("seed"),
    );
    let report = build_rch_admission_report(&input);
    assert_eq!(
        report.cargo_jobs, None,
        "with nothing collected the doctor must report no job count, not a default"
    );
}

#[test]
fn doctor_report_surfaces_a_directly_collected_job_count() {
    // The report faithfully surfaces a job count collected from any source.
    let input = RchAdmissionCollectorInput::new(
        1_000,
        "doctor",
        RchAdmissionCommandDiagnostic::new("seed"),
    )
    .with_cargo_jobs(7);
    let report = build_rch_admission_report(&input);
    assert_eq!(
        report.cargo_jobs,
        Some(7),
        "the doctor must surface the collected job count"
    );
}
