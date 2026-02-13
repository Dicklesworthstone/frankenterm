//! Property-based tests for the `simulation` module.
//!
//! Covers `EventAction` serde roundtrips, `as_str`, and `is_resize_timeline_action`;
//! `ResizeTimelineStage` serde roundtrips, `as_str`, ordering, and `ALL` constant;
//! `ResizeQueueMetrics`/`ResizeTimelineStageSample`/`ResizeTimelineEvent`/
//! `ResizeTimelineFlameSample` serde roundtrips; and `ResizeTimeline`
//! `flame_samples`/`stage_summary` structural invariants.

use frankenterm_core::simulation::{
    EventAction, ResizeQueueMetrics, ResizeTimeline, ResizeTimelineEvent,
    ResizeTimelineFlameSample, ResizeTimelineStage, ResizeTimelineStageSample,
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

fn arb_stage_sample() -> impl Strategy<Value = ResizeTimelineStageSample> {
    (
        arb_resize_timeline_stage(),
        0_u64..1_000_000,
        0_u64..1_000_000,
        proptest::option::of(arb_queue_metrics()),
    )
        .prop_map(|(stage, start_offset_ns, duration_ns, queue_metrics)| {
            ResizeTimelineStageSample {
                stage,
                start_offset_ns,
                duration_ns,
                queue_metrics,
            }
        })
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
                    pane_id,
                    action,
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

    /// is_resize_timeline_action returns true only for Resize, SetFontSize, GenerateScrollback.
    #[test]
    fn prop_is_resize_timeline_correct(action in arb_event_action()) {
        let expected = matches!(
            action,
            EventAction::Resize | EventAction::SetFontSize | EventAction::GenerateScrollback
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
            pane_id: 42,
            action: EventAction::Resize,
            scheduled_at_ns: 100,
            dispatch_offset_ns: 200,
            total_duration_ns: 500,
            stages: vec![
                ResizeTimelineStageSample {
                    stage: ResizeTimelineStage::InputIntent,
                    start_offset_ns: 0,
                    duration_ns: 100,
                    queue_metrics: None,
                },
                ResizeTimelineStageSample {
                    stage: ResizeTimelineStage::Presentation,
                    start_offset_ns: 100,
                    duration_ns: 400,
                    queue_metrics: None,
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
