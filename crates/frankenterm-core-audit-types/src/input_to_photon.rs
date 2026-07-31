//! Portable classified-input headless-render proxy evidence DTOs.
//!
//! This module intentionally stays leaf-clean: it models retained evidence and
//! deterministic, content-free proxy summaries without depending on the
//! operational renderer, latency collector, or GUI crates.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Schema for retained input-to-photon evidence rows.
pub const INPUT_TO_PHOTON_SCHEMA_VERSION: &str = "ft.renderer.input-to-photon.v2";
/// Stable claim id for the input-to-photon renderer SLO.
pub const INPUT_TO_PHOTON_CLAIM_ID: &str = "renderer.input_to_photon_p95";
/// Workload class used by the deterministic classified-input proxy substrate.
pub const INPUT_TO_PHOTON_WORKLOAD_CLASS: &str = "classified-input-headless-render";
/// macOS p95 target from `docs/perf/resize-quality-slo.json`.
pub const MACOS_P95_TARGET_US: u64 = 16_000;
/// Wayland p95 target from `docs/perf/resize-quality-slo.json`.
pub const WAYLAND_P95_TARGET_US: u64 = 20_000;
/// Maximum allowable instrumentation overhead before evidence degrades.
pub const MAX_INSTRUMENTATION_OVERHEAD_PCT: f64 = 5.0;
/// Maximum encoded byte count admitted for one classified input event.
pub const MAX_INPUT_BYTE_COUNT: u32 = 64;

/// Closed, content-free classification of the input that triggered a proxy trace.
/// Ordered stage labels in the deterministic proxy model.
///
/// The pre-render durations are synthetic workload inputs; these variants do
/// not assert that a native event, mux/PTY hop, display presentation, or photon
/// was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputToPhotonInputClass {
    PrintableText,
    Editing,
    Navigation,
    Control,
    Function,
    Keypad,
}

impl InputToPhotonInputClass {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PrintableText => "printable_text",
            Self::Editing => "editing",
            Self::Navigation => "navigation",
            Self::Control => "control",
            Self::Function => "function",
            Self::Keypad => "keypad",
        }
    }
}

/// Claim boundary carried by every trace and summary in this module.
///
/// The existing renderer harness omits physical input, the production mux/PTY
/// path, display scan-out, and photons. It therefore has exactly one admissible
/// scope. A future physical trace must use the separate live interaction
/// contract rather than extending this enum and silently upgrading this DTO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputToPhotonClaimScope {
    ProxyOnly,
}

impl InputToPhotonClaimScope {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ProxyOnly => "proxy_only",
        }
    }
}

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
#[serde(deny_unknown_fields)]
pub struct InputToPhotonStageTrace {
    pub stage: InputToPhotonStage,
    pub start_us: u64,
    pub end_us: u64,
    pub duration_us: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputToPhotonTrace {
    pub schema_version: String,
    pub claim_id: String,
    pub workload_class: String,
    pub claim_scope: InputToPhotonClaimScope,
    pub sample_id: u64,
    pub input_class: InputToPhotonInputClass,
    pub input_byte_count: u32,
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
#[serde(deny_unknown_fields)]
pub struct InputToPhotonEvidence {
    pub schema_version: String,
    pub claim_id: String,
    pub workload_class: String,
    pub claim_scope: InputToPhotonClaimScope,
    pub generated_at_ms: u64,
    pub platform: String,
    pub input_class: Option<InputToPhotonInputClass>,
    pub min_input_byte_count: Option<u32>,
    pub max_input_byte_count: Option<u32>,
    pub state: InputToPhotonState,
    pub degradation_reason: Option<String>,
    pub sample_count: usize,
    pub p50_us: Option<u64>,
    pub p95_us: Option<u64>,
    pub p99_us: Option<u64>,
    pub p999_us: Option<u64>,
    pub target_p95_us: Option<u64>,
    pub within_target: Option<bool>,
    pub max_instrumentation_overhead_pct: f64,
    pub max_observed_instrumentation_overhead_pct: f64,
    pub stage_breakdown_p50: BTreeMap<String, u64>,
}

#[must_use]
pub fn target_p95_us_for_platform(platform: &str) -> Option<u64> {
    match platform {
        "macos" => Some(MACOS_P95_TARGET_US),
        "linux" => Some(WAYLAND_P95_TARGET_US),
        _ => None,
    }
}

/// Converts the headless renderer's millisecond timer into the proxy stage unit.
#[must_use]
pub fn headless_render_duration_us(render_ms: u128) -> u64 {
    u64::try_from(render_ms)
        .unwrap_or(u64::MAX / 1_000)
        .saturating_mul(1_000)
        .max(1)
}

/// Builds and validates one content-free proxy trace.
///
/// # Errors
///
/// Returns an error when any supplied identity, byte count, timing, overhead,
/// or proxy-render metadata violates the v2 contract.
#[must_use]
pub fn classified_input_proxy_trace_from_stage_durations(
    sample_id: u64,
    input_class: InputToPhotonInputClass,
    input_byte_count: u32,
    platform: impl Into<String>,
    stage_durations_us: [u64; 5],
    instrumentation_overhead_us: u64,
    headless_render_ms: u128,
    gpu_adapter: impl Into<String>,
) -> Result<InputToPhotonTrace, String> {
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

    let trace = InputToPhotonTrace {
        schema_version: INPUT_TO_PHOTON_SCHEMA_VERSION.to_string(),
        claim_id: INPUT_TO_PHOTON_CLAIM_ID.to_string(),
        workload_class: INPUT_TO_PHOTON_WORKLOAD_CLASS.to_string(),
        claim_scope: InputToPhotonClaimScope::ProxyOnly,
        sample_id,
        input_class,
        input_byte_count,
        platform: platform.into(),
        state,
        degradation_reason,
        stages,
        total_latency_us: Some(total_latency_us),
        instrumentation_overhead_us,
        instrumentation_overhead_pct,
        headless_render_ms: Some(headless_render_ms),
        gpu_adapter: Some(gpu_adapter.into()),
    };
    validate_trace(&trace, &trace.platform)?;
    Ok(trace)
}

#[must_use]
pub fn unavailable_proxy_evidence(
    platform: impl Into<String>,
    reason: impl Into<String>,
) -> InputToPhotonEvidence {
    empty_evidence(
        platform.into(),
        InputToPhotonState::InstrumentationUnavailable,
        reason.into(),
    )
}

fn empty_evidence(
    platform: String,
    state: InputToPhotonState,
    reason: String,
) -> InputToPhotonEvidence {
    InputToPhotonEvidence {
        schema_version: INPUT_TO_PHOTON_SCHEMA_VERSION.to_string(),
        claim_id: INPUT_TO_PHOTON_CLAIM_ID.to_string(),
        workload_class: INPUT_TO_PHOTON_WORKLOAD_CLASS.to_string(),
        claim_scope: InputToPhotonClaimScope::ProxyOnly,
        generated_at_ms: now_ms(),
        target_p95_us: target_p95_us_for_platform(&platform),
        platform,
        input_class: None,
        min_input_byte_count: None,
        max_input_byte_count: None,
        state,
        degradation_reason: Some(reason),
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
        return empty_evidence(
            platform,
            InputToPhotonState::InvalidTrace,
            "no input-to-photon traces were recorded".to_string(),
        );
    }

    let mut max_overhead = 0.0_f64;
    let mut total_latencies = Vec::with_capacity(traces.len());
    let mut stage_breakdowns: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut sample_ids = BTreeSet::new();
    let mut input_class = None;
    let mut min_input_byte_count = u32::MAX;
    let mut max_input_byte_count = 0_u32;

    for trace in traces {
        if !sample_ids.insert(trace.sample_id) {
            return empty_evidence(
                platform,
                InputToPhotonState::InvalidTrace,
                format!("duplicate input-to-photon sample_id {}", trace.sample_id),
            );
        }
        max_overhead = max_overhead.max(trace.instrumentation_overhead_pct);
        if let Err(reason) = validate_trace(trace, &platform) {
            return empty_evidence(
                platform,
                InputToPhotonState::InvalidTrace,
                format!("sample_id {} is invalid: {reason}", trace.sample_id),
            );
        }
        if trace.state != InputToPhotonState::Measured {
            let mut evidence = empty_evidence(
                platform,
                trace.state,
                trace
                    .degradation_reason
                    .clone()
                    .unwrap_or_else(|| "trace is not claim-eligible".to_string()),
            );
            evidence.max_observed_instrumentation_overhead_pct = max_overhead;
            return evidence;
        }

        if let Some(expected) = input_class {
            if expected != trace.input_class {
                return empty_evidence(
                    platform,
                    InputToPhotonState::InvalidTrace,
                    format!(
                        "mixed input classes: expected {expected:?}, got {:?}",
                        trace.input_class
                    ),
                );
            }
        } else {
            input_class = Some(trace.input_class);
        }
        min_input_byte_count = min_input_byte_count.min(trace.input_byte_count);
        max_input_byte_count = max_input_byte_count.max(trace.input_byte_count);
        if let Some(total_latency_us) = trace.total_latency_us {
            total_latencies.push(total_latency_us);
        }
        record_stage_breakdowns(trace, &mut stage_breakdowns);
    }

    total_latencies.sort_unstable();
    let target_p95_us = target_p95_us_for_platform(&platform);
    let p95_us = percentile_nearest_rank_fraction(&total_latencies, 0.95);
    // This v2 contract is deliberately proxy-only. Its percentile is useful
    // for regression tracking, but cannot establish the physical SLO target.
    let within_target = None;

    InputToPhotonEvidence {
        schema_version: INPUT_TO_PHOTON_SCHEMA_VERSION.to_string(),
        claim_id: INPUT_TO_PHOTON_CLAIM_ID.to_string(),
        workload_class: INPUT_TO_PHOTON_WORKLOAD_CLASS.to_string(),
        claim_scope: InputToPhotonClaimScope::ProxyOnly,
        generated_at_ms: now_ms(),
        platform,
        input_class,
        min_input_byte_count: Some(min_input_byte_count),
        max_input_byte_count: Some(max_input_byte_count),
        state: InputToPhotonState::Measured,
        degradation_reason: None,
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

fn validate_trace(trace: &InputToPhotonTrace, expected_platform: &str) -> Result<(), String> {
    if trace.schema_version != INPUT_TO_PHOTON_SCHEMA_VERSION {
        return Err(format!(
            "schema_version mismatch: expected {INPUT_TO_PHOTON_SCHEMA_VERSION}, got {}",
            trace.schema_version
        ));
    }
    if trace.claim_id != INPUT_TO_PHOTON_CLAIM_ID {
        return Err(format!(
            "claim_id mismatch: expected {INPUT_TO_PHOTON_CLAIM_ID}, got {}",
            trace.claim_id
        ));
    }
    if trace.workload_class != INPUT_TO_PHOTON_WORKLOAD_CLASS {
        return Err(format!(
            "workload_class mismatch: expected {INPUT_TO_PHOTON_WORKLOAD_CLASS}, got {}",
            trace.workload_class
        ));
    }
    if trace.claim_scope != InputToPhotonClaimScope::ProxyOnly {
        return Err("unsupported input-to-photon claim scope".to_string());
    }
    if target_p95_us_for_platform(expected_platform).is_none() {
        return Err(format!(
            "platform {expected_platform:?} has no input-to-photon target"
        ));
    }
    if trace.platform != expected_platform {
        return Err(format!(
            "platform mismatch: expected {expected_platform}, got {}",
            trace.platform
        ));
    }
    if trace.input_byte_count == 0 || trace.input_byte_count > MAX_INPUT_BYTE_COUNT {
        return Err(format!(
            "input_byte_count {} is outside 1..={MAX_INPUT_BYTE_COUNT}",
            trace.input_byte_count
        ));
    }
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
        if let Some(previous_end) = previous_end {
            if stage.start_us != previous_end {
                return Err(format!(
                    "{} does not start at the previous stage boundary",
                    stage.stage
                ));
            }
        } else if stage.start_us != 0 {
            return Err("key_event trace must start at zero".to_string());
        }
        if stage.end_us.checked_sub(stage.start_us) != Some(stage.duration_us) {
            return Err(format!(
                "{} duration does not match timestamps",
                stage.stage
            ));
        }
        previous_end = Some(stage.end_us);
    }

    let computed_total = trace.stages.last().and_then(|last| {
        trace
            .stages
            .first()
            .and_then(|first| last.end_us.checked_sub(first.start_us))
    });
    if computed_total != trace.total_latency_us {
        return Err("trace total_latency_us does not match stage timestamps".to_string());
    }
    let Some(total_latency_us) = trace.total_latency_us else {
        return Err("trace total_latency_us is missing".to_string());
    };
    if total_latency_us == 0 {
        return Err("trace total_latency_us must be nonzero".to_string());
    }
    if !trace.instrumentation_overhead_pct.is_finite()
        || trace.instrumentation_overhead_pct < 0.0
    {
        return Err("instrumentation_overhead_pct must be finite and nonnegative".to_string());
    }
    let expected_overhead_pct =
        overhead_pct(trace.instrumentation_overhead_us, total_latency_us);
    if (trace.instrumentation_overhead_pct - expected_overhead_pct).abs() > 1.0e-9 {
        return Err("instrumentation overhead percentage does not match trace timing".to_string());
    }
    let overhead_exceeded =
        trace.instrumentation_overhead_pct > MAX_INSTRUMENTATION_OVERHEAD_PCT;
    match trace.state {
        InputToPhotonState::Measured if !overhead_exceeded => {
            if trace.degradation_reason.is_some() {
                return Err("measured trace carries a degradation reason".to_string());
            }
        }
        InputToPhotonState::InstrumentationOverheadExceeded if overhead_exceeded => {
            if trace
                .degradation_reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err("degraded trace is missing its reason".to_string());
            }
        }
        _ => {
            return Err("trace state does not match its measured overhead".to_string());
        }
    }
    let Some(headless_render_ms) = trace.headless_render_ms else {
        return Err("proxy trace is missing headless_render_ms".to_string());
    };
    let expected_render_us = headless_render_duration_us(headless_render_ms);
    if trace.stages.last().map(|stage| stage.duration_us) != Some(expected_render_us) {
        return Err(
            "headless_render_ms does not match the gpu_present stage duration".to_string(),
        );
    }
    if trace
        .gpu_adapter
        .as_deref()
        .is_none_or(|adapter| adapter.trim().is_empty())
    {
        return Err("proxy trace is missing gpu_adapter identity".to_string());
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
        if overhead_us == 0 {
            0.0
        } else {
            100.0
        }
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

    fn proxy_trace(
        sample_id: u64,
        platform: &str,
        input_class: InputToPhotonInputClass,
        input_byte_count: u32,
        stage_durations_us: [u64; 5],
        instrumentation_overhead_us: u64,
    ) -> InputToPhotonTrace {
        let headless_render_ms = u128::from(stage_durations_us[4] / 1_000);
        assert_eq!(
            headless_render_duration_us(headless_render_ms),
            stage_durations_us[4],
            "test proxy GPU stage must exactly encode whole headless milliseconds"
        );
        classified_input_proxy_trace_from_stage_durations(
            sample_id,
            input_class,
            input_byte_count,
            platform,
            stage_durations_us,
            instrumentation_overhead_us,
            headless_render_ms,
            "deterministic-test-adapter",
        )
        .expect("test proxy trace must be valid")
    }

    #[test]
    fn summary_reports_proxy_percentiles_without_physical_target_verdict() {
        let traces = [
            proxy_trace(
                0,
                "macos",
                InputToPhotonInputClass::PrintableText,
                1,
                [100, 200, 300, 400, 1_000],
                20,
            ),
            proxy_trace(
                1,
                "macos",
                InputToPhotonInputClass::PrintableText,
                2,
                [200, 300, 400, 500, 2_000],
                25,
            ),
            proxy_trace(
                2,
                "macos",
                InputToPhotonInputClass::PrintableText,
                4,
                [300, 400, 500, 600, 3_000],
                30,
            ),
        ];

        let evidence = summarize_input_to_photon_traces("macos", &traces);

        assert_eq!(evidence.state, InputToPhotonState::Measured);
        assert_eq!(evidence.sample_count, 3);
        assert_eq!(evidence.claim_scope, InputToPhotonClaimScope::ProxyOnly);
        assert_eq!(
            evidence.input_class,
            Some(InputToPhotonInputClass::PrintableText)
        );
        assert_eq!(evidence.min_input_byte_count, Some(1));
        assert_eq!(evidence.max_input_byte_count, Some(4));
        assert_eq!(evidence.target_p95_us, Some(MACOS_P95_TARGET_US));
        assert_eq!(evidence.p50_us, Some(3400));
        assert_eq!(evidence.p95_us, Some(4800));
        assert_eq!(evidence.p99_us, Some(4800));
        assert_eq!(evidence.within_target, None);
        assert_eq!(
            evidence
                .stage_breakdown_p50
                .get("term_update_to_render_submit"),
            Some(&500)
        );
    }

    #[test]
    fn excessive_instrumentation_overhead_degrades_evidence() {
        let trace = proxy_trace(
            0,
            "linux",
            InputToPhotonInputClass::Control,
            1,
            [100, 100, 100, 100, 1_000],
            100,
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
        let mut trace = proxy_trace(
            0,
            "linux",
            InputToPhotonInputClass::Navigation,
            3,
            [100, 100, 100, 100, 1_000],
            1,
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

    #[test]
    fn legacy_v1_raw_key_fixture_is_rejected() {
        let legacy = serde_json::json!({
            "schema_version": "ft.renderer.input-to-photon.v1",
            "claim_id": INPUT_TO_PHOTON_CLAIM_ID,
            "workload_class": "known-key-headless-render",
            "sample_id": 7,
            "key": "private typed content",
            "platform": "macos",
            "state": "measured",
            "degradation_reason": null,
            "stages": [],
            "total_latency_us": 1,
            "instrumentation_overhead_us": 0,
            "instrumentation_overhead_pct": 0.0,
            "headless_render_ms": 1,
            "gpu_adapter": "legacy-test-adapter"
        });

        assert!(
            serde_json::from_value::<InputToPhotonTrace>(legacy).is_err(),
            "v1 rows containing raw key text must not deserialize as v2 evidence"
        );
    }

    #[test]
    fn v2_roundtrip_has_only_classified_input_metadata() {
        let trace = proxy_trace(
            11,
            "macos",
            InputToPhotonInputClass::Editing,
            3,
            [100, 200, 300, 400, 1_000],
            1,
        );

        let value = serde_json::to_value(&trace).expect("serialize v2 trace");
        assert_eq!(
            value.get("schema_version").and_then(serde_json::Value::as_str),
            Some(INPUT_TO_PHOTON_SCHEMA_VERSION)
        );
        assert_eq!(
            value.get("input_class").and_then(serde_json::Value::as_str),
            Some("editing")
        );
        assert_eq!(
            value
                .get("input_byte_count")
                .and_then(serde_json::Value::as_u64),
            Some(3)
        );
        assert!(
            value.get("key").is_none(),
            "v2 must structurally omit raw input content"
        );

        let roundtrip: InputToPhotonTrace =
            serde_json::from_value(value).expect("deserialize v2 trace");
        assert_eq!(roundtrip, trace);
    }

    #[test]
    fn otherwise_valid_v2_fixture_rejects_an_injected_raw_key_field() {
        let trace = proxy_trace(
            12,
            "macos",
            InputToPhotonInputClass::PrintableText,
            1,
            [100, 200, 300, 400, 1_000],
            1,
        );
        let mut value = serde_json::to_value(trace).expect("serialize v2 trace");
        value
            .as_object_mut()
            .expect("trace serializes as an object")
            .insert(
                "key".to_string(),
                serde_json::Value::String("private typed content".to_string()),
            );

        assert!(
            serde_json::from_value::<InputToPhotonTrace>(value).is_err(),
            "deny_unknown_fields must reject raw input injected into an otherwise valid v2 row"
        );
    }

    #[test]
    fn duplicate_sample_ids_fail_closed_without_partial_percentiles() {
        let trace = proxy_trace(
            42,
            "linux",
            InputToPhotonInputClass::Function,
            1,
            [100, 200, 300, 400, 1_000],
            1,
        );

        let evidence =
            summarize_input_to_photon_traces("linux", &[trace.clone(), trace]);

        assert_eq!(evidence.state, InputToPhotonState::InvalidTrace);
        assert_eq!(evidence.sample_count, 0);
        assert_eq!(evidence.p50_us, None);
        assert_eq!(evidence.p95_us, None);
        assert_eq!(evidence.p99_us, None);
        assert_eq!(evidence.p999_us, None);
        assert_eq!(evidence.within_target, None);
        assert!(evidence.stage_breakdown_p50.is_empty());
        assert!(
            evidence
                .degradation_reason
                .as_deref()
                .unwrap_or_default()
                .contains("duplicate")
        );
    }

    #[test]
    fn mixed_or_invalid_identity_fails_closed_without_valid_subset() {
        let valid = proxy_trace(
            0,
            "macos",
            InputToPhotonInputClass::PrintableText,
            1,
            [100, 200, 300, 400, 1_000],
            1,
        );
        let mut wrong_schema = valid.clone();
        wrong_schema.sample_id = 1;
        wrong_schema.schema_version = "ft.renderer.input-to-photon.v1".to_string();
        let mut wrong_claim = valid.clone();
        wrong_claim.sample_id = 2;
        wrong_claim.claim_id = "renderer.unrelated_claim".to_string();
        let mut wrong_workload = valid.clone();
        wrong_workload.sample_id = 3;
        wrong_workload.workload_class = "unrelated-workload".to_string();
        let mut wrong_platform = valid.clone();
        wrong_platform.sample_id = 4;
        wrong_platform.platform = "linux".to_string();
        let mut mixed_class = valid.clone();
        mixed_class.sample_id = 5;
        mixed_class.input_class = InputToPhotonInputClass::Navigation;

        for invalid in [
            wrong_schema,
            wrong_claim,
            wrong_workload,
            wrong_platform,
            mixed_class,
        ] {
            let evidence =
                summarize_input_to_photon_traces("macos", &[valid.clone(), invalid]);
            assert_eq!(evidence.state, InputToPhotonState::InvalidTrace);
            assert_eq!(evidence.sample_count, 0);
            assert_eq!(evidence.p50_us, None);
            assert_eq!(evidence.p95_us, None);
            assert_eq!(evidence.p99_us, None);
            assert_eq!(evidence.p999_us, None);
            assert_eq!(evidence.within_target, None);
            assert!(evidence.stage_breakdown_p50.is_empty());
        }
    }

    #[test]
    fn invalid_input_byte_counts_fail_closed() {
        for (sample_id, invalid_count) in [(0, 0), (1, MAX_INPUT_BYTE_COUNT + 1)] {
            let mut trace = proxy_trace(
                sample_id,
                "linux",
                InputToPhotonInputClass::Keypad,
                1,
                [100, 200, 300, 400, 1_000],
                1,
            );
            trace.input_byte_count = invalid_count;
            let evidence = summarize_input_to_photon_traces("linux", &[trace]);

            assert_eq!(evidence.state, InputToPhotonState::InvalidTrace);
            assert_eq!(evidence.sample_count, 0);
            assert_eq!(evidence.p95_us, None);
            assert!(
                evidence
                    .degradation_reason
                    .as_deref()
                    .unwrap_or_default()
                    .contains("input_byte_count")
            );
        }
    }

    #[test]
    fn incomplete_proxy_metadata_fails_closed() {
        let valid = proxy_trace(
            0,
            "linux",
            InputToPhotonInputClass::Control,
            1,
            [100, 200, 300, 400, 1_000],
            1,
        );
        let mut missing_render_duration = valid.clone();
        missing_render_duration.sample_id = 1;
        missing_render_duration.headless_render_ms = None;
        let mut blank_gpu_identity = valid.clone();
        blank_gpu_identity.sample_id = 2;
        blank_gpu_identity.gpu_adapter = Some("   ".to_string());
        let mut inconsistent_render_duration = valid;
        inconsistent_render_duration.sample_id = 3;
        inconsistent_render_duration.headless_render_ms = Some(2);

        for invalid in [
            missing_render_duration,
            blank_gpu_identity,
            inconsistent_render_duration,
        ] {
            let evidence = summarize_input_to_photon_traces("linux", &[invalid]);
            assert_eq!(evidence.state, InputToPhotonState::InvalidTrace);
            assert_eq!(evidence.sample_count, 0);
            assert_eq!(evidence.p95_us, None);
        }
    }

    #[test]
    fn unsupported_platform_has_no_target_and_cannot_be_summarized() {
        assert_eq!(target_p95_us_for_platform("freebsd"), None);
        let mut trace = proxy_trace(
            0,
            "linux",
            InputToPhotonInputClass::PrintableText,
            1,
            [100, 200, 300, 400, 1_000],
            1,
        );
        trace.platform = "freebsd".to_string();

        let evidence = summarize_input_to_photon_traces("freebsd", &[trace]);

        assert_eq!(evidence.state, InputToPhotonState::InvalidTrace);
        assert_eq!(evidence.target_p95_us, None);
        assert_eq!(evidence.sample_count, 0);
        assert_eq!(evidence.p95_us, None);
        assert_eq!(evidence.within_target, None);
    }

    #[test]
    fn unavailable_proxy_helper_cannot_emit_a_measured_state() {
        let evidence = unavailable_proxy_evidence("linux", "headless renderer unavailable");

        assert_eq!(
            evidence.state,
            InputToPhotonState::InstrumentationUnavailable
        );
        assert_eq!(evidence.sample_count, 0);
        assert_eq!(evidence.p95_us, None);
        assert_eq!(evidence.within_target, None);
    }
}
