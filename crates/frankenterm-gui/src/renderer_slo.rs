//! Renderer SLO measurement helpers.
//!
//! The generic evidence contract lives in `frankenterm-core`; this GUI module
//! only adds the headless-render trace adapter used by the Criterion harness.

pub use frankenterm_core::render_quality::{
    INPUT_TO_PHOTON_CLAIM_ID, INPUT_TO_PHOTON_SCHEMA_VERSION, INPUT_TO_PHOTON_WORKLOAD_CLASS,
    InputToPhotonClaimScope, InputToPhotonEvidence, InputToPhotonInputClass, InputToPhotonStage,
    InputToPhotonStageTrace, InputToPhotonState, InputToPhotonTrace, MACOS_P95_TARGET_US,
    MAX_INPUT_BYTE_COUNT, MAX_INSTRUMENTATION_OVERHEAD_PCT, RENDERER_SSIM_PARITY_CURRENT_DEGRADATION,
    RENDERER_SSIM_PARITY_DEFAULT_MAX_CHANGED_PIXEL_FRACTION_PPM,
    RENDERER_SSIM_PARITY_DEFAULT_MAX_L_INF, RENDERER_SSIM_PARITY_DEFAULT_MIN_SSIM_PPM,
    RENDERER_SSIM_PARITY_MCP_RESOURCE_URI, RENDERER_SSIM_PARITY_STATUS, WAYLAND_P95_TARGET_US,
    classified_input_proxy_trace_from_stage_durations, headless_render_duration_us,
    summarize_input_to_photon_traces, target_p95_us_for_platform, unavailable_proxy_evidence,
};

#[cfg(feature = "headless-render")]
pub mod headless {
    use super::{
        InputToPhotonInputClass, InputToPhotonTrace,
        classified_input_proxy_trace_from_stage_durations, headless_render_duration_us,
    };
    use crate::headless_render::{HeadlessFixtureInput, HeadlessFrame, smoketest_input};

    pub fn classified_input_headless_fixture() -> HeadlessFixtureInput {
        let mut input = smoketest_input(800, 480, 96.0);
        input.lines = vec![
            "input-to-photon classified-input proxy fixture".to_string(),
            "input_class=printable_text input_bytes=1 claim_scope=proxy_only".to_string(),
            "deterministic frame flush path".to_string(),
        ];
        input
    }

    /// Converts a completed headless frame into a validated proxy trace.
    ///
    /// # Errors
    ///
    /// Returns an error if the classified input metadata, platform, timing, or
    /// GPU adapter identity cannot satisfy the proxy evidence contract.
    pub fn trace_from_headless_frame(
        sample_id: u64,
        input_class: InputToPhotonInputClass,
        input_byte_count: u32,
        platform: impl Into<String>,
        frame: &HeadlessFrame,
        instrumentation_overhead_us: u64,
    ) -> Result<InputToPhotonTrace, String> {
        let render_us = headless_render_duration_us(frame.render_ms);
        let platform = platform.into();
        classified_input_proxy_trace_from_stage_durations(
            sample_id,
            input_class,
            input_byte_count,
            platform,
            [250, 400, 750, 250, render_us],
            instrumentation_overhead_us,
            frame.render_ms,
            frame.gpu.adapter_name.clone(),
        )
    }
}
