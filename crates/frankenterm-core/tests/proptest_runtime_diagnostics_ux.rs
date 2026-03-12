//! Property-based tests for the `runtime_diagnostics_ux` module.
//!
//! Covers serde roundtrips, structural invariants, and rendering contracts
//! for `DiagnosticTemplate`, `RemediationStep`, `RenderedDiagnostic`,
//! `TemplateCheck`, and `CertificationReport`.

use frankenterm_core::runtime_diagnostics_ux::{
    render_diagnostic, CertificationReport, DiagnosticCatalog, DiagnosticTemplate,
    RemediationStep, RenderedDiagnostic, TemplateCheck,
};
use frankenterm_core::runtime_telemetry::FailureClass;
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_failure_class() -> impl Strategy<Value = FailureClass> {
    prop_oneof![
        Just(FailureClass::Transient),
        Just(FailureClass::Permanent),
        Just(FailureClass::Degraded),
        Just(FailureClass::Overload),
        Just(FailureClass::Corruption),
        Just(FailureClass::Timeout),
        Just(FailureClass::Panic),
        Just(FailureClass::Deadlock),
        Just(FailureClass::Safety),
        Just(FailureClass::Configuration),
    ]
}

fn arb_remediation_step() -> impl Strategy<Value = RemediationStep> {
    (
        1..100u32,
        "[a-z ]{1,50}",
        proptest::option::of("[a-z ]{1,30}"),
        "[a-z ]{1,50}",
        any::<bool>(),
    )
        .prop_map(|(step, instruction, command, expected_outcome, robot_safe)| {
            RemediationStep {
                step,
                instruction,
                command,
                expected_outcome,
                robot_safe,
            }
        })
}

fn arb_diagnostic_template() -> impl Strategy<Value = DiagnosticTemplate> {
    (
        arb_failure_class(),
        "[a-z ]{1,60}",
        "[a-z ]{1,60}",
        proptest::collection::vec(arb_remediation_step(), 1..5),
        "[a-z ]{1,40}",
        "RT-[A-Z]{3,10}-[0-9]{3}",
        any::<bool>(),
    )
        .prop_map(
            |(failure_class, cause_summary, user_impact, remediation_steps, escalation_hint, error_code, certified)| {
                DiagnosticTemplate {
                    failure_class,
                    cause_summary,
                    user_impact,
                    remediation_steps,
                    escalation_hint,
                    error_code,
                    certified,
                }
            },
        )
}

fn arb_template_check() -> impl Strategy<Value = TemplateCheck> {
    (
        "[A-Z][a-z]{3,15}",
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(failure_class, has_cause, has_impact, has_remediation, has_robot_safe_step, has_command, has_escalation, has_error_code, is_certified, passed)| {
                TemplateCheck {
                    failure_class,
                    has_cause,
                    has_impact,
                    has_remediation,
                    has_robot_safe_step,
                    has_command,
                    has_escalation,
                    has_error_code,
                    is_certified,
                    passed,
                }
            },
        )
}

fn arb_certification_report() -> impl Strategy<Value = CertificationReport> {
    (
        proptest::collection::vec(arb_template_check(), 0..10),
        proptest::collection::vec("[A-Z][a-z]{3,15}", 0..5),
        any::<bool>(),
    )
        .prop_map(|(checks, missing_classes, overall_pass)| {
            let pass_count = checks.iter().filter(|c| c.passed).count();
            let total_checks = checks.len();
            CertificationReport {
                checks,
                missing_classes,
                overall_pass,
                pass_count,
                total_checks,
            }
        })
}

fn arb_rendered_diagnostic() -> impl Strategy<Value = RenderedDiagnostic> {
    (
        "RT-[A-Z]{3,10}-[0-9]{3}",
        "[a-z ]{10,80}",
        "\\{[a-z \":,]{5,60}\\}",
    )
        .prop_map(|(error_code, human_text, robot_json)| RenderedDiagnostic {
            error_code,
            human_text,
            robot_json,
        })
}

// =========================================================================
// RemediationStep serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn remediation_step_serde_roundtrip(step in arb_remediation_step()) {
        let json = serde_json::to_string(&step).unwrap();
        let back: RemediationStep = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.step, step.step);
        prop_assert_eq!(&back.instruction, &step.instruction);
        prop_assert_eq!(&back.command, &step.command);
        prop_assert_eq!(&back.expected_outcome, &step.expected_outcome);
        prop_assert_eq!(back.robot_safe, step.robot_safe);
    }
}

// =========================================================================
// DiagnosticTemplate serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn diagnostic_template_serde_roundtrip(template in arb_diagnostic_template()) {
        let json = serde_json::to_string(&template).unwrap();
        let back: DiagnosticTemplate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.cause_summary, &template.cause_summary);
        prop_assert_eq!(&back.user_impact, &template.user_impact);
        prop_assert_eq!(&back.error_code, &template.error_code);
        prop_assert_eq!(back.certified, template.certified);
        prop_assert_eq!(back.remediation_steps.len(), template.remediation_steps.len());
        prop_assert_eq!(&back.escalation_hint, &template.escalation_hint);
    }
}

// =========================================================================
// TemplateCheck serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn template_check_serde_roundtrip(check in arb_template_check()) {
        let json = serde_json::to_string(&check).unwrap();
        let back: TemplateCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.failure_class, &check.failure_class);
        prop_assert_eq!(back.has_cause, check.has_cause);
        prop_assert_eq!(back.has_impact, check.has_impact);
        prop_assert_eq!(back.has_remediation, check.has_remediation);
        prop_assert_eq!(back.has_robot_safe_step, check.has_robot_safe_step);
        prop_assert_eq!(back.has_command, check.has_command);
        prop_assert_eq!(back.has_escalation, check.has_escalation);
        prop_assert_eq!(back.has_error_code, check.has_error_code);
        prop_assert_eq!(back.is_certified, check.is_certified);
        prop_assert_eq!(back.passed, check.passed);
    }
}

// =========================================================================
// CertificationReport serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn certification_report_serde_roundtrip(report in arb_certification_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: CertificationReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.overall_pass, report.overall_pass);
        prop_assert_eq!(back.pass_count, report.pass_count);
        prop_assert_eq!(back.total_checks, report.total_checks);
        prop_assert_eq!(back.checks.len(), report.checks.len());
        prop_assert_eq!(back.missing_classes.len(), report.missing_classes.len());
    }
}

// =========================================================================
// RenderedDiagnostic serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn rendered_diagnostic_serde_roundtrip(rd in arb_rendered_diagnostic()) {
        let json = serde_json::to_string(&rd).unwrap();
        let back: RenderedDiagnostic = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.error_code, &rd.error_code);
        prop_assert_eq!(&back.human_text, &rd.human_text);
        prop_assert_eq!(&back.robot_json, &rd.robot_json);
    }
}

// =========================================================================
// Standard catalog structural invariants
// =========================================================================

#[test]
fn standard_catalog_has_10_templates() {
    let catalog = DiagnosticCatalog::standard();
    assert_eq!(catalog.count(), 10);
}

#[test]
fn standard_catalog_all_failure_classes_covered() {
    let catalog = DiagnosticCatalog::standard();
    let classes = [
        FailureClass::Transient,
        FailureClass::Permanent,
        FailureClass::Degraded,
        FailureClass::Overload,
        FailureClass::Corruption,
        FailureClass::Timeout,
        FailureClass::Panic,
        FailureClass::Deadlock,
        FailureClass::Safety,
        FailureClass::Configuration,
    ];
    for fc in &classes {
        assert!(
            catalog.lookup(fc).is_some(),
            "missing template for {:?}",
            fc
        );
    }
}

#[test]
fn standard_catalog_certification_passes() {
    let catalog = DiagnosticCatalog::standard();
    let report = CertificationReport::certify(&catalog);
    assert!(report.overall_pass);
    assert_eq!(report.pass_count, 10);
    assert!(report.missing_classes.is_empty());
}

// =========================================================================
// Rendering contracts
// =========================================================================

proptest! {
    #[test]
    fn render_human_always_contains_error_code(template in arb_diagnostic_template()) {
        let rendered = render_diagnostic(&template);
        prop_assert!(
            rendered.human_text.contains(&template.error_code),
            "human text should contain error code"
        );
    }

    #[test]
    fn render_robot_is_valid_json_for_any_template(template in arb_diagnostic_template()) {
        let rendered = render_diagnostic(&template);
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&rendered.robot_json);
        prop_assert!(parsed.is_ok(), "robot JSON should parse: {:?}", rendered.robot_json);
    }

    #[test]
    fn render_robot_contains_error_code_field(template in arb_diagnostic_template()) {
        let rendered = render_diagnostic(&template);
        let parsed: serde_json::Value = serde_json::from_str(&rendered.robot_json).unwrap();
        prop_assert_eq!(
            parsed["error_code"].as_str().unwrap(),
            template.error_code.as_str()
        );
    }

    #[test]
    fn render_robot_remediation_count_matches(template in arb_diagnostic_template()) {
        let rendered = render_diagnostic(&template);
        let parsed: serde_json::Value = serde_json::from_str(&rendered.robot_json).unwrap();
        let steps = parsed["remediation"].as_array().unwrap();
        prop_assert_eq!(steps.len(), template.remediation_steps.len());
    }
}

// =========================================================================
// Catalog lookup invariants
// =========================================================================

proptest! {
    #[test]
    fn catalog_lookup_round_consistent(fc in arb_failure_class()) {
        let catalog = DiagnosticCatalog::standard();
        let template = catalog.lookup(&fc);
        prop_assert!(template.is_some(), "standard catalog should cover {:?}", fc);
        prop_assert_eq!(template.unwrap().failure_class, fc);
    }

    #[test]
    fn robot_safe_commands_are_subset_of_all_commands(fc in arb_failure_class()) {
        let catalog = DiagnosticCatalog::standard();
        let robot_cmds = catalog.robot_safe_commands(&fc);
        let template = catalog.lookup(&fc).unwrap();
        let all_cmds: Vec<String> = template
            .remediation_steps
            .iter()
            .filter_map(|s| s.command.clone())
            .collect();
        for cmd in &robot_cmds {
            prop_assert!(
                all_cmds.contains(cmd),
                "robot-safe command {:?} not in all commands",
                cmd
            );
        }
    }
}

// =========================================================================
// Certification report rendering
// =========================================================================

proptest! {
    #[test]
    fn certification_summary_contains_result_line(report in arb_certification_report()) {
        let summary = report.render_summary();
        let has_certified = summary.contains("CERTIFIED") || summary.contains("NOT CERTIFIED");
        prop_assert!(has_certified, "summary should contain certification result");
    }

    #[test]
    fn certification_summary_contains_pass_count(report in arb_certification_report()) {
        let summary = report.render_summary();
        let expected = format!("{}/{} pass", report.pass_count, report.total_checks);
        prop_assert!(
            summary.contains(&expected),
            "summary should contain pass count: {}",
            expected
        );
    }
}

// =========================================================================
// Edge cases
// =========================================================================

#[test]
fn empty_catalog_certification_fails_for_all() {
    let catalog = DiagnosticCatalog::from_templates(Vec::new());
    let report = CertificationReport::certify(&catalog);
    assert!(!report.overall_pass);
    assert_eq!(report.missing_classes.len(), 10);
    assert_eq!(report.checks.len(), 0);
}

#[test]
fn template_with_no_robot_safe_steps_fails_certification() {
    let template = DiagnosticTemplate {
        failure_class: FailureClass::Timeout,
        cause_summary: "test cause".into(),
        user_impact: "test impact".into(),
        remediation_steps: vec![RemediationStep {
            step: 1,
            instruction: "do something".into(),
            command: Some("ft check".into()),
            expected_outcome: "something happens".into(),
            robot_safe: false, // no robot-safe step
        }],
        escalation_hint: "escalate".into(),
        error_code: "RT-TEST-001".into(),
        certified: true,
    };

    let mut all = frankenterm_core::runtime_diagnostics_ux::standard_diagnostic_templates();
    // Replace the timeout template
    all.retain(|t| t.failure_class != FailureClass::Timeout);
    all.push(template);
    let catalog = DiagnosticCatalog::from_templates(all);
    let report = CertificationReport::certify(&catalog);
    assert!(!report.overall_pass);
}

#[test]
fn remediation_step_with_all_none_fields_roundtrips() {
    let step = RemediationStep {
        step: 1,
        instruction: String::new(),
        command: None,
        expected_outcome: String::new(),
        robot_safe: false,
    };
    let json = serde_json::to_string(&step).unwrap();
    let back: RemediationStep = serde_json::from_str(&json).unwrap();
    assert_eq!(back.step, 1);
    assert!(back.command.is_none());
}

#[test]
fn covered_classes_matches_template_count() {
    let catalog = DiagnosticCatalog::standard();
    assert_eq!(catalog.covered_classes().len(), catalog.count());
}
