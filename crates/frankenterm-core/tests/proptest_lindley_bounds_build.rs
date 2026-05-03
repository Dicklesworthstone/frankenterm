use frankenterm_core::latency_stages::{
    LINDLEY_ATTESTATION_STAGES, LatencyStage, LindleyStageTelemetry, LindleyTelemetryModel,
};
use frankenterm_core::network_calculus_bound::{
    LindleyBoundsArtifact, StageModel, pipeline_delay_bound,
};
use proptest::prelude::*;
use serde_json::Value;

fn leaf_stage_strategy() -> impl Strategy<Value = LatencyStage> {
    prop::sample::select(vec![
        LatencyStage::PtyCapture,
        LatencyStage::DeltaExtraction,
        LatencyStage::StorageWrite,
        LatencyStage::PatternDetection,
        LatencyStage::EventEmission,
        LatencyStage::WorkflowDispatch,
        LatencyStage::ActionExecution,
        LatencyStage::ApiResponse,
    ])
}

fn any_stage_strategy() -> impl Strategy<Value = LatencyStage> {
    prop::sample::select(vec![
        LatencyStage::PtyCapture,
        LatencyStage::DeltaExtraction,
        LatencyStage::StorageWrite,
        LatencyStage::PatternDetection,
        LatencyStage::EventEmission,
        LatencyStage::WorkflowDispatch,
        LatencyStage::ActionExecution,
        LatencyStage::ApiResponse,
        LatencyStage::EndToEndCapture,
        LatencyStage::EndToEndAction,
    ])
}

fn valid_stage_strategy() -> impl Strategy<Value = LindleyStageTelemetry> {
    (leaf_stage_strategy(), 1.0_f64..=10_000.0, 0.0_f64..=1_000.0).prop_map(
        |(stage, service_rate, p99_latency)| {
            LindleyStageTelemetry::try_new(stage, service_rate, p99_latency)
                .expect("generated stage telemetry is valid")
        },
    )
}

fn expected_stage_name(stage: LatencyStage) -> &'static str {
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

fn close_after_json_roundtrip(actual: f64, expected: f64) -> bool {
    let scale = actual.abs().max(expected.abs()).max(1.0);
    (actual - expected).abs() <= f64::EPSILON * scale * 8.0
}

fn artifact_from_model(
    model: LindleyTelemetryModel,
    release_version: String,
    empirical_p99_ms: f64,
) -> LindleyBoundsArtifact {
    let (arrival, stages) = model
        .to_network_calculus_inputs()
        .expect("generated model is valid");
    let analytical_bound_ms =
        pipeline_delay_bound(arrival, &stages).expect("generated model is stable");
    LindleyBoundsArtifact {
        release_version,
        arrival,
        stages,
        analytical_bound_ms,
        empirical_p99_ms,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_lindley_bounds_build_stage_validation_matches_public_contract(
        stage in any_stage_strategy(),
        service_rate in any::<f64>(),
        p99_latency in any::<f64>(),
    ) {
        let result = LindleyStageTelemetry::try_new(stage, service_rate, p99_latency);
        let expected_ok = !stage.is_aggregate()
            && service_rate.is_finite()
            && service_rate > 0.0
            && p99_latency.is_finite()
            && p99_latency >= 0.0;

        prop_assert_eq!(result.is_ok(), expected_ok);
        if let Ok(row) = result {
            let stage_model = row.to_stage_model();
            prop_assert_eq!(stage_model.name.as_str(), expected_stage_name(stage));
            prop_assert_eq!(stage_model.service.rate(), service_rate);
            prop_assert_eq!(stage_model.service.latency(), p99_latency);
        }
    }

    #[test]
    fn proptest_lindley_bounds_build_model_conversion_preserves_arrival_and_stage_rows(
        burst in 0.0_f64..=1_000.0,
        arrival_rate in 0.000_001_f64..=9_999.0,
        stages in prop::collection::vec(valid_stage_strategy(), 1..=12),
    ) {
        let model = LindleyTelemetryModel::try_new(burst, arrival_rate, stages.clone())
            .expect("generated model is valid");
        let (arrival, stage_models) = model
            .to_network_calculus_inputs()
            .expect("validated model converts");

        prop_assert_eq!(arrival.burst(), burst);
        prop_assert_eq!(arrival.rate(), arrival_rate);
        prop_assert_eq!(stage_models.len(), stages.len());
        for (actual, expected) in stage_models.iter().zip(stages.iter()) {
            prop_assert_eq!(actual.name.as_str(), expected_stage_name(expected.stage));
            prop_assert_eq!(actual.service.rate(), expected.service_rate_events_per_sec);
            prop_assert_eq!(actual.service.latency(), expected.p99_latency_ms);
        }
    }

    #[test]
    fn proptest_lindley_bounds_build_documented_default_stays_release_attestable(
        empirical_p99_ms in 0.0_f64..=20.0,
    ) {
        let model = LindleyTelemetryModel::documented_default();
        let (arrival, stages) = model
            .to_network_calculus_inputs()
            .expect("documented default converts");
        let analytical_bound_ms = pipeline_delay_bound(arrival, &stages)
            .expect("documented default remains stable");
        let artifact = LindleyBoundsArtifact {
            release_version: "0.0.0-substrate".to_string(),
            arrival,
            stages,
            analytical_bound_ms,
            empirical_p99_ms,
        };
        let json: Value = serde_json::from_str(&artifact.render_attestation_json())
            .expect("attestation JSON parses");

        prop_assert_eq!(artifact.stages.len(), LINDLEY_ATTESTATION_STAGES.len());
        prop_assert!(artifact.analytical_bound_ms.is_finite());
        prop_assert_eq!(json["release_version"].as_str(), Some("0.0.0-substrate"));
        prop_assert_eq!(
            json["stages"].as_array().expect("stages array").len(),
            LINDLEY_ATTESTATION_STAGES.len()
        );
        prop_assert_eq!(
            json["within_tolerance"].as_bool().expect("boolean tolerance"),
            artifact.comparison().within_tolerance(),
        );
    }

    #[test]
    fn proptest_lindley_bounds_build_artifact_json_roundtrips_escaped_release_versions(
        release_version in "[A-Za-z0-9_./\\\\\\\"-]{0,64}",
        burst in 0.0_f64..=100.0,
        arrival_rate in 0.0_f64..=999.0,
        service_margin in 1.0_f64..=10_000.0,
        latency in 0.0_f64..=1_000.0,
        empirical_delta_pct in -20.0_f64..=20.0,
    ) {
        let stage = LindleyStageTelemetry::try_new(
            LatencyStage::PtyCapture,
            arrival_rate + service_margin,
            latency,
        )
        .expect("stable stage");
        let model = LindleyTelemetryModel::try_new(burst, arrival_rate.max(0.000_001), vec![stage])
            .expect("valid model");
        let mut artifact = artifact_from_model(model, release_version.clone(), 0.0);
        artifact.empirical_p99_ms = artifact.analytical_bound_ms * (1.0 + empirical_delta_pct / 100.0);
        let json: Value = serde_json::from_str(&artifact.render_attestation_json())
            .expect("escaped release version keeps valid JSON");

        prop_assert_eq!(json["release_version"].as_str(), Some(release_version.as_str()));
        let json_burst = json["arrival"]["burst"].as_f64().expect("arrival burst");
        let json_rate = json["arrival"]["rate"].as_f64().expect("arrival rate");
        prop_assert!(
            close_after_json_roundtrip(json_burst, artifact.arrival.burst()),
            "arrival burst changed after JSON roundtrip: json={json_burst:?} artifact={:?}",
            artifact.arrival.burst()
        );
        prop_assert!(
            close_after_json_roundtrip(json_rate, artifact.arrival.rate()),
            "arrival rate changed after JSON roundtrip: json={json_rate:?} artifact={:?}",
            artifact.arrival.rate()
        );
        prop_assert_eq!(
            json["stages"][0]["name"].as_str(),
            Some(expected_stage_name(LatencyStage::PtyCapture))
        );
        prop_assert_eq!(json["within_tolerance"].as_bool(), Some(true));
    }

    #[test]
    fn proptest_lindley_bounds_build_pipeline_bound_uses_composed_bottleneck_once(
        burst in 0.0_f64..=500.0,
        arrival_rate in 0.000_001_f64..=500.0,
        margins in prop::collection::vec(1.0_f64..=5_000.0, 1..=8),
        latencies in prop::collection::vec(0.0_f64..=200.0, 1..=8),
    ) {
        let len = margins.len().min(latencies.len());
        prop_assume!(len > 0);
        let stages: Vec<LindleyStageTelemetry> = (0..len)
            .map(|idx| {
                LindleyStageTelemetry::try_new(
                    LatencyStage::PIPELINE_STAGES[idx % LatencyStage::PIPELINE_STAGES.len()],
                    arrival_rate + margins[idx],
                    latencies[idx],
                )
                .expect("stable generated stage")
            })
            .collect();
        let model = LindleyTelemetryModel::try_new(burst, arrival_rate, stages.clone())
            .expect("valid stable model");
        let (arrival, stage_models) = model.to_network_calculus_inputs().expect("converts");
        let bound = pipeline_delay_bound(arrival, &stage_models).expect("stable pipeline");
        let bottleneck = stage_models
            .iter()
            .map(|stage: &StageModel| stage.service.rate())
            .fold(f64::INFINITY, f64::min);
        let total_latency: f64 = stage_models.iter().map(|stage| stage.service.latency()).sum();

        prop_assert_eq!(bound, total_latency + burst / bottleneck);
    }
}
