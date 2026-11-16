//! SLO and validation support for latency-stage decomposition.

use super::InvariantDomain;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt;

// ── F5: Input-to-Paint QoE Guardrail Lane ──────────────────────────

/// QoE metric kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QoEMetric {
    /// Input-to-first-paint latency (μs).
    InputToPaint,
    /// Frame-to-frame jitter (μs).
    FrameJitter,
    /// Smoothness score (0.0..=1.0, 1.0 = perfect).
    Smoothness,
    /// Keystroke echo latency (μs).
    KeystrokeEcho,
}

impl fmt::Display for QoEMetric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputToPaint => write!(f, "input-to-paint"),
            Self::FrameJitter => write!(f, "frame-jitter"),
            Self::Smoothness => write!(f, "smoothness"),
            Self::KeystrokeEcho => write!(f, "keystroke-echo"),
        }
    }
}

/// SLO target for a QoE metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QoESLO {
    /// Metric being targeted.
    pub metric: QoEMetric,
    /// Target value (latency: max μs, smoothness: min score).
    pub target: f64,
    /// Percentile this target applies to (e.g., 0.95 for p95).
    pub percentile: f64,
    /// Human-readable description.
    pub description: String,
}

/// A single QoE measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QoEMeasurement {
    /// Metric.
    pub metric: QoEMetric,
    /// Measured value.
    pub value: f64,
    /// Timestamp (μs).
    pub timestamp_us: u64,
}

/// QoE guardrail configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QoEGuardrailConfig {
    /// SLO targets.
    pub slos: Vec<QoESLO>,
    /// Window size for rolling statistics (number of samples).
    pub window_size: usize,
    /// Minimum samples before SLO evaluation.
    pub min_samples: usize,
}

impl Default for QoEGuardrailConfig {
    fn default() -> Self {
        Self {
            slos: vec![
                QoESLO {
                    metric: QoEMetric::InputToPaint,
                    target: 16_667.0, // 16.67ms = 60fps frame budget.
                    percentile: 0.95,
                    description: "p95 input-to-paint under 16.67ms".to_string(),
                },
                QoESLO {
                    metric: QoEMetric::FrameJitter,
                    target: 4_000.0, // 4ms max jitter.
                    percentile: 0.99,
                    description: "p99 frame jitter under 4ms".to_string(),
                },
                QoESLO {
                    metric: QoEMetric::Smoothness,
                    target: 0.90,
                    percentile: 0.50,
                    description: "median smoothness above 0.90".to_string(),
                },
                QoESLO {
                    metric: QoEMetric::KeystrokeEcho,
                    target: 50_000.0, // 50ms.
                    percentile: 0.99,
                    description: "p99 keystroke echo under 50ms".to_string(),
                },
            ],
            window_size: 1000,
            min_samples: 30,
        }
    }
}

/// Invalid QoE guardrail configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QoEGuardrailConfigError {
    /// SLO target is NaN or infinite.
    NonFiniteTarget,
    /// SLO percentile is NaN or infinite.
    NonFinitePercentile,
    /// SLO percentile is outside the inclusive 0.0..=1.0 range.
    PercentileOutOfRange,
    /// Rolling windows must retain at least one sample.
    ZeroWindowSize,
    /// SLO evaluation must require at least one sample.
    ZeroMinSamples,
    /// Minimum samples cannot exceed the retained rolling window.
    MinSamplesExceedsWindow,
}

impl QoESLO {
    /// Validate a single SLO target.
    pub fn validate(&self) -> Result<(), QoEGuardrailConfigError> {
        if !self.target.is_finite() {
            return Err(QoEGuardrailConfigError::NonFiniteTarget);
        }
        validate_percentile(self.percentile)
    }
}

impl QoEGuardrailConfig {
    /// Validate serde/operator supplied QoE guardrail settings before use.
    pub fn validate(&self) -> Result<(), QoEGuardrailConfigError> {
        if self.window_size == 0 {
            return Err(QoEGuardrailConfigError::ZeroWindowSize);
        }
        if self.min_samples == 0 {
            return Err(QoEGuardrailConfigError::ZeroMinSamples);
        }
        if self.min_samples > self.window_size {
            return Err(QoEGuardrailConfigError::MinSamplesExceedsWindow);
        }
        for slo in &self.slos {
            slo.validate()?;
        }
        Ok(())
    }

    fn validated_or_default(self) -> Self {
        if self.validate().is_ok() {
            self
        } else {
            Self::default()
        }
    }
}

fn validate_percentile(percentile: f64) -> Result<(), QoEGuardrailConfigError> {
    if !percentile.is_finite() {
        return Err(QoEGuardrailConfigError::NonFinitePercentile);
    }
    if !(0.0..=1.0).contains(&percentile) {
        return Err(QoEGuardrailConfigError::PercentileOutOfRange);
    }
    Ok(())
}

fn percentile_index(percentile: f64, len: usize) -> Option<usize> {
    if len == 0 || validate_percentile(percentile).is_err() {
        return None;
    }
    Some(((percentile * len as f64) as usize).min(len - 1))
}

/// SLO evaluation result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SLOVerdict {
    /// Meeting the target.
    Met { measured: f64, target: f64 },
    /// Breaching the target.
    Breached { measured: f64, target: f64 },
    /// Not enough samples.
    InsufficientData { samples: usize, required: usize },
}

impl fmt::Display for SLOVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Met { measured, target } => write!(f, "met({measured:.1}/{target:.1})"),
            Self::Breached { measured, target } => {
                write!(f, "breached({measured:.1}/{target:.1})")
            }
            Self::InsufficientData { samples, required } => {
                write!(f, "insufficient({samples}/{required})")
            }
        }
    }
}

/// Snapshot of the QoE guardrail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QoEGuardrailSnapshot {
    /// Per-SLO verdicts.
    pub verdicts: Vec<(QoEMetric, SLOVerdict)>,
    /// Total measurements recorded.
    pub total_measurements: u64,
    /// Number of SLO breaches.
    pub breach_count: u64,
}

/// Degradation state for the guardrail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QoEDegradation {
    /// All SLOs met.
    Healthy,
    /// Some SLOs breached.
    SLOBreach { breach_count: u64 },
    /// Not enough data to evaluate.
    WarmingUp { samples: usize },
}

impl fmt::Display for QoEDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::SLOBreach { breach_count } => write!(f, "slo-breach({breach_count})"),
            Self::WarmingUp { samples } => write!(f, "warming-up({samples})"),
        }
    }
}

/// Log entry for QoE events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QoELogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Metric.
    pub metric: QoEMetric,
    /// Event description.
    pub event: String,
}

/// Manages QoE guardrail measurements and SLO evaluation.
pub struct QoEGuardrail {
    config: QoEGuardrailConfig,
    /// Per-metric rolling window of values.
    windows: HashMap<QoEMetric, VecDeque<f64>>,
    total_measurements: u64,
}

impl QoEGuardrail {
    /// Create a new guardrail.
    pub fn new(config: QoEGuardrailConfig) -> Self {
        let config = config.validated_or_default();
        Self {
            config,
            windows: HashMap::new(),
            total_measurements: 0,
        }
    }

    /// Try to create a new guardrail, rejecting malformed configuration.
    pub fn try_new(config: QoEGuardrailConfig) -> Result<Self, QoEGuardrailConfigError> {
        config.validate()?;
        Ok(Self::new(config))
    }

    /// Record a measurement.
    pub fn record(&mut self, measurement: QoEMeasurement) {
        if !measurement.value.is_finite() {
            return;
        }
        let window = self.windows.entry(measurement.metric).or_default();
        window.push_back(measurement.value);
        if window.len() > self.config.window_size {
            window.pop_front();
        }
        self.total_measurements += 1;
    }

    /// Evaluate a single SLO.
    pub fn evaluate_slo(&self, slo: &QoESLO) -> SLOVerdict {
        let window = match self.windows.get(&slo.metric) {
            Some(w) => w,
            None => {
                return SLOVerdict::InsufficientData {
                    samples: 0,
                    required: self.config.min_samples,
                };
            }
        };
        if window.len() < self.config.min_samples {
            return SLOVerdict::InsufficientData {
                samples: window.len(),
                required: self.config.min_samples,
            };
        }
        if slo.validate().is_err() {
            return SLOVerdict::InsufficientData {
                samples: window.len(),
                required: self.config.min_samples,
            };
        }
        let mut sorted: Vec<f64> = window.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let Some(idx) = percentile_index(slo.percentile, sorted.len()) else {
            return SLOVerdict::InsufficientData {
                samples: window.len(),
                required: self.config.min_samples,
            };
        };
        let measured = sorted[idx];
        // For smoothness, higher is better (target is minimum).
        // For latency/jitter, lower is better (target is maximum).
        let is_met = match slo.metric {
            QoEMetric::Smoothness => measured >= slo.target,
            _ => measured <= slo.target,
        };
        if is_met {
            SLOVerdict::Met {
                measured,
                target: slo.target,
            }
        } else {
            SLOVerdict::Breached {
                measured,
                target: slo.target,
            }
        }
    }

    /// Evaluate all SLOs.
    pub fn evaluate_all(&self) -> Vec<(QoEMetric, SLOVerdict)> {
        self.config
            .slos
            .iter()
            .map(|slo| (slo.metric, self.evaluate_slo(slo)))
            .collect()
    }

    /// Get a snapshot.
    pub fn snapshot(&self) -> QoEGuardrailSnapshot {
        let verdicts = self.evaluate_all();
        let breach_count = verdicts
            .iter()
            .filter(|(_, v)| matches!(v, SLOVerdict::Breached { .. }))
            .count() as u64;
        QoEGuardrailSnapshot {
            verdicts,
            total_measurements: self.total_measurements,
            breach_count,
        }
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> QoEDegradation {
        let verdicts = self.evaluate_all();
        let breach_count = verdicts
            .iter()
            .filter(|(_, v)| matches!(v, SLOVerdict::Breached { .. }))
            .count() as u64;
        if breach_count > 0 {
            return QoEDegradation::SLOBreach { breach_count };
        }
        if verdicts
            .iter()
            .any(|(_, v)| matches!(v, SLOVerdict::InsufficientData { .. }))
        {
            let total_samples: usize = self.windows.values().map(|w| w.len()).sum();
            return QoEDegradation::WarmingUp {
                samples: total_samples,
            };
        }
        QoEDegradation::Healthy
    }

    /// Create a log entry.
    pub fn log_entry(&self, metric: QoEMetric, event: String, timestamp_us: u64) -> QoELogEntry {
        QoELogEntry {
            timestamp_us,
            metric,
            event,
        }
    }

    /// Reset all windows.
    pub fn reset(&mut self) {
        self.windows.clear();
        self.total_measurements = 0;
    }

    /// Total measurements recorded.
    pub fn total_measurements(&self) -> u64 {
        self.total_measurements
    }

    /// Window size for a metric.
    pub fn window_len(&self, metric: QoEMetric) -> usize {
        self.windows.get(&metric).map_or(0, |w| w.len())
    }

    /// Access config.
    pub fn config(&self) -> &QoEGuardrailConfig {
        &self.config
    }

    /// Map to InvariantDomain.
    pub fn to_invariant_domain() -> InvariantDomain {
        InvariantDomain::Composition
    }

    // ── F5 Impl: Bridge methods ──

    /// Number of SLOs currently being tracked.
    pub fn slo_count(&self) -> usize {
        self.config.slos.len()
    }

    /// Number of SLOs currently met.
    pub fn met_count(&self) -> usize {
        self.evaluate_all()
            .iter()
            .filter(|(_, v)| matches!(v, SLOVerdict::Met { .. }))
            .count()
    }

    /// Number of SLOs currently breached.
    pub fn breach_count(&self) -> usize {
        self.evaluate_all()
            .iter()
            .filter(|(_, v)| matches!(v, SLOVerdict::Breached { .. }))
            .count()
    }

    /// SLO compliance rate (met / evaluable).
    pub fn compliance_rate(&self) -> f64 {
        let verdicts = self.evaluate_all();
        let evaluable = verdicts
            .iter()
            .filter(|(_, v)| !matches!(v, SLOVerdict::InsufficientData { .. }))
            .count();
        if evaluable == 0 {
            return 1.0;
        }
        let met = verdicts
            .iter()
            .filter(|(_, v)| matches!(v, SLOVerdict::Met { .. }))
            .count();
        met as f64 / evaluable as f64
    }

    /// Record a batch of measurements for a single metric.
    pub fn record_batch(&mut self, metric: QoEMetric, values: &[f64], start_us: u64) {
        for (i, v) in values.iter().enumerate() {
            self.record(QoEMeasurement {
                metric,
                value: *v,
                timestamp_us: start_us + i as u64,
            });
        }
    }

    /// Get the current percentile value for a metric (None if insufficient data).
    pub fn current_percentile(&self, metric: QoEMetric, percentile: f64) -> Option<f64> {
        let window = self.windows.get(&metric)?;
        if window.len() < self.config.min_samples {
            return None;
        }
        let mut sorted: Vec<f64> = window.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        percentile_index(percentile, sorted.len()).map(|idx| sorted[idx])
    }

    /// Whether all SLOs are met (or insufficient data).
    pub fn all_slos_met(&self) -> bool {
        self.evaluate_all()
            .iter()
            .all(|(_, v)| !matches!(v, SLOVerdict::Breached { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn slo(metric: QoEMetric, target: f64) -> QoESLO {
        QoESLO {
            metric,
            target,
            percentile: 0.95,
            description: "test".to_string(),
        }
    }

    #[test]
    fn degradation_stays_warming_until_each_slo_is_evaluable() {
        let config = QoEGuardrailConfig {
            slos: vec![
                slo(QoEMetric::InputToPaint, 20_000.0),
                slo(QoEMetric::FrameJitter, 5_000.0),
            ],
            window_size: 100,
            min_samples: 5,
        };
        let mut guard = QoEGuardrail::new(config);
        for i in 0..5 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::InputToPaint,
                value: 10_000.0,
                timestamp_us: i,
            });
        }

        assert!(matches!(
            guard.detect_degradation(),
            QoEDegradation::WarmingUp { samples: 5 }
        ));
    }

    #[test]
    fn degradation_reports_breach_even_with_other_slos_warming() {
        let config = QoEGuardrailConfig {
            slos: vec![
                slo(QoEMetric::InputToPaint, 5_000.0),
                slo(QoEMetric::FrameJitter, 5_000.0),
            ],
            window_size: 100,
            min_samples: 5,
        };
        let mut guard = QoEGuardrail::new(config);
        for i in 0..5 {
            guard.record(QoEMeasurement {
                metric: QoEMetric::InputToPaint,
                value: 10_000.0,
                timestamp_us: i,
            });
        }

        assert!(matches!(
            guard.detect_degradation(),
            QoEDegradation::SLOBreach { breach_count: 1 }
        ));
    }

    #[test]
    fn config_validation_rejects_invalid_percentiles_and_targets() {
        let mut config = QoEGuardrailConfig::default();
        config.slos[0].percentile = f64::NAN;
        assert_eq!(
            config.validate(),
            Err(QoEGuardrailConfigError::NonFinitePercentile)
        );

        let mut config = QoEGuardrailConfig::default();
        config.slos[0].percentile = 1.25;
        assert_eq!(
            config.validate(),
            Err(QoEGuardrailConfigError::PercentileOutOfRange)
        );

        let mut config = QoEGuardrailConfig::default();
        config.slos[0].target = f64::INFINITY;
        assert_eq!(
            config.validate(),
            Err(QoEGuardrailConfigError::NonFiniteTarget)
        );

        let mut config = QoEGuardrailConfig::default();
        config.min_samples = config.window_size + 1;
        assert_eq!(
            config.validate(),
            Err(QoEGuardrailConfigError::MinSamplesExceedsWindow)
        );
    }

    #[test]
    fn new_normalizes_invalid_config_and_try_new_rejects_it() {
        let mut config = QoEGuardrailConfig::default();
        config.slos[0].percentile = f64::NEG_INFINITY;

        assert_eq!(
            QoEGuardrail::try_new(config.clone()).map(|_| ()),
            Err(QoEGuardrailConfigError::NonFinitePercentile)
        );

        let guard = QoEGuardrail::new(config);
        assert_eq!(guard.config(), &QoEGuardrailConfig::default());
    }

    #[test]
    fn non_finite_measurements_are_ignored() {
        let config = QoEGuardrailConfig {
            window_size: 8,
            min_samples: 1,
            ..Default::default()
        };
        let mut guard = QoEGuardrail::new(config);

        guard.record_batch(
            QoEMetric::InputToPaint,
            &[f64::NAN, 12.0, f64::INFINITY, f64::NEG_INFINITY],
            100,
        );

        assert_eq!(guard.total_measurements(), 1);
        assert_eq!(guard.window_len(QoEMetric::InputToPaint), 1);
        assert_eq!(
            guard.current_percentile(QoEMetric::InputToPaint, 0.50),
            Some(12.0)
        );
    }

    #[test]
    fn invalid_current_percentile_returns_none() {
        let config = QoEGuardrailConfig {
            window_size: 8,
            min_samples: 1,
            ..Default::default()
        };
        let mut guard = QoEGuardrail::new(config);
        guard.record(QoEMeasurement {
            metric: QoEMetric::InputToPaint,
            value: 12.0,
            timestamp_us: 1,
        });

        assert_eq!(
            guard.current_percentile(QoEMetric::InputToPaint, f64::NAN),
            None
        );
        assert_eq!(
            guard.current_percentile(QoEMetric::InputToPaint, -0.1),
            None
        );
        assert_eq!(guard.current_percentile(QoEMetric::InputToPaint, 1.1), None);
    }

    proptest! {
        #[test]
        fn proptest_config_validation_rejects_bad_percentiles(
            bad_percentile in prop_oneof![
                Just(f64::NAN),
                Just(f64::INFINITY),
                Just(f64::NEG_INFINITY),
                -1000.0f64..0.0,
                1.000_001f64..1000.0,
            ],
        ) {
            let mut config = QoEGuardrailConfig::default();
            config.slos[0].percentile = bad_percentile;

            prop_assert!(matches!(
                config.validate(),
                Err(QoEGuardrailConfigError::NonFinitePercentile | QoEGuardrailConfigError::PercentileOutOfRange)
            ));
            prop_assert!(QoEGuardrail::try_new(config.clone()).is_err());
            let guardrail = QoEGuardrail::new(config);
            prop_assert_eq!(guardrail.config(), &QoEGuardrailConfig::default());
        }

        #[test]
        fn proptest_guardrail_retains_only_finite_samples(
            values in prop::collection::vec(
                prop_oneof![
                    Just(f64::NAN),
                    Just(f64::INFINITY),
                    Just(f64::NEG_INFINITY),
                    -1_000_000.0f64..1_000_000.0,
                ],
                0..64
            ),
        ) {
            let config = QoEGuardrailConfig {
                window_size: 128,
                min_samples: 1,
                ..Default::default()
            };
            let mut guard = QoEGuardrail::new(config);
            guard.record_batch(QoEMetric::InputToPaint, &values, 0);

            let finite_count = values.iter().filter(|value| value.is_finite()).count();
            prop_assert_eq!(guard.total_measurements(), finite_count as u64);
            prop_assert_eq!(guard.window_len(QoEMetric::InputToPaint), finite_count);

            if finite_count == 0 {
                prop_assert_eq!(guard.current_percentile(QoEMetric::InputToPaint, 0.50), None);
            } else {
                prop_assert!(
                    guard
                        .current_percentile(QoEMetric::InputToPaint, 0.50)
                        .is_some_and(f64::is_finite)
                );
            }
        }
    }
}
