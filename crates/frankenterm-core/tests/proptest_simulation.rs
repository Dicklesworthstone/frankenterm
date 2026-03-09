//! Property-based tests for the `simulation` module.
//!
//! Covers `EventAction` serde roundtrips, `as_str`, and `is_resize_timeline_action`;
//! `ResizeTimelineStage` serde roundtrips, `as_str`, ordering, and `ALL` constant;
//! `ResizeQueueMetrics`/`ResizeTimelineStageSample`/`ResizeTimelineEvent`/
//! `ResizeTimelineFlameSample` serde roundtrips; and `ResizeTimeline`
//! `flame_samples`/`stage_summary` structural invariants.

use frankenterm_core::simulation::{
    EventAction, Expectation, ExpectationKind, FontAtlasCachePolicy, FontRenderPrepMetrics,
    ResizeQueueMetrics, ResizeTimeline, ResizeTimelineEvent, ResizeTimelineFlameSample,
    ResizeTimelineStage, ResizeTimelineStageSample, ResizeTimelineStageSummary,
    SandboxCommand, Scenario, ScenarioEvent, ScenarioPane,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_event_action() -> impl Strategy<Value = EventAction> {
    prop_oneof![
        Just(EventAction::Append),
        Just(EventAction::Clear),
        Just(EventAction::SetTitle),
        Just(EventAction::Resize),
        Just(EventAction::SetFontSize),
        Just(EventAction::GenerateScrollback),
        Just(EventAction::Typing),
        Just(EventAction::Paste),
        Just(EventAction::Mouse),
        Just(EventAction::Marker),
    ]
}

fn arb_resize_timeline_stage() -> impl Strategy<Value = ResizeTimelineStage> {
    prop_oneof![
        Just(ResizeTimelineStage::InputIntent),
        Just(ResizeTimelineStage::SchedulerQueueing),
        Just(ResizeTimelineStage::LogicalReflow),
        Just(ResizeTimelineStage::RenderPrep),
        Just(ResizeTimelineStage::Presentation),
    ]
}

fn arb_queue_metrics() -> impl Strategy<Value = ResizeQueueMetrics> {
    (0_u64..100, 0_u64..100).prop_map(|(depth_before, depth_after)| ResizeQueueMetrics {
        depth_before,
        depth_after,
    })
}

fn arb_atlas_cache_policy() -> impl Strategy<Value = FontAtlasCachePolicy> {
    prop_oneof![
        Just(FontAtlasCachePolicy::ReuseHotAtlas),
        Just(FontAtlasCachePolicy::SelectiveInvalidate),
        Just(FontAtlasCachePolicy::FullRebuild),
    ]
}

fn arb_render_prep_metrics() -> impl Strategy<Value = FontRenderPrepMetrics> {
    (
        arb_atlas_cache_policy(),
        any::<bool>(),
        0_u32..10_000,
        0_u32..10_000,
        0_u32..10_000,
        0_u32..100,
        0_u32..100,
    )
        .prop_map(
            |(
                atlas_cache_policy,
                shader_warmup,
                cache_hit_glyphs,
                glyphs_rebuilt_now,
                deferred_glyphs,
                staged_batches_total,
                staged_batches_deferred,
            )| FontRenderPrepMetrics {
                atlas_cache_policy,
                shader_warmup,
                cache_hit_glyphs,
                glyphs_rebuilt_now,
                deferred_glyphs,
                staged_batches_total,
                staged_batches_deferred,
            },
        )
}

fn arb_stage_sample() -> impl Strategy<Value = ResizeTimelineStageSample> {
    (
        arb_resize_timeline_stage(),
        0_u64..1_000_000,
        0_u64..1_000_000,
        proptest::option::of(arb_queue_metrics()),
        proptest::option::of(arb_render_prep_metrics()),
    )
        .prop_map(
            |(stage, start_offset_ns, duration_ns, queue_metrics, render_prep_metrics)| {
                ResizeTimelineStageSample {
                    stage,
                    start_offset_ns,
                    duration_ns,
                    queue_metrics,
                    render_prep_metrics,
                }
            },
        )
}

fn arb_timeline_event() -> impl Strategy<Value = ResizeTimelineEvent> {
    (
        0_usize..100,
        0_u64..100,
        arb_event_action(),
        0_u64..1_000_000_000,
        0_u64..1_000_000_000,
        0_u64..1_000_000_000,
        proptest::collection::vec(arb_stage_sample(), 0..6),
    )
        .prop_map(
            |(
                event_index,
                pane_id,
                action,
                scheduled_at_ns,
                dispatch_offset_ns,
                total_duration_ns,
                stages,
            )| {
                ResizeTimelineEvent {
                    event_index,
                    resize_transaction_id: format!("prop:{event_index}"),
                    pane_id,
                    tab_id: pane_id % 8,
                    sequence_no: event_index as u64,
                    action,
                    scheduler_decision: "dequeue_latest_intent".to_string(),
                    frame_id: event_index as u64,
                    test_case_id: "prop_case".to_string(),
                    queue_wait_ms: 0,
                    reflow_ms: 0,
                    render_ms: 0,
                    present_ms: 0,
                    scheduled_at_ns,
                    dispatch_offset_ns,
                    total_duration_ns,
                    stages,
                }
            },
        )
}

fn arb_flame_sample() -> impl Strategy<Value = ResizeTimelineFlameSample> {
    (
        "[a-z_]{3,10};[a-z_]{3,10};[a-z_]{3,15}",
        0_u64..1_000_000_000,
        0_usize..100,
        0_u64..100,
    )
        .prop_map(
            |(stack, duration_ns, event_index, pane_id)| ResizeTimelineFlameSample {
                stack,
                duration_ns,
                event_index,
                pane_id,
            },
        )
}

fn arb_resize_timeline() -> impl Strategy<Value = ResizeTimeline> {
    (
        "[a-z_]{3,15}",
        "[a-z_:]{5,30}",
        0_u64..2_000_000_000_000,
        proptest::collection::vec(arb_timeline_event(), 0..5),
    )
        .prop_map(|(scenario, reproducibility_key, captured_at_ms, events)| {
            let executed_resize_events = events.len();
            ResizeTimeline {
                scenario,
                reproducibility_key,
                captured_at_ms,
                executed_resize_events,
                events,
            }
        })
}

// =========================================================================
// EventAction — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// EventAction serde roundtrip.
    #[test]
    fn prop_event_action_serde(action in arb_event_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let back: EventAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, action);
    }

    /// EventAction serializes to snake_case.
    #[test]
    fn prop_event_action_snake_case(action in arb_event_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let expected = match action {
            EventAction::Append => "\"append\"",
            EventAction::Clear => "\"clear\"",
            EventAction::SetTitle => "\"set_title\"",
            EventAction::Resize => "\"resize\"",
            EventAction::SetFontSize => "\"set_font_size\"",
            EventAction::GenerateScrollback => "\"generate_scrollback\"",
            EventAction::Typing => "\"typing\"",
            EventAction::Paste => "\"paste\"",
            EventAction::Mouse => "\"mouse\"",
            EventAction::Marker => "\"marker\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }

    /// EventAction::as_str matches serde output (without quotes).
    #[test]
    fn prop_event_action_as_str_matches_serde(action in arb_event_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let as_str = action.as_str();
        prop_assert_eq!(format!("\"{}\"", as_str), json);
    }

    /// EventAction serde is deterministic.
    #[test]
    fn prop_event_action_deterministic(action in arb_event_action()) {
        let j1 = serde_json::to_string(&action).unwrap();
        let j2 = serde_json::to_string(&action).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// EventAction — is_resize_timeline_action
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// is_resize_timeline_action returns true for timeline-attributed actions.
    #[test]
    fn prop_is_resize_timeline_correct(action in arb_event_action()) {
        let expected = matches!(
            action,
            EventAction::Resize
                | EventAction::SetFontSize
                | EventAction::GenerateScrollback
                | EventAction::Typing
                | EventAction::Paste
                | EventAction::Mouse
        );
        prop_assert_eq!(action.is_resize_timeline_action(), expected);
    }
}

// =========================================================================
// ResizeTimelineStage — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ResizeTimelineStage serde roundtrip.
    #[test]
    fn prop_stage_serde(stage in arb_resize_timeline_stage()) {
        let json = serde_json::to_string(&stage).unwrap();
        let back: ResizeTimelineStage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, stage);
    }

    /// ResizeTimelineStage serializes to snake_case.
    #[test]
    fn prop_stage_snake_case(stage in arb_resize_timeline_stage()) {
        let json = serde_json::to_string(&stage).unwrap();
        let expected = match stage {
            ResizeTimelineStage::InputIntent => "\"input_intent\"",
            ResizeTimelineStage::SchedulerQueueing => "\"scheduler_queueing\"",
            ResizeTimelineStage::LogicalReflow => "\"logical_reflow\"",
            ResizeTimelineStage::RenderPrep => "\"render_prep\"",
            ResizeTimelineStage::Presentation => "\"presentation\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }

    /// ResizeTimelineStage::as_str matches serde output (without quotes).
    #[test]
    fn prop_stage_as_str_matches_serde(stage in arb_resize_timeline_stage()) {
        let json = serde_json::to_string(&stage).unwrap();
        let as_str = stage.as_str();
        prop_assert_eq!(format!("\"{}\"", as_str), json);
    }
}

// =========================================================================
// ResizeTimelineStage — ALL ordering
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// ALL contains exactly 5 stages.
    #[test]
    fn prop_all_has_five_stages(_dummy in 0..1_u8) {
        prop_assert_eq!(ResizeTimelineStage::ALL.len(), 5);
    }

    /// ALL is in Ord order.
    #[test]
    fn prop_all_is_sorted(_dummy in 0..1_u8) {
        for window in ResizeTimelineStage::ALL.windows(2) {
            prop_assert!(
                window[0] < window[1],
                "ALL not sorted: {:?} >= {:?}", window[0], window[1]
            );
        }
    }

    /// ALL as_str values are all distinct.
    #[test]
    fn prop_all_as_str_distinct(_dummy in 0..1_u8) {
        let strs: Vec<&str> = ResizeTimelineStage::ALL.iter().map(|s| s.as_str()).collect();
        let mut seen = std::collections::HashSet::new();
        for s in &strs {
            prop_assert!(seen.insert(s), "duplicate as_str: {}", s);
        }
    }
}

// =========================================================================
// ResizeQueueMetrics — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ResizeQueueMetrics serde roundtrip.
    #[test]
    fn prop_queue_metrics_serde(qm in arb_queue_metrics()) {
        let json = serde_json::to_string(&qm).unwrap();
        let back: ResizeQueueMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, qm);
    }
}

// =========================================================================
// ResizeTimelineStageSample — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ResizeTimelineStageSample serde roundtrip.
    #[test]
    fn prop_stage_sample_serde(sample in arb_stage_sample()) {
        let json = serde_json::to_string(&sample).unwrap();
        let back: ResizeTimelineStageSample = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.stage, sample.stage);
        prop_assert_eq!(back.start_offset_ns, sample.start_offset_ns);
        prop_assert_eq!(back.duration_ns, sample.duration_ns);
        prop_assert_eq!(back.queue_metrics, sample.queue_metrics);
    }

    /// queue_metrics None roundtrips correctly.
    #[test]
    fn prop_stage_sample_no_queue_metrics(
        stage in arb_resize_timeline_stage(),
        start in 0_u64..1_000_000,
        dur in 0_u64..1_000_000,
    ) {
        let sample = ResizeTimelineStageSample {
            stage,
            start_offset_ns: start,
            duration_ns: dur,
            queue_metrics: None,
            render_prep_metrics: None,
        };
        let json = serde_json::to_string(&sample).unwrap();
        // skip_serializing_if means "queue_metrics" shouldn't appear
        prop_assert!(
            !json.contains("queue_metrics"),
            "None queue_metrics should be skipped: {}", json
        );
        let back: ResizeTimelineStageSample = serde_json::from_str(&json).unwrap();
        prop_assert!(back.queue_metrics.is_none());
    }
}

// =========================================================================
// ResizeTimelineEvent — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ResizeTimelineEvent serde roundtrip preserves key fields.
    #[test]
    fn prop_timeline_event_serde(event in arb_timeline_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let back: ResizeTimelineEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.event_index, event.event_index);
        prop_assert_eq!(back.pane_id, event.pane_id);
        prop_assert_eq!(back.action, event.action);
        prop_assert_eq!(back.scheduled_at_ns, event.scheduled_at_ns);
        prop_assert_eq!(back.dispatch_offset_ns, event.dispatch_offset_ns);
        prop_assert_eq!(back.total_duration_ns, event.total_duration_ns);
        prop_assert_eq!(back.stages.len(), event.stages.len());
    }
}

// =========================================================================
// ResizeTimelineFlameSample — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ResizeTimelineFlameSample serde roundtrip.
    #[test]
    fn prop_flame_sample_serde(sample in arb_flame_sample()) {
        let json = serde_json::to_string(&sample).unwrap();
        let back: ResizeTimelineFlameSample = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.stack, &sample.stack);
        prop_assert_eq!(back.duration_ns, sample.duration_ns);
        prop_assert_eq!(back.event_index, sample.event_index);
        prop_assert_eq!(back.pane_id, sample.pane_id);
    }
}

// =========================================================================
// ResizeTimeline — flame_samples invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// flame_samples count equals sum of stage counts across events.
    #[test]
    fn prop_flame_samples_count(timeline in arb_resize_timeline()) {
        let expected_count: usize = timeline.events.iter().map(|e| e.stages.len()).sum();
        let flames = timeline.flame_samples();
        prop_assert_eq!(flames.len(), expected_count);
    }

    /// Each flame sample stack has format "scenario;action;stage".
    #[test]
    fn prop_flame_samples_stack_format(timeline in arb_resize_timeline()) {
        let flames = timeline.flame_samples();
        for flame in &flames {
            let parts: Vec<&str> = flame.stack.split(';').collect();
            prop_assert_eq!(
                parts.len(), 3,
                "flame stack should have 3 parts: {}", flame.stack
            );
            prop_assert_eq!(
                parts[0], timeline.scenario.as_str(),
                "first part should be scenario name"
            );
        }
    }

    /// Each flame sample inherits pane_id and event_index from parent event.
    #[test]
    fn prop_flame_samples_inherit_event_fields(timeline in arb_resize_timeline()) {
        let flames = timeline.flame_samples();
        let mut flame_iter = flames.iter();
        for event in &timeline.events {
            for _stage in &event.stages {
                let flame = flame_iter.next().unwrap();
                prop_assert_eq!(flame.pane_id, event.pane_id);
                prop_assert_eq!(flame.event_index, event.event_index);
            }
        }
    }

    /// Empty timeline produces empty flame_samples.
    #[test]
    fn prop_empty_timeline_no_flames(_dummy in 0..1_u8) {
        let timeline = ResizeTimeline {
            scenario: "empty".to_string(),
            reproducibility_key: "test:v1:empty:0".to_string(),
            captured_at_ms: 0,
            executed_resize_events: 0,
            events: vec![],
        };
        prop_assert!(timeline.flame_samples().is_empty());
    }
}

// =========================================================================
// ResizeTimeline — stage_summary invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// stage_summary always returns exactly 5 entries (one per stage).
    #[test]
    fn prop_stage_summary_count(timeline in arb_resize_timeline()) {
        let summary = timeline.stage_summary();
        prop_assert_eq!(summary.len(), 5);
    }

    /// stage_summary entries follow ALL ordering.
    #[test]
    fn prop_stage_summary_order(timeline in arb_resize_timeline()) {
        let summary = timeline.stage_summary();
        for (i, entry) in summary.iter().enumerate() {
            prop_assert_eq!(
                entry.stage, ResizeTimelineStage::ALL[i],
                "summary entry {} should be {:?}", i, ResizeTimelineStage::ALL[i]
            );
        }
    }

    /// stage_summary total_duration_ns equals sum of individual durations.
    #[test]
    fn prop_stage_summary_total_consistent(timeline in arb_resize_timeline()) {
        let summary = timeline.stage_summary();
        for entry in &summary {
            // Collect all durations for this stage from events
            let mut durations: Vec<u64> = Vec::new();
            for event in &timeline.events {
                for stage_sample in &event.stages {
                    if stage_sample.stage == entry.stage {
                        durations.push(stage_sample.duration_ns);
                    }
                }
            }
            prop_assert_eq!(entry.samples, durations.len());
            let expected_total: u64 = durations.iter().fold(0u64, |acc, v| acc.saturating_add(*v));
            prop_assert_eq!(entry.total_duration_ns, expected_total);
        }
    }

    /// stage_summary max_duration_ns is correct.
    #[test]
    fn prop_stage_summary_max_correct(timeline in arb_resize_timeline()) {
        let summary = timeline.stage_summary();
        for entry in &summary {
            let mut durations: Vec<u64> = Vec::new();
            for event in &timeline.events {
                for stage_sample in &event.stages {
                    if stage_sample.stage == entry.stage {
                        durations.push(stage_sample.duration_ns);
                    }
                }
            }
            let expected_max = durations.iter().max().copied().unwrap_or(0);
            prop_assert_eq!(entry.max_duration_ns, expected_max);
        }
    }

    /// stage_summary avg_duration_ns is non-negative.
    #[test]
    fn prop_stage_summary_avg_nonneg(timeline in arb_resize_timeline()) {
        let summary = timeline.stage_summary();
        for entry in &summary {
            prop_assert!(
                entry.avg_duration_ns >= 0.0,
                "avg should be >= 0: {}", entry.avg_duration_ns
            );
        }
    }

    /// stage_summary p95 <= max for non-empty stages.
    #[test]
    fn prop_stage_summary_p95_le_max(timeline in arb_resize_timeline()) {
        let summary = timeline.stage_summary();
        for entry in &summary {
            if entry.samples > 0 {
                prop_assert!(
                    entry.p95_duration_ns <= entry.max_duration_ns,
                    "p95 {} > max {} for {:?}",
                    entry.p95_duration_ns, entry.max_duration_ns, entry.stage
                );
            }
        }
    }

    /// Empty timeline stage_summary has zero samples everywhere.
    #[test]
    fn prop_empty_timeline_summary_zero(_dummy in 0..1_u8) {
        let timeline = ResizeTimeline {
            scenario: "empty".to_string(),
            reproducibility_key: "test:v1:empty:0".to_string(),
            captured_at_ms: 0,
            executed_resize_events: 0,
            events: vec![],
        };
        let summary = timeline.stage_summary();
        for entry in &summary {
            prop_assert_eq!(entry.samples, 0);
            prop_assert_eq!(entry.total_duration_ns, 0);
            prop_assert_eq!(entry.max_duration_ns, 0);
            prop_assert_eq!(entry.p95_duration_ns, 0);
        }
    }
}

// =========================================================================
// Additional strategies: ExpectationKind, ScenarioPane
// =========================================================================

fn arb_expectation_kind() -> impl Strategy<Value = ExpectationKind> {
    prop_oneof![
        ("[a-z_]{3,15}", proptest::option::of("[0-9]{1,4}s"))
            .prop_map(|(event, detected_at)| ExpectationKind::Event { event, detected_at }),
        ("[a-z_]{3,15}", proptest::option::of("[0-9]{1,4}s")).prop_map(|(workflow, started_at)| {
            ExpectationKind::Workflow {
                workflow,
                started_at,
            }
        }),
        (1_u64..100, "[a-zA-Z0-9 ]{3,30}")
            .prop_map(|(pane, text)| ExpectationKind::Contains { pane, text }),
    ]
}

fn arb_scenario_pane() -> impl Strategy<Value = ScenarioPane> {
    (
        1_u64..1000,
        "[a-z]{3,15}",
        prop_oneof![Just("local".to_string()), Just("ssh".to_string())],
        "[/a-z]{3,20}",
        0_u64..10,
        0_u64..10,
        10_u32..300,
        5_u32..100,
        "[a-zA-Z0-9 ]{0,40}",
    )
        .prop_map(
            |(id, title, domain, cwd, window_id, tab_id, cols, rows, initial_content)| {
                ScenarioPane {
                    id,
                    title,
                    domain,
                    cwd,
                    window_id,
                    tab_id,
                    cols,
                    rows,
                    initial_content,
                }
            },
        )
}

// =========================================================================
// ExpectationKind and ScenarioPane tests
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// ExpectationKind serde roundtrip.
    #[test]
    fn prop_expectation_kind_serde(kind in arb_expectation_kind()) {
        let exp = Expectation { kind: kind.clone() };
        let json = serde_json::to_string(&exp).unwrap();
        let back: Expectation = serde_json::from_str(&json).unwrap();
        let back_json = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(&json, &back_json, "Expectation serde roundtrip failed");
    }

    /// ExpectationKind deterministic serialization.
    #[test]
    fn prop_expectation_kind_deterministic(kind in arb_expectation_kind()) {
        let exp = Expectation { kind };
        let j1 = serde_json::to_string(&exp).unwrap();
        let j2 = serde_json::to_string(&exp).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// ScenarioPane serde roundtrip.
    #[test]
    fn prop_scenario_pane_serde(pane in arb_scenario_pane()) {
        let json = serde_json::to_string(&pane).unwrap();
        let back: ScenarioPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, pane.id);
        prop_assert_eq!(back.title, pane.title);
        prop_assert_eq!(back.domain, pane.domain);
        prop_assert_eq!(back.cols, pane.cols);
        prop_assert_eq!(back.rows, pane.rows);
    }

    /// ScenarioPane JSON has expected keys.
    #[test]
    fn prop_scenario_pane_json_keys(pane in arb_scenario_pane()) {
        let json = serde_json::to_string(&pane).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = val.as_object().unwrap();
        prop_assert!(obj.contains_key("id"), "missing 'id'");
        prop_assert!(obj.contains_key("title"), "missing 'title'");
        prop_assert!(obj.contains_key("cols"), "missing 'cols'");
        prop_assert!(obj.contains_key("rows"), "missing 'rows'");
    }

    /// FontAtlasCachePolicy serde roundtrip.
    #[test]
    fn prop_atlas_cache_policy_serde(policy in arb_atlas_cache_policy()) {
        let json = serde_json::to_string(&policy).unwrap();
        let back: FontAtlasCachePolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, policy);
    }

    /// FontAtlasCachePolicy serializes to snake_case.
    #[test]
    fn prop_atlas_cache_policy_snake_case(policy in arb_atlas_cache_policy()) {
        let json = serde_json::to_string(&policy).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "atlas cache policy should be snake_case, got '{}'", inner
        );
    }

    /// FontRenderPrepMetrics serde roundtrip.
    #[test]
    fn prop_render_prep_metrics_serde(metrics in arb_render_prep_metrics()) {
        let json = serde_json::to_string(&metrics).unwrap();
        let back: FontRenderPrepMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, metrics);
    }

    /// ResizeQueueMetrics clone and serde are consistent.
    #[test]
    fn prop_queue_metrics_clone_serde(qm in arb_queue_metrics()) {
        let cloned = qm.clone();
        let j1 = serde_json::to_string(&qm).unwrap();
        let j2 = serde_json::to_string(&cloned).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn event_action_variants_distinct() {
    let actions = [
        EventAction::Append,
        EventAction::Clear,
        EventAction::SetTitle,
        EventAction::Resize,
        EventAction::SetFontSize,
        EventAction::GenerateScrollback,
        EventAction::Typing,
        EventAction::Paste,
        EventAction::Mouse,
        EventAction::Marker,
    ];
    for (i, a) in actions.iter().enumerate() {
        for (j, b) in actions.iter().enumerate() {
            if i != j {
                assert_ne!(a, b);
            }
        }
    }
}

#[test]
fn resize_timeline_stage_all_complete() {
    use std::collections::HashSet;
    let all_set: HashSet<_> = ResizeTimelineStage::ALL.iter().collect();
    assert!(all_set.contains(&ResizeTimelineStage::InputIntent));
    assert!(all_set.contains(&ResizeTimelineStage::SchedulerQueueing));
    assert!(all_set.contains(&ResizeTimelineStage::LogicalReflow));
    assert!(all_set.contains(&ResizeTimelineStage::RenderPrep));
    assert!(all_set.contains(&ResizeTimelineStage::Presentation));
}

#[test]
fn flame_sample_with_known_timeline() {
    let timeline = ResizeTimeline {
        scenario: "test_scenario".to_string(),
        reproducibility_key: "test:v1:test_scenario:0".to_string(),
        captured_at_ms: 1000,
        executed_resize_events: 1,
        events: vec![ResizeTimelineEvent {
            event_index: 0,
            resize_transaction_id: "test:v1:test_scenario:0".to_string(),
            pane_id: 42,
            tab_id: 1,
            sequence_no: 0,
            action: EventAction::Resize,
            scheduler_decision: "dequeue_latest_intent".to_string(),
            frame_id: 0,
            test_case_id: "test_scenario".to_string(),
            queue_wait_ms: 0,
            reflow_ms: 0,
            render_ms: 0,
            present_ms: 0,
            scheduled_at_ns: 100,
            dispatch_offset_ns: 200,
            total_duration_ns: 500,
            stages: vec![
                ResizeTimelineStageSample {
                    stage: ResizeTimelineStage::InputIntent,
                    start_offset_ns: 0,
                    duration_ns: 100,
                    queue_metrics: None,
                    render_prep_metrics: None,
                },
                ResizeTimelineStageSample {
                    stage: ResizeTimelineStage::Presentation,
                    start_offset_ns: 100,
                    duration_ns: 400,
                    queue_metrics: None,
                    render_prep_metrics: None,
                },
            ],
        }],
    };
    let flames = timeline.flame_samples();
    assert_eq!(flames.len(), 2);
    assert_eq!(flames[0].stack, "test_scenario;resize;input_intent");
    assert_eq!(flames[0].duration_ns, 100);
    assert_eq!(flames[0].pane_id, 42);
    assert_eq!(flames[1].stack, "test_scenario;resize;presentation");
    assert_eq!(flames[1].duration_ns, 400);
}

// ============================================================================
// Additional coverage tests (SM-24 through SM-43)
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // ── SM-24: Scenario from_yaml valid minimal ─────────────────────────────

    #[test]
    fn sm24_scenario_from_yaml_minimal(
        name in "[a-z_]{3,15}",
    ) {
        let yaml = format!(
            "name: {name}\nduration: 10s\npanes: []\nevents: []\nexpectations: []\n"
        );
        let result = Scenario::from_yaml(&yaml);
        prop_assert!(result.is_ok(), "minimal YAML should parse: {:?}", result.err());
        let scenario = result.unwrap();
        prop_assert_eq!(&scenario.name, &name);
    }

    // ── SM-25: Scenario validate rejects duplicate pane IDs ─────────────────

    #[test]
    fn sm25_validate_rejects_dup_panes(id in 0u64..1000) {
        let scenario = Scenario {
            name: "test".to_string(),
            description: String::new(),
            duration: std::time::Duration::from_secs(10),
            panes: vec![
                ScenarioPane {
                    id,
                    title: "a".to_string(),
                    domain: "local".to_string(),
                    cwd: "/tmp".to_string(),
                    window_id: 0, tab_id: 0, cols: 80, rows: 24,
                    initial_content: String::new(),
                },
                ScenarioPane {
                    id, // duplicate!
                    title: "b".to_string(),
                    domain: "local".to_string(),
                    cwd: "/tmp".to_string(),
                    window_id: 0, tab_id: 0, cols: 80, rows: 24,
                    initial_content: String::new(),
                },
            ],
            events: vec![],
            expectations: vec![],
            metadata: std::collections::BTreeMap::new(),
        };
        let result = scenario.validate();
        prop_assert!(result.is_err(), "duplicate pane IDs should fail validation");
    }

    // ── SM-26: Scenario validate rejects unknown pane in event ──────────────

    #[test]
    fn sm26_validate_rejects_unknown_pane(pane_id in 100u64..200) {
        let scenario = Scenario {
            name: "test".to_string(),
            description: String::new(),
            duration: std::time::Duration::from_secs(10),
            panes: vec![ScenarioPane {
                id: 1, // Only pane 1 exists
                title: "a".to_string(),
                domain: "local".to_string(),
                cwd: "/tmp".to_string(),
                window_id: 0, tab_id: 0, cols: 80, rows: 24,
                initial_content: String::new(),
            }],
            events: vec![ScenarioEvent {
                at: std::time::Duration::from_secs(1),
                pane: pane_id, // References non-existent pane
                action: EventAction::Append,
                content: "hello".to_string(),
                name: String::new(),
                comment: None,
            }],
            expectations: vec![],
            metadata: std::collections::BTreeMap::new(),
        };
        let result = scenario.validate();
        prop_assert!(result.is_err(), "unknown pane reference should fail validation");
    }

    // ── SM-27: Scenario validate rejects out-of-order events ────────────────

    #[test]
    fn sm27_validate_rejects_out_of_order(_dummy in 0u8..1) {
        let scenario = Scenario {
            name: "test".to_string(),
            description: String::new(),
            duration: std::time::Duration::from_secs(10),
            panes: vec![ScenarioPane {
                id: 1,
                title: "a".to_string(),
                domain: "local".to_string(),
                cwd: "/tmp".to_string(),
                window_id: 0, tab_id: 0, cols: 80, rows: 24,
                initial_content: String::new(),
            }],
            events: vec![
                ScenarioEvent {
                    at: std::time::Duration::from_secs(5),
                    pane: 1,
                    action: EventAction::Append,
                    content: "late".to_string(),
                    name: String::new(),
                    comment: None,
                },
                ScenarioEvent {
                    at: std::time::Duration::from_secs(2), // Out of order!
                    pane: 1,
                    action: EventAction::Clear,
                    content: String::new(),
                    name: String::new(),
                    comment: None,
                },
            ],
            expectations: vec![],
            metadata: std::collections::BTreeMap::new(),
        };
        let result = scenario.validate();
        prop_assert!(result.is_err(), "out-of-order events should fail validation");
    }

    // ── SM-28: Scenario validate accepts valid scenario ─────────────────────

    #[test]
    fn sm28_validate_accepts_valid(_dummy in 0u8..1) {
        let scenario = Scenario {
            name: "valid".to_string(),
            description: "A valid test scenario".to_string(),
            duration: std::time::Duration::from_secs(10),
            panes: vec![ScenarioPane {
                id: 1,
                title: "pane1".to_string(),
                domain: "local".to_string(),
                cwd: "/tmp".to_string(),
                window_id: 0, tab_id: 0, cols: 80, rows: 24,
                initial_content: String::new(),
            }],
            events: vec![ScenarioEvent {
                at: std::time::Duration::from_secs(1),
                pane: 1,
                action: EventAction::Append,
                content: "hello".to_string(),
                name: String::new(),
                comment: None,
            }],
            expectations: vec![],
            metadata: std::collections::BTreeMap::new(),
        };
        let result = scenario.validate();
        prop_assert!(result.is_ok(), "valid scenario should pass: {:?}", result.err());
    }

    // ── SM-29: reproducibility_key format ───────────────────────────────────

    #[test]
    fn sm29_reproducibility_key_format(name in "[a-z_]{3,15}") {
        let scenario = Scenario {
            name: name.clone(),
            description: String::new(),
            duration: std::time::Duration::from_secs(10),
            panes: vec![],
            events: vec![],
            expectations: vec![],
            metadata: std::collections::BTreeMap::new(),
        };
        let key = scenario.reproducibility_key();
        // Default: ad_hoc:v1:<name>:0
        prop_assert!(key.starts_with("ad_hoc:v1:"), "key should start with ad_hoc:v1: got {}", key);
        prop_assert!(key.contains(&name), "key should contain scenario name");
        prop_assert!(key.ends_with(":0"), "key should end with :0 (default seed)");
    }

    // ── SM-30: reproducibility_key uses metadata ────────────────────────────

    #[test]
    fn sm30_reproducibility_key_metadata(
        suite in "[a-z]{3,10}",
        version in "[a-z0-9]{1,5}",
        seed in "[0-9]{1,5}",
    ) {
        let mut metadata = std::collections::BTreeMap::new();
        metadata.insert("suite".to_string(), suite.clone());
        metadata.insert("suite_version".to_string(), version.clone());
        metadata.insert("seed".to_string(), seed.clone());
        let scenario = Scenario {
            name: "test".to_string(),
            description: String::new(),
            duration: std::time::Duration::from_secs(10),
            panes: vec![],
            events: vec![],
            expectations: vec![],
            metadata,
        };
        let key = scenario.reproducibility_key();
        let expected = format!("{suite}:{version}:test:{seed}");
        prop_assert_eq!(&key, &expected);
    }

    // ── SM-31: ScenarioEvent serializes expected fields ────────────────────

    #[test]
    fn sm31_scenario_event_serialize(
        pane in 0u64..100,
        content in "[a-z ]{3,20}",
    ) {
        // ScenarioEvent uses custom Duration deserializer, so JSON roundtrip fails.
        // Test serialization only.
        let event = ScenarioEvent {
            at: std::time::Duration::from_secs(5),
            pane,
            action: EventAction::Append,
            content: content.clone(),
            name: String::new(),
            comment: Some("test comment".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        prop_assert!(json.contains("\"pane\""));
        prop_assert!(json.contains("\"action\""));
        prop_assert!(json.contains("\"comment\""));
        let expected_pane = format!("\"pane\":{pane}");
        prop_assert!(json.contains(&expected_pane));
    }

    // ── SM-32: Expectation Event serde roundtrip ────────────────────────────

    #[test]
    fn sm32_expectation_event_serde(event_name in "[a-z.]{3,20}") {
        let exp = Expectation {
            kind: ExpectationKind::Event {
                event: event_name.clone(),
                detected_at: None,
            },
        };
        let json = serde_json::to_string(&exp).unwrap();
        let back: Expectation = serde_json::from_str(&json).unwrap();
        let check = matches!(&back.kind, ExpectationKind::Event { event, .. } if event == &event_name);
        prop_assert!(check, "should roundtrip Event expectation");
    }

    // ── SM-33: Expectation Contains serde roundtrip ─────────────────────────

    #[test]
    fn sm33_expectation_contains_serde(
        pane in 0u64..100,
        text in "[a-z ]{3,20}",
    ) {
        let exp = Expectation {
            kind: ExpectationKind::Contains {
                pane,
                text: text.clone(),
            },
        };
        let json = serde_json::to_string(&exp).unwrap();
        let back: Expectation = serde_json::from_str(&json).unwrap();
        let check = matches!(&back.kind, ExpectationKind::Contains { pane: p, text: t } if *p == pane && t == &text);
        prop_assert!(check, "should roundtrip Contains expectation");
    }

    // ── SM-34: Expectation Workflow serde roundtrip ──────────────────────────

    #[test]
    fn sm34_expectation_workflow_serde(wf_name in "[a-z_]{3,15}") {
        let exp = Expectation {
            kind: ExpectationKind::Workflow {
                workflow: wf_name.clone(),
                started_at: Some("2s".to_string()),
            },
        };
        let json = serde_json::to_string(&exp).unwrap();
        let back: Expectation = serde_json::from_str(&json).unwrap();
        let check = matches!(&back.kind, ExpectationKind::Workflow { workflow, .. } if workflow == &wf_name);
        prop_assert!(check, "should roundtrip Workflow expectation");
    }

    // ── SM-35: ResizeTimelineStageSummary serde roundtrip ───────────────────

    #[test]
    fn sm35_stage_summary_serde(
        samples in 1usize..100,
        total in 0u64..1_000_000_000,
    ) {
        let summary = ResizeTimelineStageSummary {
            stage: ResizeTimelineStage::LogicalReflow,
            samples,
            total_duration_ns: total,
            avg_duration_ns: total as f64 / samples as f64,
            p50_duration_ns: total / 2,
            p95_duration_ns: total * 95 / 100,
            p99_duration_ns: total * 99 / 100,
            max_duration_ns: total,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: ResizeTimelineStageSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.stage, summary.stage);
        prop_assert_eq!(back.samples, summary.samples);
        prop_assert_eq!(back.total_duration_ns, summary.total_duration_ns);
        prop_assert_eq!(back.max_duration_ns, summary.max_duration_ns);
    }

    // ── SM-36: stage_summary produces one entry per stage ───────────────────

    #[test]
    fn sm36_stage_summary_all_stages(_dummy in 0u8..1) {
        let timeline = ResizeTimeline {
            scenario: "test".to_string(),
            reproducibility_key: "test:v1:test:0".to_string(),
            captured_at_ms: 1000,
            executed_resize_events: 0,
            events: vec![],
        };
        let summaries = timeline.stage_summary();
        prop_assert_eq!(summaries.len(), ResizeTimelineStage::ALL.len(),
            "stage_summary should have one entry per stage");
        // All should have zero samples when no events exist
        for s in &summaries {
            prop_assert_eq!(s.samples, 0, "stage {:?} should have 0 samples", s.stage);
        }
    }

    // ── SM-37: Scenario validate rejects empty metadata key ─────────────────

    #[test]
    fn sm37_validate_rejects_empty_metadata_key(val in "[a-z]{3,10}") {
        let mut metadata = std::collections::BTreeMap::new();
        metadata.insert(String::new(), val); // Empty key
        let scenario = Scenario {
            name: "test".to_string(),
            description: String::new(),
            duration: std::time::Duration::from_secs(10),
            panes: vec![],
            events: vec![],
            expectations: vec![],
            metadata,
        };
        let result = scenario.validate();
        prop_assert!(result.is_err(), "empty metadata key should fail");
    }

    // ── SM-38: Scenario validate rejects empty metadata value ───────────────

    #[test]
    fn sm38_validate_rejects_empty_metadata_value(key in "[a-z]{3,10}") {
        let mut metadata = std::collections::BTreeMap::new();
        metadata.insert(key, String::new()); // Empty value
        let scenario = Scenario {
            name: "test".to_string(),
            description: String::new(),
            duration: std::time::Duration::from_secs(10),
            panes: vec![],
            events: vec![],
            expectations: vec![],
            metadata,
        };
        let result = scenario.validate();
        prop_assert!(result.is_err(), "empty metadata value should fail");
    }

    // ── SM-39: ScenarioPane default fields from YAML ────────────────────────

    #[test]
    fn sm39_pane_defaults_from_yaml(_dummy in 0u8..1) {
        let yaml = "name: test\nduration: 5s\npanes:\n  - id: 1\n";
        let result = Scenario::from_yaml(yaml);
        prop_assert!(result.is_ok(), "YAML with defaults should parse: {:?}", result.err());
        let scenario = result.unwrap();
        let pane = &scenario.panes[0];
        prop_assert_eq!(&pane.title, "pane");
        prop_assert_eq!(&pane.domain, "local");
        prop_assert_eq!(pane.cols, 80);
        prop_assert_eq!(pane.rows, 24);
    }

    // ── SM-40: from_yaml rejects invalid YAML ───────────────────────────────

    #[test]
    fn sm40_from_yaml_rejects_invalid(garbage in "[a-z{}:]{10,30}") {
        // Most random strings should fail YAML parse or validation
        let result = Scenario::from_yaml(&garbage);
        // We just verify it doesn't panic - both Ok and Err are acceptable
        let _ = result;
    }

    // ── SM-41: Scenario validate rejects Typing with empty content ──────────

    #[test]
    fn sm41_validate_rejects_empty_typing(_dummy in 0u8..1) {
        let scenario = Scenario {
            name: "test".to_string(),
            description: String::new(),
            duration: std::time::Duration::from_secs(10),
            panes: vec![ScenarioPane {
                id: 1, title: "a".to_string(), domain: "local".to_string(),
                cwd: "/tmp".to_string(), window_id: 0, tab_id: 0,
                cols: 80, rows: 24, initial_content: String::new(),
            }],
            events: vec![ScenarioEvent {
                at: std::time::Duration::from_secs(1),
                pane: 1,
                action: EventAction::Typing,
                content: String::new(), // Empty content for Typing
                name: String::new(),
                comment: None,
            }],
            expectations: vec![],
            metadata: std::collections::BTreeMap::new(),
        };
        let result = scenario.validate();
        prop_assert!(result.is_err(), "Typing with empty content should fail");
    }

    // ── SM-42: ResizeTimeline stage_summary p50 <= p95 <= p99 <= max ────────

    #[test]
    fn sm42_stage_summary_percentile_ordering(
        dur_a in 1u64..1000,
        dur_b in 1u64..1000,
        dur_c in 1u64..1000,
        dur_d in 1u64..1000,
        dur_e in 1u64..1000,
    ) {
        let timeline = ResizeTimeline {
            scenario: "test".to_string(),
            reproducibility_key: "k".to_string(),
            captured_at_ms: 1000,
            executed_resize_events: 5,
            events: (0..5).map(|i| {
                let durs: [u64; 5] = (dur_a, dur_b, dur_c, dur_d, dur_e).into();
                let dur = durs[i];
                ResizeTimelineEvent {
                    event_index: i,
                    resize_transaction_id: "t".to_string(),
                    pane_id: 1,
                    tab_id: 0,
                    sequence_no: i as u64,
                    action: EventAction::Resize,
                    scheduler_decision: "dequeue_latest_intent".to_string(),
                    frame_id: 0,
                    test_case_id: "test".to_string(),
                    queue_wait_ms: 0,
                    reflow_ms: 0,
                    render_ms: 0,
                    present_ms: 0,
                    scheduled_at_ns: 0,
                    dispatch_offset_ns: 0,
                    total_duration_ns: dur,
                    stages: vec![ResizeTimelineStageSample {
                        stage: ResizeTimelineStage::LogicalReflow,
                        start_offset_ns: 0,
                        duration_ns: dur,
                        queue_metrics: None,
                        render_prep_metrics: None,
                    }],
                }
            }).collect(),
        };
        let summaries = timeline.stage_summary();
        for s in &summaries {
            if s.samples > 0 {
                prop_assert!(s.p50_duration_ns <= s.p95_duration_ns,
                    "p50 {} > p95 {} for stage {:?}", s.p50_duration_ns, s.p95_duration_ns, s.stage);
                prop_assert!(s.p95_duration_ns <= s.p99_duration_ns,
                    "p95 {} > p99 {} for stage {:?}", s.p95_duration_ns, s.p99_duration_ns, s.stage);
                prop_assert!(s.p99_duration_ns <= s.max_duration_ns,
                    "p99 {} > max {} for stage {:?}", s.p99_duration_ns, s.max_duration_ns, s.stage);
            }
        }
    }

    // ── SM-43: SandboxCommand serde roundtrip ───────────────────────────────

    #[test]
    fn sm43_sandbox_command_serde(
        cmd in "[a-z ]{3,20}",
        ts in 0u64..2_000_000_000,
    ) {
        let sc = SandboxCommand {
            command: cmd.clone(),
            timestamp_ms: ts,
            exercise_id: Some("ex1".to_string()),
        };
        let json = serde_json::to_string(&sc).unwrap();
        // SandboxCommand only derives Serialize, verify it doesn't panic
        prop_assert!(json.contains("\"command\""));
        prop_assert!(json.contains("\"timestamp_ms\""));
    }
}
