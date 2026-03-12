//! Dependency eradication and cutover governance (ft-e34d9.10.8).
//!
//! Ensures forbidden runtime dependencies (tokio, smol, async-io) are
//! removed and provides compile-time/runtime guards against regression.
//!
//! # Architecture
//!
//! ```text
//! ForbiddenDependencyScanner
//!   ├── scan_source_tree() → ScanReport
//!   │     ├── ForbiddenImport (file, line, pattern)
//!   │     └── ScanVerdict (Clean/Violations)
//!   │
//!   └── ForbiddenPattern (configurable blocklist)
//!
//! RegressionGuardSet
//!   ├── CompileTimeGuard (no forbidden imports)
//!   ├── CargoGuard (no forbidden dependencies)
//!   └── RuntimeGuard (no forbidden API calls)
//!
//! MigrationReport
//!   ├── scan results
//!   ├── guard results
//!   ├── surface contract status
//!   └── residual risk summary
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Forbidden dependency patterns
// =============================================================================

/// A pattern that should not appear in the source tree after migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForbiddenPattern {
    /// Pattern identifier.
    pub pattern_id: String,
    /// The pattern to match (e.g., "use tokio::", "extern crate smol").
    pub pattern: String,
    /// Which runtime this belongs to.
    pub runtime: ForbiddenRuntime,
    /// Severity if found.
    pub severity: ViolationSeverity,
    /// Whether matches in `#[cfg(test)]` blocks are allowed.
    pub allow_in_tests: bool,
    /// File patterns to exclude from scanning (e.g., "vendored/").
    pub exclude_paths: Vec<String>,
}

/// Forbidden runtime categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ForbiddenRuntime {
    /// Tokio runtime and its ecosystem.
    Tokio,
    /// Smol runtime.
    Smol,
    /// async-io crate.
    AsyncIo,
    /// async-executor crate.
    AsyncExecutor,
}

impl ForbiddenRuntime {
    /// Human-readable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Tokio => "tokio",
            Self::Smol => "smol",
            Self::AsyncIo => "async-io",
            Self::AsyncExecutor => "async-executor",
        }
    }
}

/// Severity of a forbidden dependency violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ViolationSeverity {
    /// Informational — may be intentional (e.g., compatibility shim).
    Info,
    /// Warning — should be reviewed.
    Warning,
    /// Error — must be removed before cutover.
    Error,
    /// Critical — blocks migration entirely.
    Critical,
}

/// Standard set of forbidden patterns for post-asupersync migration.
#[must_use]
pub fn standard_forbidden_patterns() -> Vec<ForbiddenPattern> {
    vec![
        ForbiddenPattern {
            pattern_id: "FP-01-tokio-use".into(),
            pattern: "use tokio::".into(),
            runtime: ForbiddenRuntime::Tokio,
            severity: ViolationSeverity::Critical,
            allow_in_tests: false,
            exclude_paths: vec!["vendored/".into(), "target/".into()],
        },
        ForbiddenPattern {
            pattern_id: "FP-02-tokio-extern".into(),
            pattern: "extern crate tokio".into(),
            runtime: ForbiddenRuntime::Tokio,
            severity: ViolationSeverity::Critical,
            allow_in_tests: false,
            exclude_paths: vec!["vendored/".into()],
        },
        ForbiddenPattern {
            pattern_id: "FP-03-tokio-test".into(),
            pattern: "#[tokio::test]".into(),
            runtime: ForbiddenRuntime::Tokio,
            severity: ViolationSeverity::Error,
            allow_in_tests: true,
            exclude_paths: vec!["vendored/".into()],
        },
        ForbiddenPattern {
            pattern_id: "FP-04-tokio-main".into(),
            pattern: "#[tokio::main]".into(),
            runtime: ForbiddenRuntime::Tokio,
            severity: ViolationSeverity::Critical,
            allow_in_tests: false,
            exclude_paths: vec!["vendored/".into()],
        },
        ForbiddenPattern {
            pattern_id: "FP-05-smol-use".into(),
            pattern: "use smol::".into(),
            runtime: ForbiddenRuntime::Smol,
            severity: ViolationSeverity::Error,
            allow_in_tests: false,
            exclude_paths: vec!["vendored/".into()],
        },
        ForbiddenPattern {
            pattern_id: "FP-06-async-io-use".into(),
            pattern: "use async_io::".into(),
            runtime: ForbiddenRuntime::AsyncIo,
            severity: ViolationSeverity::Error,
            allow_in_tests: false,
            exclude_paths: vec!["vendored/".into()],
        },
        ForbiddenPattern {
            pattern_id: "FP-07-async-executor-use".into(),
            pattern: "use async_executor::".into(),
            runtime: ForbiddenRuntime::AsyncExecutor,
            severity: ViolationSeverity::Error,
            allow_in_tests: false,
            exclude_paths: vec!["vendored/".into()],
        },
        ForbiddenPattern {
            pattern_id: "FP-08-tokio-spawn".into(),
            pattern: "tokio::spawn".into(),
            runtime: ForbiddenRuntime::Tokio,
            severity: ViolationSeverity::Critical,
            allow_in_tests: false,
            exclude_paths: vec!["vendored/".into(), "runtime_compat".into()],
        },
        ForbiddenPattern {
            pattern_id: "FP-09-tokio-runtime".into(),
            pattern: "tokio::runtime".into(),
            runtime: ForbiddenRuntime::Tokio,
            severity: ViolationSeverity::Critical,
            allow_in_tests: false,
            exclude_paths: vec!["vendored/".into(), "runtime_compat".into()],
        },
    ]
}

// =============================================================================
// Scan report
// =============================================================================

/// A single forbidden import match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForbiddenImport {
    /// Which pattern was matched.
    pub pattern_id: String,
    /// File path where the match was found.
    pub file_path: String,
    /// Line number (1-based).
    pub line_number: u32,
    /// The matched line content.
    pub line_content: String,
    /// Violation severity.
    pub severity: ViolationSeverity,
    /// Whether this is in a test-only context.
    pub in_test_context: bool,
    /// Which forbidden runtime.
    pub runtime: ForbiddenRuntime,
}

/// Result of scanning the source tree for forbidden dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanReport {
    /// When the scan was performed.
    pub scanned_at_ms: u64,
    /// Total files scanned.
    pub files_scanned: u64,
    /// Total lines scanned.
    pub lines_scanned: u64,
    /// Patterns used for scanning.
    pub patterns_used: usize,
    /// Violations found.
    pub violations: Vec<ForbiddenImport>,
    /// Overall verdict.
    pub verdict: ScanVerdict,
}

/// Overall scan verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScanVerdict {
    /// No forbidden dependencies found.
    Clean,
    /// Only informational/test-only matches.
    CleanWithNotes,
    /// Violations found that must be addressed.
    Violations,
}

impl ScanReport {
    /// Create a clean scan report.
    #[must_use]
    pub fn clean(files_scanned: u64, lines_scanned: u64, patterns_used: usize) -> Self {
        Self {
            scanned_at_ms: 0,
            files_scanned,
            lines_scanned,
            patterns_used,
            violations: Vec::new(),
            verdict: ScanVerdict::Clean,
        }
    }

    /// Create a report from violations.
    #[must_use]
    pub fn from_violations(
        files_scanned: u64,
        lines_scanned: u64,
        patterns_used: usize,
        violations: Vec<ForbiddenImport>,
    ) -> Self {
        let verdict = if violations.is_empty() {
            ScanVerdict::Clean
        } else if violations
            .iter()
            .all(|v| v.severity == ViolationSeverity::Info || v.in_test_context)
        {
            ScanVerdict::CleanWithNotes
        } else {
            ScanVerdict::Violations
        };

        Self {
            scanned_at_ms: 0,
            files_scanned,
            lines_scanned,
            patterns_used,
            violations,
            verdict,
        }
    }

    /// Count of critical violations.
    #[must_use]
    pub fn critical_count(&self) -> usize {
        self.violations
            .iter()
            .filter(|v| v.severity == ViolationSeverity::Critical && !v.in_test_context)
            .count()
    }

    /// Count of error-level violations.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.violations
            .iter()
            .filter(|v| v.severity == ViolationSeverity::Error && !v.in_test_context)
            .count()
    }

    /// Violations grouped by runtime.
    #[must_use]
    pub fn by_runtime(&self) -> BTreeMap<String, Vec<&ForbiddenImport>> {
        let mut map: BTreeMap<String, Vec<&ForbiddenImport>> = BTreeMap::new();
        for v in &self.violations {
            map.entry(v.runtime.label().into()).or_default().push(v);
        }
        map
    }

    /// Whether the scan is clean enough for cutover.
    #[must_use]
    pub fn is_cutover_ready(&self) -> bool {
        matches!(
            self.verdict,
            ScanVerdict::Clean | ScanVerdict::CleanWithNotes
        )
    }
}

// =============================================================================
// Regression guard set
// =============================================================================

/// A set of guards that prevent runtime dependency regression.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionGuardSet {
    /// Individual guards.
    pub guards: Vec<DependencyGuard>,
}

/// A single dependency regression guard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyGuard {
    /// Guard identifier.
    pub guard_id: String,
    /// What this guard protects.
    pub description: String,
    /// Guard type.
    pub guard_type: GuardType,
    /// Whether the guard passed.
    pub passed: bool,
    /// Evidence for the result.
    pub evidence: String,
    /// Verification command.
    pub command: String,
}

/// Type of regression guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuardType {
    /// Source code scanning (no forbidden imports).
    SourceScan,
    /// Cargo.toml dependency check (no forbidden crates).
    CargoDependency,
    /// Feature flag guard (no forbidden features enabled).
    FeatureFlag,
    /// Runtime API guard (no forbidden API usage).
    RuntimeApi,
    /// Build script guard (no forbidden build dependencies).
    BuildScript,
}

impl RegressionGuardSet {
    /// Create an empty guard set.
    #[must_use]
    pub fn new() -> Self {
        Self { guards: Vec::new() }
    }

    /// Add a guard.
    pub fn add(&mut self, guard: DependencyGuard) {
        self.guards.push(guard);
    }

    /// Whether all guards pass.
    #[must_use]
    pub fn all_pass(&self) -> bool {
        !self.guards.is_empty() && self.guards.iter().all(|g| g.passed)
    }

    /// Count of passing guards.
    #[must_use]
    pub fn pass_count(&self) -> usize {
        self.guards.iter().filter(|g| g.passed).count()
    }

    /// Total guards.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.guards.len()
    }

    /// Failing guards.
    #[must_use]
    pub fn failing(&self) -> Vec<&DependencyGuard> {
        self.guards.iter().filter(|g| !g.passed).collect()
    }

    /// Standard guards for the asupersync migration.
    #[must_use]
    pub fn standard_guards(scan: &ScanReport) -> Self {
        let mut set = Self::new();

        set.add(DependencyGuard {
            guard_id: "DG-01-no-tokio-imports".into(),
            description: "No tokio:: imports in source tree".into(),
            guard_type: GuardType::SourceScan,
            passed: scan
                .violations
                .iter()
                .filter(|v| v.runtime == ForbiddenRuntime::Tokio && !v.in_test_context)
                .count()
                == 0,
            evidence: format!(
                "{} tokio violations found",
                scan.violations
                    .iter()
                    .filter(|v| v.runtime == ForbiddenRuntime::Tokio && !v.in_test_context)
                    .count()
            ),
            command: "grep -rn 'use tokio::' src/ --include='*.rs'".into(),
        });

        set.add(DependencyGuard {
            guard_id: "DG-02-no-smol-imports".into(),
            description: "No smol:: imports in source tree".into(),
            guard_type: GuardType::SourceScan,
            passed: scan
                .violations
                .iter()
                .filter(|v| v.runtime == ForbiddenRuntime::Smol && !v.in_test_context)
                .count()
                == 0,
            evidence: format!(
                "{} smol violations found",
                scan.violations
                    .iter()
                    .filter(|v| v.runtime == ForbiddenRuntime::Smol && !v.in_test_context)
                    .count()
            ),
            command: "grep -rn 'use smol::' src/ --include='*.rs'".into(),
        });

        set.add(DependencyGuard {
            guard_id: "DG-03-no-async-io".into(),
            description: "No async-io imports in source tree".into(),
            guard_type: GuardType::SourceScan,
            passed: scan
                .violations
                .iter()
                .filter(|v| v.runtime == ForbiddenRuntime::AsyncIo && !v.in_test_context)
                .count()
                == 0,
            evidence: format!(
                "{} async-io violations found",
                scan.violations
                    .iter()
                    .filter(|v| v.runtime == ForbiddenRuntime::AsyncIo && !v.in_test_context)
                    .count()
            ),
            command: "grep -rn 'use async_io::' src/ --include='*.rs'".into(),
        });

        set.add(DependencyGuard {
            guard_id: "DG-04-scan-verdict-clean".into(),
            description: "Overall scan verdict is clean or clean-with-notes".into(),
            guard_type: GuardType::SourceScan,
            passed: scan.is_cutover_ready(),
            evidence: format!("verdict: {:?}", scan.verdict),
            command: "cargo run -- scan-forbidden-deps".into(),
        });

        set
    }
}

impl Default for RegressionGuardSet {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Migration report
// =============================================================================

/// Complete migration report assembling scan, guard, and status information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationReport {
    /// Report title.
    pub title: String,
    /// Migration identifier.
    pub migration_id: String,
    /// When the report was generated.
    pub generated_at_ms: u64,
    /// Source scan results.
    pub scan: ScanReport,
    /// Regression guard results.
    pub guards: RegressionGuardSet,
    /// Surface contract status (Keep/Replace/Retire counts).
    pub surface_contract: SurfaceContractStatus,
    /// Residual risks.
    pub residual_risks: Vec<ResidualRisk>,
    /// Overall migration status.
    pub status: MigrationStatus,
}

/// Summary of runtime_compat surface contract disposition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceContractStatus {
    /// APIs classified as Keep (permanent).
    pub keep_count: usize,
    /// APIs classified as Replace (transitional, have replacement).
    pub replace_count: usize,
    /// APIs classified as Retire (transitional, pending removal).
    pub retire_count: usize,
    /// APIs that have been fully replaced.
    pub replaced_count: usize,
    /// APIs that have been fully retired.
    pub retired_count: usize,
}

impl SurfaceContractStatus {
    /// Whether all Replace/Retire APIs have been handled.
    #[must_use]
    pub fn all_transitional_resolved(&self) -> bool {
        self.replace_count == self.replaced_count && self.retire_count == self.retired_count
    }

    /// Total APIs in the contract.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.keep_count + self.replace_count + self.retire_count
    }

    /// Remaining transitional APIs.
    #[must_use]
    pub fn remaining_transitional(&self) -> usize {
        (self.replace_count.saturating_sub(self.replaced_count))
            + (self.retire_count.saturating_sub(self.retired_count))
    }
}

/// A residual risk from the migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResidualRisk {
    /// Risk identifier.
    pub risk_id: String,
    /// Description.
    pub description: String,
    /// Severity.
    pub severity: ViolationSeverity,
    /// Mitigation applied.
    pub mitigation: Option<String>,
    /// Follow-up owner.
    pub owner: Option<String>,
    /// Whether accepted.
    pub accepted: bool,
}

/// Overall migration status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MigrationStatus {
    /// Migration incomplete — forbidden dependencies remain.
    InProgress,
    /// Migration complete but pending final verification.
    PendingVerification,
    /// Migration complete and verified.
    Complete,
    /// Migration blocked by critical issues.
    Blocked,
}

impl MigrationReport {
    /// Create a new migration report.
    #[must_use]
    pub fn new(
        migration_id: impl Into<String>,
        scan: ScanReport,
        guards: RegressionGuardSet,
    ) -> Self {
        let status = if !scan.is_cutover_ready() || !guards.all_pass() {
            MigrationStatus::InProgress
        } else {
            MigrationStatus::PendingVerification
        };

        Self {
            title: "Asupersync Migration Report".into(),
            migration_id: migration_id.into(),
            generated_at_ms: 0,
            scan,
            guards,
            surface_contract: SurfaceContractStatus {
                keep_count: 0,
                replace_count: 0,
                retire_count: 0,
                replaced_count: 0,
                retired_count: 0,
            },
            residual_risks: Vec::new(),
            status,
        }
    }

    /// Set surface contract status.
    pub fn set_surface_contract(&mut self, status: SurfaceContractStatus) {
        self.surface_contract = status;
    }

    /// Add a residual risk.
    pub fn add_risk(&mut self, risk: ResidualRisk) {
        self.residual_risks.push(risk);
    }

    /// Mark migration as complete.
    pub fn mark_complete(&mut self) {
        self.status = MigrationStatus::Complete;
    }

    /// Whether the migration is ready for cutover.
    #[must_use]
    pub fn is_cutover_ready(&self) -> bool {
        self.scan.is_cutover_ready()
            && self.guards.all_pass()
            && self
                .residual_risks
                .iter()
                .filter(|r| r.severity >= ViolationSeverity::Error)
                .all(|r| r.mitigation.is_some() || r.accepted)
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("=== {} ===", self.title));
        lines.push(format!("Migration: {}", self.migration_id));
        lines.push(format!("Status: {:?}", self.status));
        lines.push(String::new());

        lines.push("--- Dependency Scan ---".to_string());
        lines.push(format!(
            "Verdict: {:?} ({} files, {} violations)",
            self.scan.verdict,
            self.scan.files_scanned,
            self.scan.violations.len()
        ));
        if !self.scan.violations.is_empty() {
            lines.push(format!(
                "  Critical: {}, Error: {}",
                self.scan.critical_count(),
                self.scan.error_count()
            ));
        }

        lines.push(String::new());
        lines.push("--- Regression Guards ---".to_string());
        lines.push(format!(
            "{}/{} passing",
            self.guards.pass_count(),
            self.guards.total_count()
        ));
        for guard in self.guards.failing() {
            lines.push(format!("  FAIL: {} — {}", guard.guard_id, guard.evidence));
        }

        lines.push(String::new());
        lines.push("--- Surface Contract ---".to_string());
        lines.push(format!(
            "Keep: {}, Replace: {}/{}, Retire: {}/{}",
            self.surface_contract.keep_count,
            self.surface_contract.replaced_count,
            self.surface_contract.replace_count,
            self.surface_contract.retired_count,
            self.surface_contract.retire_count,
        ));

        if !self.residual_risks.is_empty() {
            lines.push(String::new());
            lines.push("--- Residual Risks ---".to_string());
            for risk in &self.residual_risks {
                let status = if risk.accepted {
                    "ACCEPTED"
                } else if risk.mitigation.is_some() {
                    "MITIGATED"
                } else {
                    "OPEN"
                };
                lines.push(format!(
                    "  [{:?}] {} — {} [{}]",
                    risk.severity, risk.risk_id, risk.description, status
                ));
            }
        }

        lines.push(String::new());
        lines.push(format!(
            "Cutover ready: {}",
            if self.is_cutover_ready() { "YES" } else { "NO" }
        ));

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
    fn test_standard_forbidden_patterns() {
        let patterns = standard_forbidden_patterns();
        assert!(patterns.len() >= 7);
        // All should have non-empty pattern and ID.
        for p in &patterns {
            assert!(!p.pattern_id.is_empty());
            assert!(!p.pattern.is_empty());
        }
    }

    #[test]
    fn test_clean_scan_report() {
        let report = ScanReport::clean(100, 5000, 9);
        assert_eq!(report.verdict, ScanVerdict::Clean);
        assert!(report.is_cutover_ready());
        assert_eq!(report.critical_count(), 0);
        assert_eq!(report.error_count(), 0);
    }

    #[test]
    fn test_scan_report_with_violations() {
        let violations = vec![ForbiddenImport {
            pattern_id: "FP-01".into(),
            file_path: "src/foo.rs".into(),
            line_number: 5,
            line_content: "use tokio::runtime;".into(),
            severity: ViolationSeverity::Critical,
            in_test_context: false,
            runtime: ForbiddenRuntime::Tokio,
        }];

        let report = ScanReport::from_violations(100, 5000, 9, violations);
        assert_eq!(report.verdict, ScanVerdict::Violations);
        assert!(!report.is_cutover_ready());
        assert_eq!(report.critical_count(), 1);
    }

    #[test]
    fn test_scan_report_test_only_is_clean_with_notes() {
        let violations = vec![ForbiddenImport {
            pattern_id: "FP-03".into(),
            file_path: "src/foo.rs".into(),
            line_number: 10,
            line_content: "#[tokio::test]".into(),
            severity: ViolationSeverity::Error,
            in_test_context: true,
            runtime: ForbiddenRuntime::Tokio,
        }];

        let report = ScanReport::from_violations(100, 5000, 9, violations);
        assert_eq!(report.verdict, ScanVerdict::CleanWithNotes);
        assert!(report.is_cutover_ready());
    }

    #[test]
    fn test_scan_report_info_only_is_clean_with_notes() {
        let violations = vec![ForbiddenImport {
            pattern_id: "FP-INFO".into(),
            file_path: "src/compat.rs".into(),
            line_number: 1,
            line_content: "// compatibility shim for tokio".into(),
            severity: ViolationSeverity::Info,
            in_test_context: false,
            runtime: ForbiddenRuntime::Tokio,
        }];

        let report = ScanReport::from_violations(100, 5000, 9, violations);
        assert_eq!(report.verdict, ScanVerdict::CleanWithNotes);
    }

    #[test]
    fn test_by_runtime_grouping() {
        let violations = vec![
            ForbiddenImport {
                pattern_id: "FP-01".into(),
                file_path: "src/a.rs".into(),
                line_number: 1,
                line_content: "use tokio::spawn;".into(),
                severity: ViolationSeverity::Critical,
                in_test_context: false,
                runtime: ForbiddenRuntime::Tokio,
            },
            ForbiddenImport {
                pattern_id: "FP-05".into(),
                file_path: "src/b.rs".into(),
                line_number: 1,
                line_content: "use smol::Timer;".into(),
                severity: ViolationSeverity::Error,
                in_test_context: false,
                runtime: ForbiddenRuntime::Smol,
            },
        ];

        let report = ScanReport::from_violations(50, 2000, 9, violations);
        let by_rt = report.by_runtime();
        assert_eq!(by_rt.get("tokio").unwrap().len(), 1);
        assert_eq!(by_rt.get("smol").unwrap().len(), 1);
    }

    #[test]
    fn test_regression_guard_set_all_pass() {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        assert!(guards.all_pass());
        assert_eq!(guards.total_count(), 4);
        assert_eq!(guards.pass_count(), 4);
        assert!(guards.failing().is_empty());
    }

    #[test]
    fn test_regression_guard_set_with_violations() {
        let violations = vec![ForbiddenImport {
            pattern_id: "FP-01".into(),
            file_path: "src/bad.rs".into(),
            line_number: 1,
            line_content: "use tokio::runtime;".into(),
            severity: ViolationSeverity::Critical,
            in_test_context: false,
            runtime: ForbiddenRuntime::Tokio,
        }];

        let scan = ScanReport::from_violations(100, 5000, 9, violations);
        let guards = RegressionGuardSet::standard_guards(&scan);
        assert!(!guards.all_pass());
        assert!(!guards.failing().is_empty());
    }

    #[test]
    fn test_migration_report_clean() {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let report = MigrationReport::new("asupersync", scan, guards);

        assert_eq!(report.status, MigrationStatus::PendingVerification);
        assert!(report.is_cutover_ready());
    }

    #[test]
    fn test_migration_report_with_violations() {
        let violations = vec![ForbiddenImport {
            pattern_id: "FP-01".into(),
            file_path: "src/bad.rs".into(),
            line_number: 1,
            line_content: "use tokio::runtime;".into(),
            severity: ViolationSeverity::Critical,
            in_test_context: false,
            runtime: ForbiddenRuntime::Tokio,
        }];

        let scan = ScanReport::from_violations(100, 5000, 9, violations);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let report = MigrationReport::new("asupersync", scan, guards);

        assert_eq!(report.status, MigrationStatus::InProgress);
        assert!(!report.is_cutover_ready());
    }

    #[test]
    fn test_migration_report_with_unmitigated_risk() {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let mut report = MigrationReport::new("asupersync", scan, guards);

        report.add_risk(ResidualRisk {
            risk_id: "R-01".into(),
            description: "Edge case not covered".into(),
            severity: ViolationSeverity::Error,
            mitigation: None,
            owner: None,
            accepted: false,
        });

        assert!(!report.is_cutover_ready());
    }

    #[test]
    fn test_migration_report_with_accepted_risk() {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let mut report = MigrationReport::new("asupersync", scan, guards);

        report.add_risk(ResidualRisk {
            risk_id: "R-01".into(),
            description: "Edge case".into(),
            severity: ViolationSeverity::Error,
            mitigation: None,
            owner: Some("team".into()),
            accepted: true,
        });

        assert!(report.is_cutover_ready());
    }

    #[test]
    fn test_migration_report_with_mitigated_risk() {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let mut report = MigrationReport::new("asupersync", scan, guards);

        report.add_risk(ResidualRisk {
            risk_id: "R-01".into(),
            description: "Edge case".into(),
            severity: ViolationSeverity::Critical,
            mitigation: Some("Added monitoring".into()),
            owner: Some("team".into()),
            accepted: false,
        });

        assert!(report.is_cutover_ready());
    }

    #[test]
    fn test_surface_contract_status() {
        let status = SurfaceContractStatus {
            keep_count: 10,
            replace_count: 5,
            retire_count: 3,
            replaced_count: 5,
            retired_count: 2,
        };

        assert!(!status.all_transitional_resolved()); // 1 retire remaining
        assert_eq!(status.remaining_transitional(), 1);
        assert_eq!(status.total_count(), 18);

        let complete = SurfaceContractStatus {
            keep_count: 10,
            replace_count: 5,
            retire_count: 3,
            replaced_count: 5,
            retired_count: 3,
        };
        assert!(complete.all_transitional_resolved());
        assert_eq!(complete.remaining_transitional(), 0);
    }

    #[test]
    fn test_render_summary_clean() {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let report = MigrationReport::new("asupersync", scan, guards);

        let summary = report.render_summary();
        assert!(summary.contains("Migration Report"));
        assert!(summary.contains("PendingVerification"));
        assert!(summary.contains("Clean"));
        assert!(summary.contains("Cutover ready: YES"));
    }

    #[test]
    fn test_render_summary_with_failures() {
        let violations = vec![ForbiddenImport {
            pattern_id: "FP-01".into(),
            file_path: "src/bad.rs".into(),
            line_number: 1,
            line_content: "use tokio::runtime;".into(),
            severity: ViolationSeverity::Critical,
            in_test_context: false,
            runtime: ForbiddenRuntime::Tokio,
        }];
        let scan = ScanReport::from_violations(100, 5000, 9, violations);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let report = MigrationReport::new("asupersync", scan, guards);

        let summary = report.render_summary();
        assert!(summary.contains("Cutover ready: NO"));
        assert!(summary.contains("FAIL"));
    }

    #[test]
    fn test_mark_complete() {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let mut report = MigrationReport::new("asupersync", scan, guards);
        assert_eq!(report.status, MigrationStatus::PendingVerification);

        report.mark_complete();
        assert_eq!(report.status, MigrationStatus::Complete);
    }

    #[test]
    fn test_serde_roundtrip() {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let report = MigrationReport::new("asupersync", scan, guards);

        let json = serde_json::to_string(&report).expect("serialize");
        let restored: MigrationReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.migration_id, "asupersync");
        assert_eq!(restored.guards.total_count(), 4);
    }

    #[test]
    fn test_violation_severity_ordering() {
        assert!(ViolationSeverity::Info < ViolationSeverity::Warning);
        assert!(ViolationSeverity::Warning < ViolationSeverity::Error);
        assert!(ViolationSeverity::Error < ViolationSeverity::Critical);
    }

    #[test]
    fn test_guard_type_variants() {
        // Verify all guard types exist and are distinct.
        let types = [
            GuardType::SourceScan,
            GuardType::CargoDependency,
            GuardType::FeatureFlag,
            GuardType::RuntimeApi,
            GuardType::BuildScript,
        ];
        for (i, a) in types.iter().enumerate() {
            for (j, b) in types.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn test_low_severity_risk_does_not_block() {
        let scan = ScanReport::clean(100, 5000, 9);
        let guards = RegressionGuardSet::standard_guards(&scan);
        let mut report = MigrationReport::new("asupersync", scan, guards);

        report.add_risk(ResidualRisk {
            risk_id: "R-LOW".into(),
            description: "Minor concern".into(),
            severity: ViolationSeverity::Warning,
            mitigation: None,
            owner: None,
            accepted: false,
        });

        // Warning-level risk doesn't block cutover.
        assert!(report.is_cutover_ready());
    }
}
