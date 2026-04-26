//! Replay performance budgets and regression evaluation.
//!
//! Bead: ft-og6q6.7.3
//!
//! This module centralizes the replay performance contract:
//! - Absolute budgets for capture/replay/diff/report/artifact-read metrics
//! - Baseline-vs-current regression classification (warning/blocking)
//! - Machine-readable report structures for CI and E2E harnesses

use serde::{Deserialize, Serialize};

pub const REPLAY_PERF_REPORT_VERSION: &str = "1";
pub const REPLAY_PERF_REPORT_FORMAT: &str = "ft-replay-performance-report";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPerformanceMetric {
    CaptureOverheadMsPerEvent,
    ReplayThroughputEventsPerSec,
    DiffLatencyMsPer1000Divergences,
    ReportGenerationMs,
    ArtifactReadEventsPerSec,
}

impl ReplayPerformanceMetric {
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::CaptureOverheadMsPerEvent => "capture_overhead_ms",
            Self::ReplayThroughputEventsPerSec => "replay_throughput_eps",
            Self::DiffLatencyMsPer1000Divergences => "diff_latency_ms",
            Self::ReportGenerationMs => "report_generation_ms",
            Self::ArtifactReadEventsPerSec => "artifact_read_eps",
        }
    }

    #[must_use]
    pub fn lower_is_better(self) -> bool {
        matches!(
            self,
            Self::CaptureOverheadMsPerEvent
                | Self::DiffLatencyMsPer1000Divergences
                | Self::ReportGenerationMs
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReplayPerformanceSample {
    pub capture_overhead_ms_per_event: f64,
    pub replay_throughput_events_per_sec: f64,
    pub diff_latency_ms_per_1000_divergences: f64,
    pub report_generation_ms: f64,
    pub artifact_read_events_per_sec: f64,
}

impl ReplayPerformanceSample {
    #[must_use]
    pub fn value_for(self, metric: ReplayPerformanceMetric) -> f64 {
        match metric {
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent => {
                self.capture_overhead_ms_per_event
            }
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec => {
                self.replay_throughput_events_per_sec
            }
            ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences => {
                self.diff_latency_ms_per_1000_divergences
            }
            ReplayPerformanceMetric::ReportGenerationMs => self.report_generation_ms,
            ReplayPerformanceMetric::ArtifactReadEventsPerSec => self.artifact_read_events_per_sec,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayPerformanceBudgets {
    pub capture_overhead_ms_per_event: f64,
    pub replay_throughput_events_per_sec: f64,
    pub diff_latency_ms_per_1000_divergences: f64,
    pub report_generation_ms: f64,
    pub artifact_read_events_per_sec: f64,
    /// Fractional regression threshold (0.10 = 10%).
    pub warning_regression_fraction: f64,
    /// Fractional regression threshold (0.25 = 25%).
    pub blocking_regression_fraction: f64,
}

impl Default for ReplayPerformanceBudgets {
    fn default() -> Self {
        Self {
            capture_overhead_ms_per_event: 1.0,
            replay_throughput_events_per_sec: 100_000.0,
            diff_latency_ms_per_1000_divergences: 1_000.0,
            report_generation_ms: 100.0,
            artifact_read_events_per_sec: 500_000.0,
            warning_regression_fraction: 0.10,
            blocking_regression_fraction: 0.25,
        }
    }
}

impl ReplayPerformanceBudgets {
    #[must_use]
    pub fn value_for(&self, metric: ReplayPerformanceMetric) -> f64 {
        match metric {
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent => {
                self.capture_overhead_ms_per_event
            }
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec => {
                self.replay_throughput_events_per_sec
            }
            ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences => {
                self.diff_latency_ms_per_1000_divergences
            }
            ReplayPerformanceMetric::ReportGenerationMs => self.report_generation_ms,
            ReplayPerformanceMetric::ArtifactReadEventsPerSec => self.artifact_read_events_per_sec,
        }
    }

    #[must_use]
    pub fn within_budget(&self, metric: ReplayPerformanceMetric, value: f64) -> bool {
        let budget = self.value_for(metric);
        if metric.lower_is_better() {
            value <= budget
        } else {
            value >= budget
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayPerformanceBaseline {
    pub version: String,
    pub source: String,
    pub generated_at: String,
    pub sample: ReplayPerformanceSample,
}

impl ReplayPerformanceBaseline {
    #[must_use]
    pub fn from_sample(
        source: impl Into<String>,
        generated_at: impl Into<String>,
        sample: ReplayPerformanceSample,
    ) -> Self {
        Self {
            version: REPLAY_PERF_REPORT_VERSION.to_string(),
            source: source.into(),
            generated_at: generated_at.into(),
            sample,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPerformanceStatus {
    Pass,
    Improvement,
    Warning,
    Blocking,
}

impl ReplayPerformanceStatus {
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Improvement => 1,
            Self::Warning => 2,
            Self::Blocking => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayPerformanceMetricResult {
    pub metric: ReplayPerformanceMetric,
    pub metric_key: String,
    pub value: f64,
    pub budget: f64,
    pub within_budget: bool,
    pub baseline: Option<f64>,
    pub regression_fraction: Option<f64>,
    pub regression_percent: Option<f64>,
    pub status: ReplayPerformanceStatus,
    pub reason_code: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayCapacityGuidance {
    pub replay_seconds_for_1m_events: f64,
    pub replay_seconds_for_10m_events: f64,
    pub artifact_read_seconds_for_1m_events: f64,
    pub artifact_read_seconds_for_10m_events: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayPerformanceReport {
    pub version: String,
    pub format: String,
    pub budgets: ReplayPerformanceBudgets,
    pub baseline: Option<ReplayPerformanceBaseline>,
    pub sample: ReplayPerformanceSample,
    pub metrics: Vec<ReplayPerformanceMetricResult>,
    pub warning_count: usize,
    pub blocking_count: usize,
    pub overall_status: ReplayPerformanceStatus,
    pub capacity_guidance: ReplayCapacityGuidance,
}

fn all_metrics() -> [ReplayPerformanceMetric; 5] {
    [
        ReplayPerformanceMetric::CaptureOverheadMsPerEvent,
        ReplayPerformanceMetric::ReplayThroughputEventsPerSec,
        ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences,
        ReplayPerformanceMetric::ReportGenerationMs,
        ReplayPerformanceMetric::ArtifactReadEventsPerSec,
    ]
}

#[must_use]
pub fn regression_fraction(
    metric: ReplayPerformanceMetric,
    baseline: f64,
    current: f64,
) -> Option<f64> {
    if !baseline.is_finite() || baseline <= 0.0 || !current.is_finite() {
        return None;
    }

    let fraction = if metric.lower_is_better() {
        (current - baseline) / baseline
    } else {
        (baseline - current) / baseline
    };
    Some(fraction)
}

#[must_use]
pub fn classify_metric_result(
    budgets: &ReplayPerformanceBudgets,
    metric: ReplayPerformanceMetric,
    current_value: f64,
    baseline_value: Option<f64>,
) -> ReplayPerformanceMetricResult {
    let budget = budgets.value_for(metric);
    let within_budget = budgets.within_budget(metric, current_value);

    let regression_fraction =
        baseline_value.and_then(|baseline| regression_fraction(metric, baseline, current_value));
    let regression_percent = regression_fraction.map(|v| v * 100.0);

    let (status, reason_code) = if !within_budget {
        (ReplayPerformanceStatus::Blocking, "budget_exceeded")
    } else {
        match regression_fraction {
            Some(fraction) if fraction > budgets.blocking_regression_fraction => {
                (ReplayPerformanceStatus::Blocking, "regression_blocking")
            }
            Some(fraction) if fraction > budgets.warning_regression_fraction => {
                (ReplayPerformanceStatus::Warning, "regression_warning")
            }
            Some(fraction) if fraction < 0.0 => (
                ReplayPerformanceStatus::Improvement,
                "regression_improvement",
            ),
            Some(_) => (ReplayPerformanceStatus::Pass, "regression_within_tolerance"),
            None => (ReplayPerformanceStatus::Pass, "baseline_missing_or_invalid"),
        }
    };

    ReplayPerformanceMetricResult {
        metric,
        metric_key: metric.key().to_string(),
        value: current_value,
        budget,
        within_budget,
        baseline: baseline_value,
        regression_fraction,
        regression_percent,
        status,
        reason_code: reason_code.to_string(),
    }
}

#[must_use]
pub fn capacity_guidance(sample: ReplayPerformanceSample) -> ReplayCapacityGuidance {
    fn seconds(events: f64, eps: f64) -> f64 {
        if eps <= 0.0 || !eps.is_finite() {
            return f64::INFINITY;
        }
        events / eps
    }

    ReplayCapacityGuidance {
        replay_seconds_for_1m_events: seconds(1_000_000.0, sample.replay_throughput_events_per_sec),
        replay_seconds_for_10m_events: seconds(
            10_000_000.0,
            sample.replay_throughput_events_per_sec,
        ),
        artifact_read_seconds_for_1m_events: seconds(
            1_000_000.0,
            sample.artifact_read_events_per_sec,
        ),
        artifact_read_seconds_for_10m_events: seconds(
            10_000_000.0,
            sample.artifact_read_events_per_sec,
        ),
    }
}

#[must_use]
pub fn compare_against_baseline(
    budgets: ReplayPerformanceBudgets,
    baseline: Option<ReplayPerformanceBaseline>,
    sample: ReplayPerformanceSample,
) -> ReplayPerformanceReport {
    let baseline_sample = baseline.as_ref().map(|b| b.sample);
    let mut warning_count = 0usize;
    let mut blocking_count = 0usize;
    let mut overall = ReplayPerformanceStatus::Pass;

    let mut metrics = Vec::with_capacity(5);
    for metric in all_metrics() {
        let current_value = sample.value_for(metric);
        let baseline_value = baseline_sample.map(|s| s.value_for(metric));
        let row = classify_metric_result(&budgets, metric, current_value, baseline_value);
        match row.status {
            ReplayPerformanceStatus::Warning => warning_count += 1,
            ReplayPerformanceStatus::Blocking => blocking_count += 1,
            ReplayPerformanceStatus::Pass | ReplayPerformanceStatus::Improvement => {}
        }
        if row.status.rank() > overall.rank() {
            overall = row.status;
        }
        metrics.push(row);
    }

    ReplayPerformanceReport {
        version: REPLAY_PERF_REPORT_VERSION.to_string(),
        format: REPLAY_PERF_REPORT_FORMAT.to_string(),
        budgets,
        baseline,
        sample,
        metrics,
        warning_count,
        blocking_count,
        overall_status: overall,
        capacity_guidance: capacity_guidance(sample),
    }
}

/// Returns true when run-to-run spread is within `max_relative_spread`.
///
/// Spread is defined as `(max - min) / mean`.
#[must_use]
pub fn runs_within_relative_spread(values: &[f64], max_relative_spread: f64) -> bool {
    if values.is_empty() || max_relative_spread < 0.0 {
        return false;
    }
    if values.len() == 1 {
        return true;
    }

    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut sum = 0.0;
    let mut count = 0.0;

    for value in values {
        if !value.is_finite() {
            return false;
        }
        min = min.min(*value);
        max = max.max(*value);
        sum += *value;
        count += 1.0;
    }

    if count == 0.0 {
        return false;
    }

    let mean = sum / count;
    if mean == 0.0 {
        return (min - max).abs() < f64::EPSILON;
    }

    let spread = (max - min) / mean;
    spread <= max_relative_spread
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "left={a}, right={b}");
    }

    fn sample() -> ReplayPerformanceSample {
        ReplayPerformanceSample {
            capture_overhead_ms_per_event: 0.50,
            replay_throughput_events_per_sec: 200_000.0,
            diff_latency_ms_per_1000_divergences: 500.0,
            report_generation_ms: 20.0,
            artifact_read_events_per_sec: 1_000_000.0,
        }
    }

    #[test]
    fn default_budget_capture_value() {
        near(
            ReplayPerformanceBudgets::default().capture_overhead_ms_per_event,
            1.0,
        );
    }

    #[test]
    fn default_budget_replay_value() {
        near(
            ReplayPerformanceBudgets::default().replay_throughput_events_per_sec,
            100_000.0,
        );
    }

    #[test]
    fn default_budget_diff_value() {
        near(
            ReplayPerformanceBudgets::default().diff_latency_ms_per_1000_divergences,
            1_000.0,
        );
    }

    #[test]
    fn default_budget_report_generation_value() {
        near(
            ReplayPerformanceBudgets::default().report_generation_ms,
            100.0,
        );
    }

    #[test]
    fn default_budget_artifact_read_value() {
        near(
            ReplayPerformanceBudgets::default().artifact_read_events_per_sec,
            500_000.0,
        );
    }

    #[test]
    fn metric_key_names_stable() {
        assert_eq!(
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent.key(),
            "capture_overhead_ms"
        );
        assert_eq!(
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec.key(),
            "replay_throughput_eps"
        );
        assert_eq!(
            ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences.key(),
            "diff_latency_ms"
        );
    }

    #[test]
    fn within_budget_capture_boundary_passes() {
        let budgets = ReplayPerformanceBudgets::default();
        assert!(budgets.within_budget(ReplayPerformanceMetric::CaptureOverheadMsPerEvent, 1.0));
    }

    #[test]
    fn within_budget_capture_violation_fails() {
        let budgets = ReplayPerformanceBudgets::default();
        assert!(!budgets.within_budget(ReplayPerformanceMetric::CaptureOverheadMsPerEvent, 1.01));
    }

    #[test]
    fn within_budget_replay_boundary_passes() {
        let budgets = ReplayPerformanceBudgets::default();
        assert!(budgets.within_budget(
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec,
            100_000.0,
        ));
    }

    #[test]
    fn within_budget_replay_violation_fails() {
        let budgets = ReplayPerformanceBudgets::default();
        assert!(!budgets.within_budget(
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec,
            99_999.0,
        ));
    }

    #[test]
    fn within_budget_diff_boundary_passes() {
        let budgets = ReplayPerformanceBudgets::default();
        assert!(budgets.within_budget(
            ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences,
            1_000.0,
        ));
    }

    #[test]
    fn within_budget_diff_violation_fails() {
        let budgets = ReplayPerformanceBudgets::default();
        assert!(!budgets.within_budget(
            ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences,
            1_100.0,
        ));
    }

    #[test]
    fn within_budget_report_boundary_passes() {
        let budgets = ReplayPerformanceBudgets::default();
        assert!(budgets.within_budget(ReplayPerformanceMetric::ReportGenerationMs, 100.0));
    }

    #[test]
    fn within_budget_report_violation_fails() {
        let budgets = ReplayPerformanceBudgets::default();
        assert!(!budgets.within_budget(ReplayPerformanceMetric::ReportGenerationMs, 120.0));
    }

    #[test]
    fn within_budget_artifact_boundary_passes() {
        let budgets = ReplayPerformanceBudgets::default();
        assert!(
            budgets.within_budget(ReplayPerformanceMetric::ArtifactReadEventsPerSec, 500_000.0,)
        );
    }

    #[test]
    fn within_budget_artifact_violation_fails() {
        let budgets = ReplayPerformanceBudgets::default();
        assert!(
            !budgets.within_budget(ReplayPerformanceMetric::ArtifactReadEventsPerSec, 499_999.0,)
        );
    }

    #[test]
    fn regression_fraction_lower_is_better_worse_is_positive() {
        let value = regression_fraction(
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent,
            1.0,
            1.25,
        )
        .unwrap();
        near(value, 0.25);
    }

    #[test]
    fn regression_fraction_lower_is_better_improvement_is_negative() {
        let value = regression_fraction(
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent,
            1.0,
            0.80,
        )
        .unwrap();
        near(value, -0.20);
    }

    #[test]
    fn regression_fraction_higher_is_better_worse_is_positive() {
        let value = regression_fraction(
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec,
            100_000.0,
            80_000.0,
        )
        .unwrap();
        near(value, 0.20);
    }

    #[test]
    fn regression_fraction_higher_is_better_improvement_is_negative() {
        let value = regression_fraction(
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec,
            100_000.0,
            125_000.0,
        )
        .unwrap();
        near(value, -0.25);
    }

    #[test]
    fn regression_fraction_zero_baseline_returns_none() {
        assert!(
            regression_fraction(
                ReplayPerformanceMetric::ReplayThroughputEventsPerSec,
                0.0,
                1.0,
            )
            .is_none()
        );
    }

    #[test]
    fn regression_fraction_non_finite_current_returns_none() {
        assert!(
            regression_fraction(
                ReplayPerformanceMetric::ReplayThroughputEventsPerSec,
                1.0,
                f64::NAN,
            )
            .is_none()
        );
    }

    #[test]
    fn classify_metric_warning_threshold_is_exclusive() {
        let mut budgets = ReplayPerformanceBudgets::default();
        budgets.warning_regression_fraction = 0.10;
        budgets.blocking_regression_fraction = 0.25;

        // Use values within budget (lower-is-better, budget=1.0).
        // Regression = (0.88 - 0.80) / 0.80 = 0.10, exactly at threshold.
        // The `>` comparison means exactly-at-threshold is Pass, not Warning.
        let row = classify_metric_result(
            &budgets,
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent,
            0.88,
            Some(0.80),
        );
        assert_eq!(row.status, ReplayPerformanceStatus::Pass);
        assert_eq!(row.reason_code, "regression_within_tolerance");
    }

    #[test]
    fn classify_metric_warning_when_regression_above_10_percent() {
        let budgets = ReplayPerformanceBudgets::default();
        let row = classify_metric_result(
            &budgets,
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent,
            0.56,
            Some(0.50),
        );
        assert_eq!(row.status, ReplayPerformanceStatus::Warning);
        assert_eq!(row.reason_code, "regression_warning");
    }

    #[test]
    fn classify_metric_blocking_when_regression_above_25_percent() {
        let budgets = ReplayPerformanceBudgets::default();
        // Use values within budget (higher-is-better, budget=100K).
        // current=100K passes budget (100K >= 100K). baseline=140K gives
        // regression = (140K - 100K) / 140K ≈ 0.286 > 0.25 blocking threshold.
        let row = classify_metric_result(
            &budgets,
            ReplayPerformanceMetric::ReplayThroughputEventsPerSec,
            100_000.0,
            Some(140_000.0),
        );
        assert_eq!(row.status, ReplayPerformanceStatus::Blocking);
        assert_eq!(row.reason_code, "regression_blocking");
    }

    #[test]
    fn classify_metric_improvement_status() {
        let budgets = ReplayPerformanceBudgets::default();
        let row = classify_metric_result(
            &budgets,
            ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences,
            400.0,
            Some(500.0),
        );
        assert_eq!(row.status, ReplayPerformanceStatus::Improvement);
        assert_eq!(row.reason_code, "regression_improvement");
    }

    #[test]
    fn classify_metric_budget_violation_blocks_even_without_baseline() {
        let budgets = ReplayPerformanceBudgets::default();
        let row = classify_metric_result(
            &budgets,
            ReplayPerformanceMetric::DiffLatencyMsPer1000Divergences,
            1200.0,
            None,
        );
        assert_eq!(row.status, ReplayPerformanceStatus::Blocking);
        assert_eq!(row.reason_code, "budget_exceeded");
    }

    #[test]
    fn compare_report_counts_warning_and_blocking() {
        let budgets = ReplayPerformanceBudgets::default();
        let baseline = ReplayPerformanceBaseline::from_sample(
            "unit",
            "2026-02-24T00:00:00Z",
            ReplayPerformanceSample {
                capture_overhead_ms_per_event: 0.5,
                replay_throughput_events_per_sec: 200_000.0,
                diff_latency_ms_per_1000_divergences: 500.0,
                report_generation_ms: 20.0,
                artifact_read_events_per_sec: 1_000_000.0,
            },
        );
        let report = compare_against_baseline(
            budgets,
            Some(baseline),
            ReplayPerformanceSample {
                capture_overhead_ms_per_event: 0.56,
                replay_throughput_events_per_sec: 120_000.0,
                diff_latency_ms_per_1000_divergences: 520.0,
                report_generation_ms: 20.0,
                artifact_read_events_per_sec: 1_000_000.0,
            },
        );
        assert_eq!(report.warning_count, 1);
        assert_eq!(report.blocking_count, 1);
        assert_eq!(report.overall_status, ReplayPerformanceStatus::Blocking);
    }

    #[test]
    fn compare_report_overall_warning_without_blocking() {
        let budgets = ReplayPerformanceBudgets::default();
        let baseline =
            ReplayPerformanceBaseline::from_sample("unit", "2026-02-24T00:00:00Z", sample());
        let report = compare_against_baseline(
            budgets,
            Some(baseline),
            ReplayPerformanceSample {
                capture_overhead_ms_per_event: 0.56,
                ..sample()
            },
        );
        assert_eq!(report.blocking_count, 0);
        assert_eq!(report.warning_count, 1);
        assert_eq!(report.overall_status, ReplayPerformanceStatus::Warning);
    }

    #[test]
    fn compare_report_overall_improvement_when_all_improved() {
        let budgets = ReplayPerformanceBudgets::default();
        let baseline =
            ReplayPerformanceBaseline::from_sample("unit", "2026-02-24T00:00:00Z", sample());
        let report = compare_against_baseline(
            budgets,
            Some(baseline),
            ReplayPerformanceSample {
                capture_overhead_ms_per_event: 0.4,
                replay_throughput_events_per_sec: 250_000.0,
                diff_latency_ms_per_1000_divergences: 400.0,
                report_generation_ms: 10.0,
                artifact_read_events_per_sec: 1_200_000.0,
            },
        );
        assert_eq!(report.warning_count, 0);
        assert_eq!(report.blocking_count, 0);
        assert_eq!(report.overall_status, ReplayPerformanceStatus::Improvement);
    }

    #[test]
    fn compare_report_handles_missing_baseline() {
        let budgets = ReplayPerformanceBudgets::default();
        let report = compare_against_baseline(budgets, None, sample());
        assert_eq!(report.warning_count, 0);
        assert_eq!(report.blocking_count, 0);
        assert!(
            report
                .metrics
                .iter()
                .all(|row| row.reason_code == "baseline_missing_or_invalid")
        );
    }

    #[test]
    fn compare_report_generates_all_five_metrics() {
        let report = compare_against_baseline(ReplayPerformanceBudgets::default(), None, sample());
        assert_eq!(report.metrics.len(), 5);
    }

    #[test]
    fn baseline_roundtrip_json() {
        let baseline =
            ReplayPerformanceBaseline::from_sample("seed", "2026-02-24T00:00:00Z", sample());
        let json = serde_json::to_string(&baseline).unwrap();
        let decoded: ReplayPerformanceBaseline = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, baseline);
    }

    #[test]
    fn report_serializes_to_json() {
        let report = compare_against_baseline(ReplayPerformanceBudgets::default(), None, sample());
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(REPLAY_PERF_REPORT_FORMAT));
    }

    #[test]
    fn report_deserializes_from_json() {
        let report = compare_against_baseline(ReplayPerformanceBudgets::default(), None, sample());
        let json = serde_json::to_string(&report).unwrap();
        let decoded: ReplayPerformanceReport = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.version, REPLAY_PERF_REPORT_VERSION);
    }

    #[test]
    fn capacity_guidance_uses_replay_throughput() {
        let guidance = capacity_guidance(ReplayPerformanceSample {
            replay_throughput_events_per_sec: 200_000.0,
            artifact_read_events_per_sec: 1_000_000.0,
            ..sample()
        });
        near(guidance.replay_seconds_for_1m_events, 5.0);
        near(guidance.replay_seconds_for_10m_events, 50.0);
    }

    #[test]
    fn capacity_guidance_uses_artifact_read_throughput() {
        let guidance = capacity_guidance(ReplayPerformanceSample {
            replay_throughput_events_per_sec: 200_000.0,
            artifact_read_events_per_sec: 500_000.0,
            ..sample()
        });
        near(guidance.artifact_read_seconds_for_1m_events, 2.0);
        near(guidance.artifact_read_seconds_for_10m_events, 20.0);
    }

    #[test]
    fn capacity_guidance_infinite_for_non_positive_throughput() {
        let guidance = capacity_guidance(ReplayPerformanceSample {
            replay_throughput_events_per_sec: 0.0,
            artifact_read_events_per_sec: 0.0,
            ..sample()
        });
        assert!(guidance.replay_seconds_for_1m_events.is_infinite());
        assert!(guidance.artifact_read_seconds_for_1m_events.is_infinite());
    }

    #[test]
    fn runs_within_relative_spread_true_for_five_percent_window() {
        assert!(runs_within_relative_spread(&[100.0, 102.0, 98.0], 0.05));
    }

    #[test]
    fn runs_within_relative_spread_false_when_exceeding_five_percent() {
        assert!(!runs_within_relative_spread(&[100.0, 120.0, 80.0], 0.05));
    }

    #[test]
    fn runs_within_relative_spread_false_for_empty_input() {
        assert!(!runs_within_relative_spread(&[], 0.05));
    }

    #[test]
    fn runs_within_relative_spread_true_for_single_value() {
        assert!(runs_within_relative_spread(&[42.0], 0.05));
    }

    #[test]
    fn runs_within_relative_spread_false_for_non_finite_value() {
        assert!(!runs_within_relative_spread(&[42.0, f64::NAN], 0.05));
    }

    #[test]
    fn runs_within_relative_spread_negative_threshold_rejected() {
        assert!(!runs_within_relative_spread(&[1.0, 1.0], -0.1));
    }

    #[test]
    fn sample_value_for_metric_roundtrip() {
        let s = sample();
        near(
            s.value_for(ReplayPerformanceMetric::CaptureOverheadMsPerEvent),
            s.capture_overhead_ms_per_event,
        );
        near(
            s.value_for(ReplayPerformanceMetric::ReplayThroughputEventsPerSec),
            s.replay_throughput_events_per_sec,
        );
    }

    #[test]
    fn budget_value_for_metric_roundtrip() {
        let b = ReplayPerformanceBudgets::default();
        near(
            b.value_for(ReplayPerformanceMetric::ArtifactReadEventsPerSec),
            b.artifact_read_events_per_sec,
        );
        near(
            b.value_for(ReplayPerformanceMetric::ReportGenerationMs),
            b.report_generation_ms,
        );
    }

    #[test]
    fn metric_result_is_json_serializable() {
        let row = classify_metric_result(
            &ReplayPerformanceBudgets::default(),
            ReplayPerformanceMetric::CaptureOverheadMsPerEvent,
            0.5,
            Some(0.5),
        );
        let json = serde_json::to_string(&row).unwrap();
        assert!(json.contains("capture_overhead_ms"));
    }
}
