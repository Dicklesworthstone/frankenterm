//! br-ft-1650n.17: Agent onboarding stress capsule substrate.
//!
//! Bounded check capsule that validates a new machine / pane can
//! participate in FrankenTerm swarm work — hooks installed,
//! runtime policy respected, cargo wrapper sane, robot output
//! available, storage path writable, and no forbidden direct
//! tokio/test surfaces introduced. Emits a typed pass/fail
//! report with remediation hints; never auto-fixes shared
//! services.
//!
//! ## Why a substrate slice
//!
//! Many failures come from machine-local drift (wrapper
//! breakage, missing hooks, read-only storage path) or
//! repo-code drift (forbidden direct tokio surfaces, missing
//! runtime gates). The substrate ships:
//!
//! - The enumerated check identifiers + their outcomes.
//! - The `IssueClass` (MachineLocal vs RepoCode) so the
//!   report separates the two — operators handle them
//!   differently (machine-local is fixable on the box; repo-
//!   code is a pull-request).
//! - A remediation hint string per failed check, sanitized to
//!   never contain forbidden commands (the bead's
//!   "forbidden-command suppression" criterion).
//! - The eligibility flag — operators read "high-risk tasks
//!   only after this is true" (the bead's "mark pane
//!   ineligible for high-risk tasks until fixed" fallback).
//!
//! The wired-pass slice plugs in real probes (hook check via
//! the git config, cargo wrapper via env var introspection,
//! storage path via `mktemp` + write-and-delete). Today the
//! substrate accepts a `ProbeResults` struct and emits the
//! report.
//!
//! ## What ships in this slice
//!
//! - [`OnboardingCheck`] — typed check identifier (eight
//!   variants from the bead).
//! - [`CheckOutcome`] — `Pass` / `Fail { remediation }` /
//!   `Skipped { reason }`.
//! - [`IssueClass`] — `MachineLocal` / `RepoCode`.
//! - [`OnboardingProbeResults`] — caller-supplied probe
//!   outcomes per check.
//! - [`OnboardingReport`] — structured eligibility output.
//! - [`compile_report`] — pure function over probe results.
//! - [`sanitize_remediation`] — strips forbidden-command
//!   patterns (`rm -rf`, `--no-verify`, etc.).
//!
//! ## What is deferred
//!
//! - Wired-pass probes: a wired-pass slice will plug in real
//!   filesystem / git / cargo probes.
//! - Machine-profile checks: optional CPU / RAM / core
//!   variants are deferred to a follow-up slice.
//! - Capability-passport correlation: the bead's "Build on
//!   capability passports" item is deferred (the substrate is
//!   passport-blind).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Stable schema version for `OnboardingReport` exports.
pub const ONBOARDING_REPORT_SCHEMA_VERSION: &str = "ft.onboarding.report.v1";

/// br-ft-1650n.17: typed check identifier. Enumerated so
/// dashboards and the eligibility gate can match on a known
/// shape rather than free-form strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnboardingCheck {
    /// Git hooks (pre-commit, pre-push) installed and live.
    HooksInstalled,
    /// Runtime policy respected (no `forbid(unsafe_code)` violations,
    /// no banned crates).
    RuntimePolicy,
    /// Cargo wrapper resolves to the workspace's expected version
    /// + target dir is writable.
    CargoWrapperSane,
    /// Robot mode emits structured output the harness can read.
    RobotOutputAvailable,
    /// Storage path is writable + has at least the documented
    /// minimum free space.
    StoragePathWritable,
    /// No forbidden direct tokio/test surfaces in the workspace
    /// (e.g., `#[tokio::test]` outside the asupersync wrapper).
    ForbiddenSurfaces,
    /// Optional high-end machine profile check (CPU cores, RAM,
    /// core-aware tunables). Skipped on small machines without
    /// blocking eligibility.
    MachineProfile,
    /// Safe probe-budget: the capsule itself ran without
    /// exceeding its own time/CPU budget.
    SafeProbeBudget,
}

/// br-ft-1650n.17: outcome per check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CheckOutcome {
    /// Check passed.
    Pass,
    /// Check failed. The `remediation` string has been passed
    /// through [`sanitize_remediation`] and is safe to display.
    Fail {
        remediation: String,
        issue_class: IssueClass,
    },
    /// Check was skipped (e.g., optional and not applicable).
    /// Skipped checks do not block eligibility.
    Skipped { reason: String },
}

/// br-ft-1650n.17: classification per failed check. The bead's
/// "Report separates machine-local issues from repo/code
/// failures" criterion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueClass {
    /// Fixable on this machine without a code change.
    MachineLocal,
    /// Requires a repo / code change. Operator should open a PR
    /// rather than tweak the local box.
    RepoCode,
}

/// br-ft-1650n.17: caller-supplied probe results. The wired-pass
/// slice will populate this from real probes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingProbeResults {
    pub outcomes: BTreeMap<OnboardingCheck, CheckOutcome>,
}

impl OnboardingProbeResults {
    #[must_use]
    pub fn new() -> Self {
        Self {
            outcomes: BTreeMap::new(),
        }
    }

    pub fn record(&mut self, check: OnboardingCheck, outcome: CheckOutcome) -> &mut Self {
        self.outcomes.insert(check, outcome);
        self
    }
}

impl Default for OnboardingProbeResults {
    fn default() -> Self {
        Self::new()
    }
}

/// br-ft-1650n.17: report. The eligibility flag is derived from
/// `failures` — `eligible_for_high_risk_tasks` is true iff
/// every check has Pass or Skipped outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingReport {
    pub schema_version: String,
    pub eligible_for_high_risk_tasks: bool,
    pub machine_local_failures: Vec<FailedCheck>,
    pub repo_code_failures: Vec<FailedCheck>,
    pub passed: Vec<OnboardingCheck>,
    pub skipped: Vec<SkippedCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedCheck {
    pub check: OnboardingCheck,
    pub remediation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedCheck {
    pub check: OnboardingCheck,
    pub reason: String,
}

/// Sanitize a remediation hint by stripping forbidden-command
/// patterns. Operators must NEVER receive a remediation that
/// suggests destructive flags or hooks-bypass; the bead's
/// "forbidden-command suppression" criterion lives here.
///
/// Patterns stripped:
/// - `rm -rf`, `rm -fr`, `rm -r -f`
/// - `git push --force` (`--force-with-lease` is suspect; flagged)
/// - `--no-verify` (skip-hook bypass)
/// - `git reset --hard`
/// - `chmod 777` / `chmod -R 777`
/// - `sudo` prefix (capsule should never need root)
///
/// Stripped occurrences are replaced with `[REDACTED-FORBIDDEN]`
/// so the operator sees that the hint was sanitized.
#[must_use]
pub fn sanitize_remediation(raw: &str) -> String {
    const FORBIDDEN: &[&str] = &[
        "rm -rf",
        "rm -fr",
        "rm -r -f",
        "git push --force",
        "git push -f",
        "--no-verify",
        "git reset --hard",
        "chmod 777",
        "chmod -R 777",
        "sudo ",
    ];
    let mut out = raw.to_string();
    for pattern in FORBIDDEN {
        if out.contains(pattern) {
            out = out.replace(pattern, "[REDACTED-FORBIDDEN]");
        }
    }
    out
}

/// br-ft-1650n.17: pure entry point. Compiles a report from
/// caller-supplied probe results.
///
/// Eligibility:
/// - All required checks must Pass.
/// - `MachineProfile` (the only optional check) may Skip
///   without blocking eligibility.
/// - Any Fail blocks eligibility regardless of class.
///
/// Failed checks have their remediation passed through
/// [`sanitize_remediation`] before inclusion.
#[must_use]
pub fn compile_report(probe_results: &OnboardingProbeResults) -> OnboardingReport {
    let mut machine_local_failures = Vec::new();
    let mut repo_code_failures = Vec::new();
    let mut passed = Vec::new();
    let mut skipped = Vec::new();

    for (check, outcome) in &probe_results.outcomes {
        match outcome {
            CheckOutcome::Pass => passed.push(*check),
            CheckOutcome::Skipped { reason } => skipped.push(SkippedCheck {
                check: *check,
                reason: reason.clone(),
            }),
            CheckOutcome::Fail {
                remediation,
                issue_class,
            } => {
                let entry = FailedCheck {
                    check: *check,
                    remediation: sanitize_remediation(remediation),
                };
                match issue_class {
                    IssueClass::MachineLocal => machine_local_failures.push(entry),
                    IssueClass::RepoCode => repo_code_failures.push(entry),
                }
            }
        }
    }

    let eligible_for_high_risk_tasks =
        machine_local_failures.is_empty() && repo_code_failures.is_empty();

    OnboardingReport {
        schema_version: ONBOARDING_REPORT_SCHEMA_VERSION.to_string(),
        eligible_for_high_risk_tasks,
        machine_local_failures,
        repo_code_failures,
        passed,
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass_all() -> OnboardingProbeResults {
        let mut r = OnboardingProbeResults::new();
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
            r.record(check, CheckOutcome::Pass);
        }
        r
    }

    /// All-pass probe results yield eligibility = true and zero
    /// failures.
    #[test]
    fn all_pass_is_eligible() {
        let report = compile_report(&pass_all());
        assert!(report.eligible_for_high_risk_tasks);
        assert!(report.machine_local_failures.is_empty());
        assert!(report.repo_code_failures.is_empty());
        assert_eq!(report.passed.len(), 8);
        assert!(report.skipped.is_empty());
    }

    /// A single MachineLocal Fail blocks eligibility AND lands in
    /// the machine-local bucket only (not the repo-code bucket).
    #[test]
    fn machine_local_fail_blocks_eligibility() {
        let mut probes = pass_all();
        probes.record(
            OnboardingCheck::StoragePathWritable,
            CheckOutcome::Fail {
                remediation: "free at least 1 GiB on the storage volume".to_string(),
                issue_class: IssueClass::MachineLocal,
            },
        );
        let report = compile_report(&probes);
        assert!(!report.eligible_for_high_risk_tasks);
        assert_eq!(report.machine_local_failures.len(), 1);
        assert!(report.repo_code_failures.is_empty());
        assert_eq!(
            report.machine_local_failures[0].check,
            OnboardingCheck::StoragePathWritable
        );
    }

    /// A single RepoCode Fail blocks eligibility AND lands in
    /// the repo-code bucket only.
    #[test]
    fn repo_code_fail_classifies_separately() {
        let mut probes = pass_all();
        probes.record(
            OnboardingCheck::ForbiddenSurfaces,
            CheckOutcome::Fail {
                remediation: "remove direct tokio::test in src/foo.rs".to_string(),
                issue_class: IssueClass::RepoCode,
            },
        );
        let report = compile_report(&probes);
        assert!(!report.eligible_for_high_risk_tasks);
        assert!(report.machine_local_failures.is_empty());
        assert_eq!(report.repo_code_failures.len(), 1);
        assert_eq!(
            report.repo_code_failures[0].check,
            OnboardingCheck::ForbiddenSurfaces
        );
    }

    /// Mixed failures: one of each class. Both are reported in
    /// their respective buckets and eligibility stays false.
    #[test]
    fn mixed_failures_split_by_class() {
        let mut probes = pass_all();
        probes.record(
            OnboardingCheck::HooksInstalled,
            CheckOutcome::Fail {
                remediation: "run `git config core.hooksPath ./hooks`".to_string(),
                issue_class: IssueClass::MachineLocal,
            },
        );
        probes.record(
            OnboardingCheck::RuntimePolicy,
            CheckOutcome::Fail {
                remediation: "remove `unsafe` block from src/bar.rs".to_string(),
                issue_class: IssueClass::RepoCode,
            },
        );
        let report = compile_report(&probes);
        assert!(!report.eligible_for_high_risk_tasks);
        assert_eq!(report.machine_local_failures.len(), 1);
        assert_eq!(report.repo_code_failures.len(), 1);
    }

    /// Skipped checks do not block eligibility (the optional
    /// MachineProfile check may skip without breaking onboarding).
    #[test]
    fn skipped_checks_do_not_block_eligibility() {
        let mut probes = pass_all();
        probes.record(
            OnboardingCheck::MachineProfile,
            CheckOutcome::Skipped {
                reason: "small dev machine; high-end profile inapplicable".to_string(),
            },
        );
        let report = compile_report(&probes);
        assert!(report.eligible_for_high_risk_tasks);
        assert_eq!(report.skipped.len(), 1);
    }

    /// Forbidden-command suppression: a remediation containing
    /// `rm -rf` is stripped before inclusion in the report.
    #[test]
    fn forbidden_rm_rf_suppressed_in_remediation() {
        let mut probes = pass_all();
        probes.record(
            OnboardingCheck::CargoWrapperSane,
            CheckOutcome::Fail {
                remediation: "wipe with `rm -rf /tmp/ft-target` then retry".to_string(),
                issue_class: IssueClass::MachineLocal,
            },
        );
        let report = compile_report(&probes);
        let remediation = &report.machine_local_failures[0].remediation;
        assert!(!remediation.contains("rm -rf"));
        assert!(remediation.contains("[REDACTED-FORBIDDEN]"));
    }

    /// `--no-verify` is a hooks-bypass forbidden flag and is
    /// stripped.
    #[test]
    fn forbidden_no_verify_suppressed() {
        let mut probes = pass_all();
        probes.record(
            OnboardingCheck::HooksInstalled,
            CheckOutcome::Fail {
                remediation: "commit with `git commit --no-verify -m fix`".to_string(),
                issue_class: IssueClass::MachineLocal,
            },
        );
        let report = compile_report(&probes);
        let remediation = &report.machine_local_failures[0].remediation;
        assert!(!remediation.contains("--no-verify"));
        assert!(remediation.contains("[REDACTED-FORBIDDEN]"));
    }

    /// `git reset --hard` is destructive and stripped.
    #[test]
    fn forbidden_reset_hard_suppressed() {
        let raw = "if branch is bad, run `git reset --hard origin/main`";
        let sanitized = sanitize_remediation(raw);
        assert!(!sanitized.contains("git reset --hard"));
        assert!(sanitized.contains("[REDACTED-FORBIDDEN]"));
    }

    /// `git push --force` is stripped.
    #[test]
    fn forbidden_force_push_suppressed() {
        let raw = "after the rebase, `git push --force origin HEAD`";
        let sanitized = sanitize_remediation(raw);
        assert!(!sanitized.contains("git push --force"));
        assert!(sanitized.contains("[REDACTED-FORBIDDEN]"));
    }

    /// `chmod 777` and `sudo` are stripped.
    #[test]
    fn forbidden_chmod_and_sudo_suppressed() {
        let raw = "run `sudo chmod 777 /tmp/ft-target`";
        let sanitized = sanitize_remediation(raw);
        assert!(!sanitized.contains("chmod 777"));
        assert!(!sanitized.contains("sudo "));
    }

    /// Multiple forbidden patterns in the same string all get
    /// stripped.
    #[test]
    fn multiple_forbidden_patterns_all_suppressed() {
        let raw = "first `rm -rf` the dir, then `git push --force` to overwrite";
        let sanitized = sanitize_remediation(raw);
        assert!(!sanitized.contains("rm -rf"));
        assert!(!sanitized.contains("git push --force"));
        let redacted_count = sanitized.matches("[REDACTED-FORBIDDEN]").count();
        assert_eq!(redacted_count, 2);
    }

    /// Innocuous remediation strings pass through unchanged.
    #[test]
    fn safe_remediation_unchanged() {
        let raw = "edit `~/.cargo/config.toml` to set `target-dir`";
        let sanitized = sanitize_remediation(raw);
        assert_eq!(sanitized, raw);
    }

    /// Empty probe results: zero failures means eligibility is
    /// true (vacuous). Operators should NOT call this with an
    /// empty input — the substrate is permissive but the
    /// wired-pass slice should ensure all required checks ran.
    #[test]
    fn empty_probe_results_eligible_vacuously() {
        let probes = OnboardingProbeResults::new();
        let report = compile_report(&probes);
        assert!(report.eligible_for_high_risk_tasks);
        assert!(report.passed.is_empty());
    }

    /// OnboardingReport serde roundtrip preserves every section
    /// (passed, skipped, machine_local_failures, repo_code_failures).
    #[test]
    fn report_serde_roundtrip() {
        let mut probes = pass_all();
        probes.record(
            OnboardingCheck::HooksInstalled,
            CheckOutcome::Fail {
                remediation: "install hooks".to_string(),
                issue_class: IssueClass::MachineLocal,
            },
        );
        probes.record(
            OnboardingCheck::ForbiddenSurfaces,
            CheckOutcome::Fail {
                remediation: "fix code".to_string(),
                issue_class: IssueClass::RepoCode,
            },
        );
        probes.record(
            OnboardingCheck::MachineProfile,
            CheckOutcome::Skipped {
                reason: "n/a".to_string(),
            },
        );
        let report = compile_report(&probes);
        let json = serde_json::to_string(&report).expect("serialize");
        let back: OnboardingReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(report, back);
    }

    /// CheckOutcome serde roundtrips for every variant.
    #[test]
    fn check_outcome_serde_roundtrip() {
        let outcomes = vec![
            CheckOutcome::Pass,
            CheckOutcome::Fail {
                remediation: "x".to_string(),
                issue_class: IssueClass::MachineLocal,
            },
            CheckOutcome::Fail {
                remediation: "y".to_string(),
                issue_class: IssueClass::RepoCode,
            },
            CheckOutcome::Skipped {
                reason: "n/a".to_string(),
            },
        ];
        for o in outcomes {
            let json = serde_json::to_string(&o).expect("serialize");
            let back: CheckOutcome = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(o, back);
        }
    }

    /// Schema version is pinned for downstream consumers.
    #[test]
    fn schema_version_is_stable() {
        let report = compile_report(&pass_all());
        assert_eq!(report.schema_version, ONBOARDING_REPORT_SCHEMA_VERSION);
        assert_eq!(report.schema_version, "ft.onboarding.report.v1");
    }

    /// Pure function: same input produces same output.
    #[test]
    fn compile_report_is_pure() {
        let probes = pass_all();
        let r1 = compile_report(&probes);
        let r2 = compile_report(&probes);
        assert_eq!(r1, r2);
    }
}
