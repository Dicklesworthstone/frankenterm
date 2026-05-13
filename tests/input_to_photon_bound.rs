use frankenterm_core::network_calculus_bound::{
    ArrivalCurve, EmpiricalComparison, ServiceCurve, StageModel, TOLERANCE_PCT,
    pipeline_delay_bound,
};
use frankenterm_gui::renderer_slo::{
    InputToPhotonTrace, known_key_trace_from_stage_durations, summarize_input_to_photon_traces,
};

fn lindley_stages_from_trace(trace: &InputToPhotonTrace) -> Vec<StageModel> {
    trace
        .stages
        .windows(2)
        .map(|window| {
            let from = window[0].stage;
            let to = window[1].stage;
            let service_latency_ms = window[1].duration_us as f64 / 1_000.0;
            StageModel::new(
                format!("{from}_to_{to}"),
                ServiceCurve::new(1_000.0, service_latency_ms),
            )
        })
        .collect()
}

#[test]
fn input_to_photon_empirical_p99_agrees_with_lindley_bound() {
    let trace = known_key_trace_from_stage_durations(
        0,
        "a",
        "macos",
        [250, 400, 750, 600, 250],
        20,
        Some(1),
        Some("deterministic-test-adapter".to_string()),
    );
    let evidence = summarize_input_to_photon_traces("macos", std::slice::from_ref(&trace));
    let empirical_p99_ms = evidence.p99_us.expect("p99 present") as f64 / 1_000.0;

    let arrival = ArrivalCurve::new(0.0, 1.0);
    let stages = lindley_stages_from_trace(&trace);
    let analytical_bound_ms =
        pipeline_delay_bound(arrival, &stages).expect("stable input-to-photon service curve");

    let comparison = EmpiricalComparison {
        analytical_bound_ms,
        empirical_p99_ms,
    };

    assert!(
        comparison.within_tolerance(),
        "empirical p99 {empirical_p99_ms:.3}ms should stay within {TOLERANCE_PCT:.1}% of Lindley bound {analytical_bound_ms:.3}ms"
    );
    assert!(
        comparison.deviation_pct().unwrap_or(f64::INFINITY) <= 1.0,
        "deterministic known-key trace should be nearly exact"
    );
}
