//! CI release-gate driver for the render-call-graph audit
//! (br-ft-ept8r sub-task 3).
//!
//! Composes the substrate's
//! [`crate::render_call_graph_populator`] with the audit
//! predicate
//! [`crate::render_call_graph_audit::audit_render_call_graph`]
//! and ships the diagnostic-rendering layer the CI lane
//! emits when an audit fails.
//!
//! The bead's sub-task 3 ask:
//!
//! > "Run audit_render_call_graph(graph, entries, guards,
//! > AuditConfig::default()). On FailedWithViolations,
//! > fail CI and print each violation.path as actionable
//! > error."
//!
//! ## Why a separate driver module
//!
//! - The populator is concerned with parsing source files;
//!   the audit predicate is concerned with the BFS over
//!   the resulting graph. The driver is concerned with
//!   *composing them* + rendering human-readable
//!   diagnostics.
//! - Keeping the driver in core (rather than as a
//!   binary) means tests can exercise the full
//!   parse → audit → render pipeline against synthetic
//!   source corpora without spawning a child process.
//! - The CI lane invokes a thin `bin/audit_render.rs` that
//!   walks the gui crate, calls
//!   [`run_audit_on_sources`], renders the report via
//!   [`render_diagnostic_report`], and exits 1 on
//!   `is_release_blocker`. That binary is deferred to the
//!   integration follow-on (it lives in the gui crate).
//!
//! ## Surface
//!
//! - [`run_audit_on_sources`] — top-level driver. Takes a
//!   list of `(file_path, source)` pairs + populator and
//!   audit configs; returns an
//!   [`AuditDriverReport`] bundling the outcome + the
//!   parsed populator state for diagnostic rendering.
//! - [`AuditDriverReport`] — owns the populated graph, the
//!   guard sites, the entry points, and the audit outcome.
//!   Read accessors expose each piece.
//! - [`render_diagnostic_report`] — renders human-readable
//!   diagnostics for the CI lane. Returns a String the
//!   integration prints to stderr.
//! - [`render_diagnostic_report_jsonl`] — JSONL alternative
//!   for the structured-log emit path.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::render_call_graph_audit::{
    AuditConfig, AuditOutcome, AuditViolation, CallSiteId, audit_render_call_graph,
};
use crate::render_call_graph_populator::{PopulatorConfig, PopulatorOutput, populate_from_sources};
use crate::render_snapshot_guard::SnapshotKind;

// ============================================================================
// Driver report
// ============================================================================

/// Bundle returned by the driver. Owns the populator output
/// (so call-site labels resolve) plus the audit outcome.
#[derive(Debug, Clone)]
pub struct AuditDriverReport {
    populator: PopulatorOutput,
    outcome: AuditOutcome,
    audit_config: AuditConfig,
}

impl AuditDriverReport {
    #[must_use]
    pub const fn outcome(&self) -> &AuditOutcome {
        &self.outcome
    }

    #[must_use]
    pub fn populator(&self) -> &PopulatorOutput {
        &self.populator
    }

    #[must_use]
    pub const fn audit_config(&self) -> &AuditConfig {
        &self.audit_config
    }

    #[must_use]
    pub fn is_release_blocker(&self) -> bool {
        self.outcome.is_release_blocker(self.audit_config)
    }

    #[must_use]
    pub fn violation_count(&self) -> usize {
        self.outcome.violation_count()
    }

    /// Build a CallSiteId → human-readable label map from
    /// the populator output. Used by both rendering modes.
    #[must_use]
    fn id_to_label(&self) -> HashMap<CallSiteId, String> {
        let mut map: HashMap<CallSiteId, String> = HashMap::new();
        for (name, id) in &self.populator.function_to_id {
            map.insert(*id, format!("fn {name}"));
        }
        for guard in &self.populator.guard_sites {
            map.insert(
                guard.id,
                format!(
                    "guard[{}] at {}:{}",
                    match guard.snapshot_kind {
                        SnapshotKind::ReadOnly => "RO",
                        SnapshotKind::Mutation => "MUT",
                    },
                    guard.file,
                    guard.line,
                ),
            );
        }
        map
    }
}

// ============================================================================
// Top-level driver
// ============================================================================

/// Run the full populate → audit pipeline over the given
/// source files.
///
/// `files` is a list of `(path, source)` pairs the integration
/// collected via filesystem walk (e.g., every `.rs` under
/// `crates/frankenterm-gui/src/termwindow/render/`).
#[must_use]
pub fn run_audit_on_sources(
    files: &[(String, String)],
    populator_config: &PopulatorConfig,
    audit_config: AuditConfig,
) -> AuditDriverReport {
    let populator = populate_from_sources(files, populator_config);
    let outcome = audit_render_call_graph(
        &populator.graph,
        &populator.entry_points,
        &populator.guard_sites,
        audit_config,
    );
    AuditDriverReport {
        populator,
        outcome,
        audit_config,
    }
}

// ============================================================================
// Diagnostic rendering — human-readable
// ============================================================================

/// Render a human-readable diagnostic report for the CI
/// lane. On `Pass` returns a one-line success banner; on
/// `FailedWithViolations` returns a multi-line block per
/// violation, each with the BFS path from the render entry
/// point through the call graph to the offending guard.
#[must_use]
pub fn render_diagnostic_report(report: &AuditDriverReport) -> String {
    let labels = report.id_to_label();
    let mut out = String::new();

    match &report.outcome {
        AuditOutcome::Pass => {
            out.push_str("RENDER AUDIT: PASS\n");
            out.push_str(&format!(
                "  scanned: {} fn defs, {} guard sites, {} render entries\n",
                report.populator.function_to_id.len(),
                report.populator.guard_sites.len(),
                report.populator.entry_points.len(),
            ));
        }
        AuditOutcome::FailedWithViolations { violations } => {
            out.push_str(&format!(
                "RENDER AUDIT: FAILED — {} violation(s)\n",
                violations.len(),
            ));
            for (i, v) in violations.iter().enumerate() {
                out.push_str(&format!("\n  Violation #{}:\n", i + 1));
                out.push_str(&format!("    {}\n", render_violation_block(v, &labels),));
            }
            if report.is_release_blocker() {
                out.push_str("\nRELEASE BLOCKED. Fix the violations above or set\n");
                out.push_str("AuditConfig::error_as_warn = true (diagnostic builds only).\n");
            }
        }
    }
    out
}

fn render_violation_block(
    violation: &AuditViolation,
    labels: &HashMap<CallSiteId, String>,
) -> String {
    let entry_label = labels
        .get(&violation.entry_point)
        .cloned()
        .unwrap_or_else(|| format!("CallSiteId({})", violation.entry_point.0));
    let guard_label = labels
        .get(&violation.guard_site)
        .cloned()
        .unwrap_or_else(|| format!("CallSiteId({})", violation.guard_site.0));
    let path_str = violation
        .path
        .iter()
        .map(|id| {
            labels
                .get(id)
                .cloned()
                .unwrap_or_else(|| format!("CallSiteId({})", id.0))
        })
        .collect::<Vec<_>>()
        .join(" → ");
    format!(
        "render entry: {entry_label}\n    reaches:    {guard_label}\n    via path:   {path_str}",
    )
}

// ============================================================================
// Diagnostic rendering — JSONL
// ============================================================================

/// JSONL row schema for one audit violation. The CI lane's
/// structured-log sink ingests these for cross-build trend
/// analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditDiagnosticRow {
    pub entry_label: String,
    pub guard_label: String,
    pub guard_kind: String,
    pub path_labels: Vec<String>,
}

/// Render the audit report as one JSON line per violation.
/// Returns the empty string on `Pass` (the CI lane's log
/// sink expects no output when the gate is green).
#[must_use]
pub fn render_diagnostic_report_jsonl(report: &AuditDriverReport) -> String {
    let labels = report.id_to_label();
    let mut out = String::new();
    if let AuditOutcome::FailedWithViolations { violations } = &report.outcome {
        for v in violations {
            let row = AuditDiagnosticRow {
                entry_label: labels
                    .get(&v.entry_point)
                    .cloned()
                    .unwrap_or_else(|| format!("CallSiteId({})", v.entry_point.0)),
                guard_label: labels
                    .get(&v.guard_site)
                    .cloned()
                    .unwrap_or_else(|| format!("CallSiteId({})", v.guard_site.0)),
                guard_kind: match v.kind {
                    SnapshotKind::ReadOnly => "ReadOnly".to_string(),
                    SnapshotKind::Mutation => "Mutation".to_string(),
                },
                path_labels: v
                    .path
                    .iter()
                    .map(|id| {
                        labels
                            .get(id)
                            .cloned()
                            .unwrap_or_else(|| format!("CallSiteId({})", id.0))
                    })
                    .collect(),
            };
            out.push_str(&serde_json::to_string(&row).expect("AuditDiagnosticRow serializes"));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // br-ft-x2oyy: render-audit fixtures intentionally embed mutation
    // snippets that look like dropped `triple_buffer.write()` results. They
    // are synthetic source text, not runtime swallows; each fixture keeps a
    // local breadcrumb so grep-based audits can distinguish test corpus data
    // from production dropped Results.
    fn pair(path: &str, source: &str) -> (String, String) {
        (path.to_string(), source.to_string())
    }

    // ----------------------------------------------------------------
    // Pass scenario: every guard is ReadOnly
    // ----------------------------------------------------------------

    #[test]
    fn driver_pass_when_no_render_path_touches_mutation() {
        let src = r#"
fn paint_impl() {
    let snap = triple_buffer.read();
    let _ = snap;
}
"#;
        let report = run_audit_on_sources(
            &[pair("paint.rs", src)],
            &PopulatorConfig::default(),
            AuditConfig::default(),
        );
        assert!(report.outcome().is_pass());
        assert_eq!(report.violation_count(), 0);
        assert!(!report.is_release_blocker());
    }

    // ----------------------------------------------------------------
    // Fail scenarios
    // ----------------------------------------------------------------

    #[test]
    fn driver_fails_when_render_entry_touches_mutation_guard() {
        let src = r#"
fn paint_impl() {
    let snap = triple_buffer.write();
    let _ = snap;
}
"#;
        let report = run_audit_on_sources(
            &[pair("paint.rs", src)],
            &PopulatorConfig::default(),
            AuditConfig::default(),
        );
        assert_eq!(report.violation_count(), 1);
        assert!(report.is_release_blocker());
    }

    #[test]
    fn driver_treats_violations_as_warnings_when_error_as_warn() {
        let src = r#"
fn paint_impl() {
    // br-ft-x2oyy: intentional synthetic mutation fixture;
    // this dropped write result is test corpus data, not production logic.
    let _ = triple_buffer.write();
}
"#;
        let cfg = AuditConfig {
            error_as_warn: true,
            ..AuditConfig::default()
        };
        let report =
            run_audit_on_sources(&[pair("paint.rs", src)], &PopulatorConfig::default(), cfg);
        assert_eq!(report.violation_count(), 1);
        // Violations exist but they don't block release.
        assert!(!report.is_release_blocker());
    }

    #[test]
    fn driver_finds_transitive_violation_across_files() {
        let paint = r#"
fn paint_impl() { helper_in_other_file(); }
"#;
        let helper = r#"
fn helper_in_other_file() {
    // br-ft-x2oyy: intentional synthetic mutation fixture;
    // this dropped write result is test corpus data, not production logic.
    let _ = triple_buffer.write();
}
"#;
        let report = run_audit_on_sources(
            &[pair("paint.rs", paint), pair("helper.rs", helper)],
            &PopulatorConfig::default(),
            AuditConfig::default(),
        );
        assert_eq!(report.violation_count(), 1);
    }

    // ----------------------------------------------------------------
    // Diagnostic rendering — Pass
    // ----------------------------------------------------------------

    #[test]
    fn render_diagnostic_pass_includes_scan_counts() {
        let src = r#"
fn paint_impl() {
    let _ = triple_buffer.read();
}
fn helper() {
    let _ = triple_buffer.read();
}
"#;
        let report = run_audit_on_sources(
            &[pair("paint.rs", src)],
            &PopulatorConfig::default(),
            AuditConfig::default(),
        );
        let rendered = render_diagnostic_report(&report);
        assert!(rendered.contains("RENDER AUDIT: PASS"));
        assert!(rendered.contains("fn defs"));
        assert!(rendered.contains("guard sites"));
        assert!(rendered.contains("render entries"));
    }

    // ----------------------------------------------------------------
    // Diagnostic rendering — Fail
    // ----------------------------------------------------------------

    #[test]
    fn render_diagnostic_fail_lists_each_violation_with_path() {
        let src = r#"
fn paint_impl() { helper_writes(); }
fn helper_writes() {
    // br-ft-x2oyy: intentional synthetic mutation fixture;
    // this dropped write result is test corpus data, not production logic.
    let _ = triple_buffer.write();
}
"#;
        let report = run_audit_on_sources(
            &[pair("paint.rs", src)],
            &PopulatorConfig::default(),
            AuditConfig::default(),
        );
        let rendered = render_diagnostic_report(&report);
        assert!(rendered.contains("FAILED"));
        assert!(rendered.contains("Violation #1"));
        assert!(rendered.contains("paint_impl"));
        assert!(rendered.contains("helper_writes"));
        assert!(rendered.contains("guard[MUT]"));
        assert!(rendered.contains("via path:"));
        assert!(rendered.contains("RELEASE BLOCKED"));
    }

    #[test]
    fn render_diagnostic_fail_warn_mode_omits_release_blocked_banner() {
        let src = r#"
fn paint_impl() {
    // br-ft-x2oyy: intentional synthetic mutation fixture;
    // this dropped write result is test corpus data, not production logic.
    let _ = triple_buffer.write();
}
"#;
        let cfg = AuditConfig {
            error_as_warn: true,
            ..AuditConfig::default()
        };
        let report =
            run_audit_on_sources(&[pair("paint.rs", src)], &PopulatorConfig::default(), cfg);
        let rendered = render_diagnostic_report(&report);
        assert!(rendered.contains("FAILED"));
        assert!(
            !rendered.contains("RELEASE BLOCKED"),
            "warn mode must not print the release-blocker banner",
        );
    }

    // ----------------------------------------------------------------
    // Diagnostic rendering — JSONL
    // ----------------------------------------------------------------

    #[test]
    fn render_jsonl_pass_returns_empty_string() {
        let src = "fn paint_impl() { let _ = triple_buffer.read(); }";
        let report = run_audit_on_sources(
            &[pair("paint.rs", src)],
            &PopulatorConfig::default(),
            AuditConfig::default(),
        );
        assert_eq!(render_diagnostic_report_jsonl(&report), "");
    }

    #[test]
    fn render_jsonl_fail_emits_one_row_per_violation() {
        let src = r#"
fn paint_impl() { helper(); }
fn helper() {
    // br-ft-x2oyy: intentional synthetic mutation fixture;
    // this dropped write result is test corpus data, not production logic.
    let _ = triple_buffer.write();
}
"#;
        let report = run_audit_on_sources(
            &[pair("paint.rs", src)],
            &PopulatorConfig::default(),
            AuditConfig::default(),
        );
        let jsonl = render_diagnostic_report_jsonl(&report);
        let rows: Vec<&str> = jsonl.lines().collect();
        assert_eq!(rows.len(), 1);
        let row: AuditDiagnosticRow = serde_json::from_str(rows[0]).unwrap();
        assert!(row.entry_label.contains("paint_impl"));
        assert!(row.guard_label.contains("guard[MUT]"));
        assert_eq!(row.guard_kind, "Mutation");
        assert!(!row.path_labels.is_empty());
    }

    #[test]
    fn render_jsonl_each_line_is_valid_json() {
        // 2 entries each reaching a mutation guard transitively.
        let src = r#"
fn paint_impl() { helper_a(); }
fn render_pane() { helper_b(); }
fn helper_a() {
    // br-ft-x2oyy: intentional synthetic mutation fixture;
    // this dropped write result is test corpus data, not production logic.
    let _ = triple_buffer.write();
}
fn helper_b() {
    // br-ft-x2oyy: intentional synthetic mutation fixture;
    // this dropped write result is test corpus data, not production logic.
    let _ = triple_buffer.write();
}
"#;
        let report = run_audit_on_sources(
            &[pair("paint.rs", src)],
            &PopulatorConfig::default(),
            AuditConfig::default(),
        );
        let jsonl = render_diagnostic_report_jsonl(&report);
        let rows: Vec<&str> = jsonl.lines().collect();
        assert!(rows.len() >= 2);
        for line in rows {
            let _: AuditDiagnosticRow =
                serde_json::from_str(line).expect("each JSONL line must be valid JSON");
        }
    }

    // ----------------------------------------------------------------
    // End-to-end: empty / no-render-entry corner cases
    // ----------------------------------------------------------------

    #[test]
    fn driver_empty_source_passes() {
        let report = run_audit_on_sources(
            &[pair("empty.rs", "")],
            &PopulatorConfig::default(),
            AuditConfig::default(),
        );
        assert!(report.outcome().is_pass());
    }

    #[test]
    fn driver_passes_when_no_render_entries() {
        let src = r#"
fn unrelated() {
    // br-ft-x2oyy: intentional synthetic mutation fixture;
    // this dropped write result is test corpus data, not production logic.
    let _ = triple_buffer.write();
}
"#;
        let report = run_audit_on_sources(
            &[pair("unrelated.rs", src)],
            &PopulatorConfig::default(),
            AuditConfig::default(),
        );
        assert!(report.outcome().is_pass());
    }

    // ----------------------------------------------------------------
    // Custom populator config: alternate render entry name
    // ----------------------------------------------------------------

    #[test]
    fn driver_respects_custom_render_entry_names() {
        let src = r#"
fn alt_paint_root() {
    // br-ft-x2oyy: intentional synthetic mutation fixture;
    // this dropped write result is test corpus data, not production logic.
    let _ = triple_buffer.write();
}
fn paint_impl() { let _ = triple_buffer.read(); }
"#;
        let mut populator_cfg = PopulatorConfig::default();
        populator_cfg.render_entry_names = vec!["alt_paint_root".to_string()];
        let report = run_audit_on_sources(
            &[pair("paint.rs", src)],
            &populator_cfg,
            AuditConfig::default(),
        );
        // alt_paint_root is the only entry; it touches mutation.
        assert_eq!(report.violation_count(), 1);
    }
}
