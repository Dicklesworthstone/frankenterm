//! User-facing diagnostics and remediation UX certification (ft-e34d9.10.7.4).
//!
//! Certifies that runtime diagnostics are actionable for both human CLI users
//! and robot-mode agents.  Each failure class has a canonical diagnostic
//! contract: cause, impact, next-step command, and escalation hint.  Outputs
//! are dual-mode: concise for humans, machine-parseable for robot mode.
//!
//! # Architecture
//!
//! ```text
//! DiagnosticContract
//!   ├── failure_class → DiagnosticTemplate
//!   │     ├── cause_summary
//!   │     ├── user_impact
//!   │     ├── remediation_commands
//!   │     └── escalation_hint
//!   │
//!   └── Renderers
//!         ├── render_human()  → formatted text
//!         └── render_robot()  → JSON
//!
//! RemediationCatalog
//!   └── lookup(failure_class, context) → Vec<RemediationStep>
//!
//! DiagnosticCertification
//!   └── certify(contracts) → CertificationReport
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::runtime_telemetry::FailureClass;

// =============================================================================
// Diagnostic template
// =============================================================================

/// Canonical diagnostic contract for a runtime failure class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticTemplate {
    /// Which failure class this covers.
    pub failure_class: FailureClass,
    /// One-line cause summary for human display.
    pub cause_summary: String,
    /// User-visible impact description.
    pub user_impact: String,
    /// Ordered remediation steps.
    pub remediation_steps: Vec<RemediationStep>,
    /// Escalation hint if remediation doesn't resolve.
    pub escalation_hint: String,
    /// Machine-parseable error code (e.g., "RT-TIMEOUT-001").
    pub error_code: String,
    /// Whether this template is certified (reviewed for completeness).
    pub certified: bool,
}

/// A single remediation step with command and context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationStep {
    /// Step number (1-based).
    pub step: u32,
    /// Human-readable instruction.
    pub instruction: String,
    /// CLI command to run (if applicable).
    pub command: Option<String>,
    /// Expected outcome of this step.
    pub expected_outcome: String,
    /// Whether this step is safe to auto-execute in robot mode.
    pub robot_safe: bool,
}

/// Standard diagnostic templates for all runtime failure classes.
#[must_use]
pub fn standard_diagnostic_templates() -> Vec<DiagnosticTemplate> {
    vec![
        DiagnosticTemplate {
            failure_class: FailureClass::Timeout,
            cause_summary: "Operation exceeded its time budget and was cancelled".into(),
            user_impact: "Command did not complete; partial results may be stale".into(),
            remediation_steps: vec![
                RemediationStep {
                    step: 1,
                    instruction: "Check system load and resource availability".into(),
                    command: Some("ft doctor --check runtime".into()),
                    expected_outcome: "Identifies resource bottleneck if present".into(),
                    robot_safe: true,
                },
                RemediationStep {
                    step: 2,
                    instruction: "Retry the operation with extended timeout".into(),
                    command: Some("ft <command> --timeout 30s".into()),
                    expected_outcome: "Operation completes within extended window".into(),
                    robot_safe: true,
                },
                RemediationStep {
                    step: 3,
                    instruction: "If persistent, check WezTerm server responsiveness".into(),
                    command: Some("ft status --verbose".into()),
                    expected_outcome: "Shows pane/session health and latency".into(),
                    robot_safe: true,
                },
            ],
            escalation_hint: "File a diagnostic bundle: ft doctor --export-bundle".into(),
            error_code: "RT-TIMEOUT-001".into(),
            certified: true,
        },
        DiagnosticTemplate {
            failure_class: FailureClass::Overload,
            cause_summary: "Task queue or resource pool exceeded capacity".into(),
            user_impact: "New operations may be rejected or delayed until backlog clears".into(),
            remediation_steps: vec![
                RemediationStep {
                    step: 1,
                    instruction: "Check current backlog and active task count".into(),
                    command: Some("ft doctor --check backpressure".into()),
                    expected_outcome: "Shows queue depth and active task count".into(),
                    robot_safe: true,
                },
                RemediationStep {
                    step: 2,
                    instruction: "Reduce concurrent operations or close unused panes".into(),
                    command: None,
                    expected_outcome: "Backlog decreases to healthy levels".into(),
                    robot_safe: false,
                },
            ],
            escalation_hint: "Consider scaling the agent fleet or adjusting capture intervals".into(),
            error_code: "RT-OVERLOAD-001".into(),
            certified: true,
        },
        DiagnosticTemplate {
            failure_class: FailureClass::Deadlock,
            cause_summary: "Task appears stuck: spawned but not completing or cancelling".into(),
            user_impact: "System resources are leaking; performance will degrade over time".into(),
            remediation_steps: vec![
                RemediationStep {
                    step: 1,
                    instruction: "Run leak diagnostics".into(),
                    command: Some("ft doctor --check task-leaks".into()),
                    expected_outcome: "Identifies leaked task count and source".into(),
                    robot_safe: true,
                },
                RemediationStep {
                    step: 2,
                    instruction: "Restart the affected subsystem".into(),
                    command: Some("ft restart --subsystem runtime".into()),
                    expected_outcome: "Leaked tasks cleaned up, subsystem recovers".into(),
                    robot_safe: false,
                },
            ],
            escalation_hint: "Capture thread dump before restart: ft doctor --thread-dump".into(),
            error_code: "RT-DEADLOCK-001".into(),
            certified: true,
        },
        DiagnosticTemplate {
            failure_class: FailureClass::Degraded,
            cause_summary: "System operating below optimal performance thresholds".into(),
            user_impact: "Operations complete but with higher latency than normal".into(),
            remediation_steps: vec![
                RemediationStep {
                    step: 1,
                    instruction: "Review runtime health overview".into(),
                    command: Some("ft doctor".into()),
                    expected_outcome: "Shows which subsystems are degraded".into(),
                    robot_safe: true,
                },
                RemediationStep {
                    step: 2,
                    instruction: "Check for external factors (disk pressure, network)".into(),
                    command: Some("ft doctor --check system".into()),
                    expected_outcome: "Identifies external constraints".into(),
                    robot_safe: true,
                },
            ],
            escalation_hint: "Monitor SLO dashboard for trend analysis".into(),
            error_code: "RT-DEGRADED-001".into(),
            certified: true,
        },
        DiagnosticTemplate {
            failure_class: FailureClass::Corruption,
            cause_summary: "Data integrity violation detected in storage or event pipeline".into(),
            user_impact: "Affected data may be incomplete or incorrect; recovery needed".into(),
            remediation_steps: vec![
                RemediationStep {
                    step: 1,
                    instruction: "Run integrity checks on storage".into(),
                    command: Some("ft doctor --check storage-integrity".into()),
                    expected_outcome: "Reports extent of corruption".into(),
                    robot_safe: true,
                },
                RemediationStep {
                    step: 2,
                    instruction: "Initiate recovery from last good checkpoint".into(),
                    command: Some("ft recover --from-checkpoint".into()),
                    expected_outcome: "Data restored to consistent state".into(),
                    robot_safe: false,
                },
            ],
            escalation_hint: "Preserve corrupted state for forensics: ft doctor --preserve-state".into(),
            error_code: "RT-CORRUPTION-001".into(),
            certified: true,
        },
        DiagnosticTemplate {
            failure_class: FailureClass::Transient,
            cause_summary: "Temporary failure that typically self-resolves".into(),
            user_impact: "Retry usually succeeds; no data loss expected".into(),
            remediation_steps: vec![
                RemediationStep {
                    step: 1,
                    instruction: "Check runtime health for transient conditions".into(),
                    command: Some("ft doctor --check runtime".into()),
                    expected_outcome: "Identifies any ongoing transient conditions".into(),
                    robot_safe: true,
                },
                RemediationStep {
                    step: 2,
                    instruction: "Retry the failed operation".into(),
                    command: None,
                    expected_outcome: "Operation succeeds on retry".into(),
                    robot_safe: true,
                },
            ],
            escalation_hint: "If retries consistently fail, check ft doctor for underlying issues".into(),
            error_code: "RT-TRANSIENT-001".into(),
            certified: true,
        },
        DiagnosticTemplate {
            failure_class: FailureClass::Permanent,
            cause_summary: "Unrecoverable failure requiring intervention".into(),
            user_impact: "Operation cannot succeed without manual corrective action".into(),
            remediation_steps: vec![
                RemediationStep {
                    step: 1,
                    instruction: "Review error details and affected components".into(),
                    command: Some("ft doctor --verbose".into()),
                    expected_outcome: "Full diagnostic context for the failure".into(),
                    robot_safe: true,
                },
                RemediationStep {
                    step: 2,
                    instruction: "Apply the recommended fix for the specific error code".into(),
                    command: None,
                    expected_outcome: "Root cause resolved".into(),
                    robot_safe: false,
                },
            ],
            escalation_hint: "Export full diagnostic bundle for support: ft doctor --export-bundle".into(),
            error_code: "RT-PERMANENT-001".into(),
            certified: true,
        },
        DiagnosticTemplate {
            failure_class: FailureClass::Configuration,
            cause_summary: "Invalid or missing configuration detected".into(),
            user_impact: "Feature/subsystem unavailable until configuration is corrected".into(),
            remediation_steps: vec![
                RemediationStep {
                    step: 1,
                    instruction: "Validate configuration".into(),
                    command: Some("ft config validate".into()),
                    expected_outcome: "Reports which config fields are invalid/missing".into(),
                    robot_safe: true,
                },
                RemediationStep {
                    step: 2,
                    instruction: "Apply recommended defaults or fix values".into(),
                    command: Some("ft config repair".into()),
                    expected_outcome: "Configuration restored to valid state".into(),
                    robot_safe: false,
                },
            ],
            escalation_hint: "Compare with reference config: ft config diff --reference".into(),
            error_code: "RT-CONFIG-001".into(),
            certified: true,
        },
        DiagnosticTemplate {
            failure_class: FailureClass::Panic,
            cause_summary: "Unexpected runtime panic in a subsystem task".into(),
            user_impact: "Affected operation failed; subsystem may need restart".into(),
            remediation_steps: vec![
                RemediationStep {
                    step: 1,
                    instruction: "Check panic log for backtrace".into(),
                    command: Some("ft doctor --check panics".into()),
                    expected_outcome: "Shows panic location and backtrace".into(),
                    robot_safe: true,
                },
                RemediationStep {
                    step: 2,
                    instruction: "Restart the affected subsystem".into(),
                    command: Some("ft restart --subsystem runtime".into()),
                    expected_outcome: "Subsystem recovers after restart".into(),
                    robot_safe: false,
                },
            ],
            escalation_hint: "Capture diagnostic bundle with crash artifacts: ft doctor --export-bundle --include-crashes".into(),
            error_code: "RT-PANIC-001".into(),
            certified: true,
        },
        DiagnosticTemplate {
            failure_class: FailureClass::Safety,
            cause_summary: "Safety policy prevented a potentially dangerous operation".into(),
            user_impact: "Operation blocked; approval or policy override required".into(),
            remediation_steps: vec![
                RemediationStep {
                    step: 1,
                    instruction: "Review the safety policy that triggered".into(),
                    command: Some("ft policy explain --last-denied".into()),
                    expected_outcome: "Shows which policy rule and why it blocked".into(),
                    robot_safe: true,
                },
                RemediationStep {
                    step: 2,
                    instruction: "Request approval if the operation is intentional".into(),
                    command: Some("ft approve --last-denied".into()),
                    expected_outcome: "Operation proceeds after approval".into(),
                    robot_safe: false,
                },
            ],
            escalation_hint: "Check audit chain for policy decision history: ft policy audit".into(),
            error_code: "RT-SAFETY-001".into(),
            certified: true,
        },
    ]
}

// =============================================================================
// Dual-mode rendering
// =============================================================================

/// Rendered diagnostic output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedDiagnostic {
    /// Error code for programmatic matching.
    pub error_code: String,
    /// Human-readable formatted text.
    pub human_text: String,
    /// Machine-parseable JSON for robot mode.
    pub robot_json: String,
}

/// Render a diagnostic template for dual-mode output.
#[must_use]
pub fn render_diagnostic(template: &DiagnosticTemplate) -> RenderedDiagnostic {
    let human_text = render_human(template);
    let robot_json = render_robot(template);

    RenderedDiagnostic {
        error_code: template.error_code.clone(),
        human_text,
        robot_json,
    }
}

/// Render for human CLI consumption.
fn render_human(template: &DiagnosticTemplate) -> String {
    let mut lines = Vec::new();
    lines.push(format!("[{}] {:?}", template.error_code, template.failure_class));
    lines.push(format!("Cause: {}", template.cause_summary));
    lines.push(format!("Impact: {}", template.user_impact));
    lines.push(String::new());
    lines.push("Remediation:".to_string());
    for step in &template.remediation_steps {
        let cmd = step
            .command
            .as_ref()
            .map(|c| format!("\n     $ {}", c))
            .unwrap_or_default();
        lines.push(format!("  {}. {}{}", step.step, step.instruction, cmd));
    }
    lines.push(String::new());
    lines.push(format!("Escalation: {}", template.escalation_hint));
    lines.join("\n")
}

/// Render for robot-mode consumption (JSON with structured fields).
fn render_robot(template: &DiagnosticTemplate) -> String {
    #[derive(Serialize)]
    struct RobotDiagnostic<'a> {
        error_code: &'a str,
        failure_class: &'a FailureClass,
        cause: &'a str,
        impact: &'a str,
        remediation: Vec<RobotStep<'a>>,
        escalation: &'a str,
    }

    #[derive(Serialize)]
    struct RobotStep<'a> {
        step: u32,
        instruction: &'a str,
        command: Option<&'a str>,
        robot_safe: bool,
    }

    let robot = RobotDiagnostic {
        error_code: &template.error_code,
        failure_class: &template.failure_class,
        cause: &template.cause_summary,
        impact: &template.user_impact,
        remediation: template
            .remediation_steps
            .iter()
            .map(|s| RobotStep {
                step: s.step,
                instruction: &s.instruction,
                command: s.command.as_deref(),
                robot_safe: s.robot_safe,
            })
            .collect(),
        escalation: &template.escalation_hint,
    };

    serde_json::to_string(&robot).unwrap_or_default()
}

// =============================================================================
// Diagnostic catalog (lookup by failure class)
// =============================================================================

/// Catalog of all diagnostic templates indexed by failure class.
#[derive(Debug, Clone)]
pub struct DiagnosticCatalog {
    templates: BTreeMap<String, DiagnosticTemplate>,
}

impl DiagnosticCatalog {
    /// Build a catalog from a list of templates.
    #[must_use]
    pub fn from_templates(templates: Vec<DiagnosticTemplate>) -> Self {
        let mut map = BTreeMap::new();
        for t in templates {
            map.insert(format!("{:?}", t.failure_class), t);
        }
        Self { templates: map }
    }

    /// Standard catalog with all runtime failure classes.
    #[must_use]
    pub fn standard() -> Self {
        Self::from_templates(standard_diagnostic_templates())
    }

    /// Look up a diagnostic template by failure class.
    #[must_use]
    pub fn lookup(&self, failure_class: &FailureClass) -> Option<&DiagnosticTemplate> {
        self.templates.get(&format!("{:?}", failure_class))
    }

    /// All covered failure classes.
    #[must_use]
    pub fn covered_classes(&self) -> Vec<&str> {
        self.templates.keys().map(String::as_str).collect()
    }

    /// Total templates.
    #[must_use]
    pub fn count(&self) -> usize {
        self.templates.len()
    }

    /// Robot-safe commands for a failure class.
    #[must_use]
    pub fn robot_safe_commands(&self, failure_class: &FailureClass) -> Vec<String> {
        match self.lookup(failure_class) {
            Some(t) => t
                .remediation_steps
                .iter()
                .filter(|s| s.robot_safe)
                .filter_map(|s| s.command.clone())
                .collect(),
            None => Vec::new(),
        }
    }
}

// =============================================================================
// Certification report
// =============================================================================

/// Certification check for a single diagnostic template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateCheck {
    /// Failure class covered.
    pub failure_class: String,
    /// Whether cause summary is non-empty.
    pub has_cause: bool,
    /// Whether user impact is non-empty.
    pub has_impact: bool,
    /// Whether at least one remediation step exists.
    pub has_remediation: bool,
    /// Whether at least one robot-safe step exists.
    pub has_robot_safe_step: bool,
    /// Whether a command is provided in at least one step.
    pub has_command: bool,
    /// Whether escalation hint is non-empty.
    pub has_escalation: bool,
    /// Whether error code is non-empty.
    pub has_error_code: bool,
    /// Whether the template is marked certified.
    pub is_certified: bool,
    /// Overall pass for this template.
    pub passed: bool,
}

/// Report certifying diagnostic completeness across all failure classes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertificationReport {
    /// Per-template certification checks.
    pub checks: Vec<TemplateCheck>,
    /// Failure classes missing templates.
    pub missing_classes: Vec<String>,
    /// Overall certification pass.
    pub overall_pass: bool,
    /// Pass count.
    pub pass_count: usize,
    /// Total checks.
    pub total_checks: usize,
}

/// All failure classes that must have diagnostic templates.
fn required_failure_classes() -> Vec<FailureClass> {
    vec![
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
    ]
}

impl CertificationReport {
    /// Certify a set of diagnostic templates against completeness criteria.
    #[must_use]
    pub fn certify(catalog: &DiagnosticCatalog) -> Self {
        let required = required_failure_classes();
        let mut checks = Vec::new();
        let mut missing = Vec::new();

        for fc in &required {
            let key = format!("{:?}", fc);
            match catalog.lookup(fc) {
                Some(t) => {
                    let has_cause = !t.cause_summary.is_empty();
                    let has_impact = !t.user_impact.is_empty();
                    let has_remediation = !t.remediation_steps.is_empty();
                    let has_robot_safe_step =
                        t.remediation_steps.iter().any(|s| s.robot_safe);
                    let has_command =
                        t.remediation_steps.iter().any(|s| s.command.is_some());
                    let has_escalation = !t.escalation_hint.is_empty();
                    let has_error_code = !t.error_code.is_empty();

                    let passed = has_cause
                        && has_impact
                        && has_remediation
                        && has_robot_safe_step
                        && has_command
                        && has_escalation
                        && has_error_code
                        && t.certified;

                    checks.push(TemplateCheck {
                        failure_class: key,
                        has_cause,
                        has_impact,
                        has_remediation,
                        has_robot_safe_step,
                        has_command,
                        has_escalation,
                        has_error_code,
                        is_certified: t.certified,
                        passed,
                    });
                }
                None => {
                    missing.push(key);
                }
            }
        }

        let pass_count = checks.iter().filter(|c| c.passed).count();
        let total_checks = checks.len();
        let overall_pass = missing.is_empty() && checks.iter().all(|c| c.passed);

        Self {
            checks,
            missing_classes: missing,
            overall_pass,
            pass_count,
            total_checks,
        }
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push("=== Diagnostic UX Certification ===".to_string());
        lines.push(format!(
            "Result: {}",
            if self.overall_pass { "CERTIFIED" } else { "NOT CERTIFIED" }
        ));
        lines.push(format!(
            "Templates: {}/{} pass",
            self.pass_count, self.total_checks
        ));

        if !self.missing_classes.is_empty() {
            lines.push(format!("Missing: {}", self.missing_classes.join(", ")));
        }

        for check in &self.checks {
            let status = if check.passed { "PASS" } else { "FAIL" };
            let mut issues = Vec::new();
            if !check.has_cause {
                issues.push("no cause");
            }
            if !check.has_impact {
                issues.push("no impact");
            }
            if !check.has_remediation {
                issues.push("no remediation");
            }
            if !check.has_robot_safe_step {
                issues.push("no robot-safe step");
            }
            if !check.has_command {
                issues.push("no command");
            }
            if !check.has_escalation {
                issues.push("no escalation");
            }
            if !check.is_certified {
                issues.push("not certified");
            }
            let issue_str = if issues.is_empty() {
                String::new()
            } else {
                format!(" ({})", issues.join(", "))
            };
            lines.push(format!("  [{}] {}{}", status, check.failure_class, issue_str));
        }

        lines.join("\n")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_templates_cover_all_required_classes() {
        let catalog = DiagnosticCatalog::standard();
        let required = required_failure_classes();
        for fc in &required {
            assert!(
                catalog.lookup(fc).is_some(),
                "missing template for {:?}",
                fc
            );
        }
    }

    #[test]
    fn standard_templates_all_certified() {
        let templates = standard_diagnostic_templates();
        for t in &templates {
            assert!(t.certified, "template {:?} not certified", t.failure_class);
        }
    }

    #[test]
    fn certification_passes_for_standard_catalog() {
        let catalog = DiagnosticCatalog::standard();
        let report = CertificationReport::certify(&catalog);
        assert!(report.overall_pass, "certification should pass: {:?}", report.render_summary());
        assert!(report.missing_classes.is_empty());
    }

    #[test]
    fn certification_fails_when_template_missing() {
        // Build catalog missing one class.
        let mut templates = standard_diagnostic_templates();
        templates.retain(|t| t.failure_class != FailureClass::Panic);
        let catalog = DiagnosticCatalog::from_templates(templates);
        let report = CertificationReport::certify(&catalog);
        assert!(!report.overall_pass);
        assert!(report.missing_classes.contains(&"Panic".to_string()));
    }

    #[test]
    fn certification_fails_when_not_certified() {
        let mut templates = standard_diagnostic_templates();
        if let Some(t) = templates.iter_mut().find(|t| t.failure_class == FailureClass::Timeout) {
            t.certified = false;
        }
        let catalog = DiagnosticCatalog::from_templates(templates);
        let report = CertificationReport::certify(&catalog);
        assert!(!report.overall_pass);
    }

    #[test]
    fn certification_fails_when_no_remediation() {
        let mut templates = standard_diagnostic_templates();
        if let Some(t) = templates.iter_mut().find(|t| t.failure_class == FailureClass::Timeout) {
            t.remediation_steps.clear();
        }
        let catalog = DiagnosticCatalog::from_templates(templates);
        let report = CertificationReport::certify(&catalog);
        assert!(!report.overall_pass);
    }

    #[test]
    fn render_human_includes_error_code_and_steps() {
        let templates = standard_diagnostic_templates();
        let timeout = templates.iter().find(|t| t.failure_class == FailureClass::Timeout).unwrap();
        let human = render_human(timeout);
        assert!(human.contains("RT-TIMEOUT-001"));
        assert!(human.contains("Remediation:"));
        assert!(human.contains("ft doctor"));
    }

    #[test]
    fn render_robot_is_valid_json() {
        let templates = standard_diagnostic_templates();
        for t in &templates {
            let rendered = render_diagnostic(t);
            let parsed: serde_json::Value =
                serde_json::from_str(&rendered.robot_json).expect("valid JSON");
            assert!(parsed.get("error_code").is_some());
            assert!(parsed.get("remediation").is_some());
        }
    }

    #[test]
    fn robot_json_contains_robot_safe_flags() {
        let templates = standard_diagnostic_templates();
        let timeout = templates.iter().find(|t| t.failure_class == FailureClass::Timeout).unwrap();
        let rendered = render_diagnostic(timeout);
        let parsed: serde_json::Value = serde_json::from_str(&rendered.robot_json).unwrap();
        let steps = parsed["remediation"].as_array().unwrap();
        assert!(steps.iter().any(|s| s["robot_safe"] == true));
    }

    #[test]
    fn catalog_lookup_returns_correct_template() {
        let catalog = DiagnosticCatalog::standard();
        let t = catalog.lookup(&FailureClass::Deadlock).unwrap();
        assert_eq!(t.failure_class, FailureClass::Deadlock);
        assert!(t.error_code.contains("DEADLOCK"));
    }

    #[test]
    fn catalog_robot_safe_commands() {
        let catalog = DiagnosticCatalog::standard();
        let cmds = catalog.robot_safe_commands(&FailureClass::Timeout);
        assert!(!cmds.is_empty());
        assert!(cmds.iter().any(|c| c.contains("ft doctor")));
    }

    #[test]
    fn catalog_robot_safe_commands_empty_for_unknown() {
        let catalog = DiagnosticCatalog::standard();
        // Use a failure class we handle but check for a non-existent one.
        let empty_catalog = DiagnosticCatalog::from_templates(Vec::new());
        let cmds = empty_catalog.robot_safe_commands(&FailureClass::Timeout);
        assert!(cmds.is_empty());
    }

    #[test]
    fn all_templates_have_at_least_one_step() {
        let templates = standard_diagnostic_templates();
        for t in &templates {
            assert!(
                !t.remediation_steps.is_empty(),
                "{:?} has no remediation steps",
                t.failure_class
            );
        }
    }

    #[test]
    fn all_templates_have_robot_safe_step() {
        let templates = standard_diagnostic_templates();
        for t in &templates {
            assert!(
                t.remediation_steps.iter().any(|s| s.robot_safe),
                "{:?} has no robot-safe step",
                t.failure_class
            );
        }
    }

    #[test]
    fn error_codes_unique() {
        let templates = standard_diagnostic_templates();
        let codes: Vec<&str> = templates.iter().map(|t| t.error_code.as_str()).collect();
        for (i, a) in codes.iter().enumerate() {
            for (j, b) in codes.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "duplicate error code: {}", a);
                }
            }
        }
    }

    #[test]
    fn serde_roundtrip_template() {
        let templates = standard_diagnostic_templates();
        let json = serde_json::to_string(&templates).expect("serialize");
        let restored: Vec<DiagnosticTemplate> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.len(), templates.len());
    }

    #[test]
    fn serde_roundtrip_certification() {
        let catalog = DiagnosticCatalog::standard();
        let report = CertificationReport::certify(&catalog);
        let json = serde_json::to_string(&report).expect("serialize");
        let restored: CertificationReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.overall_pass, report.overall_pass);
    }

    #[test]
    fn render_certification_summary() {
        let catalog = DiagnosticCatalog::standard();
        let report = CertificationReport::certify(&catalog);
        let summary = report.render_summary();
        assert!(summary.contains("CERTIFIED"));
        assert!(summary.contains("PASS"));
    }

    #[test]
    fn render_certification_summary_shows_failures() {
        let mut templates = standard_diagnostic_templates();
        templates.retain(|t| t.failure_class != FailureClass::Panic);
        let catalog = DiagnosticCatalog::from_templates(templates);
        let report = CertificationReport::certify(&catalog);
        let summary = report.render_summary();
        assert!(summary.contains("NOT CERTIFIED"));
        assert!(summary.contains("Missing"));
    }

    #[test]
    fn catalog_count() {
        let catalog = DiagnosticCatalog::standard();
        assert_eq!(catalog.count(), 10);
    }

    #[test]
    fn covered_classes_matches_templates() {
        let catalog = DiagnosticCatalog::standard();
        assert_eq!(catalog.covered_classes().len(), catalog.count());
    }
}
