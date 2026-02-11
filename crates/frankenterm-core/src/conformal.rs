//! Split conformal prediction for resource forecasting.
//!
//! Provides distribution-free prediction intervals with formal coverage
//! guarantees for FrankenTerm resource metrics (RSS, CPU, I/O, queue depth).
//!
//! # Method
//!
//! Uses split conformal prediction with Holt's linear exponential smoothing
//! as the point predictor:
//!
//! ```text
//! Prediction interval: [ŷ - q, ŷ + q]
//! where q = ⌈(1-α)(n+1)⌉-th smallest nonconformity score
//! ```
//!
//! # Coverage guarantee
//!
//! For exchangeable data:
//! ```text
//! P(y_new ∈ C(x_new)) ≥ 1 - α
//! ```
//!
//! This holds regardless of the underlying distribution.
//!
//! # Performance
//!
//! - Holt update: O(1) per observation
//! - Interval computation: O(n_cal × log n_cal) for sorting

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use tracing::debug;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for conformal prediction forecasting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConformalConfig {
    /// Target coverage probability (1-α). Must be in (0, 1).
    pub coverage: f64,

    /// Maximum calibration scores retained per horizon.
    pub calibration_window: usize,

    /// Forecast horizons in observation steps.
    ///
    /// Default at 30s intervals: `[60, 120, 240, 480, 960]` = 30m, 1h, 2h, 4h, 8h.
    pub horizon_steps: Vec<usize>,

    /// Holt's level smoothing parameter α ∈ (0, 1).
    pub holt_alpha: f64,

    /// Holt's trend smoothing parameter β ∈ (0, 1).
    pub holt_beta: f64,

    /// Observation interval in seconds (for wall-time conversion).
    pub observation_interval_secs: u64,

    /// RSS alarm threshold as fraction of available system memory.
    pub rss_alarm_fraction: f64,

    /// CPU alarm threshold (percentage, 0–100).
    pub cpu_alarm_percent: f64,

    /// Maximum history entries retained per metric.
    pub max_history: usize,
}

impl Default for ConformalConfig {
    fn default() -> Self {
        Self {
            coverage: 0.95,
            calibration_window: 200,
            horizon_steps: vec![60, 120, 240, 480, 960],
            holt_alpha: 0.3,
            holt_beta: 0.1,
            observation_interval_secs: 30,
            rss_alarm_fraction: 0.80,
            cpu_alarm_percent: 90.0,
            max_history: 8640, // 72h at 30s intervals
        }
    }
}

// =============================================================================
// Holt's linear exponential smoothing
// =============================================================================

/// Holt's method for time series with trend.
///
/// Maintains level and trend components, updated online in O(1) per step.
///
/// ```text
/// Level: ℓₜ = α×yₜ + (1-α)(ℓₜ₋₁ + bₜ₋₁)
/// Trend: bₜ = β(ℓₜ - ℓₜ₋₁) + (1-β)bₜ₋₁
/// Forecast: ŷₜ₊ₕ = ℓₜ + h×bₜ
/// ```
#[derive(Debug, Clone)]
pub struct HoltPredictor {
    alpha: f64,
    beta: f64,
    level: f64,
    trend: f64,
    observations: u64,
}

impl HoltPredictor {
    /// Create a new predictor with smoothing parameters.
    ///
    /// Both α and β are clamped to \[0.001, 0.999\] for numerical stability.
    pub fn new(alpha: f64, beta: f64) -> Self {
        Self {
            alpha: alpha.clamp(0.001, 0.999),
            beta: beta.clamp(0.001, 0.999),
            level: 0.0,
            trend: 0.0,
            observations: 0,
        }
    }

    /// Update the model with a new observation. Skips NaN/Inf values.
    pub fn update(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        if self.observations == 0 {
            self.level = value;
            self.trend = 0.0;
            self.observations = 1;
            return;
        }
        let prev_level = self.level;
        self.level = self.alpha * value + (1.0 - self.alpha) * (self.level + self.trend);
        self.trend = self.beta * (self.level - prev_level) + (1.0 - self.beta) * self.trend;
        self.observations += 1;
        // Clamp to prevent divergence on extreme inputs
        if !self.level.is_finite() {
            self.level = value;
        }
        if !self.trend.is_finite() {
            self.trend = 0.0;
        }
    }

    /// Forecast h steps ahead from the current state.
    #[must_use]
    pub fn forecast(&self, steps_ahead: f64) -> f64 {
        self.level + steps_ahead * self.trend
    }

    /// Current level estimate.
    #[must_use]
    pub fn level(&self) -> f64 {
        self.level
    }

    /// Current trend estimate.
    #[must_use]
    pub fn trend(&self) -> f64 {
        self.trend
    }

    /// Number of observations processed.
    #[must_use]
    pub fn observation_count(&self) -> u64 {
        self.observations
    }
}

// =============================================================================
// Calibration
// =============================================================================

/// Nonconformity score calibration set for one forecast horizon.
///
/// Stores scores in FIFO order, evicting the oldest when at capacity.
#[derive(Debug, Clone)]
struct CalibrationSet {
    scores: VecDeque<f64>,
    max_size: usize,
}

impl CalibrationSet {
    fn new(max_size: usize) -> Self {
        Self {
            scores: VecDeque::with_capacity(max_size.min(1024)),
            max_size,
        }
    }

    /// Add a nonconformity score, evicting the oldest if at capacity.
    fn push(&mut self, score: f64) {
        if !score.is_finite() || score < 0.0 {
            return;
        }
        if self.scores.len() >= self.max_size {
            self.scores.pop_front();
        }
        self.scores.push_back(score);
    }

    /// Compute the conformal quantile for the given coverage level.
    ///
    /// Returns `None` if there aren't enough calibration points.
    fn quantile(&self, coverage: f64) -> Option<f64> {
        let n = self.scores.len();
        if n == 0 {
            return None;
        }

        // k = ⌈(1-α)(n+1)⌉ where coverage = 1-α, 1-indexed
        let k = ((coverage * (n as f64 + 1.0)).ceil() as usize).max(1);
        if k > n {
            return None;
        }

        let mut sorted: Vec<f64> = self.scores.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(sorted[k - 1])
    }

    fn len(&self) -> usize {
        self.scores.len()
    }
}

// =============================================================================
// History entry (internal)
// =============================================================================

/// Snapshot of Holt state at one observation step.
#[derive(Debug, Clone)]
struct HistoryEntry {
    level: f64,
    trend: f64,
}

// =============================================================================
// Per-metric forecaster
// =============================================================================

/// Forecaster for a single time series metric.
///
/// Combines Holt's exponential smoothing with split conformal calibration
/// across multiple forecast horizons.
#[derive(Debug, Clone)]
pub struct MetricForecaster {
    name: String,
    holt: HoltPredictor,
    history: VecDeque<HistoryEntry>,
    calibration: Vec<(usize, CalibrationSet)>,
    max_history: usize,
    coverage: f64,
}

impl MetricForecaster {
    /// Create a new forecaster for the named metric.
    pub fn new(
        name: String,
        holt_alpha: f64,
        holt_beta: f64,
        horizon_steps: &[usize],
        calibration_window: usize,
        max_history: usize,
        coverage: f64,
    ) -> Self {
        let calibration = horizon_steps
            .iter()
            .map(|&h| (h, CalibrationSet::new(calibration_window)))
            .collect();

        Self {
            name,
            holt: HoltPredictor::new(holt_alpha, holt_beta),
            history: VecDeque::with_capacity(max_history.min(8192)),
            calibration,
            max_history,
            coverage: coverage.clamp(0.01, 0.999),
        }
    }

    /// Feed a new observation and update calibration.
    pub fn observe(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        self.holt.update(value);

        let entry = HistoryEntry {
            level: self.holt.level(),
            trend: self.holt.trend(),
        };
        self.history.push_back(entry);
        if self.history.len() > self.max_history {
            self.history.pop_front();
        }

        // Update calibration: for each horizon h, the observation h steps ago
        // predicted what the current value would be.
        let history_len = self.history.len();
        for (horizon, cal) in &mut self.calibration {
            if history_len > *horizon {
                let past_idx = history_len - 1 - *horizon;
                let past = &self.history[past_idx];
                let predicted = past.level + (*horizon as f64) * past.trend;
                let score = (value - predicted).abs();
                cal.push(score);
            }
        }
    }

    /// Generate forecasts for all configured horizons.
    pub fn forecast_all(&self) -> Vec<ResourceForecast> {
        self.calibration
            .iter()
            .map(|(horizon, cal)| {
                let point = self.holt.forecast(*horizon as f64);
                let quantile = cal.quantile(self.coverage);
                let (lower, upper) = match quantile {
                    Some(q) => (point - q, point + q),
                    None => (f64::NEG_INFINITY, f64::INFINITY),
                };

                ResourceForecast {
                    metric_name: self.name.clone(),
                    horizon_steps: *horizon,
                    point_estimate: point,
                    lower_bound: lower,
                    upper_bound: upper,
                    coverage: self.coverage,
                    calibration_size: cal.len(),
                    alert: None,
                }
            })
            .collect()
    }

    /// Generate forecast for a specific horizon (in steps).
    pub fn forecast_horizon(&self, horizon_steps: usize) -> Option<ResourceForecast> {
        self.calibration
            .iter()
            .find(|(h, _)| *h == horizon_steps)
            .map(|(horizon, cal)| {
                let point = self.holt.forecast(*horizon as f64);
                let quantile = cal.quantile(self.coverage);
                let (lower, upper) = match quantile {
                    Some(q) => (point - q, point + q),
                    None => (f64::NEG_INFINITY, f64::INFINITY),
                };

                ResourceForecast {
                    metric_name: self.name.clone(),
                    horizon_steps: *horizon,
                    point_estimate: point,
                    lower_bound: lower,
                    upper_bound: upper,
                    coverage: self.coverage,
                    calibration_size: cal.len(),
                    alert: None,
                }
            })
    }

    /// Number of observations processed.
    #[must_use]
    pub fn observation_count(&self) -> u64 {
        self.holt.observation_count()
    }

    /// Name of the metric being forecasted.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

// =============================================================================
// Forecast output types
// =============================================================================

/// A prediction interval for a single metric at one horizon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceForecast {
    /// Name of the metric (e.g., "rss_bytes", "cpu_percent").
    pub metric_name: String,
    /// Forecast horizon in observation steps.
    pub horizon_steps: usize,
    /// Holt's point estimate.
    pub point_estimate: f64,
    /// Lower bound of the prediction interval.
    pub lower_bound: f64,
    /// Upper bound of the prediction interval.
    pub upper_bound: f64,
    /// Coverage level (1-α).
    pub coverage: f64,
    /// Number of calibration points used.
    pub calibration_size: usize,
    /// Alert triggered by this forecast, if any.
    pub alert: Option<ForecastAlert>,
}

impl ResourceForecast {
    /// Wall-clock horizon in seconds, given the observation interval.
    #[must_use]
    pub fn horizon_secs(&self, interval_secs: u64) -> u64 {
        self.horizon_steps as u64 * interval_secs
    }

    /// Whether the interval is calibrated (finite bounds).
    #[must_use]
    pub fn is_calibrated(&self) -> bool {
        self.lower_bound.is_finite() && self.upper_bound.is_finite()
    }

    /// Width of the prediction interval.
    #[must_use]
    pub fn interval_width(&self) -> f64 {
        self.upper_bound - self.lower_bound
    }
}

/// Alert when a forecast upper bound exceeds a safety threshold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ForecastAlert {
    /// RSS upper bound exceeds configured memory fraction.
    RssThreshold {
        upper_bound_bytes: f64,
        threshold_bytes: f64,
        horizon_steps: usize,
    },
    /// CPU upper bound exceeds configured percentage.
    CpuThreshold {
        upper_bound_percent: f64,
        threshold_percent: f64,
        horizon_steps: usize,
    },
}

// =============================================================================
// Multi-metric forecaster
// =============================================================================

/// Orchestrates conformal prediction across multiple resource metrics.
#[derive(Debug)]
pub struct ConformalForecaster {
    config: ConformalConfig,
    metrics: HashMap<String, MetricForecaster>,
    available_memory_bytes: u64,
}

impl ConformalForecaster {
    /// Create a new forecaster with the given configuration.
    pub fn new(config: ConformalConfig) -> Self {
        Self {
            config,
            metrics: HashMap::new(),
            available_memory_bytes: 0,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ConformalConfig::default())
    }

    /// Update the available system memory (for RSS alert threshold).
    pub fn set_available_memory(&mut self, bytes: u64) {
        self.available_memory_bytes = bytes;
    }

    /// Get or create a metric forecaster by name.
    fn ensure_metric(&mut self, name: &str) -> &mut MetricForecaster {
        let config = &self.config;
        self.metrics.entry(name.to_string()).or_insert_with(|| {
            MetricForecaster::new(
                name.to_string(),
                config.holt_alpha,
                config.holt_beta,
                &config.horizon_steps,
                config.calibration_window,
                config.max_history,
                config.coverage,
            )
        })
    }

    /// Feed a new observation for a named metric.
    pub fn observe(&mut self, metric_name: &str, value: f64) {
        self.ensure_metric(metric_name).observe(value);
    }

    /// Feed a `ResourceSnapshot` from the telemetry pipeline.
    pub fn observe_snapshot(&mut self, snapshot: &crate::telemetry::ResourceSnapshot) {
        self.observe("rss_bytes", snapshot.rss_bytes as f64);
        self.observe("virt_bytes", snapshot.virt_bytes as f64);
        self.observe("fd_count", snapshot.fd_count as f64);

        if let Some(cpu) = snapshot.cpu_percent {
            self.observe("cpu_percent", cpu);
        }
        if let Some(io_r) = snapshot.io_read_bytes {
            self.observe("io_read_bytes", io_r as f64);
        }
        if let Some(io_w) = snapshot.io_write_bytes {
            self.observe("io_write_bytes", io_w as f64);
        }
    }

    /// Generate forecasts for all metrics and horizons, with alerts.
    pub fn forecast_all(&self) -> Vec<ResourceForecast> {
        let mut forecasts = Vec::new();
        for forecaster in self.metrics.values() {
            let mut metric_forecasts = forecaster.forecast_all();
            for fc in &mut metric_forecasts {
                fc.alert = self.check_alert(fc);
            }
            forecasts.extend(metric_forecasts);
        }
        forecasts
    }

    /// Generate forecasts for a specific metric.
    pub fn forecast_metric(&self, name: &str) -> Vec<ResourceForecast> {
        match self.metrics.get(name) {
            Some(forecaster) => {
                let mut forecasts = forecaster.forecast_all();
                for fc in &mut forecasts {
                    fc.alert = self.check_alert(fc);
                }
                forecasts
            }
            None => Vec::new(),
        }
    }

    fn check_alert(&self, forecast: &ResourceForecast) -> Option<ForecastAlert> {
        if !forecast.is_calibrated() {
            return None;
        }
        match forecast.metric_name.as_str() {
            "rss_bytes" if self.available_memory_bytes > 0 => {
                let threshold =
                    self.config.rss_alarm_fraction * self.available_memory_bytes as f64;
                if forecast.upper_bound > threshold {
                    debug!(
                        upper_bound = forecast.upper_bound,
                        threshold,
                        horizon = forecast.horizon_steps,
                        "RSS forecast exceeds threshold"
                    );
                    Some(ForecastAlert::RssThreshold {
                        upper_bound_bytes: forecast.upper_bound,
                        threshold_bytes: threshold,
                        horizon_steps: forecast.horizon_steps,
                    })
                } else {
                    None
                }
            }
            "cpu_percent" => {
                if forecast.upper_bound > self.config.cpu_alarm_percent {
                    debug!(
                        upper_bound = forecast.upper_bound,
                        threshold = self.config.cpu_alarm_percent,
                        horizon = forecast.horizon_steps,
                        "CPU forecast exceeds threshold"
                    );
                    Some(ForecastAlert::CpuThreshold {
                        upper_bound_percent: forecast.upper_bound,
                        threshold_percent: self.config.cpu_alarm_percent,
                        horizon_steps: forecast.horizon_steps,
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Number of tracked metrics.
    #[must_use]
    pub fn metric_count(&self) -> usize {
        self.metrics.len()
    }

    /// Check if a specific metric has been observed.
    #[must_use]
    pub fn has_metric(&self, name: &str) -> bool {
        self.metrics.contains_key(name)
    }

    /// Observation count for a specific metric.
    pub fn observation_count(&self, name: &str) -> Option<u64> {
        self.metrics.get(name).map(|m| m.observation_count())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // -- HoltPredictor --

    #[test]
    fn holt_constant_series() {
        let mut holt = HoltPredictor::new(0.3, 0.1);
        for _ in 0..100 {
            holt.update(42.0);
        }
        assert!((holt.level() - 42.0).abs() < 0.01);
        assert!(holt.trend().abs() < 0.01);
        assert!((holt.forecast(10.0) - 42.0).abs() < 0.1);
    }

    #[test]
    fn holt_linear_trend() {
        let mut holt = HoltPredictor::new(0.5, 0.3);
        for i in 0..100 {
            holt.update(i as f64);
        }
        let forecast_10 = holt.forecast(10.0);
        assert!(
            (forecast_10 - 109.0).abs() < 5.0,
            "forecast={forecast_10}"
        );
    }

    #[test]
    fn holt_skips_nan_and_inf() {
        let mut holt = HoltPredictor::new(0.3, 0.1);
        holt.update(10.0);
        holt.update(f64::NAN);
        holt.update(f64::INFINITY);
        holt.update(f64::NEG_INFINITY);
        assert_eq!(holt.observation_count(), 1);
        assert!((holt.level() - 10.0).abs() < 0.001);
    }

    #[test]
    fn holt_first_observation_initializes() {
        let mut holt = HoltPredictor::new(0.3, 0.1);
        assert_eq!(holt.observation_count(), 0);
        holt.update(50.0);
        assert_eq!(holt.observation_count(), 1);
        assert_eq!(holt.level(), 50.0);
        assert_eq!(holt.trend(), 0.0);
        assert_eq!(holt.forecast(5.0), 50.0);
    }

    // -- CalibrationSet --

    #[test]
    fn calibration_quantile_basic() {
        let mut cal = CalibrationSet::new(100);
        for i in 1..=10 {
            cal.push(i as f64);
        }
        // n=10, coverage=0.90: k = ceil(0.9 * 11) = 10
        assert_eq!(cal.quantile(0.90), Some(10.0));
        // n=10, coverage=0.50: k = ceil(0.5 * 11) = 6
        assert_eq!(cal.quantile(0.50), Some(6.0));
    }

    #[test]
    fn calibration_insufficient_data() {
        let mut cal = CalibrationSet::new(100);
        cal.push(1.0);
        cal.push(2.0);
        // n=2, coverage=0.95: k = ceil(0.95 * 3) = 3 > 2
        assert_eq!(cal.quantile(0.95), None);
    }

    #[test]
    fn calibration_evicts_oldest() {
        let mut cal = CalibrationSet::new(5);
        for i in 0..10 {
            cal.push(i as f64);
        }
        assert_eq!(cal.len(), 5);
        // Contains [5, 6, 7, 8, 9], sorted same
        // coverage=0.50: k = ceil(0.5 * 6) = 3 → sorted[2] = 7.0
        assert_eq!(cal.quantile(0.50), Some(7.0));
    }

    #[test]
    fn calibration_rejects_negative_and_nan() {
        let mut cal = CalibrationSet::new(100);
        cal.push(-1.0);
        cal.push(f64::NAN);
        cal.push(f64::INFINITY);
        cal.push(5.0);
        assert_eq!(cal.len(), 1);
    }

    #[test]
    fn calibration_empty() {
        let cal = CalibrationSet::new(100);
        assert_eq!(cal.quantile(0.95), None);
    }

    // -- MetricForecaster --

    #[test]
    fn metric_forecaster_constant() {
        let mut mf = MetricForecaster::new(
            "test".into(),
            0.3,
            0.1,
            &[5, 10],
            100,
            1000,
            0.95,
        );
        for _ in 0..200 {
            mf.observe(100.0);
        }
        let forecasts = mf.forecast_all();
        assert_eq!(forecasts.len(), 2);
        for fc in &forecasts {
            assert!(fc.is_calibrated(), "should be calibrated after 200 obs");
            assert!(
                (fc.point_estimate - 100.0).abs() < 1.0,
                "point={}",
                fc.point_estimate
            );
            assert!(fc.interval_width() < 5.0, "width={}", fc.interval_width());
        }
    }

    #[test]
    fn metric_forecaster_linear_growth() {
        let mut mf = MetricForecaster::new("rss".into(), 0.5, 0.3, &[10], 200, 2000, 0.90);
        for i in 0..500 {
            mf.observe(1000.0 + i as f64);
        }
        let forecasts = mf.forecast_all();
        assert_eq!(forecasts.len(), 1);
        let fc = &forecasts[0];
        assert!(fc.is_calibrated());
        assert!(
            fc.point_estimate > 1490.0 && fc.point_estimate < 1520.0,
            "point={}",
            fc.point_estimate
        );
    }

    #[test]
    fn metric_forecaster_needs_warmup() {
        let mut mf =
            MetricForecaster::new("test".into(), 0.3, 0.1, &[50], 100, 1000, 0.95);
        for i in 0..10 {
            mf.observe(i as f64);
        }
        let forecasts = mf.forecast_all();
        assert_eq!(forecasts.len(), 1);
        assert!(
            !forecasts[0].is_calibrated(),
            "should not be calibrated with only 10 obs"
        );
    }

    // -- ConformalForecaster --

    #[test]
    fn forecaster_multi_metric() {
        let config = ConformalConfig {
            horizon_steps: vec![5],
            calibration_window: 50,
            max_history: 500,
            ..Default::default()
        };
        let mut forecaster = ConformalForecaster::new(config);
        for i in 0..200 {
            forecaster.observe("rss_bytes", 1_000_000.0 + i as f64 * 100.0);
            forecaster.observe("cpu_percent", 50.0 + (i as f64 * 0.1).sin() * 10.0);
        }
        assert_eq!(forecaster.metric_count(), 2);
        let all = forecaster.forecast_all();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn forecaster_rss_alert() {
        let config = ConformalConfig {
            horizon_steps: vec![5],
            calibration_window: 50,
            max_history: 500,
            rss_alarm_fraction: 0.80,
            ..Default::default()
        };
        let mut forecaster = ConformalForecaster::new(config);
        forecaster.set_available_memory(1_000_000);
        for i in 0..200 {
            forecaster.observe("rss_bytes", 500_000.0 + i as f64 * 5000.0);
        }
        let forecasts = forecaster.forecast_metric("rss_bytes");
        let has_alert = forecasts.iter().any(|f| f.alert.is_some());
        assert!(has_alert, "should trigger RSS alert for rapidly growing memory");
    }

    #[test]
    fn forecaster_cpu_alert() {
        let config = ConformalConfig {
            horizon_steps: vec![5],
            calibration_window: 50,
            max_history: 500,
            cpu_alarm_percent: 90.0,
            ..Default::default()
        };
        let mut forecaster = ConformalForecaster::new(config);
        for i in 0..200 {
            forecaster.observe("cpu_percent", 80.0 + i as f64 * 0.1);
        }
        let forecasts = forecaster.forecast_metric("cpu_percent");
        let has_alert = forecasts.iter().any(|f| f.alert.is_some());
        assert!(has_alert, "should trigger CPU alert for high usage");
    }

    #[test]
    fn forecaster_no_alert_below_threshold() {
        let config = ConformalConfig {
            horizon_steps: vec![5],
            calibration_window: 50,
            max_history: 500,
            rss_alarm_fraction: 0.80,
            ..Default::default()
        };
        let mut forecaster = ConformalForecaster::new(config);
        forecaster.set_available_memory(10_000_000);
        for _ in 0..200 {
            forecaster.observe("rss_bytes", 1_000_000.0);
        }
        let forecasts = forecaster.forecast_metric("rss_bytes");
        let has_alert = forecasts.iter().any(|f| f.alert.is_some());
        assert!(!has_alert, "should not alert for low stable RSS");
    }

    #[test]
    fn forecast_horizon_secs_conversion() {
        let fc = ResourceForecast {
            metric_name: "test".into(),
            horizon_steps: 60,
            point_estimate: 0.0,
            lower_bound: 0.0,
            upper_bound: 0.0,
            coverage: 0.95,
            calibration_size: 0,
            alert: None,
        };
        assert_eq!(fc.horizon_secs(30), 1800);
    }

    #[test]
    fn forecast_is_calibrated_check() {
        let calibrated = ResourceForecast {
            metric_name: "a".into(),
            horizon_steps: 5,
            point_estimate: 10.0,
            lower_bound: 5.0,
            upper_bound: 15.0,
            coverage: 0.95,
            calibration_size: 100,
            alert: None,
        };
        assert!(calibrated.is_calibrated());
        assert!((calibrated.interval_width() - 10.0).abs() < 1e-10);

        let uncalibrated = ResourceForecast {
            metric_name: "b".into(),
            horizon_steps: 5,
            point_estimate: 10.0,
            lower_bound: f64::NEG_INFINITY,
            upper_bound: f64::INFINITY,
            coverage: 0.95,
            calibration_size: 0,
            alert: None,
        };
        assert!(!uncalibrated.is_calibrated());
    }

    #[test]
    fn coverage_guarantee_stationary_noise() {
        let horizon = 5;
        let coverage = 0.90;
        let mut mf =
            MetricForecaster::new("test".into(), 0.3, 0.1, &[horizon], 500, 2000, coverage);

        // Deterministic pseudo-random via LCG
        let mut rng = 12345u64;
        let next_val = |state: &mut u64| -> f64 {
            *state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let u = (*state >> 33) as f64 / (1u64 << 31) as f64;
            1000.0 + (u - 0.5) * 200.0
        };

        // Warm up
        for _ in 0..200 {
            mf.observe(next_val(&mut rng));
        }

        // Track predictions and verify later
        let mut pending: VecDeque<(f64, f64, usize)> = VecDeque::new(); // (lower, upper, due_step)
        let mut covered = 0u32;
        let mut total = 0u32;

        for step in 200..800 {
            let value = next_val(&mut rng);
            mf.observe(value);

            // Check due predictions
            while let Some(&(lower, upper, due)) = pending.front() {
                if due == step {
                    total += 1;
                    if value >= lower && value <= upper {
                        covered += 1;
                    }
                    pending.pop_front();
                } else {
                    break;
                }
            }

            // Make new prediction
            if let Some(fc) = mf.forecast_horizon(horizon) {
                if fc.is_calibrated() {
                    pending.push_back((fc.lower_bound, fc.upper_bound, step + horizon));
                }
            }
        }

        if total > 20 {
            let empirical = covered as f64 / total as f64;
            assert!(
                empirical >= coverage - 0.15,
                "empirical={empirical}, target={coverage}, total={total}, covered={covered}"
            );
        }
    }

    #[test]
    fn forecaster_with_defaults() {
        let forecaster = ConformalForecaster::with_defaults();
        assert_eq!(forecaster.metric_count(), 0);
        assert!(!forecaster.has_metric("rss"));
    }

    #[test]
    fn forecaster_observation_count() {
        let config = ConformalConfig {
            horizon_steps: vec![5],
            ..Default::default()
        };
        let mut f = ConformalForecaster::new(config);
        assert_eq!(f.observation_count("rss"), None);
        f.observe("rss", 100.0);
        f.observe("rss", 200.0);
        assert_eq!(f.observation_count("rss"), Some(2));
    }

    #[test]
    fn metric_forecaster_specific_horizon() {
        let mut mf =
            MetricForecaster::new("test".into(), 0.3, 0.1, &[5, 10, 20], 100, 1000, 0.90);
        for _ in 0..200 {
            mf.observe(50.0);
        }
        assert!(mf.forecast_horizon(5).is_some());
        assert!(mf.forecast_horizon(10).is_some());
        assert!(mf.forecast_horizon(20).is_some());
        assert!(mf.forecast_horizon(99).is_none());
    }

    #[test]
    fn forecaster_observe_snapshot_integration() {
        let config = ConformalConfig {
            horizon_steps: vec![5],
            ..Default::default()
        };
        let mut f = ConformalForecaster::new(config);

        let snapshot = crate::telemetry::ResourceSnapshot {
            pid: 1234,
            rss_bytes: 500_000_000,
            virt_bytes: 1_000_000_000,
            fd_count: 42,
            io_read_bytes: Some(1000),
            io_write_bytes: Some(2000),
            cpu_percent: Some(15.5),
            timestamp_secs: 1000,
        };

        f.observe_snapshot(&snapshot);
        assert_eq!(f.metric_count(), 6);
    }

    // -- Proptest --

    proptest! {
        #[test]
        fn proptest_holt_numerical_stability(
            values in proptest::collection::vec(-1e15_f64..1e15, 10..100)
        ) {
            let mut holt = HoltPredictor::new(0.3, 0.1);
            for v in &values {
                holt.update(*v);
            }
            prop_assert!(holt.level().is_finite(), "level={}", holt.level());
            prop_assert!(holt.trend().is_finite(), "trend={}", holt.trend());
            let forecast = holt.forecast(10.0);
            prop_assert!(forecast.is_finite(), "forecast={forecast}");
        }

        #[test]
        fn proptest_calibration_scores_nonnegative(
            scores in proptest::collection::vec(0.0_f64..1e10, 1..200)
        ) {
            let mut cal = CalibrationSet::new(500);
            for s in &scores {
                cal.push(*s);
            }
            for s in &cal.scores {
                prop_assert!(*s >= 0.0);
            }
        }

        #[test]
        fn proptest_interval_width_monotonicity(
            scores in proptest::collection::vec(0.0_f64..1000.0, 50..200)
        ) {
            let mut cal = CalibrationSet::new(500);
            for s in &scores {
                cal.push(*s);
            }

            let coverages = [0.50, 0.70, 0.80, 0.90, 0.95];
            let mut prev_q: Option<f64> = None;

            for &cov in &coverages {
                if let Some(q) = cal.quantile(cov) {
                    if let Some(pq) = prev_q {
                        prop_assert!(
                            q >= pq - 1e-10,
                            "coverage {cov}: q {q} < prev {pq}"
                        );
                    }
                    prev_q = Some(q);
                }
            }
        }

        #[test]
        fn proptest_coverage_guarantee_quantile(
            seed in 0u64..10000,
            n_cal in 50usize..200,
        ) {
            let n_test = 50;
            let total_n = n_cal + n_test;

            // Generate exchangeable (iid) scores via LCG
            let mut rng = seed.wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let mut scores: Vec<f64> = Vec::with_capacity(total_n);
            for _ in 0..total_n {
                rng = rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let u = (rng >> 33) as f64 / (1u64 << 31) as f64;
                scores.push(u * 100.0);
            }

            let coverage_target = 0.90;

            let mut cal = CalibrationSet::new(n_cal + 10);
            for s in &scores[..n_cal] {
                cal.push(*s);
            }

            if let Some(q) = cal.quantile(coverage_target) {
                let mut covered = 0;
                for s in &scores[n_cal..] {
                    if *s <= q {
                        covered += 1;
                    }
                }
                let empirical = covered as f64 / n_test as f64;
                let margin = 3.0 / (n_test as f64).sqrt();
                prop_assert!(
                    empirical >= coverage_target - margin,
                    "empirical={empirical}, target={coverage_target}, q={q}, n_cal={n_cal}"
                );
            }
        }
    }
}
