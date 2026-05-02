use frankenterm_core::render_audit_driver::{
    AuditDiagnosticRow, render_diagnostic_report, render_diagnostic_report_jsonl,
    run_audit_on_sources,
};
use frankenterm_core::render_call_graph_audit::AuditConfig;
use frankenterm_core::render_call_graph_populator::PopulatorConfig;

fn source(path: &str, body: &str) -> (String, String) {
    (path.to_string(), body.to_string())
}

#[test]
fn render_audit_driver_reports_transitive_mutation_guard_as_ci_diagnostic() {
    let paint = r#"
fn paint_impl() {
    draw_cells();
}
"#;
    let helpers = r#"
fn draw_cells() {
    acquire_mutation_guard();
}

fn acquire_mutation_guard() {
    let _guard = triple_buffer.write();
}
"#;

    let report = run_audit_on_sources(
        &[
            source("termwindow/render/paint.rs", paint),
            source("termwindow/render/helpers.rs", helpers),
        ],
        &PopulatorConfig::default(),
        AuditConfig::default(),
    );

    assert_eq!(report.violation_count(), 1);
    assert!(report.is_release_blocker());

    let human = render_diagnostic_report(&report);
    assert!(human.contains("RENDER AUDIT: FAILED"));
    assert!(human.contains("Violation #1"));
    assert!(human.contains("fn paint_impl"));
    assert!(human.contains("fn draw_cells"));
    assert!(human.contains("guard[MUT]"));
    assert!(human.contains("RELEASE BLOCKED"));

    let jsonl = render_diagnostic_report_jsonl(&report);
    let rows = jsonl.lines().collect::<Vec<_>>();
    assert_eq!(rows.len(), 1);

    let row: AuditDiagnosticRow = serde_json::from_str(rows[0]).expect("valid JSONL row");
    assert_eq!(row.guard_kind, "Mutation");
    assert!(row.entry_label.contains("paint_impl"));
    assert!(row.guard_label.contains("guard[MUT]"));
    assert!(
        row.path_labels
            .iter()
            .any(|label| label.contains("draw_cells")),
        "JSONL path should name the transitive helper for actionable CI output: {row:?}",
    );
}

#[test]
fn render_audit_driver_warn_mode_keeps_findings_but_does_not_block_release() {
    let source_text = r#"
fn paint_impl() {
    let _guard = triple_buffer.write();
}
"#;
    let audit_config = AuditConfig {
        error_as_warn: true,
        ..AuditConfig::default()
    };
    let report = run_audit_on_sources(
        &[source("termwindow/render/paint.rs", source_text)],
        &PopulatorConfig::default(),
        audit_config,
    );

    assert_eq!(report.violation_count(), 1);
    assert!(!report.is_release_blocker());

    let human = render_diagnostic_report(&report);
    assert!(human.contains("RENDER AUDIT: FAILED"));
    assert!(!human.contains("RELEASE BLOCKED"));
    assert_eq!(render_diagnostic_report_jsonl(&report).lines().count(), 1);
}
