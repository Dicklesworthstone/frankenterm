use frankenterm_core::large_swarm_replay::{
    LARGE_SWARM_REPLAY_CORPUS_VERSION, LargeSwarmRegressionThresholds, LargeSwarmReplayCorpus,
    LargeSwarmScenario, evaluate_large_swarm_thresholds, generate_large_swarm_corpus,
    summarize_large_swarm_replay, summarize_required_scale_points,
};
use frankenterm_core::recording::RECORDER_EVENT_SCHEMA_VERSION_V1;

#[test]
fn required_scale_points_cover_large_swarm_contract() {
    let pane_counts: Vec<u64> = LargeSwarmScenario::required_scale_points()
        .iter()
        .map(|scenario| scenario.pane_count)
        .collect();
    assert_eq!(pane_counts, vec![10, 50, 200, 1_000]);
}

#[test]
fn corpus_schema_roundtrips_json() {
    let scenario = LargeSwarmScenario::scale_point(10).expect("10-pane scenario");
    let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");

    assert_eq!(corpus.version, LARGE_SWARM_REPLAY_CORPUS_VERSION);
    assert!(
        corpus
            .events
            .iter()
            .all(|event| event.schema_version == RECORDER_EVENT_SCHEMA_VERSION_V1)
    );

    let json = serde_json::to_string(&corpus).expect("serialize corpus");
    let roundtrip: LargeSwarmReplayCorpus =
        serde_json::from_str(&json).expect("deserialize corpus");
    assert_eq!(roundtrip, corpus);
}

#[test]
fn canonical_order_is_stable_after_reversal() {
    let scenario = LargeSwarmScenario::scale_point(50).expect("50-pane scenario");
    let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");
    let expected_ids: Vec<String> = corpus
        .events
        .iter()
        .map(|event| event.event_id.clone())
        .collect();

    let mut reversed = corpus.clone();
    reversed.events.reverse();
    reversed.canonicalize();
    let actual_ids: Vec<String> = reversed
        .events
        .iter()
        .map(|event| event.event_id.clone())
        .collect();

    assert_eq!(actual_ids, expected_ids);
}

#[test]
fn replay_summary_is_deterministic_for_1000_panes() {
    let scenario = LargeSwarmScenario::scale_point(1_000).expect("1000-pane scenario");
    let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");

    let first = summarize_large_swarm_replay(&corpus).expect("first summary");
    let second = summarize_large_swarm_replay(&corpus).expect("second summary");

    assert_eq!(first, second);
    assert_eq!(first.pane_count, 1_000);
    assert_eq!(first.compaction_waves, 3_000);
    assert_eq!(first.search_queries, 50);
    assert_eq!(first.mission_actions, 30);
    assert_eq!(first.event_count, first.replay_frames);
    assert_eq!(first.collectors.latency_arrivals, first.event_count);
    assert_eq!(first.collectors.latency_completions, first.event_count);
    assert_eq!(first.collectors.admission_arrivals, 80);
    assert_eq!(first.collectors.admission_completions, 80);
    assert_eq!(first.collectors.memory_panes_registered, 1_000);
    assert_eq!(first.collectors.memory_samples, 1);
    assert_eq!(first.collectors.storage_events_appended, first.event_count);
    assert_eq!(first.collectors.storage_batches, 1);
    assert_eq!(first.collectors.storage_flushes, 1);
    assert!(first.summary_digest.starts_with("fnv1a64:"));
}

#[test]
fn all_required_scale_points_pass_default_thresholds() {
    for scenario in LargeSwarmScenario::required_scale_points() {
        let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");
        let summary = summarize_large_swarm_replay(&corpus).expect("summary");
        let thresholds = LargeSwarmRegressionThresholds::for_scenario(&scenario);
        let verdict = evaluate_large_swarm_thresholds(&summary, &thresholds);

        assert!(
            verdict.passed,
            "scenario {} should pass thresholds: {:?}",
            scenario.scenario_id, verdict.diffs
        );
    }
}

#[test]
fn threshold_verdict_reports_explicit_diffs() {
    let scenario = LargeSwarmScenario::scale_point(10).expect("10-pane scenario");
    let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");
    let summary = summarize_large_swarm_replay(&corpus).expect("summary");
    let thresholds = LargeSwarmRegressionThresholds {
        max_event_count: summary.event_count - 1,
        max_duration_ms: summary.duration_ms,
        max_output_bytes: summary.output_bytes,
        max_events_per_pane: summary.max_events_per_pane,
    };

    let verdict = evaluate_large_swarm_thresholds(&summary, &thresholds);

    assert!(!verdict.passed);
    assert_eq!(verdict.diffs.len(), 1);
    assert_eq!(verdict.diffs[0].field, "event_count");
    assert!(verdict.diffs[0].message.contains("event_count"));
}

#[test]
fn summarize_required_scale_points_returns_deterministic_digest_set() {
    let first = summarize_required_scale_points().expect("first summaries");
    let second = summarize_required_scale_points().expect("second summaries");

    assert_eq!(first, second);
    assert_eq!(first.len(), 4);
    assert!(
        first
            .iter()
            .all(|summary| summary.summary_digest.starts_with("fnv1a64:"))
    );
}
