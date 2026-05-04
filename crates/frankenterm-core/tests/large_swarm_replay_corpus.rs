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

// ── br-ft-o60ul cc1 property-test slice ────────────────────────────

/// br-ft-o60ul: deterministic-summary contract MUST hold across
/// EVERY required scale point, not just the 1000-pane case the
/// existing `replay_summary_is_deterministic_for_1000_panes` test
/// pins. Property-style sweep: for each of {10, 50, 200, 1000}
/// panes, generate the corpus twice + summarize twice + assert the
/// two summaries are byte-identical (including the fnv1a64 digest).
/// Pre-fix the determinism guarantee was only pinned at the largest
/// scale; this test catches a regression at any scale point that
/// would have shipped silently.
#[test]
fn replay_summary_is_deterministic_at_every_scale_point_ft_o60ul() {
    for scenario in LargeSwarmScenario::required_scale_points() {
        let corpus_a =
            generate_large_swarm_corpus(&scenario).expect("first corpus generation must succeed");
        let corpus_b =
            generate_large_swarm_corpus(&scenario).expect("second corpus generation must succeed");
        assert_eq!(
            corpus_a, corpus_b,
            "ft-o60ul: corpus generation must be byte-identical across calls \
             (scale={}, scenario_id={})",
            scenario.pane_count, scenario.scenario_id
        );

        let summary_a =
            summarize_large_swarm_replay(&corpus_a).expect("first summary must succeed");
        let summary_b =
            summarize_large_swarm_replay(&corpus_b).expect("second summary must succeed");
        assert_eq!(
            summary_a, summary_b,
            "ft-o60ul: replay summary must be byte-identical across calls \
             (scale={}, scenario_id={})",
            scenario.pane_count, scenario.scenario_id
        );

        // The digest is the load-bearing diff anchor — pin its shape
        // (non-empty + fnv1a64: prefix) at every scale.
        assert!(
            summary_a.summary_digest.starts_with("fnv1a64:"),
            "ft-o60ul: summary_digest must use the fnv1a64 prefix at every scale \
             (scale={}, got={:?})",
            scenario.pane_count,
            summary_a.summary_digest
        );
        assert!(
            summary_a.summary_digest.len() > "fnv1a64:".len(),
            "ft-o60ul: summary_digest must have a non-empty hash portion \
             (scale={}, got={:?})",
            scenario.pane_count,
            summary_a.summary_digest
        );
    }
}

/// br-ft-o60ul: corpus serde roundtrip must hold at EVERY required
/// scale point. The existing `corpus_schema_roundtrips_json` test
/// pins this for the 10-pane case only; this property-style sweep
/// extends the contract to the larger scales where serde
/// performance + size characteristics may surface schema-shape
/// regressions that the small case wouldn't catch.
#[test]
fn corpus_schema_roundtrips_json_at_every_scale_point_ft_o60ul() {
    for scenario in LargeSwarmScenario::required_scale_points() {
        let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");
        let json =
            serde_json::to_string(&corpus).expect("ft-o60ul: corpus serializes at every scale");
        let roundtrip: LargeSwarmReplayCorpus =
            serde_json::from_str(&json).expect("ft-o60ul: corpus deserializes at every scale");
        assert_eq!(
            roundtrip, corpus,
            "ft-o60ul: serde roundtrip must be lossless at scale={} (scenario={})",
            scenario.pane_count, scenario.scenario_id
        );
        // Schema-version pin: every event must declare the expected
        // V1 recorder schema regardless of scale.
        assert!(
            corpus
                .events
                .iter()
                .all(|event| event.schema_version == RECORDER_EVENT_SCHEMA_VERSION_V1),
            "ft-o60ul: all events at scale={} must use V1 schema",
            scenario.pane_count
        );
    }
}

/// br-ft-o60ul: every threshold violation must produce a diff with
/// a non-empty field name AND a non-empty message that cites the
/// field. Pins the "useful diffs when changed" acceptance criterion
/// via a universal-quantifier check across all 4 threshold fields.
/// Pre-fix a generic / empty diff message would slip through; this
/// test forces every field violation to carry actionable context
/// for operator triage.
#[test]
fn threshold_diffs_are_actionable_for_every_field_ft_o60ul() {
    let scenario = LargeSwarmScenario::scale_point(10).expect("10-pane scenario");
    let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");
    let summary = summarize_large_swarm_replay(&corpus).expect("summary");

    // Force ALL four thresholds below the actual value so every
    // field violates and produces a diff.
    let thresholds = LargeSwarmRegressionThresholds {
        max_event_count: summary.event_count.saturating_sub(1),
        max_duration_ms: summary.duration_ms.saturating_sub(1),
        max_output_bytes: summary.output_bytes.saturating_sub(1),
        max_events_per_pane: summary.max_events_per_pane.saturating_sub(1),
    };

    let verdict = evaluate_large_swarm_thresholds(&summary, &thresholds);

    assert!(
        !verdict.passed,
        "ft-o60ul: forcing all thresholds below actual must fail the verdict"
    );
    // Each diff must name its field + carry a non-empty message
    // citing the field name. Operators reading a regression report
    // rely on these for actionable triage.
    for diff in &verdict.diffs {
        assert!(
            !diff.field.is_empty(),
            "ft-o60ul: threshold diff must name its field (got empty)"
        );
        assert!(
            !diff.message.is_empty(),
            "ft-o60ul: threshold diff must carry a non-empty message (field={})",
            diff.field
        );
        assert!(
            diff.message.contains(&diff.field),
            "ft-o60ul: threshold diff message must cite the field name for triage \
             (field={}, message={})",
            diff.field,
            diff.message
        );
    }
}
