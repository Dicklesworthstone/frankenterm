//! Input-to-display latency measurement framework (ft-1memj.25).
//!
//! Measures and reports latency along the GUI input pipeline:
//!
//! ```text
//! KeyEvent → PtyWrite → PtyRead → TermUpdate → RenderSubmit → GpuPresent
//! ```
//!
//! Each stage is independently timed. The framework computes p50/p95/p99
//! percentiles, compares against configurable budgets, and produces
//! structured reports suitable for CI regression gating.
//!
//! # Design Principles
//!
//! - **Zero-allocation hot path**: Measurement recording uses pre-allocated buffers.
//! - **Deterministic percentiles**: Uses the nearest-rank method (no interpolation).
//! - **Stage independence**: Each stage is measured independently; no cross-stage coupling.
//! - **Budget algebra**: Per-stage budgets compose to an aggregate ceiling.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Stage Definitions ────────────────────────────────────────────────────────

/// Stages on the input-to-display critical path.
///
/// Ordered by pipeline position: user keypress through to visible pixel update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputLatencyStage {
    /// Key event received from the OS/window system.
    KeyEvent,
    /// Key event encoded and written to the PTY master fd.
    PtyWrite,
    /// Response bytes read from the PTY slave fd.
    PtyRead,
    /// Terminal state machine updated (cell grid, cursor, attributes).
    TermUpdate,
    /// Render command buffer submitted to GPU API (wgpu/Metal).
    RenderSubmit,
    /// GPU present completed (frame visible on screen).
    GpuPresent,
}

impl InputLatencyStage {
    /// All stages in pipeline order.
    pub const ALL: &'static [Self] = &[
        Self::KeyEvent,
        Self::PtyWrite,
        Self::PtyRead,
        Self::TermUpdate,
        Self::RenderSubmit,
        Self::GpuPresent,
    ];

    /// Human-readable label for this stage.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::KeyEvent => "key_event",
            Self::PtyWrite => "pty_write",
            Self::PtyRead => "pty_read",
            Self::TermUpdate => "term_update",
            Self::RenderSubmit => "render_submit",
            Self::GpuPresent => "gpu_present",
        }
    }
}

impl std::fmt::Display for InputLatencyStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ── Single Measurement ──────────────────────────────────────────────────────

/// A single input-to-display latency measurement.
///
/// Records timestamps (in microseconds since measurement epoch) at each stage
/// the input event passes through. Not all stages may be present if measurement
/// was only instrumented for a subset of the pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputLatencyMeasurement {
    /// Monotonic measurement ID.
    pub id: u64,
    /// Timestamps in microseconds at each stage.
    pub stages: BTreeMap<InputLatencyStage, u64>,
}

impl InputLatencyMeasurement {
    /// Create a new measurement with the given ID.
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id,
            stages: BTreeMap::new(),
        }
    }

    /// Record a stage timestamp.
    pub fn record_stage(&mut self, stage: InputLatencyStage, timestamp_us: u64) {
        self.stages.insert(stage, timestamp_us);
    }

    /// Total end-to-end latency in microseconds (first stage to last stage).
    /// Returns `None` if fewer than 2 stages are recorded.
    #[must_use]
    pub fn total_latency_us(&self) -> Option<u64> {
        let first = self.stages.values().next()?;
        let last = self.stages.values().next_back()?;
        if last > first {
            Some(last - first)
        } else {
            None
        }
    }

    /// Latency between two specific stages in microseconds.
    /// Returns `None` if either stage is missing or `to` precedes `from`.
    #[must_use]
    pub fn stage_latency_us(&self, from: InputLatencyStage, to: InputLatencyStage) -> Option<u64> {
        let from_ts = self.stages.get(&from)?;
        let to_ts = self.stages.get(&to)?;
        to_ts.checked_sub(*from_ts)
    }

    /// Number of stages recorded.
    #[must_use]
    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }
}

// ── Percentile Computation ──────────────────────────────────────────────────

/// Percentile targets for latency reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Percentile {
    P50,
    P95,
    P99,
    P999,
}

impl Percentile {
    /// The fraction this percentile represents (0.0–1.0).
    #[must_use]
    pub fn fraction(self) -> f64 {
        match self {
            Self::P50 => 0.50,
            Self::P95 => 0.95,
            Self::P99 => 0.99,
            Self::P999 => 0.999,
        }
    }

    /// All standard percentiles.
    pub const ALL: &'static [Self] = &[Self::P50, Self::P95, Self::P99, Self::P999];
}

impl std::fmt::Display for Percentile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::P50 => f.write_str("p50"),
            Self::P95 => f.write_str("p95"),
            Self::P99 => f.write_str("p99"),
            Self::P999 => f.write_str("p999"),
        }
    }
}

/// Compute the percentile value from a sorted slice using nearest-rank method.
///
/// Returns `None` if the slice is empty.
#[must_use]
pub fn percentile_nearest_rank(sorted_values: &[u64], percentile: Percentile) -> Option<u64> {
    if sorted_values.is_empty() {
        return None;
    }
    let n = sorted_values.len();
    let rank = (percentile.fraction() * n as f64).ceil() as usize;
    let idx = rank.min(n).saturating_sub(1);
    Some(sorted_values[idx])
}

// ── Latency Collector ───────────────────────────────────────────────────────

/// Collects latency measurements and computes aggregate statistics.
///
/// Pre-allocates storage for the expected measurement count to avoid
/// allocation on the measurement hot path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputLatencyCollector {
    /// Raw measurements in recording order.
    measurements: Vec<InputLatencyMeasurement>,
    /// Maximum measurements to retain (ring buffer semantics).
    capacity: usize,
    /// Next measurement ID.
    next_id: u64,
}

impl InputLatencyCollector {
    /// Create a new collector with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            measurements: Vec::with_capacity(capacity.min(4096)),
            capacity: capacity.max(1),
            next_id: 0,
        }
    }

    /// Start a new measurement and return its handle.
    pub fn begin_measurement(&mut self) -> InputLatencyMeasurement {
        let id = self.next_id;
        self.next_id += 1;
        InputLatencyMeasurement::new(id)
    }

    /// Record a completed measurement.
    pub fn record(&mut self, measurement: InputLatencyMeasurement) {
        if self.measurements.len() >= self.capacity {
            self.measurements.remove(0);
        }
        self.measurements.push(measurement);
    }

    /// Number of recorded measurements.
    #[must_use]
    pub fn count(&self) -> usize {
        self.measurements.len()
    }

    /// Compute the percentile for end-to-end latency across all measurements.
    #[must_use]
    pub fn total_latency_percentile(&self, percentile: Percentile) -> Option<u64> {
        let mut values: Vec<u64> = self
            .measurements
            .iter()
            .filter_map(|m| m.total_latency_us())
            .collect();
        values.sort_unstable();
        percentile_nearest_rank(&values, percentile)
    }

    /// Compute the percentile for a specific stage-to-stage latency.
    #[must_use]
    pub fn stage_latency_percentile(
        &self,
        from: InputLatencyStage,
        to: InputLatencyStage,
        percentile: Percentile,
    ) -> Option<u64> {
        let mut values: Vec<u64> = self
            .measurements
            .iter()
            .filter_map(|m| m.stage_latency_us(from, to))
            .collect();
        values.sort_unstable();
        percentile_nearest_rank(&values, percentile)
    }

    /// Compute all standard percentiles for end-to-end latency.
    #[must_use]
    pub fn total_latency_summary(&self) -> BTreeMap<Percentile, u64> {
        let mut values: Vec<u64> = self
            .measurements
            .iter()
            .filter_map(|m| m.total_latency_us())
            .collect();
        values.sort_unstable();
        Percentile::ALL
            .iter()
            .filter_map(|&p| percentile_nearest_rank(&values, p).map(|v| (p, v)))
            .collect()
    }

    /// Clear all recorded measurements.
    pub fn clear(&mut self) {
        self.measurements.clear();
    }
}

// ── Budget Configuration ────────────────────────────────────────────────────

/// Per-stage latency budget in microseconds at each percentile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageBudget {
    /// Stage this budget applies to.
    pub stage: InputLatencyStage,
    /// Budget targets: percentile → maximum allowed microseconds.
    pub targets: BTreeMap<Percentile, u64>,
}

/// Complete latency budget configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputLatencyBudget {
    /// Per-stage budgets.
    pub stages: Vec<StageBudget>,
    /// Aggregate end-to-end budget (KeyEvent → GpuPresent).
    pub aggregate: BTreeMap<Percentile, u64>,
    /// Regression threshold: if measured latency exceeds budget by this fraction,
    /// the check fails. 1.0 = exactly at budget, 1.1 = 10% over budget.
    pub regression_threshold: f64,
}

impl Default for InputLatencyBudget {
    fn default() -> Self {
        Self {
            stages: Vec::new(),
            aggregate: [
                (Percentile::P50, 2000),  // 2ms p50
                (Percentile::P95, 4000),  // 4ms p95
                (Percentile::P99, 8000),  // 8ms p99
                (Percentile::P999, 16000), // 16ms p999
            ]
            .into_iter()
            .collect(),
            regression_threshold: 1.0,
        }
    }
}

// ── Budget Evaluation ───────────────────────────────────────────────────────

/// Result of evaluating a latency measurement against a budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetCheckResult {
    /// Whether all budget checks passed.
    pub passed: bool,
    /// Per-percentile results.
    pub details: Vec<BudgetCheckDetail>,
    /// Overall reason code.
    pub reason_code: String,
}

/// Detail for a single percentile budget check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetCheckDetail {
    /// The percentile checked.
    pub percentile: Percentile,
    /// Budget target in microseconds.
    pub budget_us: u64,
    /// Measured value in microseconds.
    pub measured_us: u64,
    /// Whether this check passed.
    pub passed: bool,
    /// Ratio of measured/budget (1.0 = exactly at budget).
    pub ratio: f64,
    /// Reason code.
    pub reason_code: String,
}

/// Evaluate a collector's measurements against a budget.
#[must_use]
pub fn evaluate_budget(
    collector: &InputLatencyCollector,
    budget: &InputLatencyBudget,
) -> BudgetCheckResult {
    let summary = collector.total_latency_summary();
    let mut details = Vec::new();
    let mut all_passed = true;

    for (&percentile, &budget_us) in &budget.aggregate {
        let measured_us = summary.get(&percentile).copied().unwrap_or(0);
        let effective_budget = (budget_us as f64 * budget.regression_threshold) as u64;
        let passed = measured_us <= effective_budget;
        let ratio = if budget_us > 0 {
            measured_us as f64 / budget_us as f64
        } else {
            0.0
        };

        if !passed {
            all_passed = false;
        }

        details.push(BudgetCheckDetail {
            percentile,
            budget_us,
            measured_us,
            passed,
            ratio,
            reason_code: if passed {
                format!("BUDGET_OK_{}", percentile)
            } else {
                format!("BUDGET_EXCEEDED_{}", percentile)
            },
        });
    }

    BudgetCheckResult {
        passed: all_passed,
        details,
        reason_code: if all_passed {
            "ALL_BUDGETS_MET".to_string()
        } else {
            "BUDGET_VIOLATION".to_string()
        },
    }
}

// ── Latency Report ──────────────────────────────────────────────────────────

/// Structured latency report suitable for CI output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputLatencyReport {
    /// Number of measurements in the sample.
    pub sample_count: usize,
    /// Per-percentile end-to-end latency in microseconds.
    pub percentiles: BTreeMap<Percentile, u64>,
    /// Per-stage breakdown at p50.
    pub stage_breakdown_p50: BTreeMap<String, u64>,
    /// Budget evaluation result (None if no budget configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_check: Option<BudgetCheckResult>,
}

/// Generate a latency report from a collector with optional budget evaluation.
#[must_use]
pub fn generate_report(
    collector: &InputLatencyCollector,
    budget: Option<&InputLatencyBudget>,
) -> InputLatencyReport {
    let percentiles = collector.total_latency_summary();

    // Stage breakdown at p50
    let mut stage_breakdown_p50 = BTreeMap::new();
    let stages = InputLatencyStage::ALL;
    for window in stages.windows(2) {
        let from = window[0];
        let to = window[1];
        if let Some(lat) = collector.stage_latency_percentile(from, to, Percentile::P50) {
            let label = format!("{}_to_{}", from.label(), to.label());
            stage_breakdown_p50.insert(label, lat);
        }
    }

    let budget_check = budget.map(|b| evaluate_budget(collector, b));

    InputLatencyReport {
        sample_count: collector.count(),
        percentiles,
        stage_breakdown_p50,
        budget_check,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_measurement(id: u64, base: u64, step: u64) -> InputLatencyMeasurement {
        let mut m = InputLatencyMeasurement::new(id);
        for (i, &stage) in InputLatencyStage::ALL.iter().enumerate() {
            m.record_stage(stage, base + step * i as u64);
        }
        m
    }

    #[test]
    fn measurement_total_latency() {
        let m = make_measurement(0, 1000, 500);
        // KeyEvent=1000, PtyWrite=1500, ..., GpuPresent=3500
        assert_eq!(m.total_latency_us(), Some(2500)); // 3500 - 1000
    }

    #[test]
    fn measurement_stage_latency() {
        let m = make_measurement(0, 1000, 500);
        assert_eq!(
            m.stage_latency_us(InputLatencyStage::KeyEvent, InputLatencyStage::PtyWrite),
            Some(500)
        );
        assert_eq!(
            m.stage_latency_us(InputLatencyStage::KeyEvent, InputLatencyStage::GpuPresent),
            Some(2500)
        );
    }

    #[test]
    fn measurement_missing_stage_returns_none() {
        let mut m = InputLatencyMeasurement::new(0);
        m.record_stage(InputLatencyStage::KeyEvent, 1000);
        assert_eq!(
            m.stage_latency_us(InputLatencyStage::KeyEvent, InputLatencyStage::PtyWrite),
            None
        );
    }

    #[test]
    fn measurement_single_stage_no_total() {
        let mut m = InputLatencyMeasurement::new(0);
        m.record_stage(InputLatencyStage::KeyEvent, 1000);
        // BTreeMap has only one entry, first == last, latency = 0 is filtered
        assert_eq!(m.total_latency_us(), None);
    }

    #[test]
    fn collector_basic_operations() {
        let mut collector = InputLatencyCollector::new(100);
        assert_eq!(collector.count(), 0);

        let m = make_measurement(0, 1000, 500);
        collector.record(m);
        assert_eq!(collector.count(), 1);
    }

    #[test]
    fn collector_capacity_eviction() {
        let mut collector = InputLatencyCollector::new(3);
        for i in 0..5 {
            let m = make_measurement(i, 1000, 500);
            collector.record(m);
        }
        assert_eq!(collector.count(), 3);
    }

    #[test]
    fn collector_percentile_computation() {
        let mut collector = InputLatencyCollector::new(100);
        // Add measurements with increasing latency
        for i in 0..100 {
            let m = make_measurement(i, 1000, 100 + i * 10);
            collector.record(m);
        }

        let p50 = collector.total_latency_percentile(Percentile::P50);
        let p99 = collector.total_latency_percentile(Percentile::P99);

        assert!(p50.is_some());
        assert!(p99.is_some());
        assert!(p99.unwrap() >= p50.unwrap(), "p99 >= p50");
    }

    #[test]
    fn collector_empty_percentile_returns_none() {
        let collector = InputLatencyCollector::new(100);
        assert_eq!(collector.total_latency_percentile(Percentile::P50), None);
    }

    #[test]
    fn percentile_nearest_rank_basic() {
        let values = vec![100, 200, 300, 400, 500, 600, 700, 800, 900, 1000];
        assert_eq!(percentile_nearest_rank(&values, Percentile::P50), Some(500));
        assert_eq!(
            percentile_nearest_rank(&values, Percentile::P95),
            Some(1000)
        );
    }

    #[test]
    fn percentile_nearest_rank_single_element() {
        let values = vec![42];
        assert_eq!(percentile_nearest_rank(&values, Percentile::P50), Some(42));
        assert_eq!(percentile_nearest_rank(&values, Percentile::P99), Some(42));
    }

    #[test]
    fn percentile_nearest_rank_empty() {
        let values: Vec<u64> = vec![];
        assert_eq!(percentile_nearest_rank(&values, Percentile::P50), None);
    }

    #[test]
    fn budget_check_all_passing() {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..50 {
            let m = make_measurement(i, 1000, 100); // total = 500us
            collector.record(m);
        }

        let budget = InputLatencyBudget::default(); // p50=2000, p95=4000
        let result = evaluate_budget(&collector, &budget);

        assert!(result.passed);
        assert_eq!(result.reason_code, "ALL_BUDGETS_MET");
        assert!(result.details.iter().all(|d| d.passed));
    }

    #[test]
    fn budget_check_violation() {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..50 {
            // total = 50000us (50ms) — way over budget
            let m = make_measurement(i, 1000, 10000);
            collector.record(m);
        }

        let budget = InputLatencyBudget::default();
        let result = evaluate_budget(&collector, &budget);

        assert!(!result.passed);
        assert_eq!(result.reason_code, "BUDGET_VIOLATION");
    }

    #[test]
    fn budget_check_with_regression_threshold() {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..50 {
            let m = make_measurement(i, 1000, 500); // total = 2500us
            collector.record(m);
        }

        let mut budget = InputLatencyBudget::default(); // p50=2000
        budget.regression_threshold = 1.5; // allow 50% over
        let result = evaluate_budget(&collector, &budget);

        // 2500 < 2000*1.5=3000, so should pass
        assert!(result.passed);
    }

    #[test]
    fn report_generation() {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..20 {
            let m = make_measurement(i, 1000, 200);
            collector.record(m);
        }

        let report = generate_report(&collector, Some(&InputLatencyBudget::default()));

        assert_eq!(report.sample_count, 20);
        assert!(!report.percentiles.is_empty());
        assert!(!report.stage_breakdown_p50.is_empty());
        assert!(report.budget_check.is_some());
    }

    #[test]
    fn report_without_budget() {
        let mut collector = InputLatencyCollector::new(100);
        let m = make_measurement(0, 1000, 200);
        collector.record(m);

        let report = generate_report(&collector, None);
        assert!(report.budget_check.is_none());
    }

    #[test]
    fn stage_display_and_label() {
        for stage in InputLatencyStage::ALL {
            let label = stage.label();
            let display = format!("{stage}");
            assert_eq!(label, display);
            assert!(!label.is_empty());
        }
    }

    #[test]
    fn percentile_display() {
        assert_eq!(format!("{}", Percentile::P50), "p50");
        assert_eq!(format!("{}", Percentile::P95), "p95");
        assert_eq!(format!("{}", Percentile::P99), "p99");
        assert_eq!(format!("{}", Percentile::P999), "p999");
    }

    #[test]
    fn percentile_fraction_ordering() {
        assert!(Percentile::P50.fraction() < Percentile::P95.fraction());
        assert!(Percentile::P95.fraction() < Percentile::P99.fraction());
        assert!(Percentile::P99.fraction() < Percentile::P999.fraction());
    }

    #[test]
    fn stage_all_has_correct_count() {
        assert_eq!(InputLatencyStage::ALL.len(), 6);
    }

    #[test]
    fn collector_clear_resets() {
        let mut collector = InputLatencyCollector::new(100);
        collector.record(make_measurement(0, 1000, 100));
        assert_eq!(collector.count(), 1);
        collector.clear();
        assert_eq!(collector.count(), 0);
    }

    #[test]
    fn measurement_serde_roundtrip() {
        let m = make_measurement(42, 1000, 500);
        let json = serde_json::to_string(&m).unwrap();
        let back: InputLatencyMeasurement = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 42);
        assert_eq!(back.stages.len(), 6);
        assert_eq!(back.total_latency_us(), m.total_latency_us());
    }

    #[test]
    fn collector_serde_roundtrip() {
        let mut collector = InputLatencyCollector::new(50);
        for i in 0..5 {
            collector.record(make_measurement(i, 1000, 200));
        }
        let json = serde_json::to_string(&collector).unwrap();
        let back: InputLatencyCollector = serde_json::from_str(&json).unwrap();
        assert_eq!(back.count(), 5);
    }

    #[test]
    fn budget_serde_roundtrip() {
        let budget = InputLatencyBudget::default();
        let json = serde_json::to_string(&budget).unwrap();
        let back: InputLatencyBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.aggregate.len(), budget.aggregate.len());
        assert!((back.regression_threshold - budget.regression_threshold).abs() < 1e-9);
    }

    #[test]
    fn report_serde_roundtrip() {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..10 {
            collector.record(make_measurement(i, 1000, 300));
        }
        let report = generate_report(&collector, Some(&InputLatencyBudget::default()));
        let json = serde_json::to_string(&report).unwrap();
        let back: InputLatencyReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sample_count, report.sample_count);
    }

    #[test]
    fn budget_check_result_serde_roundtrip() {
        let result = BudgetCheckResult {
            passed: true,
            details: vec![BudgetCheckDetail {
                percentile: Percentile::P50,
                budget_us: 2000,
                measured_us: 1500,
                passed: true,
                ratio: 0.75,
                reason_code: "BUDGET_OK_p50".to_string(),
            }],
            reason_code: "ALL_BUDGETS_MET".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: BudgetCheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.passed, true);
        assert_eq!(back.details.len(), 1);
    }

    #[test]
    fn stage_breakdown_labels_follow_convention() {
        let mut collector = InputLatencyCollector::new(100);
        collector.record(make_measurement(0, 1000, 200));
        let report = generate_report(&collector, None);

        for key in report.stage_breakdown_p50.keys() {
            assert!(key.contains("_to_"), "Stage key must contain '_to_': {key}");
        }
    }

    #[test]
    fn begin_measurement_assigns_incrementing_ids() {
        let mut collector = InputLatencyCollector::new(100);
        let m1 = collector.begin_measurement();
        let m2 = collector.begin_measurement();
        let m3 = collector.begin_measurement();
        assert_eq!(m1.id, 0);
        assert_eq!(m2.id, 1);
        assert_eq!(m3.id, 2);
    }
}
