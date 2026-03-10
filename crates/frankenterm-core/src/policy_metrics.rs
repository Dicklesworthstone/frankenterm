//! Policy metrics aggregation and health assessment.
//!
//! Unifies metrics from policy_dsl, policy_decision_log, policy_quarantine,
//! policy_compliance, policy_audit_chain, and forensic_export into a single
//! health dashboard model. Computes derived indicators like denial rate,
//! quarantine density, and chain integrity health.
//!
//! Part of ft-3681t.7.1/ft-3681t.7.2 precursor work.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

// =============================================================================
// Metric time series — compact counters
// =============================================================================

/// A single metric sample at a point in time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricSample {
    pub timestamp_ms: u64,
    pub value: u64,
}

/// A bounded time series of metric samples.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricTimeSeries {
    pub name: String,
    pub unit: MetricUnit,
    samples: Vec<MetricSample>,
    max_samples: usize,
}

/// Unit type for a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    Count,
    Milliseconds,
    Percentage,
    BytesPerSecond,
}

impl fmt::Display for MetricUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count => write!(f, "count"),
            Self::Milliseconds => write!(f, "ms"),
            Self::Percentage => write!(f, "%"),
            Self::BytesPerSecond => write!(f, "B/s"),
        }
    }
}

impl MetricTimeSeries {
    pub fn new(name: &str, unit: MetricUnit, max_samples: usize) -> Self {
        Self {
            name: name.to_string(),
            unit,
            samples: Vec::new(),
            max_samples: max_samples.max(1),
        }
    }

    pub fn push(&mut self, timestamp_ms: u64, value: u64) {
        if self.samples.len() >= self.max_samples {
            self.samples.remove(0);
        }
        self.samples.push(MetricSample {
            timestamp_ms,
            value,
        });
    }

    pub fn latest(&self) -> Option<&MetricSample> {
        self.samples.last()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn average(&self) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        let sum: u64 = self.samples.iter().map(|s| s.value).sum();
        Some(sum / self.samples.len() as u64)
    }

    pub fn max(&self) -> Option<u64> {
        self.samples.iter().map(|s| s.value).max()
    }

    pub fn min(&self) -> Option<u64> {
        self.samples.iter().map(|s| s.value).min()
    }

    pub fn samples_in_range(&self, start_ms: u64, end_ms: u64) -> Vec<&MetricSample> {
        self.samples
            .iter()
            .filter(|s| s.timestamp_ms >= start_ms && s.timestamp_ms <= end_ms)
            .collect()
    }
}

// =============================================================================
// Health indicator — derived from metrics
// =============================================================================

/// A health indicator derived from one or more metrics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthIndicator {
    pub name: String,
    pub status: HealthStatus,
    pub value: String,
    pub threshold_warning: String,
    pub threshold_critical: String,
    pub description: String,
}

/// Health status levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Warning,
    Critical,
    Unknown,
}

impl fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Warning => write!(f, "warning"),
            Self::Critical => write!(f, "critical"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

// =============================================================================
// Policy metrics dashboard
// =============================================================================

/// Aggregated policy subsystem metrics dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyMetricsDashboard {
    pub captured_at_ms: u64,
    /// Overall policy subsystem health.
    pub overall_health: HealthStatus,
    /// Health indicators.
    pub indicators: Vec<HealthIndicator>,
    /// Per-subsystem metric summaries.
    pub subsystem_metrics: BTreeMap<String, SubsystemMetricSummary>,
    /// Aggregate counters.
    pub counters: PolicyMetricsCounters,
}

/// Per-subsystem metric summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubsystemMetricSummary {
    pub subsystem: String,
    pub health: HealthStatus,
    pub evaluations: u64,
    pub denials: u64,
    pub denial_rate_pct: u32,
    pub active_quarantines: u32,
    pub active_violations: u32,
}

/// Aggregate policy metrics counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PolicyMetricsCounters {
    pub total_evaluations: u64,
    pub total_denials: u64,
    pub total_quarantines_active: u32,
    pub total_violations_active: u32,
    pub audit_chain_length: u64,
    pub audit_chain_valid: bool,
    pub forensic_records_count: u64,
    pub kill_switch_active: bool,
    pub snapshots_generated: u64,
}

// =============================================================================
// Policy metrics collector
// =============================================================================

/// Input data for metrics collection from individual subsystems.
#[derive(Debug, Clone, Default)]
pub struct PolicySubsystemInput {
    pub evaluations: u64,
    pub denials: u64,
    pub active_quarantines: u32,
    pub active_violations: u32,
}

/// Thresholds for health indicator computation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyMetricsThresholds {
    /// Denial rate above this (pct) triggers warning.
    pub denial_rate_warning_pct: u32,
    /// Denial rate above this (pct) triggers critical.
    pub denial_rate_critical_pct: u32,
    /// Active quarantines above this triggers warning.
    pub quarantine_warning_count: u32,
    /// Active quarantines above this triggers critical.
    pub quarantine_critical_count: u32,
    /// Active violations above this triggers warning.
    pub violation_warning_count: u32,
    /// Active violations above this triggers critical.
    pub violation_critical_count: u32,
}

impl Default for PolicyMetricsThresholds {
    fn default() -> Self {
        Self {
            denial_rate_warning_pct: 10,
            denial_rate_critical_pct: 25,
            quarantine_warning_count: 3,
            quarantine_critical_count: 10,
            violation_warning_count: 5,
            violation_critical_count: 15,
        }
    }
}

/// Collects and aggregates policy metrics from subsystems.
pub struct PolicyMetricsCollector {
    subsystem_inputs: BTreeMap<String, PolicySubsystemInput>,
    thresholds: PolicyMetricsThresholds,
    audit_chain_length: u64,
    audit_chain_valid: bool,
    forensic_records_count: u64,
    kill_switch_active: bool,
    denial_rate_series: MetricTimeSeries,
    quarantine_series: MetricTimeSeries,
    snapshots_generated: u64,
}

impl PolicyMetricsCollector {
    pub fn new(thresholds: PolicyMetricsThresholds) -> Self {
        Self {
            subsystem_inputs: BTreeMap::new(),
            thresholds,
            audit_chain_length: 0,
            audit_chain_valid: true,
            forensic_records_count: 0,
            kill_switch_active: false,
            denial_rate_series: MetricTimeSeries::new("denial_rate", MetricUnit::Percentage, 100),
            quarantine_series: MetricTimeSeries::new("active_quarantines", MetricUnit::Count, 100),
            snapshots_generated: 0,
        }
    }

    /// Update metrics for a subsystem.
    pub fn update_subsystem(&mut self, name: &str, input: PolicySubsystemInput) {
        self.subsystem_inputs.insert(name.to_string(), input);
    }

    /// Update audit chain status.
    pub fn update_audit_chain(&mut self, length: u64, valid: bool) {
        self.audit_chain_length = length;
        self.audit_chain_valid = valid;
    }

    /// Update forensic records count.
    pub fn update_forensic_count(&mut self, count: u64) {
        self.forensic_records_count = count;
    }

    /// Update kill switch status.
    pub fn update_kill_switch(&mut self, active: bool) {
        self.kill_switch_active = active;
    }

    /// Record a time-series sample of the current denial rate.
    pub fn sample_denial_rate(&mut self, timestamp_ms: u64) {
        let total_evals: u64 = self.subsystem_inputs.values().map(|i| i.evaluations).sum();
        let total_denials: u64 = self.subsystem_inputs.values().map(|i| i.denials).sum();
        let rate = (total_denials * 100).checked_div(total_evals).unwrap_or(0);
        self.denial_rate_series.push(timestamp_ms, rate);
    }

    /// Record a time-series sample of active quarantines.
    pub fn sample_quarantine_count(&mut self, timestamp_ms: u64) {
        let total: u64 = self
            .subsystem_inputs
            .values()
            .map(|i| u64::from(i.active_quarantines))
            .sum();
        self.quarantine_series.push(timestamp_ms, total);
    }

    /// Generate a dashboard snapshot.
    pub fn dashboard(&mut self, now_ms: u64) -> PolicyMetricsDashboard {
        self.snapshots_generated += 1;

        let total_evals: u64 = self.subsystem_inputs.values().map(|i| i.evaluations).sum();
        let total_denials: u64 = self.subsystem_inputs.values().map(|i| i.denials).sum();
        let total_quarantines: u32 = self
            .subsystem_inputs
            .values()
            .map(|i| i.active_quarantines)
            .sum();
        let total_violations: u32 = self
            .subsystem_inputs
            .values()
            .map(|i| i.active_violations)
            .sum();

        let denial_rate_pct = (total_denials * 100).checked_div(total_evals).unwrap_or(0) as u32;

        // Build indicators
        let mut indicators = Vec::new();

        // Denial rate indicator
        let denial_status = if denial_rate_pct >= self.thresholds.denial_rate_critical_pct {
            HealthStatus::Critical
        } else if denial_rate_pct >= self.thresholds.denial_rate_warning_pct {
            HealthStatus::Warning
        } else {
            HealthStatus::Healthy
        };
        indicators.push(HealthIndicator {
            name: "denial_rate".to_string(),
            status: denial_status,
            value: format!("{denial_rate_pct}%"),
            threshold_warning: format!("{}%", self.thresholds.denial_rate_warning_pct),
            threshold_critical: format!("{}%", self.thresholds.denial_rate_critical_pct),
            description: "Policy denial rate across all surfaces".to_string(),
        });

        // Quarantine density indicator
        let quarantine_status =
            if total_quarantines >= self.thresholds.quarantine_critical_count {
                HealthStatus::Critical
            } else if total_quarantines >= self.thresholds.quarantine_warning_count {
                HealthStatus::Warning
            } else {
                HealthStatus::Healthy
            };
        indicators.push(HealthIndicator {
            name: "quarantine_density".to_string(),
            status: quarantine_status,
            value: format!("{total_quarantines}"),
            threshold_warning: format!("{}", self.thresholds.quarantine_warning_count),
            threshold_critical: format!("{}", self.thresholds.quarantine_critical_count),
            description: "Active quarantined components".to_string(),
        });

        // Violation count indicator
        let violation_status = if total_violations >= self.thresholds.violation_critical_count {
            HealthStatus::Critical
        } else if total_violations >= self.thresholds.violation_warning_count {
            HealthStatus::Warning
        } else {
            HealthStatus::Healthy
        };
        indicators.push(HealthIndicator {
            name: "compliance_violations".to_string(),
            status: violation_status,
            value: format!("{total_violations}"),
            threshold_warning: format!("{}", self.thresholds.violation_warning_count),
            threshold_critical: format!("{}", self.thresholds.violation_critical_count),
            description: "Active compliance violations".to_string(),
        });

        // Audit chain integrity indicator
        let chain_status = if self.audit_chain_valid {
            HealthStatus::Healthy
        } else {
            HealthStatus::Critical
        };
        indicators.push(HealthIndicator {
            name: "audit_chain_integrity".to_string(),
            status: chain_status,
            value: if self.audit_chain_valid {
                "valid".to_string()
            } else {
                "INVALID".to_string()
            },
            threshold_warning: "n/a".to_string(),
            threshold_critical: "invalid".to_string(),
            description: "Hash chain integrity of audit trail".to_string(),
        });

        // Kill switch indicator
        let ks_status = if self.kill_switch_active {
            HealthStatus::Critical
        } else {
            HealthStatus::Healthy
        };
        indicators.push(HealthIndicator {
            name: "kill_switch".to_string(),
            status: ks_status,
            value: if self.kill_switch_active {
                "ACTIVE".to_string()
            } else {
                "disarmed".to_string()
            },
            threshold_warning: "n/a".to_string(),
            threshold_critical: "active".to_string(),
            description: "Emergency kill switch state".to_string(),
        });

        // Overall health = worst of all indicators
        let overall_health = indicators
            .iter()
            .map(|i| i.status)
            .max()
            .unwrap_or(HealthStatus::Unknown);

        // Per-subsystem summaries
        let mut subsystem_metrics = BTreeMap::new();
        for (name, input) in &self.subsystem_inputs {
            let rate = (input.denials * 100).checked_div(input.evaluations).unwrap_or(0) as u32;

            let health = if rate >= self.thresholds.denial_rate_critical_pct
                || input.active_quarantines >= self.thresholds.quarantine_critical_count
            {
                HealthStatus::Critical
            } else if rate >= self.thresholds.denial_rate_warning_pct
                || input.active_quarantines >= self.thresholds.quarantine_warning_count
            {
                HealthStatus::Warning
            } else {
                HealthStatus::Healthy
            };

            subsystem_metrics.insert(
                name.clone(),
                SubsystemMetricSummary {
                    subsystem: name.clone(),
                    health,
                    evaluations: input.evaluations,
                    denials: input.denials,
                    denial_rate_pct: rate,
                    active_quarantines: input.active_quarantines,
                    active_violations: input.active_violations,
                },
            );
        }

        let counters = PolicyMetricsCounters {
            total_evaluations: total_evals,
            total_denials,
            total_quarantines_active: total_quarantines,
            total_violations_active: total_violations,
            audit_chain_length: self.audit_chain_length,
            audit_chain_valid: self.audit_chain_valid,
            forensic_records_count: self.forensic_records_count,
            kill_switch_active: self.kill_switch_active,
            snapshots_generated: self.snapshots_generated,
        };

        PolicyMetricsDashboard {
            captured_at_ms: now_ms,
            overall_health,
            indicators,
            subsystem_metrics,
            counters,
        }
    }

    /// Access the denial rate time series.
    pub fn denial_rate_series(&self) -> &MetricTimeSeries {
        &self.denial_rate_series
    }

    /// Access the quarantine count time series.
    pub fn quarantine_series(&self) -> &MetricTimeSeries {
        &self.quarantine_series
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_collector_is_healthy() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        let dash = c.dashboard(1000);
        assert_eq!(dash.overall_health, HealthStatus::Healthy);
        assert!(dash.subsystem_metrics.is_empty());
        assert_eq!(dash.counters.total_evaluations, 0);
    }

    #[test]
    fn low_denial_rate_healthy() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_subsystem(
            "policy",
            PolicySubsystemInput {
                evaluations: 100,
                denials: 5,
                ..Default::default()
            },
        );
        let dash = c.dashboard(1000);
        let denial_ind = dash.indicators.iter().find(|i| i.name == "denial_rate").unwrap();
        assert_eq!(denial_ind.status, HealthStatus::Healthy);
        assert_eq!(denial_ind.value, "5%");
    }

    #[test]
    fn high_denial_rate_warning() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_subsystem(
            "policy",
            PolicySubsystemInput {
                evaluations: 100,
                denials: 15,
                ..Default::default()
            },
        );
        let dash = c.dashboard(1000);
        let denial_ind = dash.indicators.iter().find(|i| i.name == "denial_rate").unwrap();
        assert_eq!(denial_ind.status, HealthStatus::Warning);
    }

    #[test]
    fn very_high_denial_rate_critical() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_subsystem(
            "policy",
            PolicySubsystemInput {
                evaluations: 100,
                denials: 30,
                ..Default::default()
            },
        );
        let dash = c.dashboard(1000);
        let denial_ind = dash.indicators.iter().find(|i| i.name == "denial_rate").unwrap();
        assert_eq!(denial_ind.status, HealthStatus::Critical);
    }

    #[test]
    fn quarantine_density_thresholds() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_subsystem(
            "connectors",
            PolicySubsystemInput {
                active_quarantines: 5,
                ..Default::default()
            },
        );
        let dash = c.dashboard(1000);
        let q_ind = dash
            .indicators
            .iter()
            .find(|i| i.name == "quarantine_density")
            .unwrap();
        assert_eq!(q_ind.status, HealthStatus::Warning);
    }

    #[test]
    fn kill_switch_active_is_critical() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_kill_switch(true);
        let dash = c.dashboard(1000);
        let ks_ind = dash.indicators.iter().find(|i| i.name == "kill_switch").unwrap();
        assert_eq!(ks_ind.status, HealthStatus::Critical);
        assert_eq!(ks_ind.value, "ACTIVE");
    }

    #[test]
    fn audit_chain_invalid_is_critical() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_audit_chain(100, false);
        let dash = c.dashboard(1000);
        let chain_ind = dash
            .indicators
            .iter()
            .find(|i| i.name == "audit_chain_integrity")
            .unwrap();
        assert_eq!(chain_ind.status, HealthStatus::Critical);
        assert_eq!(chain_ind.value, "INVALID");
    }

    #[test]
    fn overall_health_worst_of_indicators() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        // Everything healthy except kill switch
        c.update_kill_switch(true);
        let dash = c.dashboard(1000);
        assert_eq!(dash.overall_health, HealthStatus::Critical);
    }

    #[test]
    fn multiple_subsystems() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_subsystem(
            "policy",
            PolicySubsystemInput {
                evaluations: 100,
                denials: 5,
                active_quarantines: 0,
                active_violations: 0,
            },
        );
        c.update_subsystem(
            "connectors",
            PolicySubsystemInput {
                evaluations: 50,
                denials: 20,
                active_quarantines: 2,
                active_violations: 1,
            },
        );

        let dash = c.dashboard(1000);
        assert_eq!(dash.subsystem_metrics.len(), 2);
        assert_eq!(dash.counters.total_evaluations, 150);
        assert_eq!(dash.counters.total_denials, 25);
        let conn = &dash.subsystem_metrics["connectors"];
        assert_eq!(conn.denial_rate_pct, 40);
        assert_eq!(conn.health, HealthStatus::Critical);
    }

    #[test]
    fn time_series_basic() {
        let mut ts = MetricTimeSeries::new("test", MetricUnit::Count, 5);
        assert!(ts.is_empty());
        ts.push(1000, 10);
        ts.push(2000, 20);
        ts.push(3000, 30);
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.average(), Some(20));
        assert_eq!(ts.max(), Some(30));
        assert_eq!(ts.min(), Some(10));
    }

    #[test]
    fn time_series_eviction() {
        let mut ts = MetricTimeSeries::new("test", MetricUnit::Count, 3);
        for i in 0..5 {
            ts.push(i * 1000, i);
        }
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.latest().unwrap().value, 4);
    }

    #[test]
    fn time_series_range_query() {
        let mut ts = MetricTimeSeries::new("test", MetricUnit::Count, 100);
        ts.push(1000, 10);
        ts.push(2000, 20);
        ts.push(3000, 30);

        let range = ts.samples_in_range(1500, 2500);
        assert_eq!(range.len(), 1);
        assert_eq!(range[0].value, 20);
    }

    #[test]
    fn denial_rate_sampling() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_subsystem(
            "policy",
            PolicySubsystemInput {
                evaluations: 100,
                denials: 10,
                ..Default::default()
            },
        );
        c.sample_denial_rate(1000);
        assert_eq!(c.denial_rate_series().latest().unwrap().value, 10);

        c.update_subsystem(
            "policy",
            PolicySubsystemInput {
                evaluations: 200,
                denials: 40,
                ..Default::default()
            },
        );
        c.sample_denial_rate(2000);
        assert_eq!(c.denial_rate_series().latest().unwrap().value, 20);
    }

    #[test]
    fn quarantine_sampling() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_subsystem(
            "a",
            PolicySubsystemInput {
                active_quarantines: 3,
                ..Default::default()
            },
        );
        c.update_subsystem(
            "b",
            PolicySubsystemInput {
                active_quarantines: 2,
                ..Default::default()
            },
        );
        c.sample_quarantine_count(1000);
        assert_eq!(c.quarantine_series().latest().unwrap().value, 5);
    }

    #[test]
    fn forensic_count_tracked() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_forensic_count(42);
        let dash = c.dashboard(1000);
        assert_eq!(dash.counters.forensic_records_count, 42);
    }

    #[test]
    fn dashboard_serde_roundtrip() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_subsystem(
            "policy",
            PolicySubsystemInput {
                evaluations: 100,
                denials: 10,
                active_quarantines: 1,
                active_violations: 2,
            },
        );
        c.update_audit_chain(50, true);
        c.update_forensic_count(25);

        let dash = c.dashboard(1000);
        let json = serde_json::to_string(&dash).unwrap();
        let back: PolicyMetricsDashboard = serde_json::from_str(&json).unwrap();
        assert_eq!(dash.captured_at_ms, back.captured_at_ms);
        assert_eq!(dash.overall_health, back.overall_health);
        assert_eq!(dash.counters, back.counters);
    }

    #[test]
    fn health_status_ordering() {
        assert!(HealthStatus::Healthy < HealthStatus::Warning);
        assert!(HealthStatus::Warning < HealthStatus::Critical);
        assert!(HealthStatus::Critical < HealthStatus::Unknown);
    }

    #[test]
    fn health_status_display() {
        assert_eq!(HealthStatus::Healthy.to_string(), "healthy");
        assert_eq!(HealthStatus::Warning.to_string(), "warning");
        assert_eq!(HealthStatus::Critical.to_string(), "critical");
        assert_eq!(HealthStatus::Unknown.to_string(), "unknown");
    }

    #[test]
    fn metric_unit_display() {
        assert_eq!(MetricUnit::Count.to_string(), "count");
        assert_eq!(MetricUnit::Milliseconds.to_string(), "ms");
        assert_eq!(MetricUnit::Percentage.to_string(), "%");
        assert_eq!(MetricUnit::BytesPerSecond.to_string(), "B/s");
    }

    #[test]
    fn counters_serde_roundtrip() {
        let counters = PolicyMetricsCounters {
            total_evaluations: 100,
            total_denials: 10,
            total_quarantines_active: 2,
            total_violations_active: 3,
            audit_chain_length: 50,
            audit_chain_valid: true,
            forensic_records_count: 25,
            kill_switch_active: false,
            snapshots_generated: 5,
        };
        let json = serde_json::to_string(&counters).unwrap();
        let back: PolicyMetricsCounters = serde_json::from_str(&json).unwrap();
        assert_eq!(counters, back);
    }

    #[test]
    fn thresholds_serde_roundtrip() {
        let t = PolicyMetricsThresholds::default();
        let json = serde_json::to_string(&t).unwrap();
        let back: PolicyMetricsThresholds = serde_json::from_str(&json).unwrap();
        assert_eq!(t.denial_rate_warning_pct, back.denial_rate_warning_pct);
    }

    #[test]
    fn snapshots_generated_increments() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.dashboard(1000);
        c.dashboard(2000);
        let dash = c.dashboard(3000);
        assert_eq!(dash.counters.snapshots_generated, 3);
    }

    #[test]
    fn subsystem_summary_serde_roundtrip() {
        let summary = SubsystemMetricSummary {
            subsystem: "policy".to_string(),
            health: HealthStatus::Warning,
            evaluations: 100,
            denials: 15,
            denial_rate_pct: 15,
            active_quarantines: 2,
            active_violations: 3,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: SubsystemMetricSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(summary, back);
    }

    #[test]
    fn zero_evaluations_zero_denial_rate() {
        let mut c = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        c.update_subsystem(
            "policy",
            PolicySubsystemInput {
                evaluations: 0,
                denials: 0,
                ..Default::default()
            },
        );
        let dash = c.dashboard(1000);
        let denial_ind = dash.indicators.iter().find(|i| i.name == "denial_rate").unwrap();
        assert_eq!(denial_ind.value, "0%");
        assert_eq!(denial_ind.status, HealthStatus::Healthy);
    }
}
