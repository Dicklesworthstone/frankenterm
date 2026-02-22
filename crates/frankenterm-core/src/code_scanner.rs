//! Subprocess bridge for `ultimate_bug_scanner` (`ubs`) integration.
//!
//! Wraps the `ubs` CLI via [`SubprocessBridge`] to scan code and pane
//! output for potential issues.  All calls fail-open: when `ubs` is
//! unavailable the bridge returns empty results.
//!
//! Feature-gated behind `subprocess-bridge`.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::subprocess_bridge::SubprocessBridge;

// =============================================================================
// Types
// =============================================================================

/// Severity of a scan finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FindingSeverity {
    Info,
    Warning,
    Critical,
}

impl std::fmt::Display for FindingSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Info => f.write_str("info"),
            Self::Warning => f.write_str("warning"),
            Self::Critical => f.write_str("critical"),
        }
    }
}

/// A single finding from the scanner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanFinding {
    pub severity: FindingSeverity,
    pub category: String,
    pub message: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub suggestion: Option<String>,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Per-scanner summary from ubs JSON output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannerSummary {
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub critical: usize,
    #[serde(default)]
    pub warning: usize,
    #[serde(default)]
    pub info: usize,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Aggregated scan totals.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanTotals {
    pub critical: usize,
    pub warning: usize,
    pub info: usize,
    pub files: usize,
}

impl ScanTotals {
    /// Total finding count across all severities.
    #[must_use]
    pub fn total(&self) -> usize {
        self.critical + self.warning + self.info
    }

    /// Whether any critical findings exist.
    #[must_use]
    pub fn has_critical(&self) -> bool {
        self.critical > 0
    }
}

/// Full scan report from ubs `--format=json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanReport {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub scanners: Vec<ScannerSummary>,
    #[serde(default)]
    pub totals: ScanTotals,
    /// Forward-compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

// =============================================================================
// Bridge
// =============================================================================

/// High-level code scanner bridge wrapping the `ubs` CLI.
#[derive(Debug, Clone)]
pub struct CodeScanner {
    bridge: SubprocessBridge<ScanReport>,
}

impl CodeScanner {
    /// Create a new scanner looking for `ubs` in PATH and `/dp`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bridge: SubprocessBridge::new("ubs"),
        }
    }

    /// Check whether the `ubs` binary can be found.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.bridge.is_available()
    }

    /// Scan a directory or file and return the summary report.
    ///
    /// Returns `None` on any failure (fail-open).
    pub fn scan_path(&self, path: &Path) -> Option<ScanReport> {
        let path_str = path.to_string_lossy();
        match self.bridge.invoke(&["--format=json", "--ci", &path_str]) {
            Ok(report) => {
                info!(
                    scanner = "ubs",
                    findings = report.totals.total(),
                    critical = report.totals.critical,
                    warning = report.totals.warning,
                    "scan complete"
                );
                Some(report)
            }
            Err(err) => {
                warn!(scanner = "ubs", error = %err, "scan failed, degrading gracefully");
                None
            }
        }
    }

    /// Scan only staged git changes (calls `ubs --staged --format=json`).
    pub fn scan_staged(&self, project_dir: &Path) -> Option<ScanReport> {
        let path_str = project_dir.to_string_lossy();
        match self
            .bridge
            .invoke(&["--format=json", "--ci", "--staged", &path_str])
        {
            Ok(report) => {
                debug!(
                    scanner = "ubs",
                    staged = true,
                    findings = report.totals.total(),
                    "staged scan complete"
                );
                Some(report)
            }
            Err(err) => {
                warn!(scanner = "ubs", error = %err, "staged scan failed");
                None
            }
        }
    }

    /// Scan only modified files (calls `ubs --diff --format=json`).
    pub fn scan_diff(&self, project_dir: &Path) -> Option<ScanReport> {
        let path_str = project_dir.to_string_lossy();
        match self
            .bridge
            .invoke(&["--format=json", "--ci", "--diff", &path_str])
        {
            Ok(report) => {
                debug!(
                    scanner = "ubs",
                    diff = true,
                    findings = report.totals.total(),
                    "diff scan complete"
                );
                Some(report)
            }
            Err(err) => {
                warn!(scanner = "ubs", error = %err, "diff scan failed");
                None
            }
        }
    }

    /// Classify the scan result into a severity tier.
    pub fn classify(report: &ScanReport) -> ScanClassification {
        if report.totals.critical > 0 {
            ScanClassification::Critical
        } else if report.totals.warning > 100 {
            ScanClassification::HighWarning
        } else if report.totals.warning > 0 {
            ScanClassification::Warning
        } else {
            ScanClassification::Clean
        }
    }
}

impl Default for CodeScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// Overall scan classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanClassification {
    /// No findings.
    Clean,
    /// Some warnings, no critical.
    Warning,
    /// Many warnings (> 100).
    HighWarning,
    /// Critical findings present.
    Critical,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // FindingSeverity
    // -------------------------------------------------------------------------

    #[test]
    fn test_finding_severity_display() {
        assert_eq!(FindingSeverity::Info.to_string(), "info");
        assert_eq!(FindingSeverity::Warning.to_string(), "warning");
        assert_eq!(FindingSeverity::Critical.to_string(), "critical");
    }

    #[test]
    fn test_finding_severity_ord() {
        assert!(FindingSeverity::Info < FindingSeverity::Warning);
        assert!(FindingSeverity::Warning < FindingSeverity::Critical);
    }

    #[test]
    fn test_finding_severity_serde_roundtrip() {
        for sev in [
            FindingSeverity::Info,
            FindingSeverity::Warning,
            FindingSeverity::Critical,
        ] {
            let json = serde_json::to_string(&sev).unwrap();
            let back: FindingSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(back, sev);
        }
    }

    #[test]
    fn test_finding_severity_deserialize_lowercase() {
        let sev: FindingSeverity = serde_json::from_str("\"critical\"").unwrap();
        assert_eq!(sev, FindingSeverity::Critical);
    }

    // -------------------------------------------------------------------------
    // ScanFinding
    // -------------------------------------------------------------------------

    #[test]
    fn test_scan_finding_full() {
        let finding = ScanFinding {
            severity: FindingSeverity::Critical,
            category: "resource-lifecycle".to_string(),
            message: "Potential resource leak".to_string(),
            file: Some("main.rs".to_string()),
            line: Some(42),
            suggestion: Some("Add drop guard".to_string()),
            extra: HashMap::new(),
        };
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert_eq!(finding.line, Some(42));
        assert!(finding.suggestion.is_some());
    }

    #[test]
    fn test_scan_finding_minimal() {
        let finding = ScanFinding {
            severity: FindingSeverity::Info,
            category: "style".to_string(),
            message: "Consider renaming".to_string(),
            file: None,
            line: None,
            suggestion: None,
            extra: HashMap::new(),
        };
        assert!(finding.file.is_none());
        assert!(finding.line.is_none());
    }

    #[test]
    fn test_scan_finding_serde_roundtrip() {
        let finding = ScanFinding {
            severity: FindingSeverity::Warning,
            category: "security".to_string(),
            message: "Hardcoded credential".to_string(),
            file: Some("config.rs".to_string()),
            line: Some(10),
            suggestion: Some("Use env var".to_string()),
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&finding).unwrap();
        let back: ScanFinding = serde_json::from_str(&json).unwrap();
        assert_eq!(back.severity, FindingSeverity::Warning);
        assert_eq!(back.category, "security");
        assert_eq!(back.file, Some("config.rs".to_string()));
    }

    #[test]
    fn test_scan_finding_forward_compat() {
        let json = r#"{
            "severity": "info",
            "category": "test",
            "message": "msg",
            "new_field": 42
        }"#;
        let finding: ScanFinding = serde_json::from_str(json).unwrap();
        assert_eq!(finding.extra.get("new_field").unwrap(), &42);
    }

    #[test]
    fn test_scan_finding_has_suggestion() {
        let with = ScanFinding {
            severity: FindingSeverity::Warning,
            category: "perf".to_string(),
            message: "Slow path".to_string(),
            file: None,
            line: None,
            suggestion: Some("Use cache".to_string()),
            extra: HashMap::new(),
        };
        assert!(with.suggestion.is_some());

        let without = ScanFinding {
            severity: FindingSeverity::Info,
            category: "style".to_string(),
            message: "Minor".to_string(),
            file: None,
            line: None,
            suggestion: None,
            extra: HashMap::new(),
        };
        assert!(without.suggestion.is_none());
    }

    // -------------------------------------------------------------------------
    // ScannerSummary
    // -------------------------------------------------------------------------

    #[test]
    fn test_scanner_summary_serde() {
        let json = r#"{
            "language": "rust",
            "files": 250,
            "critical": 10,
            "warning": 500,
            "info": 200
        }"#;
        let summary: ScannerSummary = serde_json::from_str(json).unwrap();
        assert_eq!(summary.language, Some("rust".to_string()));
        assert_eq!(summary.files, 250);
        assert_eq!(summary.critical, 10);
    }

    #[test]
    fn test_scanner_summary_minimal() {
        let json = "{}";
        let summary: ScannerSummary = serde_json::from_str(json).unwrap();
        assert!(summary.language.is_none());
        assert_eq!(summary.files, 0);
    }

    // -------------------------------------------------------------------------
    // ScanTotals
    // -------------------------------------------------------------------------

    #[test]
    fn test_scan_totals_total() {
        let totals = ScanTotals {
            critical: 5,
            warning: 100,
            info: 50,
            files: 20,
        };
        assert_eq!(totals.total(), 155);
    }

    #[test]
    fn test_scan_totals_has_critical() {
        let with = ScanTotals {
            critical: 1,
            warning: 0,
            info: 0,
            files: 1,
        };
        assert!(with.has_critical());

        let without = ScanTotals {
            critical: 0,
            warning: 100,
            info: 50,
            files: 10,
        };
        assert!(!without.has_critical());
    }

    #[test]
    fn test_scan_totals_default() {
        let totals = ScanTotals::default();
        assert_eq!(totals.total(), 0);
        assert!(!totals.has_critical());
    }

    #[test]
    fn test_scan_totals_serde_roundtrip() {
        let totals = ScanTotals {
            critical: 3,
            warning: 50,
            info: 20,
            files: 10,
        };
        let json = serde_json::to_string(&totals).unwrap();
        let back: ScanTotals = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total(), 73);
    }

    // -------------------------------------------------------------------------
    // ScanReport
    // -------------------------------------------------------------------------

    #[test]
    fn test_scan_report_deserialize_real_ubs_output() {
        let json = r#"{
            "project": "/tmp/test",
            "timestamp": "2026-02-22 01:26:54",
            "scanners": [
                {
                    "project": "/tmp/test",
                    "files": 249,
                    "critical": 420,
                    "warning": 51683,
                    "info": 12542,
                    "timestamp": "2026-02-22T06:26:54Z",
                    "format": "json",
                    "language": "rust"
                }
            ],
            "totals": {
                "critical": 420,
                "warning": 51683,
                "info": 12542,
                "files": 249
            }
        }"#;
        let report: ScanReport = serde_json::from_str(json).unwrap();
        assert_eq!(report.project, Some("/tmp/test".to_string()));
        assert_eq!(report.scanners.len(), 1);
        assert_eq!(report.totals.critical, 420);
        assert_eq!(report.totals.files, 249);
    }

    #[test]
    fn test_scan_report_minimal() {
        let json = r#"{"totals":{"critical":0,"warning":0,"info":0,"files":0}}"#;
        let report: ScanReport = serde_json::from_str(json).unwrap();
        assert!(report.project.is_none());
        assert!(report.scanners.is_empty());
        assert_eq!(report.totals.total(), 0);
    }

    #[test]
    fn test_scan_report_forward_compat() {
        let json = r#"{
            "project": "/x",
            "scanners": [],
            "totals": {"critical":0,"warning":0,"info":0,"files":0},
            "new_top_level_field": true
        }"#;
        let report: ScanReport = serde_json::from_str(json).unwrap();
        assert!(report.extra.contains_key("new_top_level_field"));
    }

    // -------------------------------------------------------------------------
    // ScanClassification
    // -------------------------------------------------------------------------

    #[test]
    fn test_classify_clean() {
        let report = ScanReport {
            project: None,
            scanners: vec![],
            totals: ScanTotals::default(),
            extra: HashMap::new(),
        };
        assert_eq!(CodeScanner::classify(&report), ScanClassification::Clean);
    }

    #[test]
    fn test_classify_warning() {
        let report = ScanReport {
            project: None,
            scanners: vec![],
            totals: ScanTotals {
                critical: 0,
                warning: 50,
                info: 10,
                files: 5,
            },
            extra: HashMap::new(),
        };
        assert_eq!(CodeScanner::classify(&report), ScanClassification::Warning);
    }

    #[test]
    fn test_classify_high_warning() {
        let report = ScanReport {
            project: None,
            scanners: vec![],
            totals: ScanTotals {
                critical: 0,
                warning: 200,
                info: 0,
                files: 10,
            },
            extra: HashMap::new(),
        };
        assert_eq!(
            CodeScanner::classify(&report),
            ScanClassification::HighWarning
        );
    }

    #[test]
    fn test_classify_critical() {
        let report = ScanReport {
            project: None,
            scanners: vec![],
            totals: ScanTotals {
                critical: 1,
                warning: 0,
                info: 0,
                files: 1,
            },
            extra: HashMap::new(),
        };
        assert_eq!(CodeScanner::classify(&report), ScanClassification::Critical);
    }

    #[test]
    fn test_classify_critical_overrides_warning() {
        let report = ScanReport {
            project: None,
            scanners: vec![],
            totals: ScanTotals {
                critical: 5,
                warning: 500,
                info: 100,
                files: 50,
            },
            extra: HashMap::new(),
        };
        assert_eq!(CodeScanner::classify(&report), ScanClassification::Critical);
    }

    // -------------------------------------------------------------------------
    // CodeScanner construction
    // -------------------------------------------------------------------------

    #[test]
    fn test_code_scanner_new() {
        let scanner = CodeScanner::new();
        assert_eq!(scanner.bridge.binary_name(), "ubs");
    }

    #[test]
    fn test_code_scanner_default() {
        let scanner = CodeScanner::default();
        assert_eq!(scanner.bridge.binary_name(), "ubs");
    }

    // -------------------------------------------------------------------------
    // Fail-open behavior
    // -------------------------------------------------------------------------

    #[test]
    fn test_fail_open_scan_when_ubs_unavailable() {
        let scanner = CodeScanner {
            bridge: SubprocessBridge::new("definitely-missing-ubs-binary-xyz"),
        };
        let result = scanner.scan_path(Path::new("/tmp"));
        assert!(result.is_none());
    }

    #[test]
    fn test_fail_open_scan_staged_when_unavailable() {
        let scanner = CodeScanner {
            bridge: SubprocessBridge::new("definitely-missing-ubs-binary-xyz"),
        };
        let result = scanner.scan_staged(Path::new("/tmp"));
        assert!(result.is_none());
    }

    #[test]
    fn test_fail_open_scan_diff_when_unavailable() {
        let scanner = CodeScanner {
            bridge: SubprocessBridge::new("definitely-missing-ubs-binary-xyz"),
        };
        let result = scanner.scan_diff(Path::new("/tmp"));
        assert!(result.is_none());
    }

    #[test]
    fn test_is_available_false_for_missing() {
        let scanner = CodeScanner {
            bridge: SubprocessBridge::new("definitely-missing-ubs-binary-xyz"),
        };
        assert!(!scanner.is_available());
    }

    // -------------------------------------------------------------------------
    // ScanClassification equality
    // -------------------------------------------------------------------------

    #[test]
    fn test_scan_classification_eq() {
        assert_eq!(ScanClassification::Clean, ScanClassification::Clean);
        assert_eq!(ScanClassification::Critical, ScanClassification::Critical);
        assert_ne!(ScanClassification::Clean, ScanClassification::Critical);
    }

    #[test]
    fn test_scan_classification_copy() {
        let c = ScanClassification::Warning;
        let copy = c;
        assert_eq!(c, copy);
    }

    #[test]
    fn test_scan_classification_debug() {
        let dbg = format!("{:?}", ScanClassification::HighWarning);
        assert!(dbg.contains("HighWarning"));
    }
}
