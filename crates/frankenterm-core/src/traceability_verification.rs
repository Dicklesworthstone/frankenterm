//! Traceability Verification Pack — bead ft-3681t.1.5
//!
//! Provides programmatic verification that every NTM and flywheel_connectors
//! capability in the traceability matrix maps to explicit FrankenTerm design
//! decisions, risks, and implementation beads.
//!
//! The matrix JSON lives at `docs/design/ntm-fcp-traceability-matrix.json`.
//! This module is standalone: no async, no file I/O in the types themselves,
//! no imports beyond serde and std.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Domain / status / severity enums
// ---------------------------------------------------------------------------

/// Domain of a capability, derived from its ID prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CapabilityDomain {
    /// `ft.*` — FrankenTerm core capabilities
    FrankenTerm,
    /// `ntm.*` — NTM (native task manager) capabilities
    Ntm,
    /// `fcp.*` — flywheel connectors platform capabilities
    Fcp,
}

impl CapabilityDomain {
    /// Return a stable string label used for coverage statistics.
    pub fn label(self) -> &'static str {
        match self {
            CapabilityDomain::FrankenTerm => "ft",
            CapabilityDomain::Ntm => "ntm",
            CapabilityDomain::Fcp => "fcp",
        }
    }
}

/// Implementation status of a capability entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImplementationStatus {
    Implemented,
    Partial,
    Gap,
}

/// Gap severity level.  Ordered from lowest to highest risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GapSeverity {
    None,
    Low,
    Medium,
    High,
}

// ---------------------------------------------------------------------------
// Matrix types (deserializable from JSON)
// ---------------------------------------------------------------------------

/// A single capability entry inside the traceability matrix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityEntry {
    pub capability_id: String,
    pub capability_name: String,
    pub source_domain: String,
    pub status: String,
    pub gap_severity: String,
    pub mapped_bead_ids: Vec<String>,
    pub surfaces: Vec<String>,
    pub implementation_anchors: Vec<String>,
    pub evidence_notes: String,
}

impl CapabilityEntry {
    /// Derive the [`CapabilityDomain`] from the `capability_id` prefix.
    pub fn domain(&self) -> CapabilityDomain {
        if self.capability_id.starts_with("ft.") {
            CapabilityDomain::FrankenTerm
        } else if self.capability_id.starts_with("ntm.") {
            CapabilityDomain::Ntm
        } else {
            CapabilityDomain::Fcp
        }
    }

    /// Parse the string `status` field into [`ImplementationStatus`].
    pub fn parsed_status(&self) -> ImplementationStatus {
        match self.status.as_str() {
            "implemented" => ImplementationStatus::Implemented,
            "partial" => ImplementationStatus::Partial,
            _ => ImplementationStatus::Gap,
        }
    }

    /// Parse the string `gap_severity` field into [`GapSeverity`].
    pub fn parsed_gap_severity(&self) -> GapSeverity {
        match self.gap_severity.as_str() {
            "none" => GapSeverity::None,
            "low" => GapSeverity::Low,
            "medium" => GapSeverity::Medium,
            _ => GapSeverity::High,
        }
    }
}

/// The full traceability matrix, deserializable directly from the JSON artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceabilityMatrix {
    pub schema_version: String,
    pub artifact: String,
    pub bead_id: String,
    pub generated_at_utc: String,
    pub required_capability_ids: Vec<String>,
    pub entries: Vec<CapabilityEntry>,
}

// ---------------------------------------------------------------------------
// Verification types
// ---------------------------------------------------------------------------

/// Category of a verification check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationCategory {
    /// Structure and field validity.
    Schema,
    /// All required capabilities are present.
    Completeness,
    /// Status/gap/bead cross-references are internally consistent.
    Consistency,
    /// Implementation anchors point to non-empty paths.
    AnchorValidity,
    /// Mapped beads exist in the issue tracker (presence only, no network call).
    BeadMapping,
    /// Per-domain coverage metrics meet thresholds.
    Coverage,
}

impl VerificationCategory {
    fn label(self) -> &'static str {
        match self {
            VerificationCategory::Schema => "Schema",
            VerificationCategory::Completeness => "Completeness",
            VerificationCategory::Consistency => "Consistency",
            VerificationCategory::AnchorValidity => "AnchorValidity",
            VerificationCategory::BeadMapping => "BeadMapping",
            VerificationCategory::Coverage => "Coverage",
        }
    }
}

/// A single verification check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationCheck {
    pub check_id: String,
    pub check_name: String,
    pub passed: bool,
    pub detail: String,
    pub category: VerificationCategory,
}

impl VerificationCheck {
    fn pass(
        check_id: &str,
        check_name: &str,
        detail: impl Into<String>,
        category: VerificationCategory,
    ) -> Self {
        Self {
            check_id: check_id.to_string(),
            check_name: check_name.to_string(),
            passed: true,
            detail: detail.into(),
            category,
        }
    }

    fn fail(
        check_id: &str,
        check_name: &str,
        detail: impl Into<String>,
        category: VerificationCategory,
    ) -> Self {
        Self {
            check_id: check_id.to_string(),
            check_name: check_name.to_string(),
            passed: false,
            detail: detail.into(),
            category,
        }
    }
}

/// Per-domain coverage statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainCoverage {
    pub domain: String,
    pub total: usize,
    pub implemented: usize,
    pub partial: usize,
    pub gap: usize,
    /// `(implemented + 0.5 * partial) / total * 100.0`; 0.0 when `total == 0`.
    pub coverage_pct: f64,
}

impl DomainCoverage {
    fn compute(domain: &str, entries: &[&CapabilityEntry]) -> Self {
        let total = entries.len();
        let implemented = entries
            .iter()
            .filter(|e| e.parsed_status() == ImplementationStatus::Implemented)
            .count();
        let partial = entries
            .iter()
            .filter(|e| e.parsed_status() == ImplementationStatus::Partial)
            .count();
        let gap = entries
            .iter()
            .filter(|e| e.parsed_status() == ImplementationStatus::Gap)
            .count();
        let coverage_pct = if total == 0 {
            0.0
        } else {
            0.5_f64.mul_add(partial as f64, implemented as f64) / total as f64 * 100.0
        };
        Self {
            domain: domain.to_string(),
            total,
            implemented,
            partial,
            gap,
            coverage_pct,
        }
    }
}

/// Risk assessment for a capability with a non-`None` gap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GapRiskAssessment {
    pub capability_id: String,
    pub gap_severity: String,
    pub mapped_beads: usize,
    /// `true` if `mapped_bead_ids` is non-empty.
    pub has_remediation_path: bool,
    /// Normalized risk score `0.0` (no risk) to `1.0` (critical unmitigated).
    pub risk_score: f64,
}

impl GapRiskAssessment {
    fn from_entry(entry: &CapabilityEntry) -> Option<Self> {
        let severity = entry.parsed_gap_severity();
        if severity == GapSeverity::None {
            return None;
        }
        let base_score = match severity {
            GapSeverity::None => 0.0,
            GapSeverity::Low => 0.3,
            GapSeverity::Medium => 0.6,
            GapSeverity::High => 1.0,
        };
        let has_remediation_path = !entry.mapped_bead_ids.is_empty();
        let risk_score = if has_remediation_path {
            (base_score - 0.2_f64).max(0.0_f64)
        } else {
            base_score
        };
        Some(Self {
            capability_id: entry.capability_id.clone(),
            gap_severity: entry.gap_severity.clone(),
            mapped_beads: entry.mapped_bead_ids.len(),
            has_remediation_path,
            risk_score,
        })
    }
}

// ---------------------------------------------------------------------------
// Verdict
// ---------------------------------------------------------------------------

/// Overall verdict for the verification pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PackVerdict {
    /// All checks pass, all required capabilities present, no high-severity gaps.
    Complete,
    /// All checks pass but at least one medium-severity gap with a remediation
    /// path (mapped beads) exists.
    ConditionalPass,
    /// Missing required capabilities, failed checks, or an unmitigated high gap.
    Incomplete,
}

// ---------------------------------------------------------------------------
// VerificationPack
// ---------------------------------------------------------------------------

/// Full programmatic verification report over a [`TraceabilityMatrix`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationPack {
    pub pack_id: String,
    pub matrix_bead: String,
    pub generated_at_ms: u64,
    pub matrix: TraceabilityMatrix,
    pub checks: Vec<VerificationCheck>,
    pub domain_coverage: Vec<DomainCoverage>,
    pub gap_risks: Vec<GapRiskAssessment>,
    pub total_capabilities: usize,
    pub required_met: usize,
    pub required_total: usize,
    pub all_checks_pass: bool,
    pub pack_verdict: PackVerdict,
}

impl VerificationPack {
    /// Build a [`VerificationPack`] from a [`TraceabilityMatrix`], running all
    /// verification checks and computing coverage/risk metrics.
    pub fn from_matrix(pack_id: &str, generated_at_ms: u64, matrix: TraceabilityMatrix) -> Self {
        let mut pack = Self {
            pack_id: pack_id.to_string(),
            matrix_bead: matrix.bead_id.clone(),
            generated_at_ms,
            total_capabilities: matrix.entries.len(),
            required_total: matrix.required_capability_ids.len(),
            required_met: 0,
            all_checks_pass: true,
            pack_verdict: PackVerdict::Complete,
            checks: Vec::new(),
            domain_coverage: Vec::new(),
            gap_risks: Vec::new(),
            matrix,
        };
        pack.run_checks();
        pack
    }

    // -----------------------------------------------------------------------
    // Internal check runners
    // -----------------------------------------------------------------------

    fn run_checks(&mut self) {
        self.check_schema();
        self.check_completeness();
        self.check_consistency();
        self.check_coverage();
        self.check_gap_risks();
        self.finalize();
    }

    fn push(&mut self, check: VerificationCheck) {
        if !check.passed {
            self.all_checks_pass = false;
        }
        self.checks.push(check);
    }

    fn check_schema(&mut self) {
        // VCK-SCH-001: schema_version non-empty
        {
            let ok = !self.matrix.schema_version.is_empty();
            self.push(if ok {
                VerificationCheck::pass(
                    "VCK-SCH-001",
                    "schema_version non-empty",
                    format!("schema_version = '{}'", self.matrix.schema_version),
                    VerificationCategory::Schema,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-SCH-001",
                    "schema_version non-empty",
                    "schema_version is empty",
                    VerificationCategory::Schema,
                )
            });
        }

        // VCK-SCH-002: artifact field matches expected sentinel
        {
            let expected = "ntm-fcp-traceability-matrix";
            let ok = self.matrix.artifact == expected;
            self.push(if ok {
                VerificationCheck::pass(
                    "VCK-SCH-002",
                    "artifact field matches expected",
                    format!("artifact = '{}'", self.matrix.artifact),
                    VerificationCategory::Schema,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-SCH-002",
                    "artifact field matches expected",
                    format!(
                        "artifact '{}' != expected '{}'",
                        self.matrix.artifact, expected
                    ),
                    VerificationCategory::Schema,
                )
            });
        }

        // VCK-SCH-003: all entries have required fields (non-empty IDs/anchors)
        {
            let bad: Vec<&str> = self
                .matrix
                .entries
                .iter()
                .filter(|e| e.capability_id.is_empty() || e.capability_name.is_empty())
                .map(|e| e.capability_id.as_str())
                .collect();
            let ok = bad.is_empty();
            self.push(if ok {
                VerificationCheck::pass(
                    "VCK-SCH-003",
                    "all entries have non-empty id and name",
                    format!("{} entries validated", self.matrix.entries.len()),
                    VerificationCategory::Schema,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-SCH-003",
                    "all entries have non-empty id and name",
                    format!("entries with empty id/name: {:?}", bad),
                    VerificationCategory::Schema,
                )
            });
        }

        // VCK-SCH-004: no duplicate capability_ids
        {
            let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
            for e in &self.matrix.entries {
                *seen.entry(e.capability_id.as_str()).or_default() += 1;
            }
            let dupes: Vec<&str> = seen
                .into_iter()
                .filter(|(_, n)| *n > 1)
                .map(|(id, _)| id)
                .collect();
            let ok = dupes.is_empty();
            self.push(if ok {
                VerificationCheck::pass(
                    "VCK-SCH-004",
                    "no duplicate capability_ids",
                    "all capability_ids are unique".to_string(),
                    VerificationCategory::Schema,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-SCH-004",
                    "no duplicate capability_ids",
                    format!("duplicate ids: {:?}", dupes),
                    VerificationCategory::Schema,
                )
            });
        }
    }

    fn check_completeness(&mut self) {
        // VCK-CMP-001: all required_capability_ids have entries
        {
            let present: std::collections::HashSet<&str> = self
                .matrix
                .entries
                .iter()
                .map(|e| e.capability_id.as_str())
                .collect();
            let missing: Vec<&str> = self
                .matrix
                .required_capability_ids
                .iter()
                .map(|s| s.as_str())
                .filter(|id| !present.contains(id))
                .collect();
            let ok = missing.is_empty();
            self.push(if ok {
                VerificationCheck::pass(
                    "VCK-CMP-001",
                    "all required capabilities have entries",
                    format!(
                        "all {} required capabilities found",
                        self.matrix.required_capability_ids.len()
                    ),
                    VerificationCategory::Completeness,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-CMP-001",
                    "all required capabilities have entries",
                    format!("missing required capabilities: {:?}", missing),
                    VerificationCategory::Completeness,
                )
            });
        }

        // VCK-CMP-002: all entries have at least one surface
        {
            let bad: Vec<&str> = self
                .matrix
                .entries
                .iter()
                .filter(|e| e.surfaces.is_empty())
                .map(|e| e.capability_id.as_str())
                .collect();
            let ok = bad.is_empty();
            self.push(if ok {
                VerificationCheck::pass(
                    "VCK-CMP-002",
                    "all entries have at least one surface",
                    "all entries have surfaces".to_string(),
                    VerificationCategory::Completeness,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-CMP-002",
                    "all entries have at least one surface",
                    format!("entries missing surfaces: {:?}", bad),
                    VerificationCategory::Completeness,
                )
            });
        }

        // VCK-CMP-003: all entries have at least one implementation anchor
        {
            let bad: Vec<&str> = self
                .matrix
                .entries
                .iter()
                .filter(|e| e.implementation_anchors.is_empty())
                .map(|e| e.capability_id.as_str())
                .collect();
            let ok = bad.is_empty();
            self.push(if ok {
                VerificationCheck::pass(
                    "VCK-CMP-003",
                    "all entries have at least one implementation anchor",
                    "all entries have anchors".to_string(),
                    VerificationCategory::Completeness,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-CMP-003",
                    "all entries have at least one implementation anchor",
                    format!("entries missing anchors: {:?}", bad),
                    VerificationCategory::Completeness,
                )
            });
        }
    }

    fn check_consistency(&mut self) {
        // VCK-CON-001: implemented entries must not have high gap_severity
        {
            let bad: Vec<&str> = self
                .matrix
                .entries
                .iter()
                .filter(|e| {
                    e.parsed_status() == ImplementationStatus::Implemented
                        && e.parsed_gap_severity() == GapSeverity::High
                })
                .map(|e| e.capability_id.as_str())
                .collect();
            let ok = bad.is_empty();
            self.push(if ok {
                VerificationCheck::pass(
                    "VCK-CON-001",
                    "implemented entries have no high gap_severity",
                    "no contradiction between implemented status and high gap".to_string(),
                    VerificationCategory::Consistency,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-CON-001",
                    "implemented entries have no high gap_severity",
                    format!("implemented entries with high gap: {:?}", bad),
                    VerificationCategory::Consistency,
                )
            });
        }

        // VCK-CON-002: gap entries must have non-none gap_severity
        {
            let bad: Vec<&str> = self
                .matrix
                .entries
                .iter()
                .filter(|e| {
                    e.parsed_status() == ImplementationStatus::Gap
                        && e.parsed_gap_severity() == GapSeverity::None
                })
                .map(|e| e.capability_id.as_str())
                .collect();
            let ok = bad.is_empty();
            self.push(if ok {
                VerificationCheck::pass(
                    "VCK-CON-002",
                    "gap entries have non-none gap_severity",
                    "all gap-status entries carry a real severity".to_string(),
                    VerificationCategory::Consistency,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-CON-002",
                    "gap entries have non-none gap_severity",
                    format!("gap entries with none severity: {:?}", bad),
                    VerificationCategory::Consistency,
                )
            });
        }

        // VCK-CON-003: high/medium gaps must have at least one mapped_bead_id
        {
            let bad: Vec<&str> = self
                .matrix
                .entries
                .iter()
                .filter(|e| {
                    let sev = e.parsed_gap_severity();
                    (sev == GapSeverity::High || sev == GapSeverity::Medium)
                        && e.mapped_bead_ids.is_empty()
                })
                .map(|e| e.capability_id.as_str())
                .collect();
            let ok = bad.is_empty();
            self.push(if ok {
                VerificationCheck::pass(
                    "VCK-CON-003",
                    "high/medium gaps have mapped beads",
                    "all high/medium gaps have at least one mapped bead".to_string(),
                    VerificationCategory::Consistency,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-CON-003",
                    "high/medium gaps have mapped beads",
                    format!("gaps without remediation beads: {:?}", bad),
                    VerificationCategory::Consistency,
                )
            });
        }
    }

    fn check_coverage(&mut self) {
        // Collect coverage data and checks into locals to avoid borrow conflicts.
        let mut coverage_entries = Vec::new();
        let mut coverage_checks = Vec::new();

        // Build per-domain buckets.
        {
            let mut buckets: BTreeMap<&str, Vec<&CapabilityEntry>> = BTreeMap::new();
            for entry in &self.matrix.entries {
                buckets
                    .entry(entry.domain().label())
                    .or_default()
                    .push(entry);
            }

            for (label, entries) in &buckets {
                coverage_entries.push(DomainCoverage::compute(label, entries));
            }

            // VCK-COV-001: overall weighted coverage >= 50 %
            let (total, weighted_sum) =
                self.matrix
                    .entries
                    .iter()
                    .fold((0usize, 0.0f64), |(t, w), e| {
                        let score = match e.parsed_status() {
                            ImplementationStatus::Implemented => 1.0,
                            ImplementationStatus::Partial => 0.5,
                            ImplementationStatus::Gap => 0.0,
                        };
                        (t + 1, w + score)
                    });
            let overall_pct = if total == 0 {
                0.0
            } else {
                weighted_sum / total as f64 * 100.0
            };
            let ok = overall_pct >= 50.0;
            coverage_checks.push(if ok {
                VerificationCheck::pass(
                    "VCK-COV-001",
                    "overall coverage >= 50%",
                    format!("overall coverage = {:.1}%", overall_pct),
                    VerificationCategory::Coverage,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-COV-001",
                    "overall coverage >= 50%",
                    format!("overall coverage {:.1}% < 50%", overall_pct),
                    VerificationCategory::Coverage,
                )
            });

            // VCK-COV-002: each expected domain has at least one entry
            let all_domains = ["ft", "ntm", "fcp"];
            let missing_domains: Vec<&str> = all_domains
                .iter()
                .copied()
                .filter(|d| !buckets.contains_key(d))
                .collect();
            let ok = missing_domains.is_empty();
            coverage_checks.push(if ok {
                VerificationCheck::pass(
                    "VCK-COV-002",
                    "each domain (ft/ntm/fcp) has at least one entry",
                    format!("domains present: {:?}", buckets.keys().collect::<Vec<_>>()),
                    VerificationCategory::Coverage,
                )
            } else {
                VerificationCheck::fail(
                    "VCK-COV-002",
                    "each domain (ft/ntm/fcp) has at least one entry",
                    format!("domains with no entries: {:?}", missing_domains),
                    VerificationCategory::Coverage,
                )
            });
        }

        self.domain_coverage.extend(coverage_entries);
        for check in coverage_checks {
            self.push(check);
        }
    }

    fn check_gap_risks(&mut self) {
        for entry in &self.matrix.entries {
            if let Some(risk) = GapRiskAssessment::from_entry(entry) {
                self.gap_risks.push(risk);
            }
        }
    }

    fn finalize(&mut self) {
        // Count required capabilities that have corresponding entries.
        let present: std::collections::HashSet<&str> = self
            .matrix
            .entries
            .iter()
            .map(|e| e.capability_id.as_str())
            .collect();
        self.required_met = self
            .matrix
            .required_capability_ids
            .iter()
            .filter(|id| present.contains(id.as_str()))
            .count();

        let has_high_gap = self
            .gap_risks
            .iter()
            .any(|r| r.gap_severity == "high" && !r.has_remediation_path);

        let has_unmitigated_high =
            self.matrix.entries.iter().any(|e| {
                e.parsed_gap_severity() == GapSeverity::High && e.mapped_bead_ids.is_empty()
            });

        let all_required_present = self.required_met == self.required_total;

        self.pack_verdict = if !self.all_checks_pass
            || !all_required_present
            || has_unmitigated_high
            || has_high_gap
        {
            PackVerdict::Incomplete
        } else {
            // All checks pass and all required present.
            // ConditionalPass if there are medium gaps that have remediation paths.
            let has_medium_with_remediation = self
                .gap_risks
                .iter()
                .any(|r| r.gap_severity == "medium" && r.has_remediation_path);

            if has_medium_with_remediation {
                PackVerdict::ConditionalPass
            } else {
                PackVerdict::Complete
            }
        };
    }

    // -----------------------------------------------------------------------
    // Public query helpers
    // -----------------------------------------------------------------------

    /// Group checks by [`VerificationCategory`] label.
    pub fn checks_by_category(&self) -> BTreeMap<String, Vec<&VerificationCheck>> {
        let mut map: BTreeMap<String, Vec<&VerificationCheck>> = BTreeMap::new();
        for check in &self.checks {
            map.entry(check.category.label().to_string())
                .or_default()
                .push(check);
        }
        map
    }

    /// Return only failed checks.
    pub fn failing_checks(&self) -> Vec<&VerificationCheck> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }

    /// One-line human-readable summary.
    pub fn summary(&self) -> String {
        let total = self.checks.len();
        let passed = self.checks.iter().filter(|c| c.passed).count();
        let verdict_str = match self.pack_verdict {
            PackVerdict::Complete => "COMPLETE",
            PackVerdict::ConditionalPass => "CONDITIONAL_PASS",
            PackVerdict::Incomplete => "INCOMPLETE",
        };
        format!(
            "[{}] {}/{} checks passed, {}/{} required capabilities met, {} gap risks; verdict={}",
            self.pack_id,
            passed,
            total,
            self.required_met,
            self.required_total,
            self.gap_risks.len(),
            verdict_str,
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_entry(
        id: &str,
        status: &str,
        gap_severity: &str,
        beads: &[&str],
        surfaces: &[&str],
        anchors: &[&str],
    ) -> CapabilityEntry {
        let domain = if id.starts_with("ft.") {
            "frankenterm"
        } else if id.starts_with("ntm.") {
            "ntm"
        } else {
            "fcp"
        };
        CapabilityEntry {
            capability_id: id.to_string(),
            capability_name: format!("Capability {}", id),
            source_domain: domain.to_string(),
            status: status.to_string(),
            gap_severity: gap_severity.to_string(),
            mapped_bead_ids: beads.iter().map(|s| s.to_string()).collect(),
            surfaces: surfaces.iter().map(|s| s.to_string()).collect(),
            implementation_anchors: anchors.iter().map(|s| s.to_string()).collect(),
            evidence_notes: "test".to_string(),
        }
    }

    /// A minimal valid matrix covering 3 entries (ft, ntm, fcp) with all
    /// required IDs declared and met.
    fn minimal_valid_matrix() -> TraceabilityMatrix {
        TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-3681t.1.5.1".to_string(),
            generated_at_utc: "2026-03-12T00:00:00Z".to_string(),
            required_capability_ids: vec![
                "ft.runtime.observation".to_string(),
                "ntm.swarm.launcher_weighted".to_string(),
                "fcp.host_runtime_native".to_string(),
            ],
            entries: vec![
                make_entry(
                    "ft.runtime.observation",
                    "implemented",
                    "none",
                    &["ft-3681t.2.1"],
                    &["ft watch"],
                    &["crates/frankenterm-core/src/runtime.rs"],
                ),
                make_entry(
                    "ntm.swarm.launcher_weighted",
                    "partial",
                    "medium",
                    &["ft-3681t.3.1", "ft-3681t.3.2"],
                    &["fleet launch plan"],
                    &["crates/frankenterm-core/src/session_profiles.rs"],
                ),
                make_entry(
                    "fcp.host_runtime_native",
                    "partial",
                    "medium",
                    &["ft-3681t.5.1"],
                    &["connector host lifecycle"],
                    &["crates/frankenterm-core/src/connector_host_runtime.rs"],
                ),
            ],
        }
    }

    // -----------------------------------------------------------------------
    // 1. capability_domain_from_id_ft
    // -----------------------------------------------------------------------
    #[test]
    fn capability_domain_from_id_ft() {
        let e = make_entry(
            "ft.runtime.observation",
            "implemented",
            "none",
            &[],
            &["s"],
            &["a"],
        );
        assert_eq!(e.domain(), CapabilityDomain::FrankenTerm);
    }

    // -----------------------------------------------------------------------
    // 2. capability_domain_from_id_ntm
    // -----------------------------------------------------------------------
    #[test]
    fn capability_domain_from_id_ntm() {
        let e = make_entry(
            "ntm.swarm.launcher_weighted",
            "partial",
            "medium",
            &["b"],
            &["s"],
            &["a"],
        );
        assert_eq!(e.domain(), CapabilityDomain::Ntm);
    }

    // -----------------------------------------------------------------------
    // 3. capability_domain_from_id_fcp
    // -----------------------------------------------------------------------
    #[test]
    fn capability_domain_from_id_fcp() {
        let e = make_entry(
            "fcp.host_runtime_native",
            "partial",
            "medium",
            &["b"],
            &["s"],
            &["a"],
        );
        assert_eq!(e.domain(), CapabilityDomain::Fcp);
    }

    // -----------------------------------------------------------------------
    // 4. parsed_status_variants
    // -----------------------------------------------------------------------
    #[test]
    fn parsed_status_variants() {
        let mk = |s: &str| make_entry("ft.x", s, "none", &[], &["s"], &["a"]);
        assert_eq!(
            mk("implemented").parsed_status(),
            ImplementationStatus::Implemented
        );
        assert_eq!(mk("partial").parsed_status(), ImplementationStatus::Partial);
        assert_eq!(mk("gap").parsed_status(), ImplementationStatus::Gap);
        assert_eq!(mk("unknown").parsed_status(), ImplementationStatus::Gap);
    }

    // -----------------------------------------------------------------------
    // 5. parsed_gap_severity_ordering
    // -----------------------------------------------------------------------
    #[test]
    fn parsed_gap_severity_ordering() {
        let mk = |g: &str| make_entry("ft.x", "partial", g, &[], &["s"], &["a"]);
        assert_eq!(mk("none").parsed_gap_severity(), GapSeverity::None);
        assert_eq!(mk("low").parsed_gap_severity(), GapSeverity::Low);
        assert_eq!(mk("medium").parsed_gap_severity(), GapSeverity::Medium);
        assert_eq!(mk("high").parsed_gap_severity(), GapSeverity::High);
        assert_eq!(mk("unknown").parsed_gap_severity(), GapSeverity::High);
    }

    // -----------------------------------------------------------------------
    // 6. gap_severity_ord_correct
    // -----------------------------------------------------------------------
    #[test]
    fn gap_severity_ord_correct() {
        assert!(GapSeverity::None < GapSeverity::Low);
        assert!(GapSeverity::Low < GapSeverity::Medium);
        assert!(GapSeverity::Medium < GapSeverity::High);
    }

    // -----------------------------------------------------------------------
    // 7. verification_category_variants
    // -----------------------------------------------------------------------
    #[test]
    fn verification_category_variants() {
        let categories = [
            VerificationCategory::Schema,
            VerificationCategory::Completeness,
            VerificationCategory::Consistency,
            VerificationCategory::AnchorValidity,
            VerificationCategory::BeadMapping,
            VerificationCategory::Coverage,
        ];
        // Each category must have a non-empty label.
        for cat in &categories {
            assert!(!cat.label().is_empty());
        }
        // All labels must be distinct.
        let labels: Vec<_> = categories.iter().map(|c| c.label()).collect();
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }

    // -----------------------------------------------------------------------
    // 8. empty_matrix_fails_completeness
    // -----------------------------------------------------------------------
    #[test]
    fn empty_matrix_fails_completeness() {
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-3681t.1.5.1".to_string(),
            generated_at_utc: "2026-03-12T00:00:00Z".to_string(),
            required_capability_ids: vec!["ft.runtime.observation".to_string()],
            entries: vec![],
        };
        let pack = VerificationPack::from_matrix("test-pack", 0, matrix);
        // VCK-CMP-001 must fail — required capability has no entry.
        let cmp001 = pack
            .checks
            .iter()
            .find(|c| c.check_id == "VCK-CMP-001")
            .unwrap();
        assert!(!cmp001.passed);
        assert_eq!(pack.pack_verdict, PackVerdict::Incomplete);
    }

    // -----------------------------------------------------------------------
    // 9. complete_matrix_passes_all
    // -----------------------------------------------------------------------
    #[test]
    fn complete_matrix_passes_all() {
        // Build a matrix where all required IDs are implemented with no gaps.
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec![
                "ft.alpha".to_string(),
                "ntm.beta".to_string(),
                "fcp.gamma".to_string(),
            ],
            entries: vec![
                make_entry(
                    "ft.alpha",
                    "implemented",
                    "none",
                    &["ft-1"],
                    &["ft watch"],
                    &["src/a.rs"],
                ),
                make_entry(
                    "ntm.beta",
                    "implemented",
                    "none",
                    &["ft-2"],
                    &["cli"],
                    &["src/b.rs"],
                ),
                make_entry(
                    "fcp.gamma",
                    "implemented",
                    "none",
                    &["ft-3"],
                    &["api"],
                    &["src/c.rs"],
                ),
            ],
        };
        let pack = VerificationPack::from_matrix("full-pack", 1000, matrix);
        assert!(
            pack.all_checks_pass,
            "failing: {:?}",
            pack.failing_checks()
                .iter()
                .map(|c| c.check_id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(pack.required_met, 3);
        assert_eq!(pack.required_total, 3);
        assert!(pack.gap_risks.is_empty());
        assert_eq!(pack.pack_verdict, PackVerdict::Complete);
    }

    // -----------------------------------------------------------------------
    // 10. duplicate_capability_id_fails_schema
    // -----------------------------------------------------------------------
    #[test]
    fn duplicate_capability_id_fails_schema() {
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec![],
            entries: vec![
                make_entry("ft.dup", "implemented", "none", &[], &["s"], &["a.rs"]),
                make_entry("ft.dup", "implemented", "none", &[], &["s"], &["b.rs"]),
            ],
        };
        let pack = VerificationPack::from_matrix("dup-pack", 0, matrix);
        let sch004 = pack
            .checks
            .iter()
            .find(|c| c.check_id == "VCK-SCH-004")
            .unwrap();
        assert!(
            !sch004.passed,
            "expected VCK-SCH-004 to fail on duplicate id"
        );
    }

    // -----------------------------------------------------------------------
    // 11. implemented_with_high_gap_fails_consistency
    // -----------------------------------------------------------------------
    #[test]
    fn implemented_with_high_gap_fails_consistency() {
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec![],
            entries: vec![make_entry(
                "ft.bad",
                "implemented",
                "high",
                &["ft-x"],
                &["s"],
                &["a.rs"],
            )],
        };
        let pack = VerificationPack::from_matrix("con-pack", 0, matrix);
        let con001 = pack
            .checks
            .iter()
            .find(|c| c.check_id == "VCK-CON-001")
            .unwrap();
        assert!(!con001.passed);
    }

    // -----------------------------------------------------------------------
    // 12. gap_without_severity_fails_consistency
    // -----------------------------------------------------------------------
    #[test]
    fn gap_without_severity_fails_consistency() {
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec![],
            entries: vec![make_entry(
                "ft.nogap",
                "gap",
                "none",
                &[],
                &["s"],
                &["a.rs"],
            )],
        };
        let pack = VerificationPack::from_matrix("gap-pack", 0, matrix);
        let con002 = pack
            .checks
            .iter()
            .find(|c| c.check_id == "VCK-CON-002")
            .unwrap();
        assert!(!con002.passed);
    }

    // -----------------------------------------------------------------------
    // 13. high_gap_without_beads_fails_consistency
    // -----------------------------------------------------------------------
    #[test]
    fn high_gap_without_beads_fails_consistency() {
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec![],
            entries: vec![make_entry(
                "ft.highgap",
                "gap",
                "high",
                &[],
                &["s"],
                &["a.rs"],
            )],
        };
        let pack = VerificationPack::from_matrix("hg-pack", 0, matrix);
        let con003 = pack
            .checks
            .iter()
            .find(|c| c.check_id == "VCK-CON-003")
            .unwrap();
        assert!(!con003.passed);
    }

    // -----------------------------------------------------------------------
    // 14. domain_coverage_calculation
    // -----------------------------------------------------------------------
    #[test]
    fn domain_coverage_calculation() {
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec![],
            entries: vec![
                // ft domain: 1 implemented + 1 partial = 1.5/2 = 75%
                make_entry("ft.alpha", "implemented", "none", &[], &["s"], &["a.rs"]),
                make_entry("ft.beta", "partial", "low", &["ft-x"], &["s"], &["a.rs"]),
            ],
        };
        let pack = VerificationPack::from_matrix("cov-pack", 0, matrix);
        let ft_cov = pack
            .domain_coverage
            .iter()
            .find(|d| d.domain == "ft")
            .unwrap();
        assert_eq!(ft_cov.total, 2);
        assert_eq!(ft_cov.implemented, 1);
        assert_eq!(ft_cov.partial, 1);
        assert_eq!(ft_cov.gap, 0);
        // (1 + 0.5*1) / 2 * 100 = 75.0
        assert!((ft_cov.coverage_pct - 75.0).abs() < 0.001);
    }

    // -----------------------------------------------------------------------
    // 15. domain_coverage_all_implemented
    // -----------------------------------------------------------------------
    #[test]
    fn domain_coverage_all_implemented() {
        let entries = vec![
            make_entry("ft.x", "implemented", "none", &[], &["s"], &["a.rs"]),
            make_entry("ft.y", "implemented", "none", &[], &["s"], &["b.rs"]),
        ];
        let coverage = DomainCoverage::compute("ft", &entries.iter().collect::<Vec<_>>());
        assert!((coverage.coverage_pct - 100.0).abs() < 0.001);
    }

    // -----------------------------------------------------------------------
    // 16. gap_risk_high_severity
    // -----------------------------------------------------------------------
    #[test]
    fn gap_risk_high_severity() {
        let entry = make_entry("ft.x", "gap", "high", &[], &["s"], &["a.rs"]);
        // No remediation path → score stays at 1.0
        let risk = GapRiskAssessment::from_entry(&entry).unwrap();
        assert_eq!(risk.gap_severity, "high");
        assert!(!risk.has_remediation_path);
        assert!((risk.risk_score - 1.0).abs() < 0.001);
    }

    // -----------------------------------------------------------------------
    // 17. gap_risk_with_remediation_reduces_score
    // -----------------------------------------------------------------------
    #[test]
    fn gap_risk_with_remediation_reduces_score() {
        let entry = make_entry("ft.x", "gap", "high", &["ft-1"], &["s"], &["a.rs"]);
        let risk = GapRiskAssessment::from_entry(&entry).unwrap();
        assert!(risk.has_remediation_path);
        // 1.0 - 0.2 = 0.8
        assert!((risk.risk_score - 0.8).abs() < 0.001);

        // Medium with remediation: 0.6 - 0.2 = 0.4
        let entry2 = make_entry("ntm.y", "partial", "medium", &["ft-2"], &["s"], &["b.rs"]);
        let risk2 = GapRiskAssessment::from_entry(&entry2).unwrap();
        assert!((risk2.risk_score - 0.4).abs() < 0.001);

        // Low with remediation: 0.3 - 0.2 = 0.1
        let entry3 = make_entry("fcp.z", "partial", "low", &["ft-3"], &["s"], &["c.rs"]);
        let risk3 = GapRiskAssessment::from_entry(&entry3).unwrap();
        assert!((risk3.risk_score - 0.1).abs() < 0.001);
    }

    // -----------------------------------------------------------------------
    // 18. pack_verdict_complete
    // -----------------------------------------------------------------------
    #[test]
    fn pack_verdict_complete() {
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec![
                "ft.a".to_string(),
                "ntm.b".to_string(),
                "fcp.c".to_string(),
            ],
            entries: vec![
                make_entry("ft.a", "implemented", "none", &[], &["s"], &["a.rs"]),
                make_entry("ntm.b", "implemented", "none", &[], &["s"], &["b.rs"]),
                make_entry("fcp.c", "implemented", "none", &[], &["s"], &["c.rs"]),
            ],
        };
        let pack = VerificationPack::from_matrix("verd-pack", 0, matrix);
        assert_eq!(pack.pack_verdict, PackVerdict::Complete);
    }

    // -----------------------------------------------------------------------
    // 19. pack_verdict_conditional_pass
    // -----------------------------------------------------------------------
    #[test]
    fn pack_verdict_conditional_pass() {
        // All required met, no check failures, but has medium gaps with remediation.
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec![
                "ft.a".to_string(),
                "ntm.b".to_string(),
                "fcp.c".to_string(),
            ],
            entries: vec![
                make_entry("ft.a", "implemented", "none", &[], &["s"], &["a.rs"]),
                // medium gap with remediation — ConditionalPass trigger
                make_entry(
                    "ntm.b",
                    "partial",
                    "medium",
                    &["ft-3681t.3.1"],
                    &["s"],
                    &["b.rs"],
                ),
                make_entry(
                    "fcp.c",
                    "partial",
                    "medium",
                    &["ft-3681t.5.1"],
                    &["s"],
                    &["c.rs"],
                ),
            ],
        };
        let pack = VerificationPack::from_matrix("cond-pack", 0, matrix);
        assert!(
            pack.all_checks_pass,
            "unexpected failures: {:?}",
            pack.failing_checks()
                .iter()
                .map(|c| &c.check_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(pack.pack_verdict, PackVerdict::ConditionalPass);
    }

    // -----------------------------------------------------------------------
    // 20. pack_verdict_incomplete
    // -----------------------------------------------------------------------
    #[test]
    fn pack_verdict_incomplete() {
        // Missing a required capability.
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec!["ft.missing".to_string()],
            entries: vec![make_entry(
                "ft.present",
                "implemented",
                "none",
                &[],
                &["s"],
                &["a.rs"],
            )],
        };
        let pack = VerificationPack::from_matrix("inc-pack", 0, matrix);
        assert_eq!(pack.pack_verdict, PackVerdict::Incomplete);
        assert_eq!(pack.required_met, 0);
        assert_eq!(pack.required_total, 1);
    }

    // -----------------------------------------------------------------------
    // 21. checks_by_category_grouping
    // -----------------------------------------------------------------------
    #[test]
    fn checks_by_category_grouping() {
        let pack = VerificationPack::from_matrix("grp-pack", 0, minimal_valid_matrix());
        let by_cat = pack.checks_by_category();
        // Schema checks must be present under "Schema".
        assert!(by_cat.contains_key("Schema"), "Schema category missing");
        assert!(
            by_cat.contains_key("Completeness"),
            "Completeness category missing"
        );
        assert!(
            by_cat.contains_key("Consistency"),
            "Consistency category missing"
        );
        assert!(by_cat.contains_key("Coverage"), "Coverage category missing");

        // Every check in the pack must appear in exactly one category group.
        let total_in_groups: usize = by_cat.values().map(|v| v.len()).sum();
        assert_eq!(total_in_groups, pack.checks.len());
    }

    // -----------------------------------------------------------------------
    // 22. summary_format
    // -----------------------------------------------------------------------
    #[test]
    fn summary_format() {
        let pack = VerificationPack::from_matrix("sum-pack", 42, minimal_valid_matrix());
        let summary = pack.summary();
        // Must contain the pack_id
        assert!(summary.contains("sum-pack"), "summary missing pack_id");
        // Must contain required_met / required_total
        assert!(
            summary.contains(&pack.required_met.to_string()),
            "summary missing required_met"
        );
        // Must contain verdict
        let verdict_str = match pack.pack_verdict {
            PackVerdict::Complete => "COMPLETE",
            PackVerdict::ConditionalPass => "CONDITIONAL_PASS",
            PackVerdict::Incomplete => "INCOMPLETE",
        };
        assert!(summary.contains(verdict_str), "summary missing verdict");
    }

    // -----------------------------------------------------------------------
    // 23. serde_roundtrip_verification_pack
    // -----------------------------------------------------------------------
    #[test]
    fn serde_roundtrip_verification_pack() {
        let pack = VerificationPack::from_matrix("rt-pack", 9999, minimal_valid_matrix());
        let json = serde_json::to_string(&pack).expect("serialize");
        let back: VerificationPack = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.pack_id, pack.pack_id);
        assert_eq!(back.pack_verdict, pack.pack_verdict);
        assert_eq!(back.checks.len(), pack.checks.len());
        assert_eq!(back.domain_coverage.len(), pack.domain_coverage.len());
        assert_eq!(back.gap_risks.len(), pack.gap_risks.len());
        assert_eq!(back.required_met, pack.required_met);
        assert_eq!(back.required_total, pack.required_total);
    }
}
