//! Portable input-to-photon evidence DTOs for renderer SLO proof lanes.
//!
//! This module intentionally stays leaf-clean: it models retained evidence and
//! deterministic known-key trace summaries without depending on the operational
//! renderer, latency collector, or GUI crates.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Schema for retained input-to-photon evidence rows.
pub const INPUT_TO_PHOTON_SCHEMA_VERSION: &str = "ft.renderer.input-to-photon.v1";
/// Stable claim id for the input-to-photon renderer SLO.
pub const INPUT_TO_PHOTON_CLAIM_ID: &str = "renderer.input_to_photon_p95";
/// Workload class used by the deterministic known-key substrate.
pub const INPUT_TO_PHOTON_WORKLOAD_CLASS: &str = "known-key-headless-render";
/// macOS p95 target from `docs/perf/resize-quality-slo.json`.
pub const MACOS_P95_TARGET_US: u64 = 16_000;
/// Wayland p95 target from `docs/perf/resize-quality-slo.json`.
pub const WAYLAND_P95_TARGET_US: u64 = 20_000;
/// Maximum allowable instrumentation overhead before evidence degrades.
pub const MAX_INSTRUMENTATION_OVERHEAD_PCT: f64 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputToPhotonStage {
    KeyEvent,
    PtyWrite,
    PtyRead,
    TermUpdate,
    RenderSubmit,
    GpuPresent,
}

impl InputToPhotonStage {
    pub const ALL: &'static [Self] = &[
        Self::KeyEvent,
        Self::PtyWrite,
        Self::PtyRead,
        Self::TermUpdate,
        Self::RenderSubmit,
        Self::GpuPresent,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
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

impl std::fmt::Display for InputToPhotonStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputToPhotonState {
    Measured,
    InstrumentationUnavailable,
    PhotonDetectionUnavailable,
    InstrumentationOverheadExceeded,
    InvalidTrace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputToPhotonStageTrace {
    pub stage: InputToPhotonStage,
    pub start_us: u64,
    pub end_us: u64,
    pub duration_us: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputToPhotonTrace {
    pub schema_version: String,
    pub claim_id: String,
    pub workload_class: String,
    pub sample_id: u64,
    pub key: String,
    pub platform: String,
    pub state: InputToPhotonState,
    pub degradation_reason: Option<String>,
    pub stages: Vec<InputToPhotonStageTrace>,
    pub total_latency_us: Option<u64>,
    pub instrumentation_overhead_us: u64,
    pub instrumentation_overhead_pct: f64,
    pub headless_render_ms: Option<u128>,
    pub gpu_adapter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputToPhotonEvidence {
    pub schema_version: String,
    pub claim_id: String,
    pub generated_at_ms: u64,
    pub platform: String,
    pub state: InputToPhotonState,
    pub degradation_reason: Option<String>,
    pub sample_count: usize,
    pub p50_us: Option<u64>,
    pub p95_us: Option<u64>,
    pub p99_us: Option<u64>,
    pub p999_us: Option<u64>,
    pub target_p95_us: u64,
    pub within_target: Option<bool>,
    pub max_instrumentation_overhead_pct: f64,
    pub max_observed_instrumentation_overhead_pct: f64,
    pub stage_breakdown_p50: BTreeMap<String, u64>,
}

#[must_use]
pub fn target_p95_us_for_platform(platform: &str) -> u64 {
    match platform {
        "macos" => MACOS_P95_TARGET_US,
        "linux" => WAYLAND_P95_TARGET_US,
        _ => WAYLAND_P95_TARGET_US,
    }
}

#[must_use]
pub fn known_key_trace_from_stage_durations(
    sample_id: u64,
    key: impl Into<String>,
    platform: impl Into<String>,
    stage_durations_us: [u64; 5],
    instrumentation_overhead_us: u64,
    headless_render_ms: Option<u128>,
    gpu_adapter: Option<String>,
) -> InputToPhotonTrace {
    let mut cursor = 0_u64;
    let mut stages = Vec::with_capacity(InputToPhotonStage::ALL.len());
    for (stage_index, &stage) in InputToPhotonStage::ALL.iter().enumerate() {
        let start_us = cursor;
        let duration_us = if stage_index == 0 {
            0
        } else {
            stage_durations_us[stage_index - 1]
        };
        cursor = cursor.saturating_add(duration_us);
        stages.push(InputToPhotonStageTrace {
            stage,
            start_us,
            end_us: cursor,
            duration_us,
        });
    }

    let total_latency_us = cursor;
    let instrumentation_overhead_pct = overhead_pct(instrumentation_overhead_us, total_latency_us);
    let state = if instrumentation_overhead_pct > MAX_INSTRUMENTATION_OVERHEAD_PCT {
        InputToPhotonState::InstrumentationOverheadExceeded
    } else {
        InputToPhotonState::Measured
    };
    let degradation_reason = (state != InputToPhotonState::Measured).then(|| {
        format!(
            "instrumentation overhead {instrumentation_overhead_pct:.2}% exceeds {MAX_INSTRUMENTATION_OVERHEAD_PCT:.2}%"
        )
    });

    InputToPhotonTrace {
        schema_version: INPUT_TO_PHOTON_SCHEMA_VERSION.to_string(),
        claim_id: INPUT_TO_PHOTON_CLAIM_ID.to_string(),
        workload_class: INPUT_TO_PHOTON_WORKLOAD_CLASS.to_string(),
        sample_id,
        key: key.into(),
        platform: platform.into(),
        state,
        degradation_reason,
        stages,
        total_latency_us: Some(total_latency_us),
        instrumentation_overhead_us,
        instrumentation_overhead_pct,
        headless_render_ms,
        gpu_adapter,
    }
}

#[must_use]
pub fn unavailable_evidence(
    platform: impl Into<String>,
    state: InputToPhotonState,
    reason: impl Into<String>,
) -> InputToPhotonEvidence {
    let platform = platform.into();
    InputToPhotonEvidence {
        schema_version: INPUT_TO_PHOTON_SCHEMA_VERSION.to_string(),
        claim_id: INPUT_TO_PHOTON_CLAIM_ID.to_string(),
        generated_at_ms: now_ms(),
        target_p95_us: target_p95_us_for_platform(&platform),
        platform,
        state,
        degradation_reason: Some(reason.into()),
        sample_count: 0,
        p50_us: None,
        p95_us: None,
        p99_us: None,
        p999_us: None,
        within_target: None,
        max_instrumentation_overhead_pct: MAX_INSTRUMENTATION_OVERHEAD_PCT,
        max_observed_instrumentation_overhead_pct: 0.0,
        stage_breakdown_p50: BTreeMap::new(),
    }
}

pub fn summarize_input_to_photon_traces(
    platform: impl Into<String>,
    traces: &[InputToPhotonTrace],
) -> InputToPhotonEvidence {
    let platform = platform.into();
    if traces.is_empty() {
        return unavailable_evidence(
            platform,
            InputToPhotonState::InvalidTrace,
            "no input-to-photon traces were recorded",
        );
    }

    let mut state = InputToPhotonState::Measured;
    let mut degradation_reason = None;
    let mut max_overhead = 0.0_f64;
    let mut total_latencies = Vec::with_capacity(traces.len());
    let mut stage_breakdowns: BTreeMap<String, Vec<u64>> = BTreeMap::new();

    for trace in traces {
        max_overhead = max_overhead.max(trace.instrumentation_overhead_pct);
        match validate_trace(trace) {
            Ok(()) => {
                if let Some(total_latency_us) = trace.total_latency_us {
                    total_latencies.push(total_latency_us);
                }
                record_stage_breakdowns(trace, &mut stage_breakdowns);
                if trace.state != InputToPhotonState::Measured
                    && state == InputToPhotonState::Measured
                {
                    state = trace.state;
                    degradation_reason.clone_from(&trace.degradation_reason);
                }
            }
            Err(reason) => {
                state = InputToPhotonState::InvalidTrace;
                degradation_reason = Some(reason);
            }
        }
    }

    total_latencies.sort_unstable();
    let target_p95_us = target_p95_us_for_platform(&platform);
    let p95_us = percentile_nearest_rank_fraction(&total_latencies, 0.95);
    let within_target = if state == InputToPhotonState::Measured {
        p95_us.map(|value| value <= target_p95_us)
    } else {
        None
    };

    InputToPhotonEvidence {
        schema_version: INPUT_TO_PHOTON_SCHEMA_VERSION.to_string(),
        claim_id: INPUT_TO_PHOTON_CLAIM_ID.to_string(),
        generated_at_ms: now_ms(),
        platform,
        state,
        degradation_reason,
        sample_count: total_latencies.len(),
        p50_us: percentile_nearest_rank_fraction(&total_latencies, 0.50),
        p95_us,
        p99_us: percentile_nearest_rank_fraction(&total_latencies, 0.99),
        p999_us: percentile_nearest_rank_fraction(&total_latencies, 0.999),
        target_p95_us,
        within_target,
        max_instrumentation_overhead_pct: MAX_INSTRUMENTATION_OVERHEAD_PCT,
        max_observed_instrumentation_overhead_pct: max_overhead,
        stage_breakdown_p50: stage_breakdown_p50(stage_breakdowns),
    }
}

fn validate_trace(trace: &InputToPhotonTrace) -> Result<(), String> {
    if trace.stages.len() != InputToPhotonStage::ALL.len() {
        return Err(format!(
            "expected {} stages, got {}",
            InputToPhotonStage::ALL.len(),
            trace.stages.len()
        ));
    }

    let mut previous_end = None;
    for (&expected, stage) in InputToPhotonStage::ALL.iter().zip(&trace.stages) {
        if stage.stage != expected {
            return Err(format!(
                "stage order mismatch: expected {expected}, got {}",
                stage.stage
            ));
        }
        if stage.end_us < stage.start_us {
            return Err(format!("{} end precedes start", stage.stage));
        }
        if let Some(previous_end) = previous_end
            && stage.start_us < previous_end
        {
            return Err(format!(
                "{} starts before previous stage ended",
                stage.stage
            ));
        }
        if stage.end_us.saturating_sub(stage.start_us) != stage.duration_us {
            return Err(format!(
                "{} duration does not match timestamps",
                stage.stage
            ));
        }
        previous_end = Some(stage.end_us);
    }

    let computed_total = trace
        .stages
        .last()
        .map(|stage| stage.end_us)
        .and_then(|last| {
            trace
                .stages
                .first()
                .map(|first| last.saturating_sub(first.start_us))
        });
    if computed_total != trace.total_latency_us {
        return Err("trace total_latency_us does not match stage timestamps".to_string());
    }
    Ok(())
}

fn record_stage_breakdowns(
    trace: &InputToPhotonTrace,
    stage_breakdowns: &mut BTreeMap<String, Vec<u64>>,
) {
    for window in trace.stages.windows(2) {
        let from = window[0].stage;
        let to = window[1].stage;
        let label = format!("{}_to_{}", from.label(), to.label());
        if let Some(latency_us) = window[1].end_us.checked_sub(window[0].end_us) {
            stage_breakdowns.entry(label).or_default().push(latency_us);
        }
    }
}

fn stage_breakdown_p50(stage_breakdowns: BTreeMap<String, Vec<u64>>) -> BTreeMap<String, u64> {
    stage_breakdowns
        .into_iter()
        .filter_map(|(label, mut values)| {
            values.sort_unstable();
            percentile_nearest_rank_fraction(&values, 0.50).map(|p50| (label, p50))
        })
        .collect()
}

fn percentile_nearest_rank_fraction(sorted_values: &[u64], fraction: f64) -> Option<u64> {
    if sorted_values.is_empty() {
        return None;
    }
    let n = sorted_values.len();
    let rank = (fraction * n as f64).ceil() as usize;
    let idx = rank.min(n).saturating_sub(1);
    Some(sorted_values[idx])
}

fn overhead_pct(overhead_us: u64, total_us: u64) -> f64 {
    if total_us == 0 {
        0.0
    } else {
        overhead_us as f64 * 100.0 / total_us as f64
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_reports_percentiles_and_target() {
        let traces = [
            known_key_trace_from_stage_durations(
                0,
                "a",
                "macos",
                [100, 200, 300, 400, 100],
                20,
                None,
                None,
            ),
            known_key_trace_from_stage_durations(
                1,
                "a",
                "macos",
                [200, 300, 400, 500, 200],
                25,
                None,
                None,
            ),
            known_key_trace_from_stage_durations(
                2,
                "a",
                "macos",
                [300, 400, 500, 600, 300],
                30,
                None,
                None,
            ),
        ];

        let evidence = summarize_input_to_photon_traces("macos", &traces);

        assert_eq!(evidence.state, InputToPhotonState::Measured);
        assert_eq!(evidence.sample_count, 3);
        assert_eq!(evidence.target_p95_us, MACOS_P95_TARGET_US);
        assert_eq!(evidence.p50_us, Some(1600));
        assert_eq!(evidence.p95_us, Some(2100));
        assert_eq!(evidence.p99_us, Some(2100));
        assert_eq!(evidence.within_target, Some(true));
        assert_eq!(
            evidence
                .stage_breakdown_p50
                .get("term_update_to_render_submit"),
            Some(&500)
        );
    }

    #[test]
    fn excessive_instrumentation_overhead_degrades_evidence() {
        let trace = known_key_trace_from_stage_durations(
            0,
            "a",
            "linux",
            [100, 100, 100, 100, 100],
            100,
            None,
            None,
        );

        let evidence = summarize_input_to_photon_traces("linux", &[trace]);

        assert_eq!(
            evidence.state,
            InputToPhotonState::InstrumentationOverheadExceeded
        );
        assert!(
            evidence
                .degradation_reason
                .as_deref()
                .unwrap_or_default()
                .contains("instrumentation overhead")
        );
        assert_eq!(evidence.within_target, None);
    }

    #[test]
    fn empty_summary_is_degraded_not_measured() {
        let evidence = summarize_input_to_photon_traces("linux", &[]);

        assert_eq!(evidence.state, InputToPhotonState::InvalidTrace);
        assert_eq!(evidence.sample_count, 0);
        assert_eq!(evidence.p95_us, None);
        assert_eq!(evidence.within_target, None);
        assert!(
            evidence
                .degradation_reason
                .as_deref()
                .unwrap_or_default()
                .contains("no input-to-photon traces")
        );
    }

    #[test]
    fn invalid_stage_order_degrades_summary() {
        let mut trace = known_key_trace_from_stage_durations(
            0,
            "a",
            "linux",
            [100, 100, 100, 100, 100],
            1,
            None,
            None,
        );
        trace.stages.swap(1, 2);

        let evidence = summarize_input_to_photon_traces("linux", &[trace]);

        assert_eq!(evidence.state, InputToPhotonState::InvalidTrace);
        assert!(
            evidence
                .degradation_reason
                .as_deref()
                .unwrap_or_default()
                .contains("stage order mismatch")
        );
    }
}
