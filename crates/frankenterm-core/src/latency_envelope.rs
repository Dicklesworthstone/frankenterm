//! Observability-only min-plus latency envelope for capture -> storage -> index.
//!
//! This module builds a deterministic network-calculus certificate from a
//! token-bucket arrival curve and per-stage rate-latency service curves. The
//! runtime monitor only reports whether observed samples stayed inside that
//! certificate; it never changes scheduling, admission, batching, or retry
//! behavior.

use std::error::Error;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::network_calculus_bound::{ArrivalCurve, ServiceCurve, delay_bound};

const DEFAULT_VIOLATION_EPSILON_MS: f64 = 0.001;
const REPLAY_EPSILON_MS: f64 = 1e-9;

fn default_violation_epsilon_ms() -> f64 {
    DEFAULT_VIOLATION_EPSILON_MS
}

/// Config gate for `telemetry.latency_envelope`.
///
/// The default is off. The monitor is intentionally passive even when enabled:
/// observations produce structured decisions and counters, but no control-plane
/// action is taken here.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LatencyEnvelopeConfig {
    /// Enables the passive runtime violation monitor.
    pub enabled: bool,

    /// Slack added when comparing floating-point measured delay to the
    /// analytical bound.
    pub violation_epsilon_ms: f64,
}

impl Default for LatencyEnvelopeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            violation_epsilon_ms: default_violation_epsilon_ms(),
        }
    }
}

impl LatencyEnvelopeConfig {
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn effective_epsilon_ms(&self) -> f64 {
        if self.violation_epsilon_ms.is_finite() && self.violation_epsilon_ms >= 0.0 {
            self.violation_epsilon_ms
        } else {
            default_violation_epsilon_ms()
        }
    }
}

impl<'de> Deserialize<'de> for LatencyEnvelopeConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum ConfigRepr {
            Enabled(bool),
            Object {
                #[serde(default)]
                enabled: bool,
                #[serde(default = "default_violation_epsilon_ms")]
                violation_epsilon_ms: f64,
            },
        }

        Ok(match ConfigRepr::deserialize(deserializer)? {
            ConfigRepr::Enabled(enabled) => Self {
                enabled,
                ..Self::default()
            },
            ConfigRepr::Object {
                enabled,
                violation_epsilon_ms,
            } => Self {
                enabled,
                violation_epsilon_ms,
            },
        })
    }
}

/// Fixed end-to-end stage names for this certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyEnvelopeStage {
    Capture,
    Storage,
    Index,
}

impl LatencyEnvelopeStage {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Storage => "storage",
            Self::Index => "index",
        }
    }
}

/// One stage's guaranteed rate-latency service curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageServiceCurve {
    pub stage: LatencyEnvelopeStage,
    pub service: ServiceCurve,
}

impl StageServiceCurve {
    #[must_use]
    pub const fn new(stage: LatencyEnvelopeStage, service: ServiceCurve) -> Self {
        Self { stage, service }
    }
}

/// Errors that make a certificate or observation non-actionable.
#[derive(Debug, Clone, PartialEq)]
pub enum LatencyEnvelopeError {
    EmptyPipeline,
    InvalidComposedService,
    UnstablePipeline {
        arrival_rate: f64,
        service_rate: f64,
    },
    NonMonotonicSample {
        capture_at_ms: f64,
        storage_at_ms: f64,
        index_at_ms: f64,
    },
}

impl fmt::Display for LatencyEnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPipeline => write!(f, "latency envelope has no service stages"),
            Self::InvalidComposedService => write!(f, "latency envelope service curve is invalid"),
            Self::UnstablePipeline {
                arrival_rate,
                service_rate,
            } => write!(
                f,
                "latency envelope is unstable: arrival rate {arrival_rate} >= service rate {service_rate}"
            ),
            Self::NonMonotonicSample {
                capture_at_ms,
                storage_at_ms,
                index_at_ms,
            } => write!(
                f,
                "latency sample is non-monotonic: capture={capture_at_ms} storage={storage_at_ms} index={index_at_ms}"
            ),
        }
    }
}

impl Error for LatencyEnvelopeError {}

/// The min-plus certificate for the capture -> storage -> index path.
#[derive(Debug, Clone, PartialEq)]
pub struct LatencyEnvelopeCertificate {
    arrival: ArrivalCurve,
    stages: Vec<StageServiceCurve>,
    composed_service: ServiceCurve,
    end_to_end_bound_ms: f64,
}

impl LatencyEnvelopeCertificate {
    /// Build a certificate by min-plus composing the stage service curves.
    pub fn new(
        arrival: ArrivalCurve,
        stages: Vec<StageServiceCurve>,
    ) -> Result<Self, LatencyEnvelopeError> {
        let composed_service = compose_stage_services(&stages)?;
        let end_to_end_bound_ms = delay_bound(arrival, composed_service).ok_or_else(|| {
            LatencyEnvelopeError::UnstablePipeline {
                arrival_rate: arrival.rate(),
                service_rate: composed_service.rate(),
            }
        })?;

        Ok(Self {
            arrival,
            stages,
            composed_service,
            end_to_end_bound_ms,
        })
    }

    /// Convenience constructor for the required capture -> storage -> index
    /// envelope shape.
    pub fn capture_storage_index(
        arrival: ArrivalCurve,
        capture: ServiceCurve,
        storage: ServiceCurve,
        index: ServiceCurve,
    ) -> Result<Self, LatencyEnvelopeError> {
        Self::new(
            arrival,
            vec![
                StageServiceCurve::new(LatencyEnvelopeStage::Capture, capture),
                StageServiceCurve::new(LatencyEnvelopeStage::Storage, storage),
                StageServiceCurve::new(LatencyEnvelopeStage::Index, index),
            ],
        )
    }

    #[must_use]
    pub const fn arrival(&self) -> ArrivalCurve {
        self.arrival
    }

    #[must_use]
    pub fn stages(&self) -> &[StageServiceCurve] {
        &self.stages
    }

    #[must_use]
    pub const fn composed_service(&self) -> ServiceCurve {
        self.composed_service
    }

    #[must_use]
    pub const fn end_to_end_bound_ms(&self) -> f64 {
        self.end_to_end_bound_ms
    }
}

fn compose_stage_services(
    stages: &[StageServiceCurve],
) -> Result<ServiceCurve, LatencyEnvelopeError> {
    let mut iter = stages.iter();
    let first = iter.next().ok_or(LatencyEnvelopeError::EmptyPipeline)?;

    let mut rate = first.service.rate();
    let mut latency = first.service.latency();
    for stage in iter {
        rate = rate.min(stage.service.rate());
        latency += stage.service.latency();
        if !latency.is_finite() {
            return Err(LatencyEnvelopeError::InvalidComposedService);
        }
    }

    ServiceCurve::try_new(rate, latency).ok_or(LatencyEnvelopeError::InvalidComposedService)
}

/// One measured capture -> storage -> index sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencyEnvelopeSample {
    pub capture_at_ms: f64,
    pub storage_at_ms: f64,
    pub index_at_ms: f64,
}

impl LatencyEnvelopeSample {
    pub fn new(
        capture_at_ms: f64,
        storage_at_ms: f64,
        index_at_ms: f64,
    ) -> Result<Self, LatencyEnvelopeError> {
        let sample = Self {
            capture_at_ms,
            storage_at_ms,
            index_at_ms,
        };
        sample.validate()?;
        Ok(sample)
    }

    pub fn validate(&self) -> Result<(), LatencyEnvelopeError> {
        if self.capture_at_ms.is_finite()
            && self.storage_at_ms.is_finite()
            && self.index_at_ms.is_finite()
            && self.capture_at_ms <= self.storage_at_ms
            && self.storage_at_ms <= self.index_at_ms
        {
            Ok(())
        } else {
            Err(LatencyEnvelopeError::NonMonotonicSample {
                capture_at_ms: self.capture_at_ms,
                storage_at_ms: self.storage_at_ms,
                index_at_ms: self.index_at_ms,
            })
        }
    }

    #[must_use]
    pub fn measured_delay_ms(&self) -> f64 {
        self.index_at_ms - self.capture_at_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyEnvelopeViolationKind {
    NonMonotonicSample,
    BoundExceeded,
}

/// Passive runtime violation record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencyEnvelopeViolation {
    pub kind: LatencyEnvelopeViolationKind,
    pub sample: LatencyEnvelopeSample,
    pub measured_delay_ms: f64,
    pub bound_ms: f64,
    pub excess_ms: f64,
}

/// Successful in-bound sample report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencyEnvelopePass {
    pub sample: LatencyEnvelopeSample,
    pub measured_delay_ms: f64,
    pub bound_ms: f64,
    pub slack_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LatencyEnvelopeDecision {
    Disabled,
    WithinBound(LatencyEnvelopePass),
    Violation(LatencyEnvelopeViolation),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencyEnvelopeMonitorSnapshot {
    pub enabled: bool,
    pub observed_samples: u64,
    pub violation_count: u64,
    pub worst_excess_ms: f64,
    pub bound_ms: f64,
}

/// Runtime violation monitor for the certificate.
///
/// The monitor is intentionally side-effect free apart from its counters. Callers
/// can emit telemetry or logs from the returned decision, but this type never
/// drives control decisions.
#[derive(Debug, Clone, PartialEq)]
pub struct LatencyEnvelopeMonitor {
    config: LatencyEnvelopeConfig,
    certificate: LatencyEnvelopeCertificate,
    observed_samples: u64,
    violation_count: u64,
    worst_excess_ms: f64,
}

impl LatencyEnvelopeMonitor {
    #[must_use]
    pub fn new(config: LatencyEnvelopeConfig, certificate: LatencyEnvelopeCertificate) -> Self {
        Self {
            config,
            certificate,
            observed_samples: 0,
            violation_count: 0,
            worst_excess_ms: 0.0,
        }
    }

    pub fn observe(&mut self, sample: LatencyEnvelopeSample) -> LatencyEnvelopeDecision {
        if !self.config.enabled {
            return LatencyEnvelopeDecision::Disabled;
        }

        self.observed_samples += 1;
        if sample.validate().is_err() {
            return self.record_violation(
                LatencyEnvelopeViolationKind::NonMonotonicSample,
                sample,
                f64::NAN,
                0.0,
            );
        }

        let measured_delay_ms = sample.measured_delay_ms();
        let bound_ms = self.certificate.end_to_end_bound_ms();
        let slack_ms = bound_ms - measured_delay_ms;
        if measured_delay_ms > bound_ms + self.config.effective_epsilon_ms() {
            return self.record_violation(
                LatencyEnvelopeViolationKind::BoundExceeded,
                sample,
                measured_delay_ms,
                measured_delay_ms - bound_ms,
            );
        }

        LatencyEnvelopeDecision::WithinBound(LatencyEnvelopePass {
            sample,
            measured_delay_ms,
            bound_ms,
            slack_ms,
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> LatencyEnvelopeMonitorSnapshot {
        LatencyEnvelopeMonitorSnapshot {
            enabled: self.config.enabled,
            observed_samples: self.observed_samples,
            violation_count: self.violation_count,
            worst_excess_ms: self.worst_excess_ms,
            bound_ms: self.certificate.end_to_end_bound_ms(),
        }
    }

    fn record_violation(
        &mut self,
        kind: LatencyEnvelopeViolationKind,
        sample: LatencyEnvelopeSample,
        measured_delay_ms: f64,
        excess_ms: f64,
    ) -> LatencyEnvelopeDecision {
        self.violation_count += 1;
        if excess_ms.is_finite() {
            self.worst_excess_ms = self.worst_excess_ms.max(excess_ms);
        }

        LatencyEnvelopeDecision::Violation(LatencyEnvelopeViolation {
            kind,
            sample,
            measured_delay_ms,
            bound_ms: self.certificate.end_to_end_bound_ms(),
            excess_ms,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdversarialReplayReport {
    pub samples: Vec<LatencyEnvelopeSample>,
    pub max_delay_ms: f64,
    pub bound_ms: f64,
    pub violation_count: u64,
}

/// Replay a worst-case token-bucket burst followed by sustained arrivals
/// through the composed rate-latency service curve.
///
/// The certificate uses fluid network-calculus semantics, so the replay tracks
/// cumulative work watermarks rather than packetizing each event. Packetizing
/// would add a serialization delay per sample that is not part of
/// `delay_bound(alpha, beta) = T + b / R`.
pub fn adversarial_arrival_replay(
    certificate: &LatencyEnvelopeCertificate,
    sustained_events: usize,
) -> Result<AdversarialReplayReport, LatencyEnvelopeError> {
    let service = certificate.composed_service();
    let unit_work = 1.0;
    let burst = certificate.arrival().burst();
    let arrival_rate = certificate.arrival().rate();

    let mut samples = Vec::new();
    let mut max_delay_ms = 0.0_f64;
    let mut monitor =
        LatencyEnvelopeMonitor::new(LatencyEnvelopeConfig::enabled(), certificate.clone());

    let mut burst_watermark = 0.0_f64;
    let mut burst_remaining = burst;
    loop {
        if burst_remaining <= REPLAY_EPSILON_MS {
            break;
        }
        let work = burst_remaining.min(unit_work);
        burst_watermark += work;
        burst_remaining -= work;

        let delay_ms = service.latency() + burst_watermark / service.rate();
        max_delay_ms = max_delay_ms.max(delay_ms);
        let sample = sample_with_delay(0.0, delay_ms)?;
        let _ = monitor.observe(sample);
        samples.push(sample);
    }

    if arrival_rate > 0.0 {
        for sustained_idx in 1..=sustained_events {
            let sustained_work = sustained_idx as f64 * unit_work;
            let capture_at_ms = sustained_work / arrival_rate;
            let delay_ms = sustained_work
                .mul_add(
                    -(1.0 / arrival_rate - 1.0 / service.rate()),
                    service.latency() + burst / service.rate(),
                )
                .max(service.latency());
            max_delay_ms = max_delay_ms.max(delay_ms);
            let sample = sample_with_delay(capture_at_ms, delay_ms)?;
            let _ = monitor.observe(sample);
            samples.push(sample);
        }
    }

    Ok(AdversarialReplayReport {
        samples,
        max_delay_ms,
        bound_ms: certificate.end_to_end_bound_ms(),
        violation_count: monitor.snapshot().violation_count,
    })
}

fn sample_with_delay(
    capture_at_ms: f64,
    delay_ms: f64,
) -> Result<LatencyEnvelopeSample, LatencyEnvelopeError> {
    let storage_at_ms = delay_ms.mul_add(0.5, capture_at_ms);
    let index_at_ms = capture_at_ms + delay_ms;
    LatencyEnvelopeSample::new(capture_at_ms, storage_at_ms, index_at_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn certificate_for_test() -> LatencyEnvelopeCertificate {
        LatencyEnvelopeCertificate::capture_storage_index(
            ArrivalCurve::new(24.0, 60.0),
            ServiceCurve::new(200.0, 1.0),
            ServiceCurve::new(120.0, 4.0),
            ServiceCurve::new(150.0, 3.0),
        )
        .unwrap()
    }

    #[test]
    fn latency_envelope_config_defaults_off() {
        let config = LatencyEnvelopeConfig::default();
        assert!(!config.enabled);
        assert!(approx(
            config.violation_epsilon_ms,
            DEFAULT_VIOLATION_EPSILON_MS
        ));

        let parsed_bool: LatencyEnvelopeConfig = serde_json::from_str("true").unwrap();
        assert!(parsed_bool.enabled);
        assert!(approx(
            parsed_bool.violation_epsilon_ms,
            DEFAULT_VIOLATION_EPSILON_MS
        ));

        let parsed_object: LatencyEnvelopeConfig =
            serde_json::from_str(r#"{"enabled":true,"violation_epsilon_ms":0.25}"#).unwrap();
        assert!(parsed_object.enabled);
        assert!(approx(parsed_object.violation_epsilon_ms, 0.25));
    }

    #[test]
    fn certificate_composes_capture_storage_index_min_plus_bound() {
        let certificate = certificate_for_test();

        assert!(approx(certificate.composed_service().rate(), 120.0));
        assert!(approx(certificate.composed_service().latency(), 8.0));
        assert!(approx(certificate.end_to_end_bound_ms(), 8.2));
        assert_eq!(certificate.stages().len(), 3);
        assert_eq!(certificate.stages()[0].stage.as_str(), "capture");
        assert_eq!(certificate.stages()[1].stage.as_str(), "storage");
        assert_eq!(certificate.stages()[2].stage.as_str(), "index");
    }

    #[test]
    fn certificate_rejects_unstable_pipeline() {
        let err = LatencyEnvelopeCertificate::capture_storage_index(
            ArrivalCurve::new(1.0, 200.0),
            ServiceCurve::new(200.0, 1.0),
            ServiceCurve::new(120.0, 4.0),
            ServiceCurve::new(150.0, 3.0),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            LatencyEnvelopeError::UnstablePipeline {
                arrival_rate: 200.0,
                service_rate: 120.0
            }
        ));
    }

    #[test]
    fn monitor_is_default_off_and_observability_only() {
        let certificate = certificate_for_test();
        let mut monitor =
            LatencyEnvelopeMonitor::new(LatencyEnvelopeConfig::default(), certificate);
        let sample = LatencyEnvelopeSample::new(10.0, 11.0, 12.0).unwrap();

        assert_eq!(monitor.observe(sample), LatencyEnvelopeDecision::Disabled);
        assert_eq!(monitor.snapshot().observed_samples, 0);
        assert_eq!(monitor.snapshot().violation_count, 0);
    }

    #[test]
    fn monitor_flags_bound_violation_without_control_action() {
        let certificate = certificate_for_test();
        let bound = certificate.end_to_end_bound_ms();
        let mut monitor =
            LatencyEnvelopeMonitor::new(LatencyEnvelopeConfig::enabled(), certificate);
        let sample = LatencyEnvelopeSample::new(1.0, 4.0, 1.0 + bound + 5.0).unwrap();

        let decision = monitor.observe(sample);
        let LatencyEnvelopeDecision::Violation(violation) = decision else {
            panic!("expected a passive violation record, got {decision:?}");
        };

        assert_eq!(violation.kind, LatencyEnvelopeViolationKind::BoundExceeded);
        assert!(approx(violation.excess_ms, 5.0));
        assert_eq!(monitor.snapshot().observed_samples, 1);
        assert_eq!(monitor.snapshot().violation_count, 1);
    }

    #[test]
    fn monitor_flags_non_monotonic_stage_timestamps() {
        let certificate = certificate_for_test();
        let mut monitor =
            LatencyEnvelopeMonitor::new(LatencyEnvelopeConfig::enabled(), certificate);
        let sample = LatencyEnvelopeSample {
            capture_at_ms: 3.0,
            storage_at_ms: 2.0,
            index_at_ms: 4.0,
        };

        let decision = monitor.observe(sample);
        let LatencyEnvelopeDecision::Violation(violation) = decision else {
            panic!("expected non-monotonic violation, got {decision:?}");
        };
        assert_eq!(
            violation.kind,
            LatencyEnvelopeViolationKind::NonMonotonicSample
        );
    }

    #[test]
    fn adversarial_arrival_replay_stays_inside_composed_bound() {
        let certificate = certificate_for_test();
        let report = adversarial_arrival_replay(&certificate, 64).unwrap();

        assert!(!report.samples.is_empty());
        assert_eq!(report.violation_count, 0);
        assert!(
            report.max_delay_ms <= report.bound_ms + REPLAY_EPSILON_MS,
            "max delay {} should stay inside bound {}",
            report.max_delay_ms,
            report.bound_ms
        );
    }

    #[test]
    fn adversarial_replay_property_grid_never_violates_bound() {
        let bursts = [0.5, 1.0, 4.0, 9.0, 16.0, 33.0];
        let arrival_rates = [0.0, 5.0, 25.0, 60.0];
        let service_sets = [
            (
                ServiceCurve::new(200.0, 1.0),
                ServiceCurve::new(150.0, 2.0),
                ServiceCurve::new(180.0, 1.5),
            ),
            (
                ServiceCurve::new(90.0, 3.0),
                ServiceCurve::new(120.0, 4.0),
                ServiceCurve::new(100.0, 2.0),
            ),
            (
                ServiceCurve::new(500.0, 0.5),
                ServiceCurve::new(250.0, 1.0),
                ServiceCurve::new(125.0, 4.0),
            ),
        ];

        for burst in bursts {
            for arrival_rate in arrival_rates {
                for (capture, storage, index) in service_sets {
                    let certificate = LatencyEnvelopeCertificate::capture_storage_index(
                        ArrivalCurve::new(burst, arrival_rate),
                        capture,
                        storage,
                        index,
                    )
                    .unwrap();
                    let report = adversarial_arrival_replay(&certificate, 128).unwrap();
                    assert_eq!(report.violation_count, 0);
                    assert!(
                        report.max_delay_ms <= report.bound_ms + REPLAY_EPSILON_MS,
                        "burst={burst} rate={arrival_rate} max={} bound={}",
                        report.max_delay_ms,
                        report.bound_ms
                    );
                }
            }
        }
    }
}
