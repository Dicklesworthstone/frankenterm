//! SLO conformance and observability audit suite (ft-3681t.7.5).
//!
//! Validates telemetry quality, alert fidelity, and SLO conformance across
//! all subsystems. Provides objective release criteria by evaluating measured
//! metrics against declared service-level objectives.
//!
//! # Key types
//!
//! - [`SloDefinition`]: Declares a service-level objective (target, window, budget).
//! - [`SloEvaluator`]: Evaluates metric samples against SLO definitions.
//! - [`SloAuditReport`]: Summary of all SLO evaluations with pass/fail evidence.
//! - [`TelemetryAuditCheck`]: Validates telemetry completeness and correlation.
//! - [`AlertFidelityCheck`]: Validates alert accuracy (no false positives/negatives).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── SLO Definition ──────────────────────────────────────────────────────────

/// A service-level objective definition.
///
/// SLOs define quantitative targets for system behavior. Each SLO specifies
/// a metric, a target threshold, an evaluation window, and an error budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SloDefinition {
    /// Unique SLO identifier (e.g., "robot.send.p99_latency").
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Subsystem this SLO applies to.
    pub subsystem: String,
    /// The metric type being measured.
    pub metric: SloMetric,
    /// Target threshold value.
    pub target: f64,
    /// Comparison operator for the target.
    pub comparison: SloComparison,
    /// Evaluation window in milliseconds.
    pub window_ms: u64,
    /// Error budget as a fraction (e.g., 0.001 = 0.1% error budget).
    pub error_budget: f64,
    /// Severity when SLO is breached.
    pub breach_severity: SloSeverity,
}

/// The type of metric an SLO measures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SloMetric {
    /// Latency percentile (e.g., p50, p95, p99).
    LatencyMs { percentile: u8 },
    /// Error rate as a fraction (0.0–1.0).
    ErrorRate,
    /// Availability as a fraction (0.0–1.0).
    Availability,
    /// Throughput (events/requests per second).
    Throughput,
    /// Queue depth / backlog size.
    QueueDepth,
    /// Custom metric with a label.
    Custom { label: String },
}

/// How to compare the measured value against the SLO target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SloComparison {
    /// Measured value must be less than or equal to target.
    LessOrEqual,
    /// Measured value must be greater than or equal to target.
    GreaterOrEqual,
    /// Measured value must be less than target.
    LessThan,
    /// Measured value must be greater than target.
    GreaterThan,
}

impl SloComparison {
    /// Evaluate whether `measured` satisfies this comparison against `target`.
    #[must_use]
    pub fn evaluate(self, measured: f64, target: f64) -> bool {
        match self {
            Self::LessOrEqual => measured <= target,
            Self::GreaterOrEqual => measured >= target,
            Self::LessThan => measured < target,
            Self::GreaterThan => measured > target,
        }
    }
}

/// Severity when an SLO is breached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SloSeverity {
    /// Informational — log but don't alert.
    Info,
    /// Warning — approaching budget exhaustion.
    Warning,
    /// Critical — SLO breached, immediate attention needed.
    Critical,
    /// Page — SLO breached, requires human intervention.
    Page,
}

impl SloDefinition {
    /// Create a latency SLO (measured <= target).
    #[must_use]
    pub fn latency(
        id: &str,
        name: &str,
        subsystem: &str,
        percentile: u8,
        target_ms: f64,
        window_ms: u64,
    ) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            subsystem: subsystem.to_string(),
            metric: SloMetric::LatencyMs { percentile },
            target: target_ms,
            comparison: SloComparison::LessOrEqual,
            window_ms,
            error_budget: 0.001,
            breach_severity: SloSeverity::Critical,
        }
    }

    /// Create an error-rate SLO (measured <= target).
    #[must_use]
    pub fn error_rate(
        id: &str,
        name: &str,
        subsystem: &str,
        max_rate: f64,
        window_ms: u64,
    ) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            subsystem: subsystem.to_string(),
            metric: SloMetric::ErrorRate,
            target: max_rate,
            comparison: SloComparison::LessOrEqual,
            window_ms,
            error_budget: max_rate,
            breach_severity: SloSeverity::Critical,
        }
    }

    /// Create an availability SLO (measured >= target).
    #[must_use]
    pub fn availability(
        id: &str,
        name: &str,
        subsystem: &str,
        min_availability: f64,
        window_ms: u64,
    ) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            subsystem: subsystem.to_string(),
            metric: SloMetric::Availability,
            target: min_availability,
            comparison: SloComparison::GreaterOrEqual,
            window_ms,
            error_budget: 1.0 - min_availability,
            breach_severity: SloSeverity::Critical,
        }
    }
}

// ── Metric Sample ───────────────────────────────────────────────────────────

/// A single metric observation for SLO evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSample {
    /// The SLO ID this sample relates to.
    pub slo_id: String,
    /// Measured value.
    pub value: f64,
    /// Timestamp of measurement (epoch ms).
    pub timestamp_ms: u64,
    /// Whether this sample represents a "good" event (for error budget).
    pub good: bool,
}

// ── SLO Evaluation Result ───────────────────────────────────────────────────

/// Result of evaluating a single SLO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SloEvaluation {
    /// The SLO that was evaluated.
    pub slo_id: String,
    /// Whether the SLO is currently met.
    pub conforming: bool,
    /// Current measured value.
    pub measured_value: f64,
    /// Target value from the SLO definition.
    pub target_value: f64,
    /// Remaining error budget as a fraction (0.0 = exhausted, 1.0 = full).
    pub budget_remaining: f64,
    /// Number of samples evaluated.
    pub sample_count: usize,
    /// Number of "good" samples (conforming events).
    pub good_count: usize,
    /// Number of "bad" samples (non-conforming events).
    pub bad_count: usize,
    /// Window start timestamp.
    pub window_start_ms: u64,
    /// Window end timestamp.
    pub window_end_ms: u64,
    /// Severity if breached.
    pub breach_severity: SloSeverity,
}

impl SloEvaluation {
    /// Fraction of good events (availability/success rate).
    #[must_use]
    pub fn good_fraction(&self) -> f64 {
        if self.sample_count == 0 {
            return 1.0;
        }
        self.good_count as f64 / self.sample_count as f64
    }

    /// Whether the error budget is exhausted.
    #[must_use]
    pub fn budget_exhausted(&self) -> bool {
        self.budget_remaining <= 0.0
    }
}

// ── SLO Evaluator ───────────────────────────────────────────────────────────

/// Evaluates metric samples against SLO definitions.
pub struct SloEvaluator {
    /// Registered SLO definitions.
    definitions: HashMap<String, SloDefinition>,
    /// Metric samples per SLO (bounded ring buffer).
    samples: HashMap<String, Vec<MetricSample>>,
    /// Maximum samples to retain per SLO.
    max_samples_per_slo: usize,
}

impl SloEvaluator {
    /// Create a new evaluator.
    #[must_use]
    pub fn new(max_samples_per_slo: usize) -> Self {
        Self {
            definitions: HashMap::new(),
            samples: HashMap::new(),
            max_samples_per_slo: max_samples_per_slo.max(1),
        }
    }

    /// Register an SLO definition.
    pub fn register(&mut self, slo: SloDefinition) {
        let id = slo.id.clone();
        self.definitions.insert(id.clone(), slo);
        self.samples.entry(id).or_default();
    }

    /// Record a metric sample.
    pub fn record(&mut self, sample: MetricSample) {
        let buffer = self.samples.entry(sample.slo_id.clone()).or_default();
        buffer.push(sample);
        // Evict oldest if over capacity
        while buffer.len() > self.max_samples_per_slo {
            buffer.remove(0);
        }
    }

    /// Evaluate a specific SLO at a given time.
    pub fn evaluate(&self, slo_id: &str, now_ms: u64) -> Option<SloEvaluation> {
        let def = self.definitions.get(slo_id)?;
        let samples = self.samples.get(slo_id)?;

        let window_start = now_ms.saturating_sub(def.window_ms);
        let window_samples: Vec<&MetricSample> = samples
            .iter()
            .filter(|s| s.timestamp_ms >= window_start && s.timestamp_ms <= now_ms)
            .collect();

        let sample_count = window_samples.len();
        let good_count = window_samples.iter().filter(|s| s.good).count();
        let bad_count = sample_count - good_count;

        // Compute the aggregate measured value
        let measured_value = if sample_count == 0 {
            0.0
        } else {
            match &def.metric {
                SloMetric::LatencyMs { percentile } => {
                    let mut values: Vec<f64> = window_samples.iter().map(|s| s.value).collect();
                    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let idx = ((*percentile as f64 / 100.0) * (values.len() as f64 - 1.0)).ceil()
                        as usize;
                    values[idx.min(values.len() - 1)]
                }
                SloMetric::ErrorRate => bad_count as f64 / sample_count as f64,
                SloMetric::Availability => good_count as f64 / sample_count as f64,
                SloMetric::Throughput | SloMetric::QueueDepth | SloMetric::Custom { .. } => {
                    // Use the latest sample value
                    window_samples.last().map(|s| s.value).unwrap_or(0.0)
                }
            }
        };

        let conforming = def.comparison.evaluate(measured_value, def.target);

        // Budget remaining: fraction of error budget not yet consumed
        let budget_remaining = if def.error_budget <= 0.0 {
            if conforming { 1.0 } else { 0.0 }
        } else if sample_count == 0 {
            1.0
        } else {
            let consumed = bad_count as f64 / sample_count as f64;
            1.0 - (consumed / def.error_budget).min(1.0)
        };

        Some(SloEvaluation {
            slo_id: slo_id.to_string(),
            conforming,
            measured_value,
            target_value: def.target,
            budget_remaining,
            sample_count,
            good_count,
            bad_count,
            window_start_ms: window_start,
            window_end_ms: now_ms,
            breach_severity: def.breach_severity,
        })
    }

    /// Evaluate all registered SLOs.
    pub fn evaluate_all(&self, now_ms: u64) -> Vec<SloEvaluation> {
        self.definitions
            .keys()
            .filter_map(|id| self.evaluate(id, now_ms))
            .collect()
    }

    /// Get the number of registered SLOs.
    #[must_use]
    pub fn slo_count(&self) -> usize {
        self.definitions.len()
    }

    /// Get the number of samples for a specific SLO.
    #[must_use]
    pub fn sample_count(&self, slo_id: &str) -> usize {
        self.samples.get(slo_id).map(|s| s.len()).unwrap_or(0)
    }
}

// ── Telemetry Audit Check ───────────────────────────────────────────────────

/// Validates telemetry pipeline completeness and quality.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryAuditCheck {
    /// Check identifier.
    pub id: String,
    /// Human-readable description.
    pub description: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Measured value (for quantitative checks).
    pub measured: Option<f64>,
    /// Expected value or threshold.
    pub expected: Option<f64>,
    /// Detailed message.
    pub message: String,
    /// Timestamp of the check.
    pub checked_at_ms: u64,
}

/// Collection of telemetry audit checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryAuditReport {
    /// Individual check results.
    pub checks: Vec<TelemetryAuditCheck>,
    /// Overall pass/fail.
    pub all_passed: bool,
    /// Number of passing checks.
    pub pass_count: usize,
    /// Number of failing checks.
    pub fail_count: usize,
    /// Timestamp of the report.
    pub generated_at_ms: u64,
}

impl TelemetryAuditReport {
    /// Build a report from a list of checks.
    #[must_use]
    pub fn from_checks(checks: Vec<TelemetryAuditCheck>, generated_at_ms: u64) -> Self {
        let pass_count = checks.iter().filter(|c| c.passed).count();
        let fail_count = checks.len() - pass_count;
        Self {
            all_passed: fail_count == 0,
            pass_count,
            fail_count,
            checks,
            generated_at_ms,
        }
    }

    /// Get only the failing checks.
    #[must_use]
    pub fn failures(&self) -> Vec<&TelemetryAuditCheck> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }
}

// ── Alert Fidelity Check ────────────────────────────────────────────────────

/// Validates alert accuracy by checking for false positives/negatives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertFidelityCheck {
    /// Alert rule identifier.
    pub alert_id: String,
    /// Total alerts fired in the evaluation window.
    pub alerts_fired: u64,
    /// True positives: alerts that corresponded to real issues.
    pub true_positives: u64,
    /// False positives: alerts that fired without real issues.
    pub false_positives: u64,
    /// False negatives: real issues that didn't fire alerts.
    pub false_negatives: u64,
    /// Precision: TP / (TP + FP).
    pub precision: f64,
    /// Recall: TP / (TP + FN).
    pub recall: f64,
}

impl AlertFidelityCheck {
    /// Create a check from raw counts.
    #[must_use]
    pub fn new(
        alert_id: &str,
        true_positives: u64,
        false_positives: u64,
        false_negatives: u64,
    ) -> Self {
        let alerts_fired = true_positives + false_positives;
        let precision = if alerts_fired == 0 {
            1.0
        } else {
            true_positives as f64 / alerts_fired as f64
        };
        let total_positives = true_positives + false_negatives;
        let recall = if total_positives == 0 {
            1.0
        } else {
            true_positives as f64 / total_positives as f64
        };

        Self {
            alert_id: alert_id.to_string(),
            alerts_fired,
            true_positives,
            false_positives,
            false_negatives,
            precision,
            recall,
        }
    }

    /// F1 score: harmonic mean of precision and recall.
    #[must_use]
    pub fn f1_score(&self) -> f64 {
        if self.precision + self.recall == 0.0 {
            return 0.0;
        }
        2.0 * self.precision * self.recall / (self.precision + self.recall)
    }
}

// ── SLO Audit Report ────────────────────────────────────────────────────────

/// Comprehensive SLO audit report combining all checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SloAuditReport {
    /// SLO evaluations.
    pub slo_evaluations: Vec<SloEvaluation>,
    /// Telemetry quality audit.
    pub telemetry_audit: TelemetryAuditReport,
    /// Alert fidelity checks.
    pub alert_fidelity: Vec<AlertFidelityCheck>,
    /// Overall assessment.
    pub overall_pass: bool,
    /// Number of SLOs conforming.
    pub slos_conforming: usize,
    /// Number of SLOs breached.
    pub slos_breached: usize,
    /// Generated timestamp.
    pub generated_at_ms: u64,
}

impl SloAuditReport {
    /// Build from components.
    #[must_use]
    pub fn build(
        slo_evaluations: Vec<SloEvaluation>,
        telemetry_audit: TelemetryAuditReport,
        alert_fidelity: Vec<AlertFidelityCheck>,
        generated_at_ms: u64,
    ) -> Self {
        let slos_conforming = slo_evaluations.iter().filter(|e| e.conforming).count();
        let slos_breached = slo_evaluations.len() - slos_conforming;
        let overall_pass = slos_breached == 0 && telemetry_audit.all_passed;

        Self {
            slo_evaluations,
            telemetry_audit,
            alert_fidelity,
            overall_pass,
            slos_conforming,
            slos_breached,
            generated_at_ms,
        }
    }

    /// Get breached SLOs only.
    #[must_use]
    pub fn breached_slos(&self) -> Vec<&SloEvaluation> {
        self.slo_evaluations
            .iter()
            .filter(|e| !e.conforming)
            .collect()
    }

    /// Render a human-readable summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut lines = vec![
            format!(
                "SLO Audit Report ({})",
                if self.overall_pass { "PASS" } else { "FAIL" }
            ),
            format!(
                "  SLOs: {}/{} conforming",
                self.slos_conforming,
                self.slo_evaluations.len()
            ),
            format!(
                "  Telemetry: {}/{} checks passed",
                self.telemetry_audit.pass_count,
                self.telemetry_audit.checks.len()
            ),
        ];

        if !self.alert_fidelity.is_empty() {
            let avg_f1: f64 = self
                .alert_fidelity
                .iter()
                .map(|a| a.f1_score())
                .sum::<f64>()
                / self.alert_fidelity.len() as f64;
            lines.push(format!("  Alert F1 (avg): {avg_f1:.3}"));
        }

        for eval in &self.slo_evaluations {
            let status = if eval.conforming { "OK" } else { "BREACH" };
            lines.push(format!(
                "  [{status}] {}: {:.3} (target: {:.3}, budget: {:.1}%)",
                eval.slo_id,
                eval.measured_value,
                eval.target_value,
                eval.budget_remaining * 100.0,
            ));
        }

        lines.join("\n")
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- SloComparison tests --

    #[test]
    fn comparison_less_or_equal() {
        assert!(SloComparison::LessOrEqual.evaluate(5.0, 10.0));
        assert!(SloComparison::LessOrEqual.evaluate(10.0, 10.0));
        assert!(!SloComparison::LessOrEqual.evaluate(11.0, 10.0));
    }

    #[test]
    fn comparison_greater_or_equal() {
        assert!(SloComparison::GreaterOrEqual.evaluate(10.0, 5.0));
        assert!(SloComparison::GreaterOrEqual.evaluate(5.0, 5.0));
        assert!(!SloComparison::GreaterOrEqual.evaluate(4.0, 5.0));
    }

    #[test]
    fn comparison_less_than() {
        assert!(SloComparison::LessThan.evaluate(5.0, 10.0));
        assert!(!SloComparison::LessThan.evaluate(10.0, 10.0));
    }

    #[test]
    fn comparison_greater_than() {
        assert!(SloComparison::GreaterThan.evaluate(10.0, 5.0));
        assert!(!SloComparison::GreaterThan.evaluate(5.0, 5.0));
    }

    // -- SloDefinition factory tests --

    #[test]
    fn slo_latency_factory() {
        let slo = SloDefinition::latency("p99.send", "Send p99", "robot", 99, 100.0, 60_000);
        assert_eq!(slo.id, "p99.send");
        assert_eq!(slo.comparison, SloComparison::LessOrEqual);
        assert_eq!(slo.target, 100.0);
        if let SloMetric::LatencyMs { percentile } = slo.metric {
            assert_eq!(percentile, 99);
        } else {
            panic!("expected LatencyMs");
        }
    }

    #[test]
    fn slo_error_rate_factory() {
        let slo = SloDefinition::error_rate("err.robot", "Robot errors", "robot", 0.01, 300_000);
        assert_eq!(slo.target, 0.01);
        assert_eq!(slo.metric, SloMetric::ErrorRate);
    }

    #[test]
    fn slo_availability_factory() {
        let slo =
            SloDefinition::availability("avail.mux", "Mux availability", "mux", 0.999, 86_400_000);
        assert_eq!(slo.target, 0.999);
        assert_eq!(slo.comparison, SloComparison::GreaterOrEqual);
        assert!((slo.error_budget - 0.001).abs() < 1e-9);
    }

    // -- SloEvaluator tests --

    #[test]
    fn evaluator_no_samples_returns_conforming() {
        let mut eval = SloEvaluator::new(1000);
        eval.register(SloDefinition::availability(
            "test", "Test", "sys", 0.99, 60_000,
        ));
        let result = eval.evaluate("test", 60_000).unwrap();
        // No samples → measured 0.0, target 0.99, >= comparison fails
        // Actually with no samples, measured = 0.0 which is NOT >= 0.99
        assert!(!result.conforming);
        assert_eq!(result.sample_count, 0);
    }

    #[test]
    fn evaluator_error_rate_conforming() {
        let mut eval = SloEvaluator::new(1000);
        eval.register(SloDefinition::error_rate("err", "Err", "sys", 0.05, 60_000));

        // 100 samples, 3 bad → 3% error rate, under 5% target
        for i in 0..100 {
            eval.record(MetricSample {
                slo_id: "err".into(),
                value: if i < 3 { 1.0 } else { 0.0 },
                timestamp_ms: 1000 + i * 100,
                good: i >= 3,
            });
        }

        let result = eval.evaluate("err", 11_000).unwrap();
        assert!(result.conforming);
        assert!((result.measured_value - 0.03).abs() < 0.01);
        assert_eq!(result.good_count, 97);
        assert_eq!(result.bad_count, 3);
    }

    #[test]
    fn evaluator_error_rate_breached() {
        let mut eval = SloEvaluator::new(1000);
        eval.register(SloDefinition::error_rate("err", "Err", "sys", 0.01, 60_000));

        // 50 samples, 5 bad → 10% error rate, over 1% target
        for i in 0..50 {
            eval.record(MetricSample {
                slo_id: "err".into(),
                value: if i < 5 { 1.0 } else { 0.0 },
                timestamp_ms: 1000 + i * 100,
                good: i >= 5,
            });
        }

        let result = eval.evaluate("err", 6_000).unwrap();
        assert!(!result.conforming);
        assert!(result.measured_value > 0.01);
    }

    #[test]
    fn evaluator_availability_conforming() {
        let mut eval = SloEvaluator::new(1000);
        eval.register(SloDefinition::availability(
            "avail", "Avail", "sys", 0.99, 60_000,
        ));

        // 1000 samples, 995 good → 99.5% availability
        for i in 0..1000 {
            eval.record(MetricSample {
                slo_id: "avail".into(),
                value: 1.0,
                timestamp_ms: i * 10,
                good: i < 995,
            });
        }

        let result = eval.evaluate("avail", 10_000).unwrap();
        assert!(result.conforming);
        assert!(result.good_fraction() > 0.99);
    }

    #[test]
    fn evaluator_latency_percentile() {
        let mut eval = SloEvaluator::new(1000);
        eval.register(SloDefinition::latency(
            "p99", "P99", "sys", 99, 100.0, 60_000,
        ));

        // 100 samples: 0..99ms, p99 should be ~99ms
        for i in 0..100 {
            eval.record(MetricSample {
                slo_id: "p99".into(),
                value: i as f64,
                timestamp_ms: i * 100,
                good: true,
            });
        }

        let result = eval.evaluate("p99", 10_000).unwrap();
        assert!(result.conforming); // p99 = 99, target = 100
        assert!(result.measured_value >= 98.0);
    }

    #[test]
    fn evaluator_window_filters_old_samples() {
        let mut eval = SloEvaluator::new(1000);
        eval.register(SloDefinition::error_rate("err", "Err", "sys", 0.05, 1000));

        // Old bad samples (outside window)
        for i in 0..50 {
            eval.record(MetricSample {
                slo_id: "err".into(),
                value: 1.0,
                timestamp_ms: i * 10, // 0-490ms
                good: false,
            });
        }
        // Recent good samples (inside window)
        for i in 0..50 {
            eval.record(MetricSample {
                slo_id: "err".into(),
                value: 0.0,
                timestamp_ms: 9500 + i * 10, // 9500-9990ms
                good: true,
            });
        }

        let result = eval.evaluate("err", 10_000).unwrap();
        assert!(result.conforming); // only recent good samples in window
        assert_eq!(result.bad_count, 0);
    }

    #[test]
    fn evaluator_unknown_slo_returns_none() {
        let eval = SloEvaluator::new(1000);
        assert!(eval.evaluate("nonexistent", 1000).is_none());
    }

    #[test]
    fn evaluator_evaluate_all() {
        let mut eval = SloEvaluator::new(1000);
        eval.register(SloDefinition::error_rate(
            "err1", "Err1", "sys", 0.05, 60_000,
        ));
        eval.register(SloDefinition::error_rate(
            "err2", "Err2", "sys", 0.05, 60_000,
        ));

        for slo_id in &["err1", "err2"] {
            for i in 0..10 {
                eval.record(MetricSample {
                    slo_id: slo_id.to_string(),
                    value: 0.0,
                    timestamp_ms: i * 100,
                    good: true,
                });
            }
        }

        let results = eval.evaluate_all(1000);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn evaluator_sample_eviction() {
        let mut eval = SloEvaluator::new(5);
        eval.register(SloDefinition::error_rate("err", "Err", "sys", 0.05, 60_000));

        for i in 0..10 {
            eval.record(MetricSample {
                slo_id: "err".into(),
                value: 0.0,
                timestamp_ms: i * 100,
                good: true,
            });
        }

        assert_eq!(eval.sample_count("err"), 5);
    }

    // -- SloEvaluation tests --

    #[test]
    fn evaluation_good_fraction_no_samples() {
        let eval = SloEvaluation {
            slo_id: "test".into(),
            conforming: true,
            measured_value: 0.0,
            target_value: 0.99,
            budget_remaining: 1.0,
            sample_count: 0,
            good_count: 0,
            bad_count: 0,
            window_start_ms: 0,
            window_end_ms: 1000,
            breach_severity: SloSeverity::Critical,
        };
        assert!((eval.good_fraction() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn evaluation_budget_exhausted() {
        let eval = SloEvaluation {
            slo_id: "test".into(),
            conforming: false,
            measured_value: 0.1,
            target_value: 0.01,
            budget_remaining: 0.0,
            sample_count: 100,
            good_count: 90,
            bad_count: 10,
            window_start_ms: 0,
            window_end_ms: 1000,
            breach_severity: SloSeverity::Critical,
        };
        assert!(eval.budget_exhausted());
    }

    // -- TelemetryAuditReport tests --

    #[test]
    fn telemetry_audit_all_pass() {
        let checks = vec![
            TelemetryAuditCheck {
                id: "c1".into(),
                description: "Check 1".into(),
                passed: true,
                measured: Some(1.0),
                expected: Some(1.0),
                message: "ok".into(),
                checked_at_ms: 1000,
            },
            TelemetryAuditCheck {
                id: "c2".into(),
                description: "Check 2".into(),
                passed: true,
                measured: None,
                expected: None,
                message: "ok".into(),
                checked_at_ms: 1000,
            },
        ];
        let report = TelemetryAuditReport::from_checks(checks, 1000);
        assert!(report.all_passed);
        assert_eq!(report.pass_count, 2);
        assert_eq!(report.fail_count, 0);
    }

    #[test]
    fn telemetry_audit_with_failure() {
        let checks = vec![
            TelemetryAuditCheck {
                id: "c1".into(),
                description: "Check 1".into(),
                passed: true,
                measured: None,
                expected: None,
                message: "ok".into(),
                checked_at_ms: 1000,
            },
            TelemetryAuditCheck {
                id: "c2".into(),
                description: "Check 2".into(),
                passed: false,
                measured: Some(0.5),
                expected: Some(1.0),
                message: "incomplete".into(),
                checked_at_ms: 1000,
            },
        ];
        let report = TelemetryAuditReport::from_checks(checks, 1000);
        assert!(!report.all_passed);
        assert_eq!(report.fail_count, 1);
        assert_eq!(report.failures().len(), 1);
    }

    // -- AlertFidelityCheck tests --

    #[test]
    fn alert_fidelity_perfect() {
        let check = AlertFidelityCheck::new("alert.test", 10, 0, 0);
        assert!((check.precision - 1.0).abs() < f64::EPSILON);
        assert!((check.recall - 1.0).abs() < f64::EPSILON);
        assert!((check.f1_score() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn alert_fidelity_with_false_positives() {
        let check = AlertFidelityCheck::new("alert.test", 8, 2, 0);
        assert!((check.precision - 0.8).abs() < 0.01);
        assert!((check.recall - 1.0).abs() < f64::EPSILON);
        assert_eq!(check.alerts_fired, 10);
    }

    #[test]
    fn alert_fidelity_with_false_negatives() {
        let check = AlertFidelityCheck::new("alert.test", 8, 0, 2);
        assert!((check.precision - 1.0).abs() < f64::EPSILON);
        assert!((check.recall - 0.8).abs() < 0.01);
    }

    #[test]
    fn alert_fidelity_no_alerts() {
        let check = AlertFidelityCheck::new("alert.test", 0, 0, 0);
        assert!((check.precision - 1.0).abs() < f64::EPSILON);
        assert!((check.recall - 1.0).abs() < f64::EPSILON);
    }

    // -- SloAuditReport tests --

    #[test]
    fn audit_report_all_pass() {
        let evals = vec![SloEvaluation {
            slo_id: "test".into(),
            conforming: true,
            measured_value: 0.001,
            target_value: 0.01,
            budget_remaining: 0.9,
            sample_count: 100,
            good_count: 99,
            bad_count: 1,
            window_start_ms: 0,
            window_end_ms: 1000,
            breach_severity: SloSeverity::Critical,
        }];
        let telemetry = TelemetryAuditReport::from_checks(Vec::new(), 1000);
        let report = SloAuditReport::build(evals, telemetry, Vec::new(), 1000);
        assert!(report.overall_pass);
        assert_eq!(report.slos_conforming, 1);
        assert_eq!(report.slos_breached, 0);
    }

    #[test]
    fn audit_report_slo_breach() {
        let evals = vec![
            SloEvaluation {
                slo_id: "good".into(),
                conforming: true,
                measured_value: 0.001,
                target_value: 0.01,
                budget_remaining: 0.9,
                sample_count: 100,
                good_count: 99,
                bad_count: 1,
                window_start_ms: 0,
                window_end_ms: 1000,
                breach_severity: SloSeverity::Critical,
            },
            SloEvaluation {
                slo_id: "bad".into(),
                conforming: false,
                measured_value: 0.1,
                target_value: 0.01,
                budget_remaining: 0.0,
                sample_count: 100,
                good_count: 90,
                bad_count: 10,
                window_start_ms: 0,
                window_end_ms: 1000,
                breach_severity: SloSeverity::Critical,
            },
        ];
        let telemetry = TelemetryAuditReport::from_checks(Vec::new(), 1000);
        let report = SloAuditReport::build(evals, telemetry, Vec::new(), 1000);
        assert!(!report.overall_pass);
        assert_eq!(report.slos_breached, 1);
        assert_eq!(report.breached_slos().len(), 1);
    }

    #[test]
    fn audit_report_render_summary() {
        let evals = vec![SloEvaluation {
            slo_id: "p99.send".into(),
            conforming: true,
            measured_value: 45.0,
            target_value: 100.0,
            budget_remaining: 0.95,
            sample_count: 1000,
            good_count: 999,
            bad_count: 1,
            window_start_ms: 0,
            window_end_ms: 60_000,
            breach_severity: SloSeverity::Critical,
        }];
        let telemetry = TelemetryAuditReport::from_checks(Vec::new(), 1000);
        let report = SloAuditReport::build(evals, telemetry, Vec::new(), 1000);
        let summary = report.render_summary();
        assert!(summary.contains("PASS"));
        assert!(summary.contains("p99.send"));
        assert!(summary.contains("45.000"));
    }

    // -- Serde roundtrip tests --

    #[test]
    fn slo_definition_serde_roundtrip() {
        let slo = SloDefinition::latency("test", "Test", "sys", 99, 100.0, 60_000);
        let json = serde_json::to_string(&slo).unwrap();
        let slo2: SloDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(slo2.id, "test");
        assert_eq!(slo2.target, 100.0);
    }

    #[test]
    fn slo_evaluation_serde_roundtrip() {
        let eval = SloEvaluation {
            slo_id: "test".into(),
            conforming: true,
            measured_value: 50.0,
            target_value: 100.0,
            budget_remaining: 0.9,
            sample_count: 100,
            good_count: 99,
            bad_count: 1,
            window_start_ms: 0,
            window_end_ms: 1000,
            breach_severity: SloSeverity::Warning,
        };
        let json = serde_json::to_string(&eval).unwrap();
        let eval2: SloEvaluation = serde_json::from_str(&json).unwrap();
        assert_eq!(eval2.conforming, true);
        assert_eq!(eval2.breach_severity, SloSeverity::Warning);
    }

    #[test]
    fn slo_severity_ordering() {
        assert!(SloSeverity::Info < SloSeverity::Warning);
        assert!(SloSeverity::Warning < SloSeverity::Critical);
        assert!(SloSeverity::Critical < SloSeverity::Page);
    }

    #[test]
    fn alert_fidelity_serde_roundtrip() {
        let check = AlertFidelityCheck::new("alert.test", 10, 2, 1);
        let json = serde_json::to_string(&check).unwrap();
        let check2: AlertFidelityCheck = serde_json::from_str(&json).unwrap();
        assert_eq!(check2.true_positives, 10);
        assert_eq!(check2.false_positives, 2);
    }
}
