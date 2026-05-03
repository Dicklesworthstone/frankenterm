//! br-ft-1650n.15: workload-profile trend aggregator.
//!
//! Consumes a chronological `Vec<WorkloadProfile>` and emits a
//! typed trend report — per-metric direction (Stable /
//! Increasing / Decreasing) + magnitude (% change between the
//! first and last window) + an overall stability flag. Lets
//! operators see whether `advise_from_event_bus_metrics` is
//! reporting a steady-state workload or a workload mid-shift,
//! which informs whether the advisor's recommendation is durable
//! or about to flip.
//!
//! Pure-function substrate; the wired-pass slice will populate
//! the profile history from telemetry storage.
//!
//! ## What ships in this slice
//!
//! - [`TrendDirection`] — `Stable` / `Increasing` /
//!   `Decreasing`.
//! - [`MetricTrend`] — direction + percent-change basis points.
//! - [`StabilityVerdict`] — `Stable` / `Mixed` / `Unstable`
//!   summary across all metrics.
//! - [`WorkloadTrendReport`] — per-metric trend bag + verdict +
//!   sample window count.
//! - [`compute_trend`] — pure function over a profile history.
//!
//! ## Stability classification
//!
//! - `TrendDirection::Stable` if `|percent_change_bps| ≤ 1_000`
//!   (≤ 10%). Tunable via `TrendThresholds`.
//! - `Increasing` if percent_change_bps > 1_000.
//! - `Decreasing` if percent_change_bps < -1_000.
//!
//! Per-metric directions roll up to a `StabilityVerdict`:
//!
//! - `Stable` — every metric is `TrendDirection::Stable`.
//! - `Mixed` — some metrics `Stable`, others not.
//! - `Unstable` — every metric is non-`Stable`.

use serde::{Deserialize, Serialize};

use crate::storage_workload_advisor::WorkloadProfile;

/// Stable schema version for `WorkloadTrendReport` exports.
pub const WORKLOAD_TREND_REPORT_SCHEMA_VERSION: &str = "ft.workload_trend.report.v1";

/// Operator-tunable thresholds for trend classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrendThresholds {
    /// Absolute percent-change basis points below which a metric
    /// is classified as `Stable`. Default 1_000 (10%).
    pub stable_band_bps: u32,
}

impl TrendThresholds {
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            stable_band_bps: 1_000,
        }
    }
}

impl Default for TrendThresholds {
    fn default() -> Self {
        Self::conservative()
    }
}

/// Direction of trend for one metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrendDirection {
    Stable,
    Increasing,
    Decreasing,
}

/// Per-metric trend description.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricTrend {
    pub direction: TrendDirection,
    /// Signed percent change × 100 (basis points). Positive =
    /// increase, negative = decrease.
    pub percent_change_bps: i32,
    /// Initial (first window) value.
    pub initial_value: u64,
    /// Final (last window) value.
    pub final_value: u64,
}

impl MetricTrend {
    #[must_use]
    pub fn classify(initial: u64, final_value: u64, thresholds: TrendThresholds) -> Self {
        let pct = if initial == 0 && final_value == 0 {
            0
        } else if initial == 0 {
            // Going from zero to non-zero: clamp to a large
            // positive band. Reported as Increasing.
            i32::MAX
        } else {
            // delta_bps = (final - initial) / initial * 10_000
            let delta = final_value as i128 - initial as i128;
            let bps = delta.saturating_mul(10_000) / initial as i128;
            i32::try_from(bps).unwrap_or(if delta > 0 { i32::MAX } else { i32::MIN })
        };
        let direction = if pct.unsigned_abs() <= thresholds.stable_band_bps {
            TrendDirection::Stable
        } else if pct > 0 {
            TrendDirection::Increasing
        } else {
            TrendDirection::Decreasing
        };
        Self {
            direction,
            percent_change_bps: pct,
            initial_value: initial,
            final_value,
        }
    }
}

/// Overall stability verdict across all metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StabilityVerdict {
    /// Every metric is `Stable`.
    Stable,
    /// Some metrics `Stable`, others not.
    Mixed,
    /// No metric is `Stable`.
    Unstable,
}

/// br-ft-1650n.15: trend report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadTrendReport {
    pub schema_version: String,
    pub window_count: usize,
    pub writes_trend: MetricTrend,
    pub reads_trend: MetricTrend,
    pub searches_trend: MetricTrend,
    pub checkpoint_lag_trend: MetricTrend,
    pub p99_write_trend: MetricTrend,
    pub p99_read_trend: MetricTrend,
    pub stability_verdict: StabilityVerdict,
    /// `None` when the report was emitted with too few windows
    /// to be informative.
    pub data_needed_reason: Option<String>,
}

/// Minimum number of profile windows before the trend
/// classifier emits a non-degenerate report. With 1 window
/// there's nothing to compare against; with 2 the percent-
/// change is well-defined.
pub const MIN_TREND_WINDOWS: usize = 2;

/// br-ft-1650n.15: pure entry point. Compares the first and
/// last profile in the chronological history and emits a
/// per-metric trend bag.
///
/// If `history.len() < MIN_TREND_WINDOWS` the report is
/// returned with `data_needed_reason = Some(...)` and every
/// trend defaults to `Stable` with zero values — operators read
/// the reason field as a directive to collect more samples.
#[must_use]
pub fn compute_trend(
    history: &[WorkloadProfile],
    thresholds: TrendThresholds,
) -> WorkloadTrendReport {
    if history.len() < MIN_TREND_WINDOWS {
        let zero = MetricTrend {
            direction: TrendDirection::Stable,
            percent_change_bps: 0,
            initial_value: 0,
            final_value: 0,
        };
        return WorkloadTrendReport {
            schema_version: WORKLOAD_TREND_REPORT_SCHEMA_VERSION.to_string(),
            window_count: history.len(),
            writes_trend: zero,
            reads_trend: zero,
            searches_trend: zero,
            checkpoint_lag_trend: zero,
            p99_write_trend: zero,
            p99_read_trend: zero,
            stability_verdict: StabilityVerdict::Stable,
            data_needed_reason: Some(format!(
                "need at least {MIN_TREND_WINDOWS} profile windows, got {}",
                history.len()
            )),
        };
    }

    let first = &history[0];
    let last = &history[history.len() - 1];

    let writes_trend = MetricTrend::classify(first.total_writes, last.total_writes, thresholds);
    let reads_trend = MetricTrend::classify(first.total_reads, last.total_reads, thresholds);
    let searches_trend =
        MetricTrend::classify(first.total_searches, last.total_searches, thresholds);
    let checkpoint_lag_trend = MetricTrend::classify(
        first.checkpoint_lag_bytes,
        last.checkpoint_lag_bytes,
        thresholds,
    );
    let p99_write_trend = MetricTrend::classify(
        first.p99_write_latency_us,
        last.p99_write_latency_us,
        thresholds,
    );
    let p99_read_trend = MetricTrend::classify(
        first.p99_read_latency_us,
        last.p99_read_latency_us,
        thresholds,
    );

    let trends = [
        writes_trend.direction,
        reads_trend.direction,
        searches_trend.direction,
        checkpoint_lag_trend.direction,
        p99_write_trend.direction,
        p99_read_trend.direction,
    ];
    let stable_count = trends
        .iter()
        .filter(|d| **d == TrendDirection::Stable)
        .count();
    let stability_verdict = if stable_count == trends.len() {
        StabilityVerdict::Stable
    } else if stable_count == 0 {
        StabilityVerdict::Unstable
    } else {
        StabilityVerdict::Mixed
    };

    WorkloadTrendReport {
        schema_version: WORKLOAD_TREND_REPORT_SCHEMA_VERSION.to_string(),
        window_count: history.len(),
        writes_trend,
        reads_trend,
        searches_trend,
        checkpoint_lag_trend,
        p99_write_trend,
        p99_read_trend,
        stability_verdict,
        data_needed_reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(
        writes: u64,
        reads: u64,
        searches: u64,
        lag: u64,
        p99_w: u64,
        p99_r: u64,
    ) -> WorkloadProfile {
        WorkloadProfile {
            total_writes: writes,
            total_reads: reads,
            total_searches: searches,
            fts_enabled: true,
            tantivy_enabled: false,
            estimated_distinct_panes: 0,
            estimated_distinct_sessions: 0,
            hot_table: None,
            p99_write_latency_us: p99_w,
            p99_read_latency_us: p99_r,
            checkpoint_lag_bytes: lag,
        }
    }

    /// Empty history → `data_needed_reason` is set + window
    /// count is 0.
    #[test]
    fn empty_history_returns_data_needed() {
        let report = compute_trend(&[], TrendThresholds::default());
        assert!(report.data_needed_reason.is_some());
        assert_eq!(report.window_count, 0);
    }

    /// Single-window history → `data_needed_reason` is set.
    #[test]
    fn single_window_returns_data_needed() {
        let history = vec![profile(1_000, 500, 100, 0, 0, 0)];
        let report = compute_trend(&history, TrendThresholds::default());
        assert!(report.data_needed_reason.is_some());
        assert_eq!(report.window_count, 1);
    }

    /// Stable workload → every metric Stable + verdict Stable.
    #[test]
    fn stable_workload_emits_stable_verdict() {
        let history = vec![
            profile(1_000, 500, 100, 1024, 5000, 2000),
            profile(1_050, 510, 105, 1024, 5100, 2050),
            profile(1_020, 495, 99, 1000, 4950, 1990),
        ];
        let report = compute_trend(&history, TrendThresholds::default());
        assert!(report.data_needed_reason.is_none());
        assert_eq!(report.stability_verdict, StabilityVerdict::Stable);
        assert_eq!(report.writes_trend.direction, TrendDirection::Stable);
    }

    /// Sustained increase in writes → Increasing direction +
    /// positive percent_change_bps.
    #[test]
    fn writes_increase_classified_as_increasing() {
        let history = vec![
            profile(1_000, 1_000, 100, 0, 0, 0),
            profile(2_000, 1_000, 100, 0, 0, 0),
        ];
        let report = compute_trend(&history, TrendThresholds::default());
        assert_eq!(report.writes_trend.direction, TrendDirection::Increasing);
        assert!(report.writes_trend.percent_change_bps > 0);
        // Other metrics ties → Stable.
        assert_eq!(report.reads_trend.direction, TrendDirection::Stable);
    }

    /// Sustained decrease → Decreasing direction + negative bps.
    #[test]
    fn searches_decrease_classified_as_decreasing() {
        let history = vec![
            profile(1_000, 1_000, 5_000, 0, 0, 0),
            profile(1_000, 1_000, 1_000, 0, 0, 0),
        ];
        let report = compute_trend(&history, TrendThresholds::default());
        assert_eq!(report.searches_trend.direction, TrendDirection::Decreasing);
        assert!(report.searches_trend.percent_change_bps < 0);
    }

    /// Mixed workload (some metrics moving, others stable) →
    /// `Mixed` verdict.
    #[test]
    fn mixed_workload_emits_mixed_verdict() {
        let history = vec![
            profile(1_000, 1_000, 1_000, 1024, 5000, 2000),
            profile(2_000, 1_000, 1_000, 1024, 5000, 2000),
        ];
        let report = compute_trend(&history, TrendThresholds::default());
        assert_eq!(report.stability_verdict, StabilityVerdict::Mixed);
    }

    /// Unstable workload (every metric moving) → `Unstable`
    /// verdict.
    #[test]
    fn unstable_workload_emits_unstable_verdict() {
        let history = vec![
            profile(1_000, 1_000, 1_000, 1024, 5000, 2000),
            profile(2_000, 2_000, 2_000, 5_000, 10_000, 4_000),
        ];
        let report = compute_trend(&history, TrendThresholds::default());
        assert_eq!(report.stability_verdict, StabilityVerdict::Unstable);
    }

    /// Zero → non-zero is reported as `Increasing` with the
    /// clamped i32::MAX bps. Operators read this as "metric just
    /// became active".
    #[test]
    fn zero_to_nonzero_clamps_to_increasing() {
        let history = vec![profile(0, 0, 0, 0, 0, 0), profile(1_000, 0, 0, 0, 0, 0)];
        let report = compute_trend(&history, TrendThresholds::default());
        assert_eq!(report.writes_trend.direction, TrendDirection::Increasing);
        assert_eq!(report.writes_trend.percent_change_bps, i32::MAX);
        // Other metrics: 0 → 0 is `Stable` with bps = 0.
        assert_eq!(report.reads_trend.direction, TrendDirection::Stable);
        assert_eq!(report.reads_trend.percent_change_bps, 0);
    }

    /// Zero-to-zero → Stable with bps=0 (no change).
    #[test]
    fn zero_to_zero_is_stable() {
        let history = vec![profile(0, 0, 0, 0, 0, 0), profile(0, 0, 0, 0, 0, 0)];
        let report = compute_trend(&history, TrendThresholds::default());
        assert_eq!(report.stability_verdict, StabilityVerdict::Stable);
    }

    /// Stable-band threshold is operator-tunable: a tighter band
    /// reclassifies near-stable metrics as moving.
    #[test]
    fn tighter_stable_band_reclassifies_near_stable() {
        let history = vec![
            profile(1_000, 0, 0, 0, 0, 0),
            profile(1_050, 0, 0, 0, 0, 0), // +5%
        ];
        // Default 10% band → Stable.
        let r1 = compute_trend(&history, TrendThresholds::default());
        assert_eq!(r1.writes_trend.direction, TrendDirection::Stable);
        // Tighter 1% band → Increasing.
        let r2 = compute_trend(
            &history,
            TrendThresholds {
                stable_band_bps: 100,
            },
        );
        assert_eq!(r2.writes_trend.direction, TrendDirection::Increasing);
    }

    /// MetricTrend::classify maps initial/final to the documented
    /// fields.
    #[test]
    fn metric_trend_threads_initial_and_final() {
        let t = MetricTrend::classify(1_000, 2_000, TrendThresholds::default());
        assert_eq!(t.initial_value, 1_000);
        assert_eq!(t.final_value, 2_000);
        assert_eq!(t.direction, TrendDirection::Increasing);
        // +100% = 10_000 bps.
        assert_eq!(t.percent_change_bps, 10_000);
    }

    /// Schema version is pinned.
    #[test]
    fn schema_version_is_stable() {
        let report = compute_trend(&[], TrendThresholds::default());
        assert_eq!(report.schema_version, WORKLOAD_TREND_REPORT_SCHEMA_VERSION);
        assert_eq!(report.schema_version, "ft.workload_trend.report.v1");
    }

    /// WorkloadTrendReport serde roundtrips both the
    /// data-needed and populated variants.
    #[test]
    fn report_serde_roundtrip() {
        let r1 = compute_trend(&[], TrendThresholds::default());
        let json = serde_json::to_string(&r1).expect("serialize");
        let back: WorkloadTrendReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r1, back);

        let history = vec![
            profile(1_000, 1_000, 1_000, 0, 0, 0),
            profile(2_000, 1_000, 1_000, 0, 0, 0),
        ];
        let r2 = compute_trend(&history, TrendThresholds::default());
        let json = serde_json::to_string(&r2).expect("serialize");
        let back: WorkloadTrendReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r2, back);
    }

    /// Pure function: same input → same output.
    #[test]
    fn compute_trend_is_pure() {
        let history = vec![
            profile(1_000, 1_000, 1_000, 0, 0, 0),
            profile(1_500, 1_000, 1_000, 0, 0, 0),
        ];
        let r1 = compute_trend(&history, TrendThresholds::default());
        let r2 = compute_trend(&history, TrendThresholds::default());
        assert_eq!(r1, r2);
    }

    /// Trends use only first + last windows; intermediate
    /// values are ignored. Pinned because operators sometimes
    /// expect the aggregator to detect mid-window spikes —
    /// the substrate explicitly does NOT (that's the
    /// prompt_drift_canary's job).
    #[test]
    fn intermediate_windows_do_not_affect_endpoints() {
        let with_spike = vec![
            profile(1_000, 0, 0, 0, 0, 0),
            profile(99_999, 0, 0, 0, 0, 0), // mid-window spike
            profile(1_050, 0, 0, 0, 0, 0),
        ];
        let without_spike = vec![profile(1_000, 0, 0, 0, 0, 0), profile(1_050, 0, 0, 0, 0, 0)];
        let r1 = compute_trend(&with_spike, TrendThresholds::default());
        let r2 = compute_trend(&without_spike, TrendThresholds::default());
        assert_eq!(r1.writes_trend, r2.writes_trend);
    }
}
