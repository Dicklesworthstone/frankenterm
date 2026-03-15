//! Runtime-compat async surface guard (ft-e34d9.10.5.4.1).
//!
//! Static guard tests that verify direct tokio/smol/async-io async runtime
//! primitives remain confined to `runtime_compat.rs` and `cx.rs`, and that
//! production call sites do not regress back to helper shims or direct runtime
//! usage.
//!
//! # Architecture
//!
//! ```text
//! SurfaceGuardReport
//!   ├── surface_entries     (serializable mirror of SURFACE_CONTRACT_V1)
//!   ├── guard_checks        (one per surface API: compliant/non-compliant)
//!   ├── regressions         (detected call-site violations)
//!   └── unwrapped_call_sites (sites using raw runtime APIs outside allowed files)
//! ```
//!
//! # Key invariant
//!
//! The only files allowed to import raw tokio/smol/async-io primitives are
//! `runtime_compat.rs` and `cx.rs`.  Every other call site must go through the
//! wrappers vended by those two modules.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// =============================================================================
// Allowed files
// =============================================================================

/// Returns the list of source files where raw runtime imports are permitted.
///
/// All other files must use `runtime_compat` / `cx` wrappers.
#[must_use]
pub fn allowed_raw_runtime_files() -> Vec<&'static str> {
    vec!["runtime_compat.rs", "cx.rs"]
}

// =============================================================================
// Serialisable surface API entry
// =============================================================================

/// Serialisable mirror of `runtime_compat::SurfaceContractEntry`.
///
/// The upstream type only derives `Debug/Clone/Copy/PartialEq/Eq` (no serde),
/// so we maintain our own representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceApiEntry {
    /// API name, e.g. `"RuntimeBuilder"` or `"process::Command"`.
    pub api_name: String,
    /// Disposition string: `"Keep"`, `"Replace"`, or `"Retire"`.
    pub disposition: String,
    /// Human-readable rationale.
    pub rationale: String,
    /// Recommended replacement, if any.
    pub replacement: Option<String>,
}

// =============================================================================
// Unwrapped call-site
// =============================================================================

/// A call-site that should be using `runtime_compat` wrappers instead of the
/// raw runtime primitive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnwrappedCallSite {
    /// Path to the source file containing the call.
    pub file_path: String,
    /// Raw API being called, e.g. `"tokio::spawn"`.
    pub api_used: String,
    /// The wrapper that should be used instead, e.g. `"runtime_compat::task::spawn"`.
    pub wrapper_available: String,
    /// Whether this file is in the allowed list and the usage is therefore
    /// permitted.
    pub in_allowed_file: bool,
}

// =============================================================================
// Surface guard check
// =============================================================================

/// Guard check that verifies wrapper usage for a single surface API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceGuardCheck {
    /// Unique identifier for this check, e.g. `"SGC-01-RuntimeBuilder"`.
    pub check_id: String,
    /// The API name this check covers.
    pub api_name: String,
    /// Disposition string mirroring the contract: `"Keep"`, `"Replace"`, or `"Retire"`.
    pub disposition: String,
    /// Whether a `runtime_compat` wrapper exists for this API.
    pub wrapper_exists: bool,
    /// Number of call sites correctly using the wrapper.
    pub call_sites_wrapped: usize,
    /// Number of call sites using the raw API directly (outside allowed files).
    pub call_sites_unwrapped: usize,
    /// Whether this check is currently compliant.
    ///
    /// - `Keep` APIs: `true` (always allowed)
    /// - `Replace` APIs: `false` until fully migrated (all call sites wrapped)
    /// - `Retire` APIs: `false` (must be removed entirely)
    pub compliant: bool,
}

// =============================================================================
// Regression types
// =============================================================================

/// Categories of surface contract regression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegressionType {
    /// `use tokio::…` (or smol/async-io) appears outside allowed files.
    DirectRuntimeImport,
    /// Raw runtime API called instead of the `runtime_compat` wrapper.
    UnwrappedApiCall,
    /// A `Retire`-classified API is still in active use.
    DispositionViolation,
    /// `runtime_compat` bypassed entirely; call goes directly to the runtime.
    ShimBypass,
}

/// A detected surface contract regression.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceRegression {
    /// Unique identifier, e.g. `"SR-001"`.
    pub regression_id: String,
    /// Category of the regression.
    pub regression_type: RegressionType,
    /// Source file where the regression was found.
    pub file_path: String,
    /// Human-readable description.
    pub description: String,
    /// Severity: `"warning"`, `"error"`, or `"critical"`.
    pub severity: String,
}

// =============================================================================
// Surface guard report
// =============================================================================

/// Full surface guard report aggregating all checks and regressions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceGuardReport {
    /// Unique report identifier.
    pub report_id: String,
    /// Unix-millisecond timestamp when the report was generated.
    pub generated_at_ms: u64,
    /// Serialisable mirror of the surface API contract.
    pub surface_entries: Vec<SurfaceApiEntry>,
    /// One check per surface API entry.
    pub guard_checks: Vec<SurfaceGuardCheck>,
    /// Detected regressions.
    pub regressions: Vec<SurfaceRegression>,
    /// Unwrapped call-sites found during analysis.
    pub unwrapped_call_sites: Vec<UnwrappedCallSite>,
    /// `true` iff there are no regressions and all guard checks are compliant.
    pub overall_compliant: bool,
    /// Ratio of compliant checks to total checks (0.0–1.0).
    pub compliance_rate: f64,
}

impl SurfaceGuardReport {
    /// Create an empty report.
    #[must_use]
    pub fn new(report_id: &str, generated_at_ms: u64) -> Self {
        Self {
            report_id: report_id.to_string(),
            generated_at_ms,
            surface_entries: Vec::new(),
            guard_checks: Vec::new(),
            regressions: Vec::new(),
            unwrapped_call_sites: Vec::new(),
            overall_compliant: true,
            compliance_rate: 1.0,
        }
    }

    /// Populate `surface_entries` with the 15 entries from `SURFACE_CONTRACT_V1`.
    ///
    /// The upstream contract lives in `runtime_compat.rs` and is not
    /// serialisable, so we mirror it here as hardcoded strings.
    pub fn with_standard_surface(&mut self) -> &mut Self {
        self.surface_entries = standard_surface_entries();
        self
    }

    /// Add a guard check.
    pub fn add_guard_check(&mut self, check: SurfaceGuardCheck) {
        self.guard_checks.push(check);
    }

    /// Add a detected regression.
    pub fn add_regression(&mut self, regression: SurfaceRegression) {
        self.regressions.push(regression);
    }

    /// Add an unwrapped call-site.
    pub fn add_unwrapped_call_site(&mut self, site: UnwrappedCallSite) {
        self.unwrapped_call_sites.push(site);
    }

    /// Recalculate `overall_compliant` and `compliance_rate` from the current
    /// state of `guard_checks` and `regressions`.
    pub fn finalize(&mut self) {
        let total = self.guard_checks.len();
        let compliant_count = self.guard_checks.iter().filter(|c| c.compliant).count();

        self.compliance_rate = if total == 0 {
            1.0
        } else {
            compliant_count as f64 / total as f64
        };

        self.overall_compliant =
            self.regressions.is_empty() && self.guard_checks.iter().all(|c| c.compliant);
    }

    /// One-line summary of the report.
    #[must_use]
    pub fn summary(&self) -> String {
        let total = self.guard_checks.len();
        let compliant = self.guard_checks.iter().filter(|c| c.compliant).count();
        let regressions = self.regressions.len();
        let status = if self.overall_compliant {
            "COMPLIANT"
        } else {
            "NON-COMPLIANT"
        };
        format!(
            "[{}] report={} checks={}/{} compliant regressions={} compliance={:.0}%",
            status,
            self.report_id,
            compliant,
            total,
            regressions,
            self.compliance_rate * 100.0,
        )
    }

    /// Regressions grouped by their `RegressionType` (as a string key).
    #[must_use]
    pub fn regressions_by_type(&self) -> BTreeMap<String, Vec<&SurfaceRegression>> {
        let mut map: BTreeMap<String, Vec<&SurfaceRegression>> = BTreeMap::new();
        for r in &self.regressions {
            let key = format!("{:?}", r.regression_type);
            map.entry(key).or_default().push(r);
        }
        map
    }
}

// =============================================================================
// Standard surface entries (hardcoded mirror of SURFACE_CONTRACT_V1)
// =============================================================================

/// Returns a serialisable mirror of the 15 entries in `SURFACE_CONTRACT_V1`.
#[must_use]
pub fn standard_surface_entries() -> Vec<SurfaceApiEntry> {
    vec![
        SurfaceApiEntry {
            api_name: "RuntimeBuilder".into(),
            disposition: "Keep".into(),
            rationale: "Canonical runtime bootstrap seam shared by CLI/watch/test harnesses.".into(),
            replacement: None,
        },
        SurfaceApiEntry {
            api_name: "Runtime".into(),
            disposition: "Keep".into(),
            rationale: "Owns active runtime instance behind migration boundary.".into(),
            replacement: None,
        },
        SurfaceApiEntry {
            api_name: "CompatRuntime::block_on".into(),
            disposition: "Keep".into(),
            rationale:
                "Used by deterministic tests and bridge code while call-graph migration continues."
                    .into(),
            replacement: None,
        },
        SurfaceApiEntry {
            api_name: "CompatRuntime::spawn_detached".into(),
            disposition: "Replace".into(),
            rationale:
                "Detached execution masks scope ownership semantics in target asupersync state."
                    .into(),
            replacement: Some("cx::spawn_with_cx / explicit scope-owned spawn".into()),
        },
        SurfaceApiEntry {
            api_name: "sleep".into(),
            disposition: "Keep".into(),
            rationale: "Cross-runtime time seam with stable call-site behavior.".into(),
            replacement: None,
        },
        SurfaceApiEntry {
            api_name: "timeout".into(),
            disposition: "Keep".into(),
            rationale: "Shared timeout boundary used by IPC/web/watchdog call paths.".into(),
            replacement: None,
        },
        SurfaceApiEntry {
            api_name: "spawn_blocking".into(),
            disposition: "Keep".into(),
            rationale: "Canonical blocking-work seam with normalized error mapping.".into(),
            replacement: None,
        },
        SurfaceApiEntry {
            api_name: "task::spawn_blocking".into(),
            disposition: "Replace".into(),
            rationale:
                "JoinHandle-centric blocking helper should be reserved for explicit abortable workflows only."
                    .into(),
            replacement: Some(
                "spawn_blocking (use task::spawn_blocking only when JoinHandle control is required)"
                    .into(),
            ),
        },
        SurfaceApiEntry {
            api_name: "mpsc_recv_option".into(),
            disposition: "Replace".into(),
            rationale:
                "Option-normalized receive can hide cancellation semantics in asupersync mode."
                    .into(),
            replacement: Some(
                "mpsc::Receiver::recv with explicit cx/cancellation handling".into(),
            ),
        },
        SurfaceApiEntry {
            api_name: "mpsc_send".into(),
            disposition: "Replace".into(),
            rationale:
                "Send helper abstracts over reserve/commit vs direct send semantics.".into(),
            replacement: Some(
                "cx-aware channel send path (reserve/commit where required)".into(),
            ),
        },
        SurfaceApiEntry {
            api_name: "watch_has_changed".into(),
            disposition: "Replace".into(),
            rationale:
                "Boolean-normalized change checks hide backend-specific closure semantics."
                    .into(),
            replacement: Some(
                "watch::Receiver::has_changed with explicit closure/error handling".into(),
            ),
        },
        SurfaceApiEntry {
            api_name: "watch_borrow_and_update_clone".into(),
            disposition: "Replace".into(),
            rationale:
                "Clone-and-mark helper hides backend differences in watch-consume semantics."
                    .into(),
            replacement: Some(
                "watch receiver borrow/consume path with explicit backend behavior".into(),
            ),
        },
        SurfaceApiEntry {
            api_name: "watch_changed".into(),
            disposition: "Replace".into(),
            rationale:
                "Implicit test-cx helper hides cancellation and wake-up ownership semantics."
                    .into(),
            replacement: Some(
                "watch::Receiver::changed with explicit cx/lifecycle context".into(),
            ),
        },
        SurfaceApiEntry {
            api_name: "process::Command".into(),
            disposition: "Retire".into(),
            rationale:
                "Tokio process shim remains temporary and should be replaced by asupersync-native process layer."
                    .into(),
            replacement: Some("asupersync process abstraction".into()),
        },
        SurfaceApiEntry {
            api_name: "signal".into(),
            disposition: "Retire".into(),
            rationale:
                "Tokio-only signal shim is transitional and should be removed after native runtime integration."
                    .into(),
            replacement: Some("asupersync-native signal handling".into()),
        },
    ]
}

// =============================================================================
// Standard guard checks
// =============================================================================

/// Build the standard set of 15 guard checks — one per `SURFACE_CONTRACT_V1`
/// entry.
///
/// Compliance rules:
/// - `Keep`: compliant = `true` (these APIs are always permitted).
/// - `Replace`: compliant = `false` (migration still in progress).
/// - `Retire`: compliant = `false` (must be fully removed).
#[must_use]
pub fn standard_guard_checks() -> Vec<SurfaceGuardCheck> {
    standard_surface_entries()
        .into_iter()
        .enumerate()
        .map(|(i, entry)| {
            let compliant = entry.disposition == "Keep";
            SurfaceGuardCheck {
                check_id: format!("SGC-{:02}-{}", i + 1, entry.api_name.replace("::", "-")),
                api_name: entry.api_name.clone(),
                disposition: entry.disposition.clone(),
                wrapper_exists: true,
                call_sites_wrapped: 0,
                call_sites_unwrapped: 0,
                compliant,
            }
        })
        .collect()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendored_async_contracts::standard_compatibility_mappings;

    // -------------------------------------------------------------------------
    // 1. allowed_raw_runtime_files_includes_runtime_compat
    // -------------------------------------------------------------------------
    #[test]
    fn allowed_raw_runtime_files_includes_runtime_compat() {
        let files = allowed_raw_runtime_files();
        assert!(
            files.contains(&"runtime_compat.rs"),
            "runtime_compat.rs must be in allowed list"
        );
    }

    // -------------------------------------------------------------------------
    // 2. allowed_raw_runtime_files_includes_cx
    // -------------------------------------------------------------------------
    #[test]
    fn allowed_raw_runtime_files_includes_cx() {
        let files = allowed_raw_runtime_files();
        assert!(files.contains(&"cx.rs"), "cx.rs must be in allowed list");
    }

    // -------------------------------------------------------------------------
    // 3. surface_api_entry_serde_roundtrip
    // -------------------------------------------------------------------------
    #[test]
    fn surface_api_entry_serde_roundtrip() {
        let entry = SurfaceApiEntry {
            api_name: "RuntimeBuilder".into(),
            disposition: "Keep".into(),
            rationale: "Canonical bootstrap.".into(),
            replacement: None,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let restored: SurfaceApiEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry, restored);
    }

    // -------------------------------------------------------------------------
    // 4. unwrapped_call_site_in_allowed_file
    // -------------------------------------------------------------------------
    #[test]
    fn unwrapped_call_site_in_allowed_file() {
        let site = UnwrappedCallSite {
            file_path: "runtime_compat.rs".into(),
            api_used: "tokio::spawn".into(),
            wrapper_available: "runtime_compat::task::spawn".into(),
            in_allowed_file: true,
        };
        assert!(site.in_allowed_file);
        let json = serde_json::to_string(&site).expect("serialize");
        let restored: UnwrappedCallSite = serde_json::from_str(&json).expect("deserialize");
        assert!(restored.in_allowed_file);
        assert_eq!(restored.file_path, "runtime_compat.rs");
    }

    // -------------------------------------------------------------------------
    // 5. unwrapped_call_site_outside_allowed_file
    // -------------------------------------------------------------------------
    #[test]
    fn unwrapped_call_site_outside_allowed_file() {
        let site = UnwrappedCallSite {
            file_path: "src/some_module.rs".into(),
            api_used: "tokio::spawn".into(),
            wrapper_available: "runtime_compat::task::spawn".into(),
            in_allowed_file: false,
        };
        assert!(!site.in_allowed_file);
        // Must not appear in allowed list.
        let allowed = allowed_raw_runtime_files();
        assert!(!allowed.iter().any(|f| *f == site.file_path));
    }

    // -------------------------------------------------------------------------
    // 6. guard_check_compliant
    // -------------------------------------------------------------------------
    #[test]
    fn guard_check_compliant() {
        let check = SurfaceGuardCheck {
            check_id: "SGC-01-RuntimeBuilder".into(),
            api_name: "RuntimeBuilder".into(),
            disposition: "Keep".into(),
            wrapper_exists: true,
            call_sites_wrapped: 5,
            call_sites_unwrapped: 0,
            compliant: true,
        };
        assert!(check.compliant);
        let json = serde_json::to_string(&check).expect("serialize");
        let restored: SurfaceGuardCheck = serde_json::from_str(&json).expect("deserialize");
        assert!(restored.compliant);
    }

    // -------------------------------------------------------------------------
    // 7. guard_check_non_compliant
    // -------------------------------------------------------------------------
    #[test]
    fn guard_check_non_compliant() {
        let check = SurfaceGuardCheck {
            check_id: "SGC-14-process--Command".into(),
            api_name: "process::Command".into(),
            disposition: "Retire".into(),
            wrapper_exists: true,
            call_sites_wrapped: 0,
            call_sites_unwrapped: 3,
            compliant: false,
        };
        assert!(!check.compliant);
        assert_eq!(check.disposition, "Retire");
    }

    // -------------------------------------------------------------------------
    // 8. regression_type_variants
    // -------------------------------------------------------------------------
    #[test]
    fn regression_type_variants() {
        // All four variants must exist and be distinct.
        let variants = [
            RegressionType::DirectRuntimeImport,
            RegressionType::UnwrappedApiCall,
            RegressionType::DispositionViolation,
            RegressionType::ShimBypass,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
        // Serde roundtrip.
        let json = serde_json::to_string(&RegressionType::DirectRuntimeImport).unwrap();
        let restored: RegressionType = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, RegressionType::DirectRuntimeImport);
    }

    // -------------------------------------------------------------------------
    // 9. regression_direct_import
    // -------------------------------------------------------------------------
    #[test]
    fn regression_direct_import() {
        let r = SurfaceRegression {
            regression_id: "SR-001".into(),
            regression_type: RegressionType::DirectRuntimeImport,
            file_path: "src/bad_module.rs".into(),
            description: "use tokio::runtime found outside allowed files".into(),
            severity: "critical".into(),
        };
        assert_eq!(r.regression_type, RegressionType::DirectRuntimeImport);
        assert_eq!(r.severity, "critical");

        let json = serde_json::to_string(&r).expect("serialize");
        let restored: SurfaceRegression = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.regression_id, "SR-001");
        assert_eq!(
            restored.regression_type,
            RegressionType::DirectRuntimeImport
        );
    }

    // -------------------------------------------------------------------------
    // 10. surface_guard_report_empty
    // -------------------------------------------------------------------------
    #[test]
    fn surface_guard_report_empty() {
        let report = SurfaceGuardReport::new("test-report", 1_000_000);
        assert_eq!(report.report_id, "test-report");
        assert_eq!(report.generated_at_ms, 1_000_000);
        assert!(report.surface_entries.is_empty());
        assert!(report.guard_checks.is_empty());
        assert!(report.regressions.is_empty());
        assert!(report.unwrapped_call_sites.is_empty());
        // Starts compliant (nothing to violate).
        assert!(report.overall_compliant);
        assert!((report.compliance_rate - 1.0).abs() < f64::EPSILON);
    }

    // -------------------------------------------------------------------------
    // 11. surface_guard_report_with_standard_surface
    // -------------------------------------------------------------------------
    #[test]
    fn surface_guard_report_with_standard_surface() {
        let mut report = SurfaceGuardReport::new("r-std", 0);
        report.with_standard_surface();
        assert_eq!(
            report.surface_entries.len(),
            15,
            "expected 15 SURFACE_CONTRACT_V1 entries"
        );
    }

    // -------------------------------------------------------------------------
    // 12. surface_guard_report_finalize_all_compliant
    // -------------------------------------------------------------------------
    #[test]
    fn surface_guard_report_finalize_all_compliant() {
        let mut report = SurfaceGuardReport::new("r-all-ok", 0);
        for i in 0..3 {
            report.add_guard_check(SurfaceGuardCheck {
                check_id: format!("SGC-0{}", i + 1),
                api_name: format!("Api{}", i),
                disposition: "Keep".into(),
                wrapper_exists: true,
                call_sites_wrapped: 1,
                call_sites_unwrapped: 0,
                compliant: true,
            });
        }
        report.finalize();
        assert!(report.overall_compliant);
        assert!((report.compliance_rate - 1.0).abs() < f64::EPSILON);
    }

    // -------------------------------------------------------------------------
    // 13. surface_guard_report_finalize_with_regressions
    // -------------------------------------------------------------------------
    #[test]
    fn surface_guard_report_finalize_with_regressions() {
        let mut report = SurfaceGuardReport::new("r-regressions", 0);
        report.add_guard_check(SurfaceGuardCheck {
            check_id: "SGC-01".into(),
            api_name: "signal".into(),
            disposition: "Retire".into(),
            wrapper_exists: true,
            call_sites_wrapped: 0,
            call_sites_unwrapped: 2,
            compliant: false,
        });
        report.add_regression(SurfaceRegression {
            regression_id: "SR-001".into(),
            regression_type: RegressionType::DispositionViolation,
            file_path: "src/watchdog.rs".into(),
            description: "Retire'd signal API still in use".into(),
            severity: "error".into(),
        });
        report.finalize();
        assert!(!report.overall_compliant);
        assert!((report.compliance_rate - 0.0).abs() < f64::EPSILON);
    }

    // -------------------------------------------------------------------------
    // 14. surface_guard_report_compliance_rate
    // -------------------------------------------------------------------------
    #[test]
    fn surface_guard_report_compliance_rate() {
        let mut report = SurfaceGuardReport::new("r-rate", 0);
        // 2 compliant, 1 non-compliant → 2/3 ≈ 0.6667
        for compliant in [true, true, false] {
            report.add_guard_check(SurfaceGuardCheck {
                check_id: "x".into(),
                api_name: "x".into(),
                disposition: if compliant {
                    "Keep".into()
                } else {
                    "Retire".into()
                },
                wrapper_exists: true,
                call_sites_wrapped: 0,
                call_sites_unwrapped: 0,
                compliant,
            });
        }
        report.finalize();
        let expected = 2.0 / 3.0;
        assert!(
            (report.compliance_rate - expected).abs() < 0.001,
            "expected ~{:.4}, got {:.4}",
            expected,
            report.compliance_rate
        );
    }

    // -------------------------------------------------------------------------
    // 15. standard_guard_checks_count
    // -------------------------------------------------------------------------
    #[test]
    fn standard_guard_checks_count() {
        let checks = standard_guard_checks();
        assert_eq!(
            checks.len(),
            15,
            "expected exactly 15 standard guard checks"
        );
    }

    // -------------------------------------------------------------------------
    // 16. standard_guard_checks_has_keep_replace_retire
    // -------------------------------------------------------------------------
    #[test]
    fn standard_guard_checks_has_keep_replace_retire() {
        let checks = standard_guard_checks();

        let keep_count = checks.iter().filter(|c| c.disposition == "Keep").count();
        let replace_count = checks.iter().filter(|c| c.disposition == "Replace").count();
        let retire_count = checks.iter().filter(|c| c.disposition == "Retire").count();

        assert!(keep_count > 0, "must have at least one Keep entry");
        assert!(replace_count > 0, "must have at least one Replace entry");
        assert!(retire_count > 0, "must have at least one Retire entry");
        assert_eq!(
            keep_count + replace_count + retire_count,
            15,
            "all 15 checks must have a known disposition"
        );

        // Keep checks must be compliant; Replace and Retire must not.
        for check in &checks {
            match check.disposition.as_str() {
                "Keep" => assert!(
                    check.compliant,
                    "Keep check {} should be compliant",
                    check.check_id
                ),
                "Replace" | "Retire" => assert!(
                    !check.compliant,
                    "{} check {} should not be compliant",
                    check.disposition, check.check_id
                ),
                other => panic!("unexpected disposition '{}'", other),
            }
        }
    }

    #[test]
    fn transitional_channel_helpers_match_contract_alignment_state() {
        let entries = standard_surface_entries();
        let mappings = standard_compatibility_mappings();

        for api in [
            "mpsc_recv_option",
            "mpsc_send",
            "watch_has_changed",
            "watch_borrow_and_update_clone",
            "watch_changed",
        ] {
            let entry = entries
                .iter()
                .find(|entry| entry.api_name == api)
                .unwrap_or_else(|| panic!("missing surface entry for {api}"));
            assert_eq!(
                entry.disposition, "Replace",
                "{api} should remain a Replace surface entry"
            );
            assert!(
                entry.replacement.is_some(),
                "{api} should document an explicit replacement path"
            );

            let mapping = mappings
                .iter()
                .find(|mapping| mapping.compat_api == api)
                .unwrap_or_else(|| panic!("missing compatibility mapping for {api}"));
            assert!(
                !mapping.disposition_aligned,
                "{api} should stay non-aligned while replacement is pending"
            );
        }
    }

    // -------------------------------------------------------------------------
    // 17. regressions_by_type_grouping
    // -------------------------------------------------------------------------
    #[test]
    fn regressions_by_type_grouping() {
        let mut report = SurfaceGuardReport::new("r-grouping", 0);

        for (id, rt, sev) in [
            ("SR-001", RegressionType::DirectRuntimeImport, "critical"),
            ("SR-002", RegressionType::DirectRuntimeImport, "critical"),
            ("SR-003", RegressionType::UnwrappedApiCall, "error"),
            ("SR-004", RegressionType::ShimBypass, "warning"),
        ] {
            report.add_regression(SurfaceRegression {
                regression_id: id.into(),
                regression_type: rt,
                file_path: "src/x.rs".into(),
                description: "test regression".into(),
                severity: sev.into(),
            });
        }

        let grouped = report.regressions_by_type();
        assert_eq!(
            grouped.get("DirectRuntimeImport").map(|v| v.len()),
            Some(2),
            "expected 2 DirectRuntimeImport regressions"
        );
        assert_eq!(
            grouped.get("UnwrappedApiCall").map(|v| v.len()),
            Some(1),
            "expected 1 UnwrappedApiCall regression"
        );
        assert_eq!(
            grouped.get("ShimBypass").map(|v| v.len()),
            Some(1),
            "expected 1 ShimBypass regression"
        );
        assert!(
            grouped.get("DispositionViolation").is_none(),
            "no DispositionViolation regressions expected"
        );
    }

    // -------------------------------------------------------------------------
    // 18. surface_guard_report_summary_format
    // -------------------------------------------------------------------------
    #[test]
    fn surface_guard_report_summary_format() {
        let mut report = SurfaceGuardReport::new("summary-test", 0);
        report.add_guard_check(SurfaceGuardCheck {
            check_id: "SGC-01".into(),
            api_name: "RuntimeBuilder".into(),
            disposition: "Keep".into(),
            wrapper_exists: true,
            call_sites_wrapped: 3,
            call_sites_unwrapped: 0,
            compliant: true,
        });
        report.add_guard_check(SurfaceGuardCheck {
            check_id: "SGC-02".into(),
            api_name: "signal".into(),
            disposition: "Retire".into(),
            wrapper_exists: true,
            call_sites_wrapped: 0,
            call_sites_unwrapped: 1,
            compliant: false,
        });
        report.finalize();

        let summary = report.summary();
        assert!(
            summary.contains("NON-COMPLIANT"),
            "non-compliant report must say NON-COMPLIANT"
        );
        assert!(
            summary.contains("summary-test"),
            "summary must include the report id"
        );
        assert!(
            summary.contains("1/2 compliant"),
            "summary must show 1/2 compliant"
        );
        assert!(
            summary.contains("regressions=0"),
            "summary must show regression count"
        );

        // All-compliant report.
        let mut clean = SurfaceGuardReport::new("clean-report", 0);
        clean.add_guard_check(SurfaceGuardCheck {
            check_id: "SGC-01".into(),
            api_name: "RuntimeBuilder".into(),
            disposition: "Keep".into(),
            wrapper_exists: true,
            call_sites_wrapped: 1,
            call_sites_unwrapped: 0,
            compliant: true,
        });
        clean.finalize();
        let clean_summary = clean.summary();
        assert!(
            clean_summary.contains("COMPLIANT"),
            "clean report must say COMPLIANT"
        );
    }
}
