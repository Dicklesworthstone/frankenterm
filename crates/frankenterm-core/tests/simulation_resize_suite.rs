use frankenterm_core::runtime_compat::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::simulation::{EventAction, ExpectationKind, ResizeTimelineStage, Scenario};
use frankenterm_core::wezterm::{MockWezterm, WeztermInterface};
use std::future::Future;

struct SuiteFixture {
    name: &'static str,
    yaml: &'static str,
    expected_panes: usize,
    min_events: usize,
}

const FIXTURES: &[SuiteFixture] = &[
    SuiteFixture {
        name: "resize_single_pane_scrollback",
        yaml: include_str!(
            "../../../fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml"
        ),
        expected_panes: 1,
        min_events: 9,
    },
    SuiteFixture {
        name: "resize_multi_tab_storm",
        yaml: include_str!(
            "../../../fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml"
        ),
        expected_panes: 8,
        min_events: 26,
    },
    SuiteFixture {
        name: "font_churn_multi_pane",
        yaml: include_str!(
            "../../../fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml"
        ),
        expected_panes: 6,
        min_events: 25,
    },
    SuiteFixture {
        name: "mixed_scale_soak",
        yaml: include_str!("../../../fixtures/simulations/resize_baseline/mixed_scale_soak.yaml"),
        expected_panes: 12,
        min_events: 30,
    },
    SuiteFixture {
        name: "mixed_workload_interactive_streaming",
        yaml: include_str!(
            "../../../fixtures/simulations/resize_baseline/mixed_workload_interactive_streaming.yaml"
        ),
        expected_panes: 4,
        min_events: 24,
    },
];

fn run_async_test<F>(future: F)
where
    F: Future<Output = ()>,
{
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("failed to build runtime_compat current-thread runtime");
    CompatRuntime::block_on(&runtime, future);
}

fn ns_to_ms_u64(duration_ns: u64) -> u64 {
    duration_ns / 1_000_000
}

#[test]
fn resize_suite_fixtures_parse_and_validate() {
    for fixture in FIXTURES {
        let scenario = Scenario::from_yaml(fixture.yaml)
            .unwrap_or_else(|err| panic!("failed to parse {}: {err}", fixture.name));

        assert_eq!(scenario.name, fixture.name);
        assert_eq!(scenario.panes.len(), fixture.expected_panes);
        assert!(
            scenario.events.len() >= fixture.min_events,
            "{} had too few events ({})",
            fixture.name,
            scenario.events.len()
        );
        assert_eq!(
            scenario.metadata.get("suite").map(String::as_str),
            Some("resize_baseline")
        );
        assert!(
            scenario
                .reproducibility_key()
                .starts_with("resize_baseline:"),
            "{} reproducibility key missing suite prefix: {}",
            fixture.name,
            scenario.reproducibility_key()
        );
    }
}

#[test]
fn mixed_workload_fixture_covers_interactive_streaming_and_scrollback() {
    let fixture = FIXTURES
        .iter()
        .find(|fixture| fixture.name == "mixed_workload_interactive_streaming")
        .expect("mixed workload fixture should be present");
    let scenario =
        Scenario::from_yaml(fixture.yaml).expect("mixed workload fixture should parse cleanly");

    let mut has_append = false;
    let mut has_resize = false;
    let mut has_font_churn = false;
    let mut has_scrollback = false;

    for event in &scenario.events {
        match event.action {
            EventAction::Append => has_append = true,
            EventAction::Resize => has_resize = true,
            EventAction::SetFontSize => has_font_churn = true,
            EventAction::GenerateScrollback => has_scrollback = true,
            _ => {}
        }
    }

    assert!(
        has_append,
        "fixture must include interactive append activity"
    );
    assert!(has_resize, "fixture must include resize churn");
    assert!(has_font_churn, "fixture must include font-size churn");
    assert!(
        has_scrollback,
        "fixture must include large scrollback generation"
    );
    assert_eq!(
        scenario
            .metadata
            .get("workload_profile")
            .map(String::as_str),
        Some("interactive_log_streaming_large_scrollback")
    );
}

#[test]
fn resize_suite_executes_and_satisfies_contains_expectations() {
    run_async_test(async {
        for fixture in FIXTURES {
            let scenario = Scenario::from_yaml(fixture.yaml)
                .unwrap_or_else(|err| panic!("failed to parse {}: {err}", fixture.name));
            let mock = MockWezterm::new();

            scenario
                .setup(&mock)
                .await
                .unwrap_or_else(|err| panic!("setup failed for {}: {err}", fixture.name));

            let executed = scenario
                .execute_all(&mock)
                .await
                .unwrap_or_else(|err| panic!("execution failed for {}: {err}", fixture.name));
            assert_eq!(executed, scenario.events.len());

            for exp in &scenario.expectations {
                if let ExpectationKind::Contains { pane, text } = &exp.kind {
                    let content = mock.get_text(*pane, false).await.unwrap_or_else(|err| {
                        panic!("get_text failed for {} pane {}: {err}", fixture.name, pane)
                    });
                    assert!(
                        content.contains(text),
                        "{} missing expectation text {:?} in pane {}",
                        fixture.name,
                        text,
                        pane
                    );
                }
            }
        }
    });
}

#[test]
fn resize_suite_preserves_window_and_tab_assignments() {
    run_async_test(async {
        let scenario = Scenario::from_yaml(FIXTURES[1].yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let pane_2 = mock.pane_state(2).await.unwrap();
        assert_eq!(pane_2.window_id, 0);
        assert_eq!(pane_2.tab_id, 1);

        let pane_7 = mock.pane_state(7).await.unwrap();
        assert_eq!(pane_7.window_id, 0);
        assert_eq!(pane_7.tab_id, 3);
    });
}

#[test]
fn resize_suite_timeline_probes_cover_required_stages() {
    run_async_test(async {
        for fixture in FIXTURES {
            let scenario = Scenario::from_yaml(fixture.yaml)
                .unwrap_or_else(|err| panic!("failed to parse {}: {err}", fixture.name));
            let mock = MockWezterm::new();
            scenario.setup(&mock).await.unwrap();

            let (executed, timeline) = scenario
                .execute_all_with_resize_timeline(&mock)
                .await
                .unwrap_or_else(|err| {
                    panic!("timeline execution failed for {}: {err}", fixture.name)
                });
            assert_eq!(executed, scenario.events.len());
            assert!(
                !timeline.events.is_empty(),
                "{} should contain resize timeline events",
                fixture.name
            );

            for event in &timeline.events {
                assert_eq!(
                    event.stages.len(),
                    ResizeTimelineStage::ALL.len(),
                    "{} stage count mismatch for event {}",
                    fixture.name,
                    event.event_index
                );
                for (sample, expected) in event.stages.iter().zip(ResizeTimelineStage::ALL.iter()) {
                    assert_eq!(
                        sample.stage, *expected,
                        "{} stage order mismatch for event {}",
                        fixture.name, event.event_index
                    );
                }
                let queue = event.stages[1]
                    .queue_metrics
                    .as_ref()
                    .expect("scheduler stage should emit queue metrics");
                assert!(
                    queue.depth_before >= queue.depth_after,
                    "{} queue depth must be non-increasing for event {}",
                    fixture.name,
                    event.event_index
                );
            }

            let summary = timeline.stage_summary();
            assert_eq!(summary.len(), ResizeTimelineStage::ALL.len());
            assert!(
                summary.iter().all(|stage| stage.samples > 0),
                "{} summary should include samples for each stage",
                fixture.name
            );
        }
    });
}

#[test]
fn resize_suite_timeline_events_emit_required_correlation_and_timing_fields() {
    run_async_test(async {
        for fixture in FIXTURES {
            let scenario = Scenario::from_yaml(fixture.yaml)
                .unwrap_or_else(|err| panic!("failed to parse {}: {err}", fixture.name));
            let mock = MockWezterm::new();
            scenario.setup(&mock).await.unwrap();

            let (executed, timeline) = scenario
                .execute_all_with_resize_timeline(&mock)
                .await
                .unwrap_or_else(|err| {
                    panic!("timeline execution failed for {}: {err}", fixture.name)
                });
            assert_eq!(executed, scenario.events.len());
            assert_eq!(timeline.scenario, scenario.name);
            assert_eq!(timeline.reproducibility_key, scenario.reproducibility_key());
            assert!(timeline.captured_at_ms > 0);

            for event in &timeline.events {
                assert_eq!(event.test_case_id, scenario.name);
                assert_eq!(event.sequence_no, event.event_index as u64);
                assert_eq!(event.frame_id, event.sequence_no);
                assert!(
                    event.scheduler_decision.contains("dequeue"),
                    "{} scheduler decision should be machine-readable for event {}",
                    fixture.name,
                    event.event_index
                );
                assert!(
                    event.resize_transaction_id.starts_with(&format!(
                        "{}:{}",
                        timeline.reproducibility_key, event.event_index
                    )),
                    "{} bad resize_transaction_id={} for event {}",
                    fixture.name,
                    event.resize_transaction_id,
                    event.event_index
                );
                let pane_state = mock.pane_state(event.pane_id).await.unwrap_or_else(|| {
                    panic!(
                        "pane_state missing for {} pane {}",
                        fixture.name, event.pane_id
                    )
                });
                assert_eq!(
                    event.tab_id, pane_state.tab_id,
                    "{} tab correlation mismatch for pane {}",
                    fixture.name, event.pane_id
                );

                assert_eq!(event.stages.len(), ResizeTimelineStage::ALL.len());
                for (sample, expected) in event.stages.iter().zip(ResizeTimelineStage::ALL.iter()) {
                    assert_eq!(
                        sample.stage, *expected,
                        "{} stage order mismatch for event {}",
                        fixture.name, event.event_index
                    );
                }

                assert_eq!(
                    event.queue_wait_ms,
                    ns_to_ms_u64(event.stages[1].duration_ns),
                    "{} queue_wait_ms mismatch for event {}",
                    fixture.name,
                    event.event_index
                );
                assert_eq!(
                    event.reflow_ms,
                    ns_to_ms_u64(event.stages[2].duration_ns),
                    "{} reflow_ms mismatch for event {}",
                    fixture.name,
                    event.event_index
                );
                assert_eq!(
                    event.render_ms,
                    ns_to_ms_u64(event.stages[3].duration_ns),
                    "{} render_ms mismatch for event {}",
                    fixture.name,
                    event.event_index
                );
                assert_eq!(
                    event.present_ms,
                    ns_to_ms_u64(event.stages[4].duration_ns),
                    "{} present_ms mismatch for event {}",
                    fixture.name,
                    event.event_index
                );
            }
        }
    });
}

#[test]
fn resize_suite_stage_summary_and_flame_samples_match_event_stage_data() {
    run_async_test(async {
        for fixture in FIXTURES {
            let scenario = Scenario::from_yaml(fixture.yaml)
                .unwrap_or_else(|err| panic!("failed to parse {}: {err}", fixture.name));
            let mock = MockWezterm::new();
            scenario.setup(&mock).await.unwrap();

            let (_executed, timeline) = scenario
                .execute_all_with_resize_timeline(&mock)
                .await
                .unwrap_or_else(|err| {
                    panic!("timeline execution failed for {}: {err}", fixture.name)
                });

            let summary = timeline.stage_summary();
            assert_eq!(summary.len(), ResizeTimelineStage::ALL.len());

            let expected_samples_per_stage = timeline.events.len();
            for entry in &summary {
                assert_eq!(
                    entry.samples, expected_samples_per_stage,
                    "{} stage {:?} sample count mismatch",
                    fixture.name, entry.stage
                );
                assert!(
                    entry.p50_duration_ns <= entry.p95_duration_ns
                        && entry.p95_duration_ns <= entry.p99_duration_ns
                        && entry.p99_duration_ns <= entry.max_duration_ns
                        && entry.total_duration_ns >= entry.max_duration_ns,
                    "{} invalid percentile ordering for stage {:?}",
                    fixture.name,
                    entry.stage
                );
            }

            for (stage_index, stage) in ResizeTimelineStage::ALL.iter().enumerate() {
                let expected_total = timeline.events.iter().fold(0u64, |acc, event| {
                    acc.saturating_add(event.stages[stage_index].duration_ns)
                });
                let actual_total = summary
                    .iter()
                    .find(|entry| entry.stage == *stage)
                    .map(|entry| entry.total_duration_ns)
                    .unwrap_or(0);
                assert_eq!(
                    actual_total, expected_total,
                    "{} stage {:?} total mismatch",
                    fixture.name, stage
                );
            }

            let flame = timeline.flame_samples();
            assert_eq!(
                flame.len(),
                timeline.events.len() * ResizeTimelineStage::ALL.len(),
                "{} flame sample cardinality mismatch",
                fixture.name
            );
            for row in &flame {
                assert!(
                    row.stack.starts_with(&format!("{};", timeline.scenario)),
                    "{} flame stack should start with scenario: {}",
                    fixture.name,
                    row.stack
                );
            }
        }
    });
}

#[test]
fn resize_suite_timeline_event_order_matches_resize_action_subset() {
    run_async_test(async {
        for fixture in FIXTURES {
            let scenario = Scenario::from_yaml(fixture.yaml)
                .unwrap_or_else(|err| panic!("failed to parse {}: {err}", fixture.name));
            let mock = MockWezterm::new();
            scenario.setup(&mock).await.unwrap();

            let (_executed, timeline) = scenario
                .execute_all_with_resize_timeline(&mock)
                .await
                .unwrap_or_else(|err| {
                    panic!("timeline execution failed for {}: {err}", fixture.name)
                });

            let expected_resize_events: Vec<(usize, u64, EventAction)> = scenario
                .events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    matches!(
                        event.action,
                        EventAction::Resize
                            | EventAction::SetFontSize
                            | EventAction::GenerateScrollback
                    )
                })
                .map(|(index, event)| (index, event.pane, event.action.clone()))
                .collect();
            assert_eq!(
                timeline.events.len(),
                expected_resize_events.len(),
                "{} resize event count mismatch",
                fixture.name
            );

            for (position, (actual, (expected_index, expected_pane, expected_action))) in timeline
                .events
                .iter()
                .zip(expected_resize_events.iter())
                .enumerate()
            {
                assert_eq!(
                    actual.event_index, *expected_index,
                    "{} event index mismatch at position {}",
                    fixture.name, position
                );
                assert_eq!(
                    actual.sequence_no,
                    u64::try_from(*expected_index).unwrap_or(u64::MAX),
                    "{} sequence number mismatch at position {}",
                    fixture.name,
                    position
                );
                assert_eq!(
                    actual.pane_id, *expected_pane,
                    "{} pane mismatch at position {}",
                    fixture.name, position
                );
                assert_eq!(
                    actual.action, *expected_action,
                    "{} action mismatch at position {}",
                    fixture.name, position
                );
                assert_eq!(
                    actual.scheduler_decision, "dequeue_latest_intent",
                    "{} scheduler decision mismatch at position {}",
                    fixture.name, position
                );

                let queue = actual.stages[1]
                    .queue_metrics
                    .as_ref()
                    .expect("scheduler stage should include queue metrics");
                let expected_depth_before =
                    u64::try_from(expected_resize_events.len().saturating_sub(position))
                        .unwrap_or(u64::MAX);
                assert_eq!(
                    queue.depth_before, expected_depth_before,
                    "{} queue depth_before mismatch at position {}",
                    fixture.name, position
                );
                assert_eq!(
                    queue.depth_after,
                    expected_depth_before.saturating_sub(1),
                    "{} queue depth_after mismatch at position {}",
                    fixture.name,
                    position
                );
            }
        }
    });
}

#[test]
fn resize_suite_stage_offsets_and_optional_metrics_follow_contract_matrix() {
    run_async_test(async {
        for fixture in FIXTURES {
            let scenario = Scenario::from_yaml(fixture.yaml)
                .unwrap_or_else(|err| panic!("failed to parse {}: {err}", fixture.name));
            let mock = MockWezterm::new();
            scenario.setup(&mock).await.unwrap();

            let (_executed, timeline) = scenario
                .execute_all_with_resize_timeline(&mock)
                .await
                .unwrap_or_else(|err| {
                    panic!("timeline execution failed for {}: {err}", fixture.name)
                });

            for event in &timeline.events {
                let mut running_offset = 0u64;
                for (stage_index, sample) in event.stages.iter().enumerate() {
                    assert_eq!(
                        sample.start_offset_ns, running_offset,
                        "{} stage offset mismatch for event {} stage {}",
                        fixture.name, event.event_index, stage_index
                    );
                    running_offset = running_offset.saturating_add(sample.duration_ns);

                    if stage_index == 1 {
                        assert!(
                            sample.queue_metrics.is_some(),
                            "{} scheduler stage must include queue metrics for event {}",
                            fixture.name,
                            event.event_index
                        );
                    } else {
                        assert!(
                            sample.queue_metrics.is_none(),
                            "{} non-scheduler stage must not include queue metrics for event {} stage {}",
                            fixture.name,
                            event.event_index,
                            stage_index
                        );
                    }

                    if stage_index == 3 && event.action == EventAction::SetFontSize {
                        let metrics = sample
                            .render_prep_metrics
                            .as_ref()
                            .expect("font-size render stage should include render prep metrics");
                        assert!(
                            metrics.staged_batches_deferred <= metrics.staged_batches_total,
                            "{} invalid batch metrics for event {}",
                            fixture.name,
                            event.event_index
                        );
                        if metrics.staged_batches_total == 0 {
                            assert_eq!(
                                metrics.glyphs_rebuilt_now, 0,
                                "{} zero-batch font event should not rebuild glyphs",
                                fixture.name
                            );
                            assert_eq!(
                                metrics.deferred_glyphs, 0,
                                "{} zero-batch font event should not defer glyphs",
                                fixture.name
                            );
                        }
                    } else {
                        assert!(
                            sample.render_prep_metrics.is_none(),
                            "{} render metrics should only appear on render_prep stage for set_font_size events (event {}, stage {})",
                            fixture.name,
                            event.event_index,
                            stage_index
                        );
                    }
                }
            }
        }
    });
}
