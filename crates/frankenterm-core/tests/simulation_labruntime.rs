//! LabRuntime port of all `#[tokio::test]` async tests from `simulation.rs`.
//!
//! Each test that previously used `#[tokio::test]` is wrapped in
//! `RuntimeFixture::current_thread()` + `rt.block_on(async { … })`.
//! Feature-gated behind `asupersync-runtime`.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::fixtures::RuntimeFixture;
use frankenterm_core::simulation::{
    EventAction, ExpectationKind, FontAtlasCachePolicy, ResizeTimelineStage, Scenario,
    TutorialSandbox,
};
use frankenterm_core::wezterm::{MockWezterm, WeztermInterface};

const BASIC_SCENARIO: &str = r#"
name: basic_test
description: "A simple test scenario"
duration: "10s"
panes:
  - id: 0
    title: "Main"
    initial_content: "$ "
events:
  - at: "1s"
    pane: 0
    action: append
    content: "hello world\n"
  - at: "3s"
    pane: 0
    action: append
    content: "done\n"
expectations:
  - contains:
      pane: 0
      text: "hello world"
"#;

/// Local equivalent of private `ns_to_ms_u64` in simulation.rs.
fn ns_to_ms_u64(duration_ns: u64) -> u64 {
    duration_ns / 1_000_000
}

// ===========================================================================
// 1. setup_creates_panes
// ===========================================================================

#[test]
fn setup_creates_panes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        assert_eq!(mock.pane_count().await, 1);
        let state = mock.pane_state(0).await.unwrap();
        assert_eq!(state.title, "Main");
        assert_eq!(state.content, "$ ");
    });
}

// ===========================================================================
// 2. execute_all_injects_events
// ===========================================================================

#[test]
fn execute_all_injects_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let count = scenario.execute_all(&mock).await.unwrap();
        assert_eq!(count, 2);

        let text = mock.get_text(0, false).await.unwrap();
        assert!(text.contains("hello world"));
        assert!(text.contains("done"));
    });
}

// ===========================================================================
// 3. execute_all_with_resize_timeline_records_stage_probes
// ===========================================================================

#[test]
fn execute_all_with_resize_timeline_records_stage_probes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: resize_probe_case
duration: "10s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: append
    content: "bootstrap\n"
  - at: "2s"
    pane: 0
    action: resize
    content: "120x40"
  - at: "3s"
    pane: 0
    action: set_font_size
    content: "1.15"
  - at: "4s"
    pane: 0
    action: generate_scrollback
    content: "4x48"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (executed, timeline) = scenario
            .execute_all_with_resize_timeline(&mock)
            .await
            .unwrap();
        assert_eq!(executed, scenario.events.len());
        assert_eq!(timeline.executed_resize_events, 3);
        assert_eq!(timeline.events.len(), 3);

        for event in &timeline.events {
            assert_eq!(event.sequence_no, event.event_index as u64);
            assert_eq!(event.frame_id, event.sequence_no);
            assert_eq!(event.scheduler_decision, "dequeue_latest_intent");
            assert_eq!(event.test_case_id, scenario.name);
            assert!(event.resize_transaction_id.starts_with(&format!(
                "{}:{}",
                timeline.reproducibility_key, event.event_index
            )));
            assert_eq!(event.stages.len(), ResizeTimelineStage::ALL.len());
            for (sample, expected) in event.stages.iter().zip(ResizeTimelineStage::ALL.iter()) {
                assert_eq!(sample.stage, *expected);
            }
            assert_eq!(
                event.queue_wait_ms,
                ns_to_ms_u64(event.stages[1].duration_ns)
            );
            assert_eq!(event.reflow_ms, ns_to_ms_u64(event.stages[2].duration_ns));
            assert_eq!(event.render_ms, ns_to_ms_u64(event.stages[3].duration_ns));
            assert_eq!(event.present_ms, ns_to_ms_u64(event.stages[4].duration_ns));
            let render_prep_metrics = event.stages[3].render_prep_metrics.as_ref();
            if event.action == EventAction::SetFontSize {
                let metrics = render_prep_metrics
                    .expect("set_font_size events should emit render-prep metrics");
                assert!(metrics.staged_batches_total >= 1);
                assert!(metrics.glyphs_rebuilt_now > 0 || metrics.cache_hit_glyphs > 0);
            } else {
                assert!(
                    render_prep_metrics.is_none(),
                    "non-font resize events should not emit font render-prep metrics"
                );
            }
            let queue = event.stages[1].queue_metrics.as_ref().unwrap();
            assert!(
                queue.depth_before >= queue.depth_after,
                "queue depth should be non-increasing for dequeued event"
            );
        }
    });
}

// ===========================================================================
// 4. resize_timeline_summary_and_flame_samples_cover_all_stages
// ===========================================================================

#[test]
fn resize_timeline_summary_and_flame_samples_cover_all_stages() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: resize_probe_summary
duration: "6s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: resize
    content: "100x30"
  - at: "2s"
    pane: 0
    action: set_font_size
    content: "1.20"
  - at: "3s"
    pane: 0
    action: generate_scrollback
    content: "5x60"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (_executed, timeline) = scenario
            .execute_all_with_resize_timeline(&mock)
            .await
            .unwrap();
        let summary = timeline.stage_summary();
        assert_eq!(summary.len(), ResizeTimelineStage::ALL.len());
        assert!(summary.iter().all(|entry| entry.samples == 3));
        assert!(summary.iter().all(|entry| {
            entry.p50_duration_ns <= entry.p95_duration_ns
                && entry.p95_duration_ns <= entry.p99_duration_ns
                && entry.p99_duration_ns <= entry.max_duration_ns
                && entry.total_duration_ns >= entry.max_duration_ns
        }));

        let flame = timeline.flame_samples();
        assert_eq!(
            flame.len(),
            timeline.events.len() * ResizeTimelineStage::ALL.len()
        );
        let mut stage_suffixes = BTreeSet::new();
        for row in &flame {
            let suffix = row.stack.rsplit(';').next().unwrap_or_default().to_string();
            stage_suffixes.insert(suffix);
        }
        for stage in ResizeTimelineStage::ALL {
            assert!(stage_suffixes.contains(stage.as_str()));
        }
    });
}

// ===========================================================================
// 5. set_font_size_render_prep_uses_staged_atlas_and_shader_warmup_policy
// ===========================================================================

#[test]
fn set_font_size_render_prep_uses_staged_atlas_and_shader_warmup_policy() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: font_pipeline_policy
duration: "8s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: set_font_size
    content: "1.00"
  - at: "2s"
    pane: 0
    action: set_font_size
    content: "1.02"
  - at: "3s"
    pane: 0
    action: set_font_size
    content: "1.60"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (_executed, timeline) = scenario
            .execute_all_with_resize_timeline(&mock)
            .await
            .unwrap();
        assert_eq!(timeline.events.len(), 3);

        let first = timeline.events[0].stages[3]
            .render_prep_metrics
            .as_ref()
            .unwrap();
        assert_eq!(first.atlas_cache_policy, FontAtlasCachePolicy::FullRebuild);
        assert!(first.shader_warmup);
        assert!(first.staged_batches_total >= 1);

        let second = timeline.events[1].stages[3]
            .render_prep_metrics
            .as_ref()
            .unwrap();
        assert_eq!(
            second.atlas_cache_policy,
            FontAtlasCachePolicy::ReuseHotAtlas
        );
        assert!(!second.shader_warmup);
        assert!(second.cache_hit_glyphs > 0);

        let third = timeline.events[2].stages[3]
            .render_prep_metrics
            .as_ref()
            .unwrap();
        assert_eq!(third.atlas_cache_policy, FontAtlasCachePolicy::FullRebuild);
        assert!(third.shader_warmup);
        assert!(third.deferred_glyphs > 0);
    });
}

// ===========================================================================
// 6. execute_until_partial
// ===========================================================================

#[test]
fn execute_until_partial() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let count = scenario
            .execute_until(&mock, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(count, 1);

        let text = mock.get_text(0, false).await.unwrap();
        assert!(text.contains("hello world"));
        assert!(!text.contains("done"));
    });
}

// ===========================================================================
// 7. scenario_with_clear
// ===========================================================================

#[test]
fn scenario_with_clear() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: clear_test
duration: "5s"
panes:
  - id: 0
    initial_content: "old content"
events:
  - at: "1s"
    pane: 0
    action: clear
  - at: "2s"
    pane: 0
    action: append
    content: "new content"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();
        scenario.execute_all(&mock).await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert!(!text.contains("old content"));
        assert!(text.contains("new content"));
    });
}

// ===========================================================================
// 8. scenario_with_resize_and_title
// ===========================================================================

#[test]
fn scenario_with_resize_and_title() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: resize_title
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: resize
    content: "120x40"
  - at: "2s"
    pane: 0
    action: set_title
    content: "Updated Title"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();
        scenario.execute_all(&mock).await.unwrap();

        let state = mock.pane_state(0).await.unwrap();
        assert_eq!(state.cols, 120);
        assert_eq!(state.rows, 40);
        assert_eq!(state.title, "Updated Title");
    });
}

// ===========================================================================
// 9. multi_pane_execution
// ===========================================================================

#[test]
fn multi_pane_execution() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: multi_exec
duration: "5s"
panes:
  - id: 0
    title: "Agent A"
  - id: 1
    title: "Agent B"
events:
  - at: "1s"
    pane: 0
    action: append
    content: "output-a"
  - at: "2s"
    pane: 1
    action: append
    content: "output-b"
  - at: "3s"
    pane: 0
    action: append
    content: " more-a"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();
        let count = scenario.execute_all(&mock).await.unwrap();
        assert_eq!(count, 3);

        let t0 = mock.get_text(0, false).await.unwrap();
        let t1 = mock.get_text(1, false).await.unwrap();
        assert!(t0.contains("output-a"));
        assert!(t0.contains("more-a"));
        assert!(t1.contains("output-b"));
        assert!(!t1.contains("output-a"));
    });
}

// ===========================================================================
// 10. marker_event_injects_marker_text
// ===========================================================================

#[test]
fn marker_event_injects_marker_text() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: marker_test
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: marker
    name: checkpoint_1
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();
        scenario.execute_all(&mock).await.unwrap();

        let text = mock.get_text(0, false).await.unwrap();
        assert!(text.contains("[MARKER:checkpoint_1]"));
    });
}

// ===========================================================================
// 11. contains_expectation_passes
// ===========================================================================

#[test]
fn contains_expectation_passes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();
        scenario.execute_all(&mock).await.unwrap();

        assert_eq!(scenario.expectations.len(), 1);
        match &scenario.expectations[0].kind {
            ExpectationKind::Contains { pane, text } => {
                let content = mock.get_text(*pane, false).await.unwrap();
                assert!(content.contains(text));
            }
            _ => panic!("Expected Contains expectation"),
        }
    });
}

// ===========================================================================
// 12. execute_until_zero_runs_nothing
// ===========================================================================

#[test]
fn execute_until_zero_runs_nothing() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let count = scenario
            .execute_until(&mock, Duration::from_millis(0))
            .await
            .unwrap();
        assert_eq!(count, 0);

        let text = mock.get_text(0, false).await.unwrap();
        assert_eq!(text, "$ ");
    });
}

// ===========================================================================
// 13. scenario_load_from_temp_file
// ===========================================================================

#[test]
fn scenario_load_from_temp_file() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "{}", BASIC_SCENARIO).unwrap();
        drop(f);

        let scenario = Scenario::load(&path).unwrap();
        assert_eq!(scenario.name, "basic_test");
        assert_eq!(scenario.events.len(), 2);
    });
}

// ===========================================================================
// 14. sandbox_creates_default_panes
// ===========================================================================

#[test]
fn sandbox_creates_default_panes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;
        assert_eq!(sandbox.mock().pane_count().await, 3);

        let p0 = sandbox.mock().pane_state(0).await.unwrap();
        assert_eq!(p0.title, "Local Shell");
        let p1 = sandbox.mock().pane_state(1).await.unwrap();
        assert_eq!(p1.title, "Codex Agent");
        let p2 = sandbox.mock().pane_state(2).await.unwrap();
        assert_eq!(p2.title, "Claude Code");
    });
}

// ===========================================================================
// 15. sandbox_initial_content
// ===========================================================================

#[test]
fn sandbox_initial_content() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;

        let t0 = sandbox.mock().get_text(0, false).await.unwrap();
        assert_eq!(t0, "$ ");
        let t1 = sandbox.mock().get_text(1, false).await.unwrap();
        assert!(t1.contains("codex>"));
    });
}

// ===========================================================================
// 16. sandbox_format_output_with_indicator
// ===========================================================================

#[test]
fn sandbox_format_output_with_indicator() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;
        assert_eq!(sandbox.format_output("hello"), "[SANDBOX] hello");
    });
}

// ===========================================================================
// 17. sandbox_format_output_without_indicator
// ===========================================================================

#[test]
fn sandbox_format_output_without_indicator() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mut sandbox = TutorialSandbox::new().await;
        sandbox.set_show_indicator(false);
        assert_eq!(sandbox.format_output("hello"), "hello");
    });
}

// ===========================================================================
// 18. sandbox_command_logging
// ===========================================================================

#[test]
fn sandbox_command_logging() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mut sandbox = TutorialSandbox::new().await;
        assert!(sandbox.command_log().is_empty());

        sandbox.log_command("ft status", Some("basics.1"));
        sandbox.log_command("ft list", None);

        assert_eq!(sandbox.command_log().len(), 2);
        assert_eq!(sandbox.command_log()[0].command, "ft status");
        assert_eq!(
            sandbox.command_log()[0].exercise_id.as_deref(),
            Some("basics.1")
        );
        assert_eq!(sandbox.command_log()[1].command, "ft list");
        assert!(sandbox.command_log()[1].exercise_id.is_none());
    });
}

// ===========================================================================
// 19. sandbox_trigger_events
// ===========================================================================

#[test]
fn sandbox_trigger_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;
        let count = sandbox.trigger_exercise_events().await.unwrap();
        assert_eq!(count, 2);

        let t1 = sandbox.mock().get_text(1, false).await.unwrap();
        assert!(t1.contains("Usage Warning"));
        let t2 = sandbox.mock().get_text(2, false).await.unwrap();
        assert!(t2.contains("Context Compaction"));
    });
}

// ===========================================================================
// 20. sandbox_check_expectations_after_events
// ===========================================================================

#[test]
fn sandbox_check_expectations_after_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;
        sandbox.trigger_exercise_events().await.unwrap();

        let (pass, fail, skip) = sandbox.check_all_expectations().await;
        assert_eq!(pass, 2);
        assert_eq!(fail, 0);
        assert_eq!(skip, 0);
    });
}

// ===========================================================================
// 21. sandbox_check_expectations_before_events
// ===========================================================================

#[test]
fn sandbox_check_expectations_before_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;
        let (pass, fail, skip) = sandbox.check_all_expectations().await;
        assert_eq!(pass, 0);
        assert_eq!(fail, 2);
        assert_eq!(skip, 0);
    });
}

// ===========================================================================
// 22. sandbox_with_custom_scenario
// ===========================================================================

#[test]
fn sandbox_with_custom_scenario() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: custom_sandbox
duration: "5s"
panes:
  - id: 0
    title: "Custom"
    initial_content: "custom> "
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let sandbox = TutorialSandbox::with_scenario(scenario).await.unwrap();

        assert_eq!(sandbox.mock().pane_count().await, 1);
        let text = sandbox.mock().get_text(0, false).await.unwrap();
        assert_eq!(text, "custom> ");
    });
}

// ===========================================================================
// 23. sandbox_empty_has_no_panes
// ===========================================================================

#[test]
fn sandbox_empty_has_no_panes() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::empty();
        assert_eq!(sandbox.mock().pane_count().await, 0);
    });
}

// ===========================================================================
// 24. sandbox_empty_trigger_events_returns_zero
// ===========================================================================

#[test]
fn sandbox_empty_trigger_events_returns_zero() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::empty();
        let count = sandbox.trigger_exercise_events().await.unwrap();
        assert_eq!(count, 0);
    });
}

// ===========================================================================
// 25. sandbox_empty_check_expectations
// ===========================================================================

#[test]
fn sandbox_empty_check_expectations() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::empty();
        let (pass, fail, skip) = sandbox.check_all_expectations().await;
        assert_eq!(pass, 0);
        assert_eq!(fail, 0);
        assert_eq!(skip, 0);
    });
}

// ===========================================================================
// 26. execute_until_exact_boundary
// ===========================================================================

#[test]
fn execute_until_exact_boundary() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let count = scenario
            .execute_until(&mock, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(count, 1);
    });
}

// ===========================================================================
// 27. execute_until_just_before_first_event
// ===========================================================================

#[test]
fn execute_until_just_before_first_event() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let count = scenario
            .execute_until(&mock, Duration::from_millis(999))
            .await
            .unwrap();
        assert_eq!(count, 0);
    });
}

// ===========================================================================
// 28. execute_until_far_future
// ===========================================================================

#[test]
fn execute_until_far_future() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let scenario = Scenario::from_yaml(BASIC_SCENARIO).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let count = scenario
            .execute_until(&mock, Duration::from_secs(9999))
            .await
            .unwrap();
        assert_eq!(count, 2);
    });
}

// ===========================================================================
// 29. setup_pane_0_is_active
// ===========================================================================

#[test]
fn setup_pane_0_is_active() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: active_test
duration: "1s"
panes:
  - id: 0
  - id: 5
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let p0 = mock.pane_state(0).await.unwrap();
        assert!(p0.is_active);
        let p5 = mock.pane_state(5).await.unwrap();
        assert!(!p5.is_active);
    });
}

// ===========================================================================
// 30. setup_panes_not_zoomed
// ===========================================================================

#[test]
fn setup_panes_not_zoomed() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: zoom_test
duration: "1s"
panes:
  - id: 0
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let p0 = mock.pane_state(0).await.unwrap();
        assert!(!p0.is_zoomed);
    });
}

// ===========================================================================
// 31. execute_until_with_resize_timeline_partial
// ===========================================================================

#[test]
fn execute_until_with_resize_timeline_partial() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: partial_resize
duration: "10s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: resize
    content: "100x30"
  - at: "5s"
    pane: 0
    action: resize
    content: "120x40"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (count, timeline) = scenario
            .execute_until_with_resize_timeline(&mock, Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(timeline.executed_resize_events, 1);
        assert_eq!(timeline.events.len(), 1);
        assert_eq!(timeline.events[0].action, EventAction::Resize);
    });
}

// ===========================================================================
// 32. execute_until_with_resize_timeline_no_resize_events
// ===========================================================================

#[test]
fn execute_until_with_resize_timeline_no_resize_events() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: no_resize
duration: "5s"
panes:
  - id: 0
events:
  - at: "1s"
    pane: 0
    action: append
    content: "text"
  - at: "2s"
    pane: 0
    action: clear
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (count, timeline) = scenario
            .execute_all_with_resize_timeline(&mock)
            .await
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(timeline.executed_resize_events, 0);
        assert!(timeline.events.is_empty());
    });
}

// ===========================================================================
// 33. resize_timeline_captured_at_is_recent
// ===========================================================================

#[test]
fn resize_timeline_captured_at_is_recent() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: ts_check
duration: "1s"
panes:
  - id: 0
events: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let mock = MockWezterm::new();
        scenario.setup(&mock).await.unwrap();

        let (_count, timeline) = scenario
            .execute_all_with_resize_timeline(&mock)
            .await
            .unwrap();
        assert!(timeline.captured_at_ms > 1_577_836_800_000);
    });
}

// ===========================================================================
// 34. sandbox_check_event_expectation_returns_false
// ===========================================================================

#[test]
fn sandbox_check_event_expectation_returns_false() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;
        let result = sandbox
            .check_expectation(&ExpectationKind::Event {
                event: "test".to_string(),
                detected_at: None,
            })
            .await;
        assert!(!result);
    });
}

// ===========================================================================
// 35. sandbox_check_workflow_expectation_returns_false
// ===========================================================================

#[test]
fn sandbox_check_workflow_expectation_returns_false() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;
        let result = sandbox
            .check_expectation(&ExpectationKind::Workflow {
                workflow: "test".to_string(),
                started_at: None,
            })
            .await;
        assert!(!result);
    });
}

// ===========================================================================
// 36. sandbox_check_contains_nonexistent_pane
// ===========================================================================

#[test]
fn sandbox_check_contains_nonexistent_pane() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;
        let result = sandbox
            .check_expectation(&ExpectationKind::Contains {
                pane: 999,
                text: "anything".to_string(),
            })
            .await;
        assert!(!result);
    });
}

// ===========================================================================
// 37. sandbox_check_contains_missing_text
// ===========================================================================

#[test]
fn sandbox_check_contains_missing_text() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;
        let result = sandbox
            .check_expectation(&ExpectationKind::Contains {
                pane: 0,
                text: "this text does not exist".to_string(),
            })
            .await;
        assert!(!result);
    });
}

// ===========================================================================
// 38. sandbox_check_contains_present_text
// ===========================================================================

#[test]
fn sandbox_check_contains_present_text() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;
        let result = sandbox
            .check_expectation(&ExpectationKind::Contains {
                pane: 0,
                text: "$ ".to_string(),
            })
            .await;
        assert!(result);
    });
}

// ===========================================================================
// 39. sandbox_indicator_toggle
// ===========================================================================

#[test]
fn sandbox_indicator_toggle() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mut sandbox = TutorialSandbox::new().await;
        assert_eq!(sandbox.format_output("x"), "[SANDBOX] x");
        sandbox.set_show_indicator(false);
        assert_eq!(sandbox.format_output("x"), "x");
        sandbox.set_show_indicator(true);
        assert_eq!(sandbox.format_output("x"), "[SANDBOX] x");
    });
}

// ===========================================================================
// 40. sandbox_command_log_timestamps_are_monotonic
// ===========================================================================

#[test]
fn sandbox_command_log_timestamps_are_monotonic() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mut sandbox = TutorialSandbox::new().await;
        sandbox.log_command("cmd1", None);
        sandbox.log_command("cmd2", None);
        sandbox.log_command("cmd3", None);

        let log = sandbox.command_log();
        assert_eq!(log.len(), 3);
        assert!(log[0].timestamp_ms <= log[1].timestamp_ms);
        assert!(log[1].timestamp_ms <= log[2].timestamp_ms);
    });
}

// ===========================================================================
// 41. sandbox_format_output_empty_text
// ===========================================================================

#[test]
fn sandbox_format_output_empty_text() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sandbox = TutorialSandbox::new().await;
        assert_eq!(sandbox.format_output(""), "[SANDBOX] ");
    });
}

// ===========================================================================
// 42. sandbox_with_expectations_mixed_types
// ===========================================================================

#[test]
fn sandbox_with_expectations_mixed_types() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
name: mixed_exp
duration: "5s"
panes:
  - id: 0
    initial_content: "present text"
events: []
expectations:
  - contains:
      pane: 0
      text: "present text"
  - event:
      event: some_event
  - workflow:
      workflow: some_workflow
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let sandbox = TutorialSandbox::with_scenario(scenario).await.unwrap();

        let (pass, fail, skip) = sandbox.check_all_expectations().await;
        assert_eq!(pass, 1);
        assert_eq!(fail, 0);
        assert_eq!(skip, 2);
    });
}
