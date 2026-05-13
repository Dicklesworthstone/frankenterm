//! Renderer SLO measurement helpers.
//!
//! The generic evidence contract lives in `frankenterm-core`; this GUI module
//! only adds the headless-render trace adapter used by the Criterion harness.

pub use frankenterm_core::render_quality::{
    INPUT_TO_PHOTON_CLAIM_ID, INPUT_TO_PHOTON_SCHEMA_VERSION, INPUT_TO_PHOTON_WORKLOAD_CLASS,
    InputToPhotonEvidence, InputToPhotonStage, InputToPhotonStageTrace, InputToPhotonState,
    InputToPhotonTrace, MACOS_P95_TARGET_US, MAX_INSTRUMENTATION_OVERHEAD_PCT,
    WAYLAND_P95_TARGET_US, known_key_trace_from_stage_durations, summarize_input_to_photon_traces,
    target_p95_us_for_platform, unavailable_evidence,
};

#[cfg(feature = "headless-render")]
pub mod headless {
    use super::{InputToPhotonTrace, known_key_trace_from_stage_durations};
    use crate::headless_render::{HeadlessFixtureInput, HeadlessFrame, smoketest_input};

    pub fn known_key_headless_input() -> HeadlessFixtureInput {
        let mut input = smoketest_input(800, 480, 96.0);
        input.lines = vec![
            "input-to-photon known-key fixture".to_string(),
            "key=a stage=term_update render=headless".to_string(),
            "deterministic frame flush path".to_string(),
        ];
        input
    }

    pub fn trace_from_headless_frame(
        sample_id: u64,
        key: impl Into<String>,
        platform: impl Into<String>,
        frame: &HeadlessFrame,
        instrumentation_overhead_us: u64,
    ) -> InputToPhotonTrace {
        let render_us = u64::try_from(frame.render_ms)
            .unwrap_or(u64::MAX / 1_000)
            .saturating_mul(1_000)
            .max(1);
        let platform = platform.into();
        known_key_trace_from_stage_durations(
            sample_id,
            key,
            platform,
            [250, 400, 750, 250, render_us],
            instrumentation_overhead_us,
            Some(frame.render_ms),
            Some(frame.gpu.adapter_name.clone()),
        )
    }
}
