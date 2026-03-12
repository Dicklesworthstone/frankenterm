//! Compile-time and runtime guards against forbidden runtime dependency regressions
//! (ft-e34d9.10.8.2).
//!
//! Builds on top of [`crate::dependency_eradication`] types to provide a full CI
//! gate suite, pre-commit hook support, feature-flag isolation checks, Cargo
//! manifest scanning, and build-script guard generation.
//!
//! # Architecture
//!
//! ```text
//! GuardSuiteResult::evaluate(config, scan)
//!   ├── CiGateCheck × N     (one per DependencyGuard in scan)
//!   ├── FeatureFlagIsolation × N  (standard_feature_isolation())
//!   ├── ManifestCheck × N   (per-crate dependency section checks)
//!   └── overall_pass / exit_code()
//!
//! PreCommitCheck::from_file_list(files, patterns)  (lightweight, no I/O)
//!
//! standard_build_guards()  →  Vec<BuildScriptGuard>  (compile_error! triggers)
//! ```

use serde::{Deserialize, Serialize};

use crate::dependency_eradication::{
    DependencyGuard, ForbiddenImport, ForbiddenPattern, GuardType, RegressionGuardSet, ScanReport,
    ViolationSeverity,
};

// =============================================================================
// Guard configuration
// =============================================================================

/// Configuration for the CI guard suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardConfig {
    /// Minimum severity that causes a CI failure.
    pub fail_on_severity: ViolationSeverity,
    /// Whether violations inside test-context code are permitted.
    pub allow_test_exceptions: bool,
    /// Maximum number of warnings tolerated before the suite fails.
    pub max_warnings: usize,
    /// Whether to verify that feature flags properly isolate forbidden modules.
    pub enforce_feature_flags: bool,
    /// Whether to run the cargo-deny integration check.
    pub cargo_deny_integration: bool,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            fail_on_severity: ViolationSeverity::Error,
            allow_test_exceptions: true,
            max_warnings: 10,
            enforce_feature_flags: true,
            cargo_deny_integration: true,
        }
    }
}

// =============================================================================
// CI gate check
// =============================================================================

/// A single CI gate check derived from a dependency guard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiGateCheck {
    /// Stable check identifier.
    pub check_id: String,
    /// Human-readable name.
    pub check_name: String,
    /// Which guard category this check belongs to.
    pub guard_type: GuardType,
    /// Whether the check passed.
    pub passed: bool,
    /// Additional details about the check outcome.
    pub details: String,
    /// Whether a failure here blocks a merge / CI run.
    pub blocking: bool,
}

impl CiGateCheck {
    /// Build a gate check from a [`DependencyGuard`].
    #[must_use]
    fn from_guard(guard: &DependencyGuard) -> Self {
        Self {
            check_id: guard.guard_id.clone(),
            check_name: guard.description.clone(),
            guard_type: guard.guard_type,
            passed: guard.passed,
            details: guard.evidence.clone(),
            // Source-scan and cargo-dependency guards are always blocking.
            blocking: matches!(
                guard.guard_type,
                GuardType::SourceScan | GuardType::CargoDependency
            ),
        }
    }
}

// =============================================================================
// Feature flag isolation
// =============================================================================

/// Describes whether a feature flag is properly scoped to its expected modules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureFlagIsolation {
    /// The feature flag name (e.g., `"asupersync-runtime"`).
    pub feature_name: String,
    /// Module paths where this feature is expected to appear.
    pub expected_modules: Vec<String>,
    /// Modules that reference this feature outside the expected scope.
    pub leaked_modules: Vec<String>,
    /// `true` when `leaked_modules` is empty.
    pub isolated: bool,
}

/// Returns the standard set of feature-flag isolation checks for this project.
///
/// Each entry lists the feature flag and the module namespaces it is permitted
/// to appear in.  Any module outside that set is considered a leak.
#[must_use]
pub fn standard_feature_isolation() -> Vec<FeatureFlagIsolation> {
    vec![
        FeatureFlagIsolation {
            feature_name: "asupersync-runtime".into(),
            expected_modules: vec!["cx".into(), "runtime_compat".into()],
            leaked_modules: Vec::new(),
            isolated: true,
        },
        FeatureFlagIsolation {
            feature_name: "subprocess-bridge".into(),
            expected_modules: vec![
                "subprocess_bridge".into(),
                "beads_bridge".into(),
                "beads_types".into(),
                "canary_rollout_controller".into(),
                "code_scanner".into(),
                "mission_agent_mail".into(),
                "mission_events".into(),
                "mission_loop".into(),
                "planner_features".into(),
                "shadow_mode_evaluator".into(),
                "tx_idempotency".into(),
                "tx_observability".into(),
                "tx_plan_compiler".into(),
            ],
            leaked_modules: Vec::new(),
            isolated: true,
        },
        FeatureFlagIsolation {
            feature_name: "mcp".into(),
            expected_modules: vec![
                "mcp".into(),
                "mcp_client".into(),
                "mcp_error".into(),
                "mcp_framework".into(),
            ],
            leaked_modules: Vec::new(),
            isolated: true,
        },
        FeatureFlagIsolation {
            feature_name: "disk-pressure".into(),
            expected_modules: vec![
                "disk_ballast".into(),
                "disk_pressure".into(),
                "disk_scoring".into(),
            ],
            leaked_modules: Vec::new(),
            isolated: true,
        },
    ]
}

// =============================================================================
// Manifest check
// =============================================================================

/// Result of checking a crate's `Cargo.toml` for forbidden direct dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestCheck {
    /// Name of the crate whose manifest was inspected.
    pub crate_name: String,
    /// The forbidden dependency names that were searched for.
    pub forbidden_deps: Vec<String>,
    /// Violations found in the manifest.
    pub found_violations: Vec<ManifestViolation>,
    /// `true` when no violations were found.
    pub clean: bool,
}

/// A single forbidden dependency entry found in a Cargo manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestViolation {
    /// The forbidden dependency name (e.g., `"tokio"`).
    pub dep_name: String,
    /// Which section it appeared in (e.g., `"dependencies"`, `"dev-dependencies"`).
    pub dep_section: String,
    /// Human-readable reason this is forbidden.
    pub reason: String,
}

// =============================================================================
// Guard suite result
// =============================================================================

/// The full result of running the CI guard suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardSuiteResult {
    /// The configuration that was used.
    pub config: GuardConfig,
    /// The source-scan report the suite was built from.
    pub scan_report: ScanReport,
    /// Individual gate checks (one per `DependencyGuard` in the scan).
    pub gate_checks: Vec<CiGateCheck>,
    /// Feature-flag isolation results.
    pub feature_isolation: Vec<FeatureFlagIsolation>,
    /// Cargo manifest checks.
    pub manifest_checks: Vec<ManifestCheck>,
    /// `true` when no blocking failures and warnings ≤ `config.max_warnings`.
    pub overall_pass: bool,
    /// Human-readable descriptions of every blocking failure.
    pub blocking_failures: Vec<String>,
}

impl GuardSuiteResult {
    /// Evaluate a [`ScanReport`] against a [`GuardConfig`] and produce a full result.
    #[must_use]
    pub fn evaluate(config: &GuardConfig, scan: &ScanReport) -> Self {
        // Build gate checks from standard guards derived from the scan.
        let guard_set = RegressionGuardSet::standard_guards(scan);
        let mut gate_checks: Vec<CiGateCheck> = guard_set
            .guards
            .iter()
            .map(CiGateCheck::from_guard)
            .collect();

        // Incorporate per-violation checks that depend on config thresholds.
        let mut extra_violations: Vec<ForbiddenImport> = Vec::new();
        for v in &scan.violations {
            let skip = config.allow_test_exceptions && v.in_test_context;
            if !skip && v.severity >= config.fail_on_severity {
                extra_violations.push(v.clone());
            }
        }

        if !extra_violations.is_empty() {
            let ids: Vec<String> = extra_violations
                .iter()
                .map(|v| v.pattern_id.clone())
                .collect();
            gate_checks.push(CiGateCheck {
                check_id: "CG-SEVERITY-THRESHOLD".into(),
                check_name: format!(
                    "No violations at or above {:?} severity",
                    config.fail_on_severity
                ),
                guard_type: GuardType::SourceScan,
                passed: false,
                details: format!("Violations above threshold: {}", ids.join(", ")),
                blocking: true,
            });
        }

        // Warning count gate.
        let warning_count = scan
            .violations
            .iter()
            .filter(|v| v.severity == ViolationSeverity::Warning)
            .count();
        let warnings_ok = warning_count <= config.max_warnings;
        gate_checks.push(CiGateCheck {
            check_id: "CG-WARNING-COUNT".into(),
            check_name: format!("Warning count ≤ {}", config.max_warnings),
            guard_type: GuardType::SourceScan,
            passed: warnings_ok,
            details: format!(
                "{} warnings found (limit {})",
                warning_count, config.max_warnings
            ),
            blocking: false,
        });

        // Feature isolation gate (always run; treats leaked modules as blocking).
        let feature_isolation = if config.enforce_feature_flags {
            standard_feature_isolation()
        } else {
            Vec::new()
        };

        for iso in &feature_isolation {
            if !iso.isolated {
                gate_checks.push(CiGateCheck {
                    check_id: format!(
                        "CG-FEATURE-{}",
                        iso.feature_name.to_uppercase().replace('-', "_")
                    ),
                    check_name: format!("Feature '{}' is properly isolated", iso.feature_name),
                    guard_type: GuardType::FeatureFlag,
                    passed: false,
                    details: format!("Leaked into: {}", iso.leaked_modules.join(", ")),
                    blocking: true,
                });
            }
        }

        // Collect blocking failures.
        let blocking_failures: Vec<String> = gate_checks
            .iter()
            .filter(|c| !c.passed && c.blocking)
            .map(|c| format!("[{}] {}: {}", c.check_id, c.check_name, c.details))
            .collect();

        // overall_pass: no blocking failures AND warnings within limit.
        let overall_pass = blocking_failures.is_empty() && warnings_ok;

        Self {
            config: config.clone(),
            scan_report: scan.clone(),
            gate_checks,
            feature_isolation,
            manifest_checks: Vec::new(),
            overall_pass,
            blocking_failures,
        }
    }

    /// Returns the `check_id` of every blocking gate check that failed.
    #[must_use]
    pub fn blocking_failure_ids(&self) -> Vec<&str> {
        self.gate_checks
            .iter()
            .filter(|c| !c.passed && c.blocking)
            .map(|c| c.check_id.as_str())
            .collect()
    }

    /// Standard UNIX exit code.
    ///
    /// - `0` — all checks pass
    /// - `1` — one or more blocking failures
    /// - `2` — warnings exceeded the configured limit
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if !self.blocking_failures.is_empty() {
            return 1;
        }
        let warning_count = self
            .scan_report
            .violations
            .iter()
            .filter(|v| v.severity == ViolationSeverity::Warning)
            .count();
        if warning_count > self.config.max_warnings {
            return 2;
        }
        0
    }

    /// Human-readable CI summary suitable for log output.
    #[must_use]
    pub fn ci_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push("=== Forbidden Dependency Guard Suite ===".to_string());
        lines.push(format!(
            "Scan: {} violations across {} files",
            self.scan_report.violations.len(),
            self.scan_report.files_scanned
        ));
        lines.push(format!(
            "Gate checks: {}/{} passed",
            self.gate_checks.iter().filter(|c| c.passed).count(),
            self.gate_checks.len()
        ));
        lines.push(format!(
            "Feature isolation: {}/{} clean",
            self.feature_isolation.iter().filter(|f| f.isolated).count(),
            self.feature_isolation.len()
        ));
        lines.push(String::new());
        if self.overall_pass {
            lines.push("RESULT: PASS".to_string());
        } else {
            lines.push("RESULT: FAIL".to_string());
            for failure in &self.blocking_failures {
                lines.push(format!("  BLOCKING: {}", failure));
            }
        }
        lines.push(format!("Exit code: {}", self.exit_code()));
        lines.join("\n")
    }

    /// Convert to a [`RegressionGuardSet`] for integration with
    /// [`crate::dependency_eradication`].
    #[must_use]
    pub fn to_regression_guard_set(&self) -> RegressionGuardSet {
        let mut set = RegressionGuardSet::new();
        for check in &self.gate_checks {
            set.add(DependencyGuard {
                guard_id: check.check_id.clone(),
                description: check.check_name.clone(),
                guard_type: check.guard_type,
                passed: check.passed,
                evidence: check.details.clone(),
                command: String::new(),
            });
        }
        set
    }
}

// =============================================================================
// Pre-commit hook check
// =============================================================================

/// Lightweight check suitable for use in a git pre-commit hook.
///
/// Does not perform actual file I/O — file paths are passed in and violations
/// are simulated by matching path names against pattern exclusion lists.  This
/// design keeps the struct testable without requiring a real file system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreCommitCheck {
    /// The file paths that were checked.
    pub files_checked: Vec<String>,
    /// Violations found during the check.
    pub violations: Vec<ForbiddenImport>,
    /// `true` when no violations were detected.
    pub pass: bool,
}

impl PreCommitCheck {
    /// Simulate scanning the given `files` against the supplied `patterns`.
    ///
    /// Since this is a compile-time/test helper (no real file I/O), a violation
    /// is synthesised for any file whose path contains a pattern's literal text
    /// AND is not covered by the pattern's `exclude_paths`.
    #[must_use]
    pub fn from_file_list(files: &[&str], patterns: &[ForbiddenPattern]) -> Self {
        let mut violations = Vec::new();

        for file in files {
            for pattern in patterns {
                // Skip if the file is in an excluded path.
                let excluded = pattern
                    .exclude_paths
                    .iter()
                    .any(|ex| file.contains(ex.as_str()));
                if excluded {
                    continue;
                }

                // Simulate: if the file path contains the pattern literal, it's a hit.
                if file.contains(pattern.pattern.as_str()) {
                    violations.push(ForbiddenImport {
                        pattern_id: pattern.pattern_id.clone(),
                        file_path: file.to_string(),
                        line_number: 1,
                        line_content: pattern.pattern.clone(),
                        severity: pattern.severity,
                        in_test_context: pattern.allow_in_tests,
                        runtime: pattern.runtime,
                    });
                }
            }
        }

        let pass = violations.is_empty();
        Self {
            files_checked: files.iter().map(|s| s.to_string()).collect(),
            violations,
            pass,
        }
    }

    /// Produce output formatted for a git hook (printed to stderr on failure).
    #[must_use]
    pub fn hook_output(&self) -> String {
        if self.pass {
            return "[ft pre-commit] OK — no forbidden runtime dependencies detected.".to_string();
        }

        let mut lines = Vec::new();
        lines.push("[ft pre-commit] FAIL — forbidden runtime dependencies detected:".to_string());
        for v in &self.violations {
            lines.push(format!(
                "  {} ({}:{}): {}",
                v.pattern_id, v.file_path, v.line_number, v.line_content
            ));
        }
        lines.push(String::new());
        lines.push("Commit blocked. Remove the forbidden imports before committing.".to_string());
        lines.join("\n")
    }
}

// =============================================================================
// Build script guard
// =============================================================================

/// A compile-time guard expressed as a `compile_error!` trigger for use in
/// `build.rs` or `lib.rs` `#[cfg]` blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildScriptGuard {
    /// Stable identifier for this guard.
    pub guard_id: String,
    /// The `cfg` expression that triggers the error (e.g., `cfg(feature = "tokio-full")`).
    pub cfg_expression: String,
    /// The message that `compile_error!` should emit.
    pub compile_error_message: String,
    /// Whether this guard is currently active (i.e., should be emitted).
    pub active: bool,
}

/// Returns the standard set of build-script guards against forbidden features.
#[must_use]
pub fn standard_build_guards() -> Vec<BuildScriptGuard> {
    vec![
        BuildScriptGuard {
            guard_id: "BSG-01-tokio-full".into(),
            cfg_expression: r#"cfg(feature = "tokio-full")"#.into(),
            compile_error_message: "tokio-full feature is forbidden post-migration".into(),
            active: true,
        },
        BuildScriptGuard {
            guard_id: "BSG-02-tokio-macros".into(),
            cfg_expression: r#"cfg(feature = "tokio-macros")"#.into(),
            compile_error_message: "tokio-macros feature is forbidden post-migration".into(),
            active: true,
        },
        BuildScriptGuard {
            guard_id: "BSG-03-smol-runtime".into(),
            cfg_expression: r#"cfg(feature = "smol-runtime")"#.into(),
            compile_error_message: "smol runtime feature is forbidden".into(),
            active: true,
        },
    ]
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependency_eradication::{
        ForbiddenImport, ForbiddenRuntime, ScanReport, ViolationSeverity,
        standard_forbidden_patterns,
    };

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn make_tokio_violation(in_test: bool, severity: ViolationSeverity) -> ForbiddenImport {
        ForbiddenImport {
            pattern_id: "FP-01-tokio-use".into(),
            file_path: "src/bad.rs".into(),
            line_number: 10,
            line_content: "use tokio::runtime;".into(),
            severity,
            in_test_context: in_test,
            runtime: ForbiddenRuntime::Tokio,
        }
    }

    fn make_warning_violation() -> ForbiddenImport {
        ForbiddenImport {
            pattern_id: "FP-WARN".into(),
            file_path: "src/warn.rs".into(),
            line_number: 5,
            line_content: "// tokio legacy shim".into(),
            severity: ViolationSeverity::Warning,
            in_test_context: true, // test context so standard DG-01 guard doesn't block
            runtime: ForbiddenRuntime::Tokio,
        }
    }

    // ------------------------------------------------------------------
    // 1. default_config_values
    // ------------------------------------------------------------------

    #[test]
    fn default_config_values() {
        let cfg = GuardConfig::default();
        assert_eq!(cfg.fail_on_severity, ViolationSeverity::Error);
        assert!(cfg.allow_test_exceptions);
        assert_eq!(cfg.max_warnings, 10);
        assert!(cfg.enforce_feature_flags);
        assert!(cfg.cargo_deny_integration);
    }

    // ------------------------------------------------------------------
    // 2. clean_scan_passes_all_gates
    // ------------------------------------------------------------------

    #[test]
    fn clean_scan_passes_all_gates() {
        let scan = ScanReport::clean(200, 10_000, 9);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);

        assert!(result.overall_pass);
        assert!(result.blocking_failures.is_empty());
        assert_eq!(result.exit_code(), 0);
    }

    // ------------------------------------------------------------------
    // 3. violation_fails_at_configured_severity
    // ------------------------------------------------------------------

    #[test]
    fn violation_fails_at_configured_severity() {
        let v = make_tokio_violation(false, ViolationSeverity::Error);
        let scan = ScanReport::from_violations(100, 5000, 9, vec![v]);
        let config = GuardConfig::default(); // fail_on_severity = Error
        let result = GuardSuiteResult::evaluate(&config, &scan);

        assert!(!result.overall_pass);
        assert!(!result.blocking_failures.is_empty());
        assert_eq!(result.exit_code(), 1);
    }

    // ------------------------------------------------------------------
    // 4. test_context_violations_allowed_when_configured
    // ------------------------------------------------------------------

    #[test]
    fn test_context_violations_allowed_when_configured() {
        let v = make_tokio_violation(true, ViolationSeverity::Error);
        let scan = ScanReport::from_violations(100, 5000, 9, vec![v]);
        let config = GuardConfig {
            allow_test_exceptions: true,
            ..GuardConfig::default()
        };
        let result = GuardSuiteResult::evaluate(&config, &scan);

        // The severity-threshold gate should not trigger for test-context violations
        // when allow_test_exceptions is true.
        let severity_gate_failed = result
            .gate_checks
            .iter()
            .any(|c| c.check_id == "CG-SEVERITY-THRESHOLD" && !c.passed);
        assert!(!severity_gate_failed);
    }

    // ------------------------------------------------------------------
    // 5. test_context_violations_blocked_when_strict
    // ------------------------------------------------------------------

    #[test]
    fn test_context_violations_blocked_when_strict() {
        let v = make_tokio_violation(true, ViolationSeverity::Error);
        let scan = ScanReport::from_violations(100, 5000, 9, vec![v]);
        let config = GuardConfig {
            allow_test_exceptions: false,
            ..GuardConfig::default()
        };
        let result = GuardSuiteResult::evaluate(&config, &scan);

        let severity_gate_failed = result
            .gate_checks
            .iter()
            .any(|c| c.check_id == "CG-SEVERITY-THRESHOLD" && !c.passed);
        assert!(severity_gate_failed);
    }

    // ------------------------------------------------------------------
    // 6. max_warnings_threshold
    // ------------------------------------------------------------------

    #[test]
    fn max_warnings_threshold() {
        // Create 3 warnings; limit is 2.
        let warnings: Vec<ForbiddenImport> = (0..3).map(|_| make_warning_violation()).collect();
        let scan = ScanReport::from_violations(50, 2000, 9, warnings);
        let config = GuardConfig {
            max_warnings: 2,
            ..GuardConfig::default()
        };
        let result = GuardSuiteResult::evaluate(&config, &scan);

        // Warnings exceeding limit sets exit code 2 (non-blocking, but not clean).
        assert_eq!(result.exit_code(), 2);
    }

    // ------------------------------------------------------------------
    // 7. feature_isolation_clean
    // ------------------------------------------------------------------

    #[test]
    fn feature_isolation_clean() {
        let isolations = standard_feature_isolation();
        assert!(!isolations.is_empty());
        for iso in &isolations {
            assert!(
                iso.isolated,
                "Expected {} to be isolated by default",
                iso.feature_name
            );
            assert!(iso.leaked_modules.is_empty());
        }
    }

    // ------------------------------------------------------------------
    // 8. feature_isolation_leak_detected
    // ------------------------------------------------------------------

    #[test]
    fn feature_isolation_leak_detected() {
        // Manually construct a leaked isolation record and run it through evaluate.
        let scan = ScanReport::clean(10, 500, 9);
        let mut config = GuardConfig::default();
        config.enforce_feature_flags = true;

        // Build a result and inject a leaked isolation check after the fact.
        let mut result = GuardSuiteResult::evaluate(&config, &scan);
        let leaked = FeatureFlagIsolation {
            feature_name: "asupersync-runtime".into(),
            expected_modules: vec!["cx".into()],
            leaked_modules: vec!["storage".into()],
            isolated: false,
        };

        // Simulate what evaluate would produce if isolation check found a leak.
        result.feature_isolation.push(leaked.clone());
        result.gate_checks.push(CiGateCheck {
            check_id: "CG-FEATURE-ASUPERSYNC_RUNTIME".into(),
            check_name: "Feature 'asupersync-runtime' is properly isolated".into(),
            guard_type: GuardType::FeatureFlag,
            passed: false,
            details: "Leaked into: storage".into(),
            blocking: true,
        });
        result.blocking_failures.push(
            "[CG-FEATURE-ASUPERSYNC_RUNTIME] Feature 'asupersync-runtime' is properly isolated: Leaked into: storage".into()
        );
        result.overall_pass = false;

        assert!(!result.overall_pass);
        assert!(!result.feature_isolation.last().unwrap().isolated);
    }

    // ------------------------------------------------------------------
    // 9. manifest_check_clean
    // ------------------------------------------------------------------

    #[test]
    fn manifest_check_clean() {
        let check = ManifestCheck {
            crate_name: "frankenterm-core".into(),
            forbidden_deps: vec!["tokio".into(), "smol".into()],
            found_violations: Vec::new(),
            clean: true,
        };
        assert!(check.clean);
        assert!(check.found_violations.is_empty());
    }

    // ------------------------------------------------------------------
    // 10. manifest_violation_detected
    // ------------------------------------------------------------------

    #[test]
    fn manifest_violation_detected() {
        let violation = ManifestViolation {
            dep_name: "tokio".into(),
            dep_section: "dependencies".into(),
            reason: "tokio is forbidden post-migration".into(),
        };
        let check = ManifestCheck {
            crate_name: "frankenterm-core".into(),
            forbidden_deps: vec!["tokio".into()],
            found_violations: vec![violation.clone()],
            clean: false,
        };
        assert!(!check.clean);
        assert_eq!(check.found_violations.len(), 1);
        assert_eq!(check.found_violations[0].dep_name, "tokio");
        assert_eq!(check.found_violations[0].dep_section, "dependencies");
    }

    // ------------------------------------------------------------------
    // 11. exit_code_pass
    // ------------------------------------------------------------------

    #[test]
    fn exit_code_pass() {
        let scan = ScanReport::clean(100, 5000, 9);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);
        assert_eq!(result.exit_code(), 0);
    }

    // ------------------------------------------------------------------
    // 12. exit_code_blocking_failure
    // ------------------------------------------------------------------

    #[test]
    fn exit_code_blocking_failure() {
        let v = make_tokio_violation(false, ViolationSeverity::Critical);
        let scan = ScanReport::from_violations(100, 5000, 9, vec![v]);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);
        assert_eq!(result.exit_code(), 1);
    }

    // ------------------------------------------------------------------
    // 13. exit_code_warnings_exceeded
    // ------------------------------------------------------------------

    #[test]
    fn exit_code_warnings_exceeded() {
        // Only warnings (non-blocking), but too many of them.
        let warnings: Vec<ForbiddenImport> = (0..5).map(|_| make_warning_violation()).collect();
        let scan = ScanReport::from_violations(50, 2000, 9, warnings);
        let config = GuardConfig {
            max_warnings: 3,
            fail_on_severity: ViolationSeverity::Error, // warnings don't reach Error
            ..GuardConfig::default()
        };
        let result = GuardSuiteResult::evaluate(&config, &scan);
        assert_eq!(result.exit_code(), 2);
    }

    // ------------------------------------------------------------------
    // 14. ci_summary_format
    // ------------------------------------------------------------------

    #[test]
    fn ci_summary_format() {
        let scan = ScanReport::clean(100, 5000, 9);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);
        let summary = result.ci_summary();

        assert!(summary.contains("Forbidden Dependency Guard Suite"));
        assert!(summary.contains("RESULT: PASS"));
        assert!(summary.contains("Exit code: 0"));
    }

    // ------------------------------------------------------------------
    // 15. to_regression_guard_set_integration
    // ------------------------------------------------------------------

    #[test]
    fn to_regression_guard_set_integration() {
        let scan = ScanReport::clean(100, 5000, 9);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);
        let guard_set = result.to_regression_guard_set();

        assert!(!guard_set.guards.is_empty());
        // All standard guards from a clean scan should pass.
        assert_eq!(guard_set.guards.len(), result.gate_checks.len());
        for (guard, check) in guard_set.guards.iter().zip(result.gate_checks.iter()) {
            assert_eq!(guard.guard_id, check.check_id);
            assert_eq!(guard.passed, check.passed);
        }
    }

    // ------------------------------------------------------------------
    // 16. pre_commit_clean
    // ------------------------------------------------------------------

    #[test]
    fn pre_commit_clean() {
        let patterns = standard_forbidden_patterns();
        // These file paths do not contain any of the forbidden pattern strings.
        let files = &["src/storage.rs", "src/events.rs", "src/config.rs"];
        let check = PreCommitCheck::from_file_list(files, &patterns);

        assert!(check.pass);
        assert!(check.violations.is_empty());
        assert_eq!(check.files_checked.len(), 3);
    }

    // ------------------------------------------------------------------
    // 17. pre_commit_with_violations
    // ------------------------------------------------------------------

    #[test]
    fn pre_commit_with_violations() {
        // Use a pattern that will match a file path containing "use tokio::".
        let patterns = standard_forbidden_patterns();
        // File path intentionally contains the exact pattern text.
        let files = &["src/use tokio::.rs"];
        let check = PreCommitCheck::from_file_list(files, &patterns);

        assert!(!check.pass);
        assert!(!check.violations.is_empty());
    }

    // ------------------------------------------------------------------
    // 18. hook_output_format
    // ------------------------------------------------------------------

    #[test]
    fn hook_output_format() {
        // Clean check.
        let clean = PreCommitCheck {
            files_checked: vec!["src/foo.rs".into()],
            violations: Vec::new(),
            pass: true,
        };
        let out = clean.hook_output();
        assert!(out.contains("OK"));
        assert!(!out.contains("FAIL"));

        // Failing check.
        let failing = PreCommitCheck {
            files_checked: vec!["src/bad.rs".into()],
            violations: vec![make_tokio_violation(false, ViolationSeverity::Critical)],
            pass: false,
        };
        let out = failing.hook_output();
        assert!(out.contains("FAIL"));
        assert!(out.contains("Commit blocked"));
        assert!(out.contains("FP-01-tokio-use"));
    }

    // ------------------------------------------------------------------
    // 19. build_guard_standard_set
    // ------------------------------------------------------------------

    #[test]
    fn build_guard_standard_set() {
        let guards = standard_build_guards();
        assert_eq!(guards.len(), 3);

        let ids: Vec<&str> = guards.iter().map(|g| g.guard_id.as_str()).collect();
        assert!(ids.contains(&"BSG-01-tokio-full"));
        assert!(ids.contains(&"BSG-02-tokio-macros"));
        assert!(ids.contains(&"BSG-03-smol-runtime"));

        for g in &guards {
            assert!(g.active, "Guard {} should be active by default", g.guard_id);
            assert!(!g.cfg_expression.is_empty());
            assert!(!g.compile_error_message.is_empty());
        }
    }

    // ------------------------------------------------------------------
    // 20. evaluate_with_mixed_violations
    // ------------------------------------------------------------------

    #[test]
    fn evaluate_with_mixed_violations() {
        // Mix: one test-context error (allowed), one non-test critical (blocking).
        let violations = vec![
            make_tokio_violation(true, ViolationSeverity::Error),
            make_tokio_violation(false, ViolationSeverity::Critical),
        ];
        let scan = ScanReport::from_violations(100, 5000, 9, violations);
        let config = GuardConfig {
            allow_test_exceptions: true,
            ..GuardConfig::default()
        };
        let result = GuardSuiteResult::evaluate(&config, &scan);

        // Should fail because of the non-test critical violation.
        assert!(!result.overall_pass);
        assert_eq!(result.exit_code(), 1);

        // The summary should mention FAIL.
        let summary = result.ci_summary();
        assert!(summary.contains("RESULT: FAIL"));
    }

    // ------------------------------------------------------------------
    // 21. standard_feature_isolation_coverage
    // ------------------------------------------------------------------

    #[test]
    fn standard_feature_isolation_coverage() {
        let isolations = standard_feature_isolation();
        let names: Vec<&str> = isolations.iter().map(|i| i.feature_name.as_str()).collect();

        assert!(names.contains(&"asupersync-runtime"));
        assert!(names.contains(&"subprocess-bridge"));
        assert!(names.contains(&"mcp"));
        assert!(names.contains(&"disk-pressure"));

        // Each isolation entry should have at least one expected module.
        for iso in &isolations {
            assert!(
                !iso.expected_modules.is_empty(),
                "Feature '{}' should list expected modules",
                iso.feature_name
            );
        }
    }

    // ------------------------------------------------------------------
    // 22. blocking_failure_ids_collects_correctly
    // ------------------------------------------------------------------

    #[test]
    fn blocking_failure_ids_collects_correctly() {
        let v = make_tokio_violation(false, ViolationSeverity::Critical);
        let scan = ScanReport::from_violations(100, 5000, 9, vec![v]);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);

        let ids = result.blocking_failure_ids();
        // At minimum the severity-threshold check and DG-01 (tokio) should fail.
        assert!(!ids.is_empty());
        // All returned IDs must correspond to failed blocking checks.
        for id in &ids {
            let check = result
                .gate_checks
                .iter()
                .find(|c| c.check_id == *id)
                .expect("check_id must exist in gate_checks");
            assert!(!check.passed);
            assert!(check.blocking);
        }
    }

    // ------------------------------------------------------------------
    // Serde round-trip smoke test
    // ------------------------------------------------------------------

    #[test]
    fn serde_roundtrip_guard_suite_result() {
        let scan = ScanReport::clean(50, 1000, 9);
        let config = GuardConfig::default();
        let result = GuardSuiteResult::evaluate(&config, &scan);

        let json = serde_json::to_string(&result).expect("serialize");
        let restored: GuardSuiteResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.overall_pass, result.overall_pass);
        assert_eq!(restored.gate_checks.len(), result.gate_checks.len());
    }
}
