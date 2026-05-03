//! Lindley-bounds release-attestation telemetry.

use super::{EnforcerSnapshot, LatencyStage};
use crate::network_calculus_bound::{ArrivalCurve, ServiceCurve, StageModel};
use serde::{Deserialize, Serialize};

/// Release-attestation capture path used by `lindley_bounds_build`.
pub const LINDLEY_ATTESTATION_STAGES: &[LatencyStage] = &[
    LatencyStage::PtyCapture,
    LatencyStage::DeltaExtraction,
    LatencyStage::StorageWrite,
];

/// Default burst for the Lindley-bounds attestation arrival curve.
pub const LINDLEY_ATTESTATION_BURST_EVENTS: f64 = 10.0;

/// Default attestation arrival rate.
///
/// The operator-facing derivation documents the worst-case rate as
/// 100 events/sec. The substrate's strict stability check requires
/// arrival rate below the slowest service rate, so the release
/// attestation uses a 10% steady-state margin.
pub const LINDLEY_ATTESTATION_ARRIVAL_RATE_EVENTS_PER_SEC: f64 = 90.0;

/// Per-stage telemetry consumed by the Lindley-bounds attestation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LindleyStageTelemetry {
    pub stage: LatencyStage,
    pub service_rate_events_per_sec: f64,
    pub p99_latency_ms: f64,
}

impl LindleyStageTelemetry {
    /// Create a validated stage telemetry row.
    pub fn try_new(
        stage: LatencyStage,
        service_rate_events_per_sec: f64,
        p99_latency_ms: f64,
    ) -> Result<Self, String> {
        if stage.is_aggregate() {
            return Err(format!(
                "aggregate stage {stage} is not a Lindley service stage"
            ));
        }
        if !service_rate_events_per_sec.is_finite() || service_rate_events_per_sec <= 0.0 {
            return Err(format!(
                "{stage}: service_rate_events_per_sec must be finite and positive"
            ));
        }
        if !p99_latency_ms.is_finite() || p99_latency_ms < 0.0 {
            return Err(format!(
                "{stage}: p99_latency_ms must be finite and non-negative"
            ));
        }
        Ok(Self {
            stage,
            service_rate_events_per_sec,
            p99_latency_ms,
        })
    }

    /// Convert this telemetry row into the network-calculus stage model.
    pub fn to_stage_model(self) -> StageModel {
        StageModel::new(
            lindley_stage_name(self.stage),
            ServiceCurve::new(self.service_rate_events_per_sec, self.p99_latency_ms),
        )
    }
}

/// Arrival and stage telemetry used to build the Lindley attestation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LindleyTelemetryModel {
    pub arrival_burst_events: f64,
    pub arrival_rate_events_per_sec: f64,
    pub stages: Vec<LindleyStageTelemetry>,
}

impl LindleyTelemetryModel {
    /// Documentation-derived defaults used when release jobs do not pass
    /// live telemetry.
    pub fn documented_default() -> Self {
        Self {
            arrival_burst_events: LINDLEY_ATTESTATION_BURST_EVENTS,
            arrival_rate_events_per_sec: LINDLEY_ATTESTATION_ARRIVAL_RATE_EVENTS_PER_SEC,
            stages: vec![
                LindleyStageTelemetry {
                    stage: LatencyStage::PtyCapture,
                    service_rate_events_per_sec: 200.0,
                    p99_latency_ms: 1.0,
                },
                LindleyStageTelemetry {
                    stage: LatencyStage::DeltaExtraction,
                    service_rate_events_per_sec: 150.0,
                    p99_latency_ms: 2.0,
                },
                LindleyStageTelemetry {
                    stage: LatencyStage::StorageWrite,
                    service_rate_events_per_sec: 100.0,
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
        arrival_rate_events_per_sec: f64,
        service_rates: &[(LatencyStage, f64)],
    ) -> Result<Self, String> {
        let mut stages = Vec::with_capacity(LINDLEY_ATTESTATION_STAGES.len());
        for &stage in LINDLEY_ATTESTATION_STAGES {
            let service_rate_events_per_sec = service_rates
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
                service_rate_events_per_sec,
                p99_us / 1000.0,
            )?);
        }

        Self::try_new(arrival_burst_events, arrival_rate_events_per_sec, stages)
    }

    /// Create a validated telemetry model from explicit rows.
    pub fn try_new(
        arrival_burst_events: f64,
        arrival_rate_events_per_sec: f64,
        stages: Vec<LindleyStageTelemetry>,
    ) -> Result<Self, String> {
        if !arrival_burst_events.is_finite() || arrival_burst_events < 0.0 {
            return Err("arrival_burst_events must be finite and non-negative".to_string());
        }
        if !arrival_rate_events_per_sec.is_finite() || arrival_rate_events_per_sec <= 0.0 {
            return Err("arrival_rate_events_per_sec must be finite and positive".to_string());
        }
        if stages.is_empty() {
            return Err("at least one Lindley stage is required".to_string());
        }
        for stage in &stages {
            LindleyStageTelemetry::try_new(
                stage.stage,
                stage.service_rate_events_per_sec,
                stage.p99_latency_ms,
            )?;
        }
        Ok(Self {
            arrival_burst_events,
            arrival_rate_events_per_sec,
            stages,
        })
    }

    /// Convert live telemetry into the network-calculus substrate inputs.
    pub fn to_network_calculus_inputs(&self) -> Result<(ArrivalCurve, Vec<StageModel>), String> {
        Self::try_new(
            self.arrival_burst_events,
            self.arrival_rate_events_per_sec,
            self.stages.clone(),
        )?;
        Ok((
            ArrivalCurve::new(self.arrival_burst_events, self.arrival_rate_events_per_sec),
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
