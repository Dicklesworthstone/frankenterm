//! Policy compliance reporting and audit integration.
//!
//! Aggregates data from the policy decision log, quarantine registry,
//! and forensic export pipeline into unified compliance snapshots and
//! reports. Tracks compliance metrics, violation trends, and generates
//! audit-ready evidence bundles.
//!
//! Part of ft-3681t.6.4/ft-3681t.6.5 precursor work.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

// =============================================================================
// Compliance status and health
// =============================================================================

/// Overall compliance status for a subsystem or the entire platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplianceStatus {
    /// All policy checks pass, no active violations.
    Compliant,
    /// Minor issues flagged but within acceptable thresholds.
    Advisory,
    /// Active violations that need attention.
    NonCompliant,
    /// Critical violations requiring immediate action.
    Critical,
}

impl fmt::Display for ComplianceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compliant => write!(f, "compliant"),
            Self::Advisory => write!(f, "advisory"),
            Self::NonCompliant => write!(f, "non_compliant"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

/// A compliance violation record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComplianceViolation {
    /// Unique violation identifier.
    pub violation_id: String,
    /// When the violation was detected.
    pub detected_at_ms: u64,
    /// Which policy rule was violated.
    pub rule_id: String,
    /// The policy surface where the violation occurred.
    pub surface: String,
    /// The actor who caused the violation.
    pub actor_id: String,
    /// Severity of the violation.
    pub severity: ViolationSeverity,
    /// What happened.
    pub description: String,
    /// Whether the violation has been remediated.
    pub remediated: bool,
    /// When remediated (if applicable).
    pub remediated_at_ms: Option<u64>,
    /// Who remediated (if applicable).
    pub remediated_by: Option<String>,
}

/// Severity of a compliance violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationSeverity {
    /// Informational — logged but no action required.
    Info,
    /// Low — should be reviewed.
    Low,
    /// Medium — requires remediation within SLA.
    Medium,
    /// High — requires prompt remediation.
    High,
    /// Critical — requires immediate action.
    Critical,
}

impl fmt::Display for ViolationSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => write!(f, "info"),
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

// =============================================================================
// Compliance snapshot — point-in-time aggregate
// =============================================================================

/// Point-in-time compliance snapshot across all subsystems.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComplianceSnapshot {
    /// When this snapshot was captured.
    pub captured_at_ms: u64,
    /// Overall platform compliance status.
    pub overall_status: ComplianceStatus,
    /// Per-subsystem compliance status.
    pub subsystem_status: BTreeMap<String, SubsystemCompliance>,
    /// Active (unremediated) violations.
    pub active_violations: Vec<ComplianceViolation>,
    /// Summary counters.
    pub counters: ComplianceCounters,
}

/// Compliance status for a single subsystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubsystemCompliance {
    /// Name of the subsystem.
    pub subsystem: String,
    /// Compliance status.
    pub status: ComplianceStatus,
    /// Number of active violations in this subsystem.
    pub active_violation_count: u32,
    /// Number of quarantined components.
    pub quarantined_count: u32,
    /// Last policy evaluation timestamp.
    pub last_evaluated_ms: u64,
}

/// Aggregate compliance counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ComplianceCounters {
    /// Total policy evaluations observed.
    pub total_evaluations: u64,
    /// Total denials.
    pub total_denials: u64,
    /// Total violations detected.
    pub total_violations_detected: u64,
    /// Total violations remediated.
    pub total_violations_remediated: u64,
    /// Total quarantines imposed.
    pub total_quarantines: u64,
    /// Total kill switch trips.
    pub total_kill_switch_trips: u64,
    /// Total forensic records captured.
    pub total_forensic_records: u64,
    /// Snapshots generated.
    pub snapshots_generated: u64,
}

// =============================================================================
// Compliance report — structured audit output
// =============================================================================

/// A structured compliance report for audit purposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComplianceReport {
    /// Report identifier.
    pub report_id: String,
    /// Report generation timestamp.
    pub generated_at_ms: u64,
    /// Time range covered by this report.
    pub period_start_ms: u64,
    pub period_end_ms: u64,
    /// Current compliance snapshot.
    pub snapshot: ComplianceSnapshot,
    /// Violation trend within the reporting period.
    pub violation_trend: ViolationTrend,
    /// Remediation summary.
    pub remediation_summary: RemediationSummary,
    /// Policy coverage assessment.
    pub coverage: PolicyCoverage,
}

/// Violation trend within a reporting period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViolationTrend {
    /// Violations detected in this period.
    pub violations_in_period: u32,
    /// Violations remediated in this period.
    pub remediations_in_period: u32,
    /// New vs carried-over violations.
    pub new_violations: u32,
    pub carried_over: u32,
    /// Trend direction.
    pub direction: TrendDirection,
}

/// Direction of a trend metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrendDirection {
    /// Getting better.
    Improving,
    /// Staying the same.
    Stable,
    /// Getting worse.
    Degrading,
}

impl fmt::Display for TrendDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Improving => write!(f, "improving"),
            Self::Stable => write!(f, "stable"),
            Self::Degrading => write!(f, "degrading"),
        }
    }
}

/// Summary of remediation activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemediationSummary {
    /// Total remediations completed.
    pub completed: u32,
    /// Average time to remediate (ms).
    pub avg_time_to_remediate_ms: u64,
    /// Longest open violation (ms since detection).
    pub oldest_open_violation_age_ms: u64,
    /// Violations past SLA.
    pub past_sla_count: u32,
}

/// Policy coverage assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyCoverage {
    /// Total policy surfaces defined.
    pub surfaces_defined: u32,
    /// Surfaces with at least one rule.
    pub surfaces_covered: u32,
    /// Surfaces with no rules (gaps).
    pub surfaces_uncovered: u32,
    /// Names of uncovered surfaces.
    pub uncovered_surface_names: Vec<String>,
}

// =============================================================================
// Configuration
// =============================================================================

/// TOML-serializable configuration for the compliance engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ComplianceConfig {
    /// Maximum number of violations retained.
    pub max_violations: usize,
    /// SLA threshold in milliseconds — violations older than this are "past SLA".
    pub sla_threshold_ms: u64,
}

impl Default for ComplianceConfig {
    fn default() -> Self {
        Self {
            max_violations: 500,
            sla_threshold_ms: 3_600_000, // 1 hour
        }
    }
}

// =============================================================================
// Compliance engine — aggregation and reporting
// =============================================================================

/// Compliance engine that aggregates violations and generates reports.
pub struct ComplianceEngine {
    violations: Vec<ComplianceViolation>,
    max_violations: usize,
    subsystem_last_eval: BTreeMap<String, u64>,
    counters: ComplianceCounters,
    sla_threshold_ms: u64,
}

impl ComplianceEngine {
    /// Create a new compliance engine.
    pub fn new(max_violations: usize, sla_threshold_ms: u64) -> Self {
        Self {
            violations: Vec::new(),
            max_violations: max_violations.max(1),
            subsystem_last_eval: BTreeMap::new(),
            counters: ComplianceCounters::default(),
            sla_threshold_ms,
        }
    }

    /// Create a compliance engine from configuration.
    pub fn from_config(config: &ComplianceConfig) -> Self {
        Self::new(config.max_violations, config.sla_threshold_ms)
    }

    /// Record a policy evaluation (allow or deny).
    pub fn record_evaluation(&mut self, denied: bool) {
        self.counters.total_evaluations += 1;
        if denied {
            self.counters.total_denials += 1;
        }
    }

    /// Record a new violation.
    pub fn record_violation(&mut self, violation: ComplianceViolation) {
        if self.violations.len() >= self.max_violations {
            // Evict the oldest remediated violation, or the oldest overall
            if let Some(pos) = self.violations.iter().position(|v| v.remediated) {
                self.violations.remove(pos);
            } else {
                self.violations.remove(0);
            }
        }
        self.violations.push(violation);
        self.counters.total_violations_detected += 1;
    }

    /// Remediate a violation.
    pub fn remediate(
        &mut self,
        violation_id: &str,
        by: &str,
        now_ms: u64,
    ) -> Option<&ComplianceViolation> {
        if let Some(v) = self
            .violations
            .iter_mut()
            .find(|v| v.violation_id == violation_id && !v.remediated)
        {
            v.remediated = true;
            v.remediated_at_ms = Some(now_ms);
            v.remediated_by = Some(by.to_string());
            self.counters.total_violations_remediated += 1;
            Some(v)
        } else {
            None
        }
    }

    /// Record a quarantine event.
    pub fn record_quarantine(&mut self) {
        self.counters.total_quarantines += 1;
    }

    /// Record a kill switch trip.
    pub fn record_kill_switch_trip(&mut self) {
        self.counters.total_kill_switch_trips += 1;
    }

    /// Record forensic records captured.
    pub fn record_forensic_records(&mut self, count: u64) {
        self.counters.total_forensic_records += count;
    }

    /// Update the last evaluation timestamp for a subsystem.
    pub fn update_subsystem_eval(&mut self, subsystem: &str, now_ms: u64) {
        self.subsystem_last_eval
            .insert(subsystem.to_string(), now_ms);
    }

    /// Get the number of active (unremediated) violations.
    pub fn active_violation_count(&self) -> usize {
        self.violations.iter().filter(|v| !v.remediated).count()
    }

    /// Get active violations by severity threshold.
    pub fn active_violations_above(
        &self,
        min_severity: ViolationSeverity,
    ) -> Vec<&ComplianceViolation> {
        self.violations
            .iter()
            .filter(|v| !v.remediated && v.severity >= min_severity)
            .collect()
    }

    /// Compute the overall compliance status.
    pub fn compute_status(&self) -> ComplianceStatus {
        let active = self.active_violations_above(ViolationSeverity::Info);
        let critical_count = active
            .iter()
            .filter(|v| v.severity == ViolationSeverity::Critical)
            .count();
        let high_count = active
            .iter()
            .filter(|v| v.severity == ViolationSeverity::High)
            .count();
        let medium_count = active
            .iter()
            .filter(|v| v.severity == ViolationSeverity::Medium)
            .count();

        if critical_count > 0 {
            ComplianceStatus::Critical
        } else if high_count > 0 {
            ComplianceStatus::NonCompliant
        } else if medium_count > 0 {
            ComplianceStatus::Advisory
        } else {
            ComplianceStatus::Compliant
        }
    }

    /// Compute per-subsystem compliance.
    fn compute_subsystem_status(&self) -> BTreeMap<String, SubsystemCompliance> {
        let mut result = BTreeMap::new();

        // Collect subsystems from violations and eval timestamps
        let mut all_subsystems: Vec<String> = self
            .violations
            .iter()
            .map(|v| v.surface.clone())
            .chain(self.subsystem_last_eval.keys().cloned())
            .collect();
        all_subsystems.sort();
        all_subsystems.dedup();

        for subsystem in all_subsystems {
            let active_violations: Vec<_> = self
                .violations
                .iter()
                .filter(|v| !v.remediated && v.surface == subsystem)
                .collect();

            let status = if active_violations
                .iter()
                .any(|v| v.severity == ViolationSeverity::Critical)
            {
                ComplianceStatus::Critical
            } else if active_violations
                .iter()
                .any(|v| v.severity == ViolationSeverity::High)
            {
                ComplianceStatus::NonCompliant
            } else if !active_violations.is_empty() {
                ComplianceStatus::Advisory
            } else {
                ComplianceStatus::Compliant
            };

            result.insert(
                subsystem.clone(),
                SubsystemCompliance {
                    subsystem: subsystem.clone(),
                    status,
                    active_violation_count: active_violations.len() as u32,
                    quarantined_count: 0, // filled from quarantine registry externally
                    last_evaluated_ms: self
                        .subsystem_last_eval
                        .get(&subsystem)
                        .copied()
                        .unwrap_or(0),
                },
            );
        }

        result
    }

    /// Generate a compliance snapshot.
    pub fn snapshot(&mut self, now_ms: u64) -> ComplianceSnapshot {
        self.counters.snapshots_generated += 1;

        let active_violations: Vec<ComplianceViolation> = self
            .violations
            .iter()
            .filter(|v| !v.remediated)
            .cloned()
            .collect();

        ComplianceSnapshot {
            captured_at_ms: now_ms,
            overall_status: self.compute_status(),
            subsystem_status: self.compute_subsystem_status(),
            active_violations,
            counters: self.counters.clone(),
        }
    }

    /// Generate a compliance report for a given time period.
    pub fn generate_report(
        &mut self,
        report_id: &str,
        period_start_ms: u64,
        period_end_ms: u64,
        now_ms: u64,
        coverage: PolicyCoverage,
    ) -> ComplianceReport {
        let snapshot = self.snapshot(now_ms);

        let new_violations = self
            .violations
            .iter()
            .filter(|v| v.detected_at_ms >= period_start_ms && v.detected_at_ms <= period_end_ms)
            .count() as u32;

        let period_remediations: Vec<_> = self
            .violations
            .iter()
            .filter(|v| {
                v.remediated
                    && v.remediated_at_ms
                        .is_some_and(|t| t >= period_start_ms && t <= period_end_ms)
            })
            .collect();
        let remediations_in_period = period_remediations.len() as u32;
        let carried_over = self
            .violations
            .iter()
            .filter(|v| !v.remediated && v.detected_at_ms < period_start_ms)
            .count() as u32;

        let direction = if new_violations == 0 && carried_over == 0 {
            TrendDirection::Improving
        } else if new_violations > remediations_in_period {
            TrendDirection::Degrading
        } else if new_violations < remediations_in_period {
            TrendDirection::Improving
        } else {
            TrendDirection::Stable
        };

        let violation_trend = ViolationTrend {
            violations_in_period: new_violations,
            remediations_in_period,
            new_violations,
            carried_over,
            direction,
        };

        // Remediation summary
        let completed = period_remediations.len() as u32;
        let avg_time_to_remediate_ms = if completed > 0 {
            let total_time: u64 = period_remediations
                .iter()
                .map(|v| v.remediated_at_ms.unwrap_or(0).saturating_sub(v.detected_at_ms))
                .sum();
            total_time / u64::from(completed)
        } else {
            0
        };

        let oldest_open = self
            .violations
            .iter()
            .filter(|v| !v.remediated)
            .map(|v| now_ms.saturating_sub(v.detected_at_ms))
            .max()
            .unwrap_or(0);

        let past_sla = self
            .violations
            .iter()
            .filter(|v| {
                !v.remediated && now_ms.saturating_sub(v.detected_at_ms) > self.sla_threshold_ms
            })
            .count() as u32;

        ComplianceReport {
            report_id: report_id.to_string(),
            generated_at_ms: now_ms,
            period_start_ms,
            period_end_ms,
            snapshot,
            violation_trend,
            remediation_summary: RemediationSummary {
                completed,
                avg_time_to_remediate_ms,
                oldest_open_violation_age_ms: oldest_open,
                past_sla_count: past_sla,
            },
            coverage,
        }
    }

    /// Get counters.
    pub fn counters(&self) -> &ComplianceCounters {
        &self.counters
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_violation(
        id: &str,
        ts: u64,
        severity: ViolationSeverity,
        surface: &str,
    ) -> ComplianceViolation {
        ComplianceViolation {
            violation_id: id.to_string(),
            detected_at_ms: ts,
            rule_id: "test-rule".to_string(),
            surface: surface.to_string(),
            actor_id: "actor-1".to_string(),
            severity,
            description: "test violation".to_string(),
            remediated: false,
            remediated_at_ms: None,
            remediated_by: None,
        }
    }

    fn make_coverage() -> PolicyCoverage {
        PolicyCoverage {
            surfaces_defined: 10,
            surfaces_covered: 8,
            surfaces_uncovered: 2,
            uncovered_surface_names: vec!["surface_a".to_string(), "surface_b".to_string()],
        }
    }

    #[test]
    fn empty_engine_is_compliant() {
        let engine = ComplianceEngine::new(100, 3600_000);
        assert_eq!(engine.compute_status(), ComplianceStatus::Compliant);
        assert_eq!(engine.active_violation_count(), 0);
    }

    #[test]
    fn violation_affects_status() {
        let mut engine = ComplianceEngine::new(100, 3600_000);

        // Info violations don't change compliance
        engine.record_violation(make_violation("v1", 1000, ViolationSeverity::Info, "policy"));
        assert_eq!(engine.compute_status(), ComplianceStatus::Compliant);

        // Medium triggers advisory
        engine.record_violation(make_violation("v2", 2000, ViolationSeverity::Medium, "policy"));
        assert_eq!(engine.compute_status(), ComplianceStatus::Advisory);

        // High triggers non-compliant
        engine.record_violation(make_violation("v3", 3000, ViolationSeverity::High, "policy"));
        assert_eq!(engine.compute_status(), ComplianceStatus::NonCompliant);

        // Critical triggers critical
        engine.record_violation(make_violation(
            "v4",
            4000,
            ViolationSeverity::Critical,
            "policy",
        ));
        assert_eq!(engine.compute_status(), ComplianceStatus::Critical);
    }

    #[test]
    fn remediation_restores_compliance() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        engine.record_violation(make_violation("v1", 1000, ViolationSeverity::High, "policy"));
        assert_eq!(engine.compute_status(), ComplianceStatus::NonCompliant);

        engine.remediate("v1", "admin", 2000);
        assert_eq!(engine.compute_status(), ComplianceStatus::Compliant);
    }

    #[test]
    fn remediation_of_nonexistent_returns_none() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        assert!(engine.remediate("nope", "admin", 1000).is_none());
    }

    #[test]
    fn double_remediation_is_idempotent() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        engine.record_violation(make_violation("v1", 1000, ViolationSeverity::High, "policy"));
        assert!(engine.remediate("v1", "admin", 2000).is_some());
        // Second remediation finds nothing unremediated
        assert!(engine.remediate("v1", "admin", 3000).is_none());
        assert_eq!(engine.counters.total_violations_remediated, 1);
    }

    #[test]
    fn eviction_prefers_remediated() {
        let mut engine = ComplianceEngine::new(3, 3600_000);
        engine.record_violation(make_violation("v1", 1000, ViolationSeverity::Low, "policy"));
        engine.record_violation(make_violation("v2", 2000, ViolationSeverity::Low, "policy"));
        engine.remediate("v1", "admin", 2500);
        engine.record_violation(make_violation("v3", 3000, ViolationSeverity::Low, "policy"));

        // At capacity, adding one more should evict the remediated one
        engine.record_violation(make_violation("v4", 4000, ViolationSeverity::Low, "policy"));
        assert_eq!(
            engine
                .violations
                .iter()
                .any(|v| v.violation_id == "v1"),
            false
        );
        assert_eq!(engine.violations.len(), 3);
    }

    #[test]
    fn active_violations_above_threshold() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        engine.record_violation(make_violation("v1", 1000, ViolationSeverity::Low, "policy"));
        engine.record_violation(make_violation("v2", 2000, ViolationSeverity::High, "policy"));
        engine.record_violation(make_violation("v3", 3000, ViolationSeverity::Critical, "policy"));

        let high_plus = engine.active_violations_above(ViolationSeverity::High);
        assert_eq!(high_plus.len(), 2);
    }

    #[test]
    fn subsystem_compliance_computed() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        engine.record_violation(make_violation("v1", 1000, ViolationSeverity::High, "policy"));
        engine.record_violation(make_violation(
            "v2",
            2000,
            ViolationSeverity::Low,
            "connector",
        ));
        engine.update_subsystem_eval("policy", 1000);
        engine.update_subsystem_eval("connector", 2000);

        let snap = engine.snapshot(3000);
        assert_eq!(
            snap.subsystem_status["policy"].status,
            ComplianceStatus::NonCompliant
        );
        assert_eq!(
            snap.subsystem_status["connector"].status,
            ComplianceStatus::Advisory
        );
    }

    #[test]
    fn snapshot_counters() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        engine.record_evaluation(false);
        engine.record_evaluation(true);
        engine.record_evaluation(true);
        engine.record_quarantine();
        engine.record_kill_switch_trip();
        engine.record_forensic_records(42);

        let snap = engine.snapshot(5000);
        assert_eq!(snap.counters.total_evaluations, 3);
        assert_eq!(snap.counters.total_denials, 2);
        assert_eq!(snap.counters.total_quarantines, 1);
        assert_eq!(snap.counters.total_kill_switch_trips, 1);
        assert_eq!(snap.counters.total_forensic_records, 42);
        assert_eq!(snap.counters.snapshots_generated, 1);
    }

    #[test]
    fn report_generation() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        engine.record_violation(make_violation("v1", 1000, ViolationSeverity::High, "policy"));
        engine.record_violation(make_violation(
            "v2",
            2000,
            ViolationSeverity::Medium,
            "connector",
        ));
        engine.remediate("v1", "admin", 2500);

        let report = engine.generate_report("rpt-1", 0, 5000, 5000, make_coverage());
        assert_eq!(report.report_id, "rpt-1");
        assert_eq!(report.violation_trend.violations_in_period, 2);
        assert_eq!(report.violation_trend.remediations_in_period, 1);
        assert_eq!(report.violation_trend.direction, TrendDirection::Degrading);
        assert_eq!(report.remediation_summary.completed, 1);
        assert_eq!(report.remediation_summary.avg_time_to_remediate_ms, 1500);
        assert_eq!(report.coverage.surfaces_defined, 10);
    }

    #[test]
    fn report_trend_degrading() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        engine.record_violation(make_violation("v1", 1000, ViolationSeverity::High, "policy"));
        engine.record_violation(make_violation("v2", 2000, ViolationSeverity::High, "policy"));
        engine.record_violation(make_violation("v3", 3000, ViolationSeverity::High, "policy"));

        let report = engine.generate_report("rpt-1", 0, 5000, 5000, make_coverage());
        assert_eq!(report.violation_trend.direction, TrendDirection::Degrading);
    }

    #[test]
    fn report_sla_violations() {
        let mut engine = ComplianceEngine::new(100, 1000); // 1s SLA
        engine.record_violation(make_violation("v1", 1000, ViolationSeverity::High, "policy"));

        // At 1500ms: within SLA
        let report = engine.generate_report("rpt-1", 0, 2000, 1500, make_coverage());
        assert_eq!(report.remediation_summary.past_sla_count, 0);

        // At 3000ms: past SLA (2000ms open, threshold 1000ms)
        let report = engine.generate_report("rpt-2", 0, 4000, 3000, make_coverage());
        assert_eq!(report.remediation_summary.past_sla_count, 1);
    }

    #[test]
    fn compliance_status_ordering() {
        assert!(ComplianceStatus::Compliant < ComplianceStatus::Advisory);
        assert!(ComplianceStatus::Advisory < ComplianceStatus::NonCompliant);
        assert!(ComplianceStatus::NonCompliant < ComplianceStatus::Critical);
    }

    #[test]
    fn violation_severity_ordering() {
        assert!(ViolationSeverity::Info < ViolationSeverity::Low);
        assert!(ViolationSeverity::Low < ViolationSeverity::Medium);
        assert!(ViolationSeverity::Medium < ViolationSeverity::High);
        assert!(ViolationSeverity::High < ViolationSeverity::Critical);
    }

    #[test]
    fn compliance_status_display() {
        assert_eq!(ComplianceStatus::Compliant.to_string(), "compliant");
        assert_eq!(ComplianceStatus::Advisory.to_string(), "advisory");
        assert_eq!(ComplianceStatus::NonCompliant.to_string(), "non_compliant");
        assert_eq!(ComplianceStatus::Critical.to_string(), "critical");
    }

    #[test]
    fn violation_severity_display() {
        assert_eq!(ViolationSeverity::Info.to_string(), "info");
        assert_eq!(ViolationSeverity::Critical.to_string(), "critical");
    }

    #[test]
    fn trend_direction_display() {
        assert_eq!(TrendDirection::Improving.to_string(), "improving");
        assert_eq!(TrendDirection::Stable.to_string(), "stable");
        assert_eq!(TrendDirection::Degrading.to_string(), "degrading");
    }

    #[test]
    fn compliance_snapshot_serde_roundtrip() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        engine.record_violation(make_violation("v1", 1000, ViolationSeverity::High, "policy"));
        engine.update_subsystem_eval("policy", 1000);

        let snap = engine.snapshot(2000);
        let json = serde_json::to_string(&snap).unwrap();
        let back: ComplianceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn compliance_report_serde_roundtrip() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        engine.record_violation(make_violation("v1", 1000, ViolationSeverity::Medium, "policy"));
        engine.remediate("v1", "admin", 2000);

        let report = engine.generate_report("rpt-1", 0, 5000, 5000, make_coverage());
        let json = serde_json::to_string(&report).unwrap();
        let back: ComplianceReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
    }

    #[test]
    fn violation_serde_roundtrip() {
        let v = ComplianceViolation {
            violation_id: "v1".to_string(),
            detected_at_ms: 1000,
            rule_id: "r1".to_string(),
            surface: "policy".to_string(),
            actor_id: "a1".to_string(),
            severity: ViolationSeverity::High,
            description: "test".to_string(),
            remediated: true,
            remediated_at_ms: Some(2000),
            remediated_by: Some("admin".to_string()),
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: ComplianceViolation = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn report_with_no_violations_is_improving() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        let report = engine.generate_report("rpt-1", 0, 5000, 5000, make_coverage());
        assert_eq!(report.violation_trend.direction, TrendDirection::Improving);
        assert_eq!(report.snapshot.overall_status, ComplianceStatus::Compliant);
    }

    #[test]
    fn carried_over_violations() {
        let mut engine = ComplianceEngine::new(100, 3600_000);
        // Violation before the reporting period
        engine.record_violation(make_violation("v1", 500, ViolationSeverity::High, "policy"));

        let report = engine.generate_report("rpt-1", 1000, 5000, 5000, make_coverage());
        assert_eq!(report.violation_trend.carried_over, 1);
        assert_eq!(report.violation_trend.new_violations, 0);
    }
}
