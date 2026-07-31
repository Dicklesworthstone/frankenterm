use frankenterm_gui::renderer_slo::{
    InputToPhotonInputClass, InputToPhotonStage, InputToPhotonState, MACOS_P95_TARGET_US,
    classified_input_proxy_trace_from_stage_durations, summarize_input_to_photon_traces,
    target_p95_us_for_platform, unavailable_proxy_evidence,
};

#[test]
fn input_to_photon_proxy_summary_reports_percentiles_without_target_verdict() {
    let traces = [
        classified_input_proxy_trace_from_stage_durations(
            0,
            InputToPhotonInputClass::PrintableText,
            1,
            "macos",
            [100, 200, 300, 400, 1_000],
            20,
            1,
            "deterministic-test-adapter",
        )
        .expect("valid proxy trace"),
        classified_input_proxy_trace_from_stage_durations(
            1,
            InputToPhotonInputClass::PrintableText,
            1,
            "macos",
            [200, 300, 400, 500, 2_000],
            25,
            2,
            "deterministic-test-adapter",
        )
        .expect("valid proxy trace"),
        classified_input_proxy_trace_from_stage_durations(
            2,
            InputToPhotonInputClass::PrintableText,
            1,
            "macos",
            [300, 400, 500, 600, 3_000],
            30,
            3,
            "deterministic-test-adapter",
        )
        .expect("valid proxy trace"),
    ];

    let evidence = summarize_input_to_photon_traces("macos", &traces);

    assert_eq!(evidence.state, InputToPhotonState::Measured);
    assert_eq!(evidence.sample_count, 3);
    assert_eq!(evidence.target_p95_us, Some(MACOS_P95_TARGET_US));
    assert_eq!(
        evidence.gpu_adapter.as_deref(),
        Some("deterministic-test-adapter")
    );
    assert_eq!(evidence.p50_us, Some(3400));
    assert_eq!(evidence.p95_us, Some(4800));
    assert_eq!(evidence.p99_us, Some(4800));
    assert_eq!(evidence.within_target, None);
    assert!(
        evidence
            .stage_breakdown_p50
            .contains_key("term_update_to_render_submit")
    );
}

#[test]
fn excessive_instrumentation_overhead_degrades_the_evidence_state() {
    let trace = classified_input_proxy_trace_from_stage_durations(
        0,
        InputToPhotonInputClass::PrintableText,
        1,
        "wayland",
        [100, 100, 100, 100, 1_000],
        100,
        1,
        "deterministic-test-adapter",
    )
    .expect("valid proxy trace");

    let evidence = summarize_input_to_photon_traces("wayland", &[trace]);

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
fn unavailable_proxy_evidence_does_not_claim_a_target_result() {
    let evidence = unavailable_proxy_evidence("wayland", "no GPU access on runner");

    assert_eq!(evidence.sample_count, 0);
    assert_eq!(evidence.p95_us, None);
    assert_eq!(evidence.within_target, None);
    assert_eq!(evidence.gpu_adapter, None);
    assert_eq!(
        evidence.target_p95_us,
        target_p95_us_for_platform("wayland")
    );
}

#[test]
fn empty_trace_summary_is_degraded_not_measured() {
    let evidence = summarize_input_to_photon_traces("wayland", &[]);

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

#[cfg(feature = "headless-render")]
#[test]
fn headless_trace_preserves_measured_present_duration() {
    use frankenterm_gui::headless_render::{
        HeadlessFrame, HeadlessGpuInfo, HeadlessMultiMonitorTelemetry,
    };
    use frankenterm_gui::renderer_slo::headless::trace_from_headless_frame;

    let frame = HeadlessFrame {
        rgba: vec![0; 4],
        width: 1,
        height: 1,
        dpi: 96.0,
        texture_format: "Rgba8UnormSrgb".to_string(),
        render_ms: 42,
        fonts_loaded: 0,
        glyphs_cached: 0,
        gpu: HeadlessGpuInfo {
            backend: "test".to_string(),
            adapter_name: "deterministic-test-adapter".to_string(),
            vendor: 0,
            device: 0,
            device_type: "test".to_string(),
            driver: None,
            driver_info: None,
        },
        multi_monitor: HeadlessMultiMonitorTelemetry::default(),
    };

    let trace = trace_from_headless_frame(
        7,
        InputToPhotonInputClass::PrintableText,
        1,
        "macos",
        &frame,
        10,
    )
    .expect("headless frame must produce a valid proxy trace");
    let render_submit = trace
        .stages
        .iter()
        .find(|stage| stage.stage == InputToPhotonStage::RenderSubmit)
        .expect("render_submit stage");
    let gpu_present = trace
        .stages
        .iter()
        .find(|stage| stage.stage == InputToPhotonStage::GpuPresent)
        .expect("gpu_present stage");

    assert_eq!(render_submit.duration_us, 250);
    assert_eq!(gpu_present.duration_us, 42_000);
    assert_eq!(trace.total_latency_us, Some(43_650));
    assert_eq!(
        trace.gpu_adapter.as_deref(),
        Some("deterministic-test-adapter")
    );
}

#[test]
fn target_mapping_preserves_platform_specific_slo() {
    assert_eq!(target_p95_us_for_platform("macos"), Some(16_000));
    assert_eq!(target_p95_us_for_platform("wayland"), Some(20_000));
    assert_eq!(target_p95_us_for_platform("linux"), None);
    assert_eq!(target_p95_us_for_platform("freebsd"), None);
    assert_eq!(
        format!("{}", InputToPhotonStage::RenderSubmit),
        "render_submit"
    );
}
