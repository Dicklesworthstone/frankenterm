//! Lindley-bounds release-attestation telemetry.

use super::{EnforcerSnapshot, LatencyStage};
use crate::network_calculus_bound::{ArrivalCurve, ServiceCurve, StageModel};
use serde::{Deserialize, Serialize};

/// Three-stage 4KB overlap benchmark path used by the current
/// release-attestation `stages` array. Wider claim surfaces use
/// explicit helpers below so they do not silently inherit the 4KB
/// benchmark's proof status.
pub const LINDLEY_ATTESTATION_STAGES: &[LatencyStage] = &[
    LatencyStage::PtyCapture,
    LatencyStage::DeltaExtraction,
    LatencyStage::StorageWrite,
];

/// Full PTY-to-event capture path. This model exists so release tooling
/// can bind `PatternDetection` and `EventEmission` deliberately instead
/// of citing the three-stage 4KB benchmark as end-to-end proof.
pub const LINDLEY_END_TO_END_CAPTURE_STAGES: &[LatencyStage] = LatencyStage::CAPTURE_PATH;

/// Default burst for the Lindley-bounds attestation arrival curve.
pub const LINDLEY_ATTESTATION_BURST_EVENTS: f64 = 10.0;

/// Default attestation arrival rate.
///
/// The operator-facing derivation documents the worst-case rate as
/// 100 events/ms. The substrate's strict stability check requires
/// arrival rate below the slowest service rate, so the release
/// attestation uses a 10% steady-state margin.
pub const LINDLEY_ATTESTATION_ARRIVAL_RATE_EVENTS_PER_MS: f64 = 90.0;

/// Per-stage telemetry consumed by the Lindley-bounds attestation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LindleyStageTelemetry {
    pub stage: LatencyStage,
    pub service_rate_events_per_ms: f64,
    pub p99_latency_ms: f64,
}

impl LindleyStageTelemetry {
    /// Create a validated stage telemetry row.
    pub fn try_new(
        stage: LatencyStage,
        service_rate_events_per_ms: f64,
        p99_latency_ms: f64,
    ) -> Result<Self, String> {
        if stage.is_aggregate() {
            return Err(format!(
                "aggregate stage {stage} is not a Lindley service stage"
            ));
        }
        if !service_rate_events_per_ms.is_finite() || service_rate_events_per_ms <= 0.0 {
            return Err(format!(
                "{stage}: service_rate_events_per_ms must be finite and positive"
            ));
        }
        if !p99_latency_ms.is_finite() || p99_latency_ms < 0.0 {
            return Err(format!(
                "{stage}: p99_latency_ms must be finite and non-negative"
            ));
        }
        Ok(Self {
            stage,
            service_rate_events_per_ms,
            p99_latency_ms,
        })
    }

    /// Convert this telemetry row into the network-calculus stage model.
    pub fn to_stage_model(self) -> StageModel {
        StageModel::new(
            lindley_stage_name(self.stage),
            ServiceCurve::new(self.service_rate_events_per_ms, self.p99_latency_ms),
        )
    }
}

/// Arrival and stage telemetry used to build the Lindley attestation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LindleyTelemetryModel {
    pub arrival_burst_events: f64,
    pub arrival_rate_events_per_ms: f64,
    pub stages: Vec<LindleyStageTelemetry>,
}

impl LindleyTelemetryModel {
    /// Documentation-derived defaults for the currently published 4KB
    /// overlap benchmark used when release jobs do not pass live
    /// telemetry.
    pub fn documented_default() -> Self {
        Self::documented_capture_benchmark_default()
    }

    /// Documentation-derived defaults for the three-stage 4KB overlap
    /// benchmark.
    pub fn documented_capture_benchmark_default() -> Self {
        Self {
            arrival_burst_events: LINDLEY_ATTESTATION_BURST_EVENTS,
            arrival_rate_events_per_ms: LINDLEY_ATTESTATION_ARRIVAL_RATE_EVENTS_PER_MS,
            stages: vec![
                LindleyStageTelemetry {
                    stage: LatencyStage::PtyCapture,
                    service_rate_events_per_ms: 200.0,
                    p99_latency_ms: 1.0,
                },
                LindleyStageTelemetry {
                    stage: LatencyStage::DeltaExtraction,
                    service_rate_events_per_ms: 150.0,
                    p99_latency_ms: 2.0,
                },
                LindleyStageTelemetry {
                    stage: LatencyStage::StorageWrite,
                    service_rate_events_per_ms: 100.0,
                    p99_latency_ms: 5.0,
                },
            ],
        }
    }

    /// Documentation-derived model for the full PTY-to-event capture
    /// path. The first three rows are the current benchmark rows; the
    /// final two rows are the checked-in p99 budget ceilings for pattern
    /// detection and event emission. That makes the stage chain complete
    /// without pretending the release artifact has an empirical
    /// agreement row for the wider path yet.
    pub fn documented_end_to_end_capture_default() -> Self {
        Self {
            arrival_burst_events: LINDLEY_ATTESTATION_BURST_EVENTS,
            arrival_rate_events_per_ms: LINDLEY_ATTESTATION_ARRIVAL_RATE_EVENTS_PER_MS,
            stages: vec![
                LindleyStageTelemetry {
                    stage: LatencyStage::PtyCapture,
                    service_rate_events_per_ms: 200.0,
                    p99_latency_ms: 1.0,
                },
                LindleyStageTelemetry {
                    stage: LatencyStage::DeltaExtraction,
                    service_rate_events_per_ms: 150.0,
                    p99_latency_ms: 2.0,
                },
                LindleyStageTelemetry {
                    stage: LatencyStage::StorageWrite,
                    service_rate_events_per_ms: 100.0,
                    p99_latency_ms: 5.0,
                },
                LindleyStageTelemetry {
                    stage: LatencyStage::PatternDetection,
                    service_rate_events_per_ms: 100.0,
                    p99_latency_ms: 10.0,
                },
                LindleyStageTelemetry {
                    stage: LatencyStage::EventEmission,
                    service_rate_events_per_ms: 100.0,
                    p99_latency_ms: 5.0,
                },
            ],
        }
    }

    /// Build the attestation model from a live budget-enforcer snapshot
    /// plus per-stage service rates from the bench/runtime harness.
    pub fn from_enforcer_snapshot(
        snapshot: &EnforcerSnapshot,
        arrival_burst_events: f64,
        arrival_rate_events_per_ms: f64,
        service_rates: &[(LatencyStage, f64)],
    ) -> Result<Self, String> {
        let mut stages = Vec::with_capacity(LINDLEY_ATTESTATION_STAGES.len());
        for &stage in LINDLEY_ATTESTATION_STAGES {
            let service_rate_events_per_ms = service_rates
                .iter()
                .find_map(|(candidate, rate)| (*candidate == stage).then_some(*rate))
                .ok_or_else(|| format!("{stage}: missing service rate"))?;
            let snapshot = snapshot
                .stages
                .iter()
                .find(|candidate| candidate.stage == stage)
                .ok_or_else(|| format!("{stage}: missing enforcer snapshot"))?;
            let p99_us = snapshot
                .percentiles
                .p99_us
                .ok_or_else(|| format!("{stage}: missing p99 latency"))?;
            stages.push(LindleyStageTelemetry::try_new(
                stage,
                service_rate_events_per_ms,
                p99_us / 1000.0,
            )?);
        }

        Self::try_new(arrival_burst_events, arrival_rate_events_per_ms, stages)
    }

    /// Create a validated telemetry model from explicit rows.
    pub fn try_new(
        arrival_burst_events: f64,
        arrival_rate_events_per_ms: f64,
        stages: Vec<LindleyStageTelemetry>,
    ) -> Result<Self, String> {
        if !arrival_burst_events.is_finite() || arrival_burst_events < 0.0 {
            return Err("arrival_burst_events must be finite and non-negative".to_string());
        }
        if !arrival_rate_events_per_ms.is_finite() || arrival_rate_events_per_ms <= 0.0 {
            return Err("arrival_rate_events_per_ms must be finite and positive".to_string());
        }
        if stages.is_empty() {
            return Err("at least one Lindley stage is required".to_string());
        }
        for stage in &stages {
            LindleyStageTelemetry::try_new(
                stage.stage,
                stage.service_rate_events_per_ms,
                stage.p99_latency_ms,
            )?;
        }
        Ok(Self {
            arrival_burst_events,
            arrival_rate_events_per_ms,
            stages,
        })
    }

    /// Convert live telemetry into the network-calculus substrate inputs.
    pub fn to_network_calculus_inputs(&self) -> Result<(ArrivalCurve, Vec<StageModel>), String> {
        Self::try_new(
            self.arrival_burst_events,
            self.arrival_rate_events_per_ms,
            self.stages.clone(),
        )?;
        Ok((
            ArrivalCurve::new(self.arrival_burst_events, self.arrival_rate_events_per_ms),
            self.stages
                .iter()
                .copied()
                .map(LindleyStageTelemetry::to_stage_model)
                .collect(),
        ))
    }
}

fn lindley_stage_name(stage: LatencyStage) -> &'static str {
    match stage {
        LatencyStage::PtyCapture => "capture",
        LatencyStage::DeltaExtraction => "delta_extract",
        LatencyStage::StorageWrite => "storage_write",
        LatencyStage::PatternDetection => "pattern_detect",
        LatencyStage::EventEmission => "event_emit",
        LatencyStage::WorkflowDispatch => "workflow_dispatch",
        LatencyStage::ActionExecution => "action_execute",
        LatencyStage::ApiResponse => "api_response",
        LatencyStage::EndToEndCapture => "end_to_end_capture",
        LatencyStage::EndToEndAction => "end_to_end_action",
    }
}
