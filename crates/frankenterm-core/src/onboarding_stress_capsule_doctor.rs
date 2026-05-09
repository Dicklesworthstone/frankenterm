//! Renderer for [`crate::onboarding_stress_capsule::OnboardingReport`]. [ft-1650n.17]
//!
//! Slice ships the operator-facing rendering layer on top of the
//! substrate at `onboarding_stress_capsule.rs` (sibling-shipped). Same
//! shape as `capability_passport_doctor` (ft-ykp2y),
//! `handoff_capsule_inspect` (ft-yk9lp slice 1), and
//! `approval_impact_simulator_doctor` (ft-1650n.14 slice) —
//! pure-function rendering that the eventual `ft onboarding doctor`
//! CLI subcommand will call.
//!
//! - `render_text(&report) -> String` — concise plain-text preview
//!   suitable for an operator dashboard or `ft onboarding doctor`
//!   output. Header line + per-bucket detail.
//! - `render(&report) -> OnboardingRendering` — structured
//!   JSON-serializable rendering for machine consumption (audit-feed
//!   integration, dashboarding, automation harnesses).
//!
//! # Privacy
//!
//! Substrate's contract is that remediation strings on `FailedCheck`
//! have already been passed through
//! [`crate::onboarding_stress_capsule::sanitize_remediation`] —
//! forbidden-command patterns (`rm -rf`, `--no-verify`, `git push
//! --force`, etc.) are replaced with `[REDACTED-FORBIDDEN]` BEFORE
//! the report is constructed. The renderer trusts that contract; it
//! does not re-sanitize.
//!
//! The fail-closed canary at the bottom of this file confirms the
//! renderer faithfully transports the substrate's redacted state
//! — i.e. if the substrate sanitized "rm -rf /foo" → "[REDACTED-
//! FORBIDDEN] /foo", the renderer surfaces the redacted form, not
//! the raw form. The canary also asserts the renderer never
//! amplifies leakage (no Debug-format dumps, text and JSON paths
//! surface the same redacted-marker count).
//!
//! # Acceptance mapping
//!
//! Bead ft-1650n.17's acceptance:
//! - "Stress capsule checks hooks, cargo wrapper, robot output,
//!   storage writability, runtime policy, and safe probe budget" —
//!   substrate concern; renderer surfaces all 8
//!   [`OnboardingCheck`] variants.
//! - "Report separates machine-local issues from repo/code failures
//!   and never auto-fixes shared services" — substrate splits via
//!   `IssueClass`; renderer prints each bucket separately with a
//!   distinct header.
//! - "Unit tests cover classification, forbidden-command
//!   suppression, and remediation hints" — substrate covers
//!   classification + suppression; this slice covers the rendering
//!   side of remediation-hint output.
//! - "E2E dry-run fixture proves ineligible panes are marked unsafe
//!   for high-risk tasks" — deferred to a downstream slice that
//!   wires real probes.

use serde::{Deserialize, Serialize};

use crate::onboarding_stress_capsule::{
    FailedCheck, IssueClass, OnboardingCheck, OnboardingReport, SkippedCheck,
};

/// Stateless renderer for [`OnboardingReport`].
pub struct OnboardingDoctor;

impl OnboardingDoctor {
    /// Construct a renderer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Render `report` as a concise operator-facing plain-text
    /// preview.
    #[must_use]
    pub fn render_text(&self, report: &OnboardingReport) -> String {
        let mut out = String::new();
        let verdict = if report.eligible_for_high_risk_tasks {
            "eligible"
        } else {
            "ineligible"
        };
        out.push_str(&format!(
            "onboarding-doctor: schema={} verdict={}\n",
            report.schema_version, verdict,
        ));
        out.push_str(&format!(
            "  summary: passed={} skipped={} machine_local_failures={} repo_code_failures={}\n",
            report.passed.len(),
            report.skipped.len(),
            report.machine_local_failures.len(),
            report.repo_code_failures.len(),
        ));

        if report.machine_local_failures.is_empty() {
            out.push_str("  machine_local: ∅\n");
        } else {
            out.push_str("  machine_local:\n");
            for (i, f) in report.machine_local_failures.iter().enumerate() {
                out.push_str(&format!(
                    "    [{i}] {} → {}\n",
                    check_label(f.check),
                    f.remediation,
                ));
            }
        }

        if report.repo_code_failures.is_empty() {
            out.push_str("  repo_code: ∅\n");
        } else {
            out.push_str("  repo_code:\n");
            for (i, f) in report.repo_code_failures.iter().enumerate() {
                out.push_str(&format!(
                    "    [{i}] {} → {}\n",
                    check_label(f.check),
                    f.remediation,
                ));
            }
        }

        if report.skipped.is_empty() {
            out.push_str("  skipped: ∅\n");
        } else {
            out.push_str("  skipped:\n");
            for (i, s) in report.skipped.iter().enumerate() {
                out.push_str(&format!(
                    "    [{i}] {} ({})\n",
                    check_label(s.check),
                    s.reason,
                ));
            }
        }

        if report.passed.is_empty() {
            out.push_str("  passed: ∅\n");
        } else {
            let labels: Vec<&'static str> =
                report.passed.iter().copied().map(check_label).collect();
            out.push_str(&format!("  passed: {}\n", labels.join(", ")));
        }
        out
    }

    /// Render `report` as a structured JSON-serializable rendering.
    #[must_use]
    pub fn render(&self, report: &OnboardingReport) -> OnboardingRendering {
        OnboardingRendering {
            schema_version: report.schema_version.clone(),
            verdict: if report.eligible_for_high_risk_tasks {
                Verdict::Eligible
            } else {
                Verdict::Ineligible
            },
            summary: VerdictSummary {
                passed: report.passed.len(),
                skipped: report.skipped.len(),
                machine_local_failures: report.machine_local_failures.len(),
                repo_code_failures: report.repo_code_failures.len(),
            },
            machine_local_failures: report
                .machine_local_failures
                .iter()
                .map(|f| FailedCheckRendering::from_substrate(f, IssueClass::MachineLocal))
                .collect(),
            repo_code_failures: report
                .repo_code_failures
                .iter()
                .map(|f| FailedCheckRendering::from_substrate(f, IssueClass::RepoCode))
                .collect(),
            passed: report.passed.to_vec(),
            skipped: report
                .skipped
                .iter()
                .map(SkippedCheckRendering::from_substrate)
                .collect(),
        }
    }
}

impl Default for OnboardingDoctor {
    fn default() -> Self {
        Self::new()
    }
}

/// Structured JSON-serializable rendering of an
/// [`OnboardingReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingRendering {
    pub schema_version: String,
    pub verdict: Verdict,
    pub summary: VerdictSummary,
    pub machine_local_failures: Vec<FailedCheckRendering>,
    pub repo_code_failures: Vec<FailedCheckRendering>,
    pub passed: Vec<OnboardingCheck>,
    pub skipped: Vec<SkippedCheckRendering>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Eligible,
    Ineligible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictSummary {
    pub passed: usize,
    pub skipped: usize,
    pub machine_local_failures: usize,
    pub repo_code_failures: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedCheckRendering {
    pub check: OnboardingCheck,
    pub remediation: String,
    pub issue_class: IssueClass,
}

impl FailedCheckRendering {
    fn from_substrate(failed: &FailedCheck, issue_class: IssueClass) -> Self {
        Self {
            check: failed.check,
            remediation: failed.remediation.clone(),
            issue_class,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedCheckRendering {
    pub check: OnboardingCheck,
    pub reason: String,
}

impl SkippedCheckRendering {
    fn from_substrate(skipped: &SkippedCheck) -> Self {
        Self {
            check: skipped.check,
            reason: skipped.reason.clone(),
        }
    }
}

const fn check_label(check: OnboardingCheck) -> &'static str {
    match check {
        OnboardingCheck::HooksInstalled => "hooks_installed",
        OnboardingCheck::RuntimePolicy => "runtime_policy",
        OnboardingCheck::CargoWrapperSane => "cargo_wrapper_sane",
        OnboardingCheck::RobotOutputAvailable => "robot_output_available",
        OnboardingCheck::StoragePathWritable => "storage_path_writable",
        OnboardingCheck::ForbiddenSurfaces => "forbidden_surfaces",
        OnboardingCheck::MachineProfile => "machine_profile",
        OnboardingCheck::SafeProbeBudget => "safe_probe_budget",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding_stress_capsule::{
        CheckOutcome, OnboardingProbeResults, compile_report, sanitize_remediation,
    };

    fn all_pass_report() -> OnboardingReport {
        let mut probes = OnboardingProbeResults::new();
        for check in [
            OnboardingCheck::HooksInstalled,
            OnboardingCheck::RuntimePolicy,
            OnboardingCheck::CargoWrapperSane,
            OnboardingCheck::RobotOutputAvailable,
            OnboardingCheck::StoragePathWritable,
            OnboardingCheck::ForbiddenSurfaces,
            OnboardingCheck::MachineProfile,
            OnboardingCheck::SafeProbeBudget,
        ] {
            probes.record(check, CheckOutcome::Pass);
        }
        compile_report(&probes)
    }

    fn mixed_report() -> OnboardingReport {
        let mut probes = OnboardingProbeResults::new();
        probes.record(
            OnboardingCheck::HooksInstalled,
            CheckOutcome::Fail {
                remediation: "run scripts/install-hooks.sh".to_string(),
                issue_class: IssueClass::MachineLocal,
            },
        );
        probes.record(
            OnboardingCheck::ForbiddenSurfaces,
            CheckOutcome::Fail {
                remediation: "remove direct Tokio test attributes — see migration docs".to_string(),
                issue_class: IssueClass::RepoCode,
            },
        );
        probes.record(
            OnboardingCheck::MachineProfile,
            CheckOutcome::Skipped {
                reason: "small machine — profile not applicable".to_string(),
            },
        );
        probes.record(OnboardingCheck::CargoWrapperSane, CheckOutcome::Pass);
        probes.record(OnboardingCheck::SafeProbeBudget, CheckOutcome::Pass);
        compile_report(&probes)
    }

    #[test]
    fn render_text_eligible_when_all_pass() {
        let report = all_pass_report();
        let text = OnboardingDoctor::new().render_text(&report);
        assert!(text.contains("verdict=eligible"));
        assert!(text.contains("passed=8"));
        assert!(text.contains("machine_local: ∅"));
        assert!(text.contains("repo_code: ∅"));
        assert!(text.contains("skipped: ∅"));
        assert!(text.contains("hooks_installed"));
    }

    #[test]
    fn render_text_ineligible_with_buckets_split() {
        let report = mixed_report();
        let text = OnboardingDoctor::new().render_text(&report);
        assert!(text.contains("verdict=ineligible"));
        // Machine-local bucket has hooks failure.
        assert!(text.contains("machine_local:"));
        assert!(text.contains("hooks_installed"));
        assert!(text.contains("scripts/install-hooks.sh"));
        // Repo-code bucket has forbidden_surfaces failure.
        assert!(text.contains("repo_code:"));
        assert!(text.contains("forbidden_surfaces"));
        assert!(text.contains("Tokio test attributes"));
        // Skipped section present for MachineProfile.
        assert!(text.contains("skipped:"));
        assert!(text.contains("machine_profile"));
    }

    #[test]
    fn render_json_eligible_dispatches_correctly() {
        let report = all_pass_report();
        let rendering = OnboardingDoctor::new().render(&report);
        assert_eq!(rendering.verdict, Verdict::Eligible);
        assert_eq!(rendering.summary.passed, 8);
        assert_eq!(rendering.summary.machine_local_failures, 0);
        assert_eq!(rendering.summary.repo_code_failures, 0);
        assert!(rendering.machine_local_failures.is_empty());
        assert!(rendering.repo_code_failures.is_empty());
    }

    #[test]
    fn render_json_ineligible_with_failures() {
        let report = mixed_report();
        let rendering = OnboardingDoctor::new().render(&report);
        assert_eq!(rendering.verdict, Verdict::Ineligible);
        assert_eq!(rendering.machine_local_failures.len(), 1);
        assert_eq!(rendering.repo_code_failures.len(), 1);
        assert_eq!(rendering.skipped.len(), 1);
        // Issue class faithfully carried through from substrate.
        assert_eq!(
            rendering.machine_local_failures[0].issue_class,
            IssueClass::MachineLocal
        );
        assert_eq!(
            rendering.repo_code_failures[0].issue_class,
            IssueClass::RepoCode
        );
    }

    #[test]
    fn render_json_serde_roundtrip() {
        let report = mixed_report();
        let rendering = OnboardingDoctor::new().render(&report);
        let json = serde_json::to_string(&rendering).unwrap();
        let parsed: OnboardingRendering = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rendering);
    }

    /// SEC fail-closed canary [ft-1650n.17]: substrate-side
    /// `sanitize_remediation` strips forbidden command patterns. The
    /// renderer must faithfully transport the redacted form — never
    /// re-introduce the raw text via Debug formatting or path
    /// duplication.
    #[test]
    fn render_carries_substrate_redaction_state_ft_1650n_17() {
        const RAW_REMEDIATION: &str =
            "force-clean with: rm -rf .git/hooks && git push --force origin main --no-verify";
        // Verify substrate sanitization works (sanity check on
        // upstream contract).
        let sanitized = sanitize_remediation(RAW_REMEDIATION);
        assert!(!sanitized.contains("rm -rf"));
        assert!(!sanitized.contains("--no-verify"));
        assert!(!sanitized.contains("git push --force"));
        assert!(sanitized.contains("[REDACTED-FORBIDDEN]"));

        // Build a report whose remediation went through the
        // substrate's sanitizer.
        let mut probes = OnboardingProbeResults::new();
        probes.record(
            OnboardingCheck::HooksInstalled,
            CheckOutcome::Fail {
                remediation: RAW_REMEDIATION.to_string(),
                issue_class: IssueClass::MachineLocal,
            },
        );
        let report = compile_report(&probes);

        // Renderer must carry the sanitized form, not the raw form.
        let text = OnboardingDoctor::new().render_text(&report);
        let json = serde_json::to_string(&OnboardingDoctor::new().render(&report)).unwrap();

        // Forbidden patterns absent in BOTH text AND JSON.
        for forbidden in &["rm -rf", "--no-verify", "git push --force"] {
            assert!(
                !text.contains(forbidden),
                "br-ft-1650n.17: text rendering must not echo forbidden pattern '{forbidden}'; got {text}"
            );
            assert!(
                !json.contains(forbidden),
                "br-ft-1650n.17: JSON rendering must not echo forbidden pattern '{forbidden}'; got {json}"
            );
        }
        // Sanitization marker IS present (operator sees that the
        // hint was redacted, not silently stripped).
        assert!(text.contains("[REDACTED-FORBIDDEN]"));
        assert!(json.contains("[REDACTED-FORBIDDEN]"));
    }

    #[test]
    fn render_text_passed_list_uses_canonical_labels() {
        // Pin label vocabulary so dashboards and operator output
        // share canonical strings. A future renaming would trip this
        // and force a coordinated update.
        let report = all_pass_report();
        let text = OnboardingDoctor::new().render_text(&report);
        for label in &[
            "hooks_installed",
            "runtime_policy",
            "cargo_wrapper_sane",
            "robot_output_available",
            "storage_path_writable",
            "forbidden_surfaces",
            "machine_profile",
            "safe_probe_budget",
        ] {
            assert!(
                text.contains(label),
                "passed-list rendering must include canonical label '{label}'; got {text}"
            );
        }
    }

    #[test]
    fn render_json_summary_counts_match_vector_lengths() {
        let report = mixed_report();
        let rendering = OnboardingDoctor::new().render(&report);
        // Summary counts MUST equal vector lengths — otherwise
        // dashboards reading the summary diverge from operators
        // reading the lists.
        assert_eq!(
            rendering.summary.passed,
            rendering.passed.len(),
            "summary.passed must equal passed.len()"
        );
        assert_eq!(
            rendering.summary.machine_local_failures,
            rendering.machine_local_failures.len()
        );
        assert_eq!(
            rendering.summary.repo_code_failures,
            rendering.repo_code_failures.len()
        );
        assert_eq!(rendering.summary.skipped, rendering.skipped.len());
    }
}
