use frankenterm_core::simulation::{EventAction, ExpectationKind, ResizeTimelineStage, Scenario};
use frankenterm_core::wezterm::{MockWezterm, WeztermInterface};

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

#[tokio::test]
async fn resize_suite_executes_and_satisfies_contains_expectations() {
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
}

#[tokio::test]
async fn resize_suite_preserves_window_and_tab_assignments() {
    let scenario = Scenario::from_yaml(FIXTURES[1].yaml).unwrap();
    let mock = MockWezterm::new();
    scenario.setup(&mock).await.unwrap();

    let pane_2 = mock.pane_state(2).await.unwrap();
    assert_eq!(pane_2.window_id, 0);
    assert_eq!(pane_2.tab_id, 1);

    let pane_7 = mock.pane_state(7).await.unwrap();
    assert_eq!(pane_7.window_id, 0);
    assert_eq!(pane_7.tab_id, 3);
}

#[tokio::test]
async fn resize_suite_timeline_probes_cover_required_stages() {
    for fixture in FIXTURES {
        let scenario = Scenario::from_yaml(fixture.yaml)
            .unwrap_or_else(|err| panic!("failed to parse {}: {err}", fixture.name));
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (executed, timeline) = scenario
            .execute_all_with_resize_timeline(&mock)
            .await
            .unwrap_or_else(|err| panic!("timeline execution failed for {}: {err}", fixture.name));
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
}
