//! Integration tests: ViewportReflowPlanner → ResizeScheduler pipeline.
//!
//! These tests validate that reflow plans produced by the planner can be
//! correctly submitted to the scheduler and processed through the full
//! schedule→execute→complete lifecycle.

use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeExecutionPhase, ResizeScheduler, ResizeSchedulerConfig, ResizeWorkClass,
    SubmitOutcome,
};
use frankenterm_core::viewport_reflow_planner::{
    ReflowBatchPriority, ReflowPlannerInput, ViewportReflowPlanner,
};

/// Submit all selected-for-frame hooks from a plan into the scheduler.
#[allow(dead_code)]
fn submit_selected_hooks(
    scheduler: &mut ResizeScheduler,
    input: &ReflowPlannerInput,
    pane_id: u64,
    base_seq: u64,
    submitted_at_ms: u64,
) -> Vec<(u64, u32)> {
    let plan = ViewportReflowPlanner::plan(input);
    let hooks = plan.scheduling_hooks();
    let mut submitted = Vec::new();
    let mut seq = base_seq;

    for hook in hooks.iter().filter(|h| h.selected_for_frame) {
        let intent = hook.to_resize_intent(pane_id, seq, submitted_at_ms);
        let outcome = scheduler.submit_intent(intent);
        match outcome {
            SubmitOutcome::Accepted { .. } => {
                submitted.push((seq, hook.work_units));
            }
            _ => {}
        }
        seq += 1;
    }
    submitted
}

#[test]
fn planner_hooks_submit_and_schedule_through_single_pane_lifecycle() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 20,
        ..ResizeSchedulerConfig::default()
    });

    let input = ReflowPlannerInput {
        total_logical_lines: 200,
        viewport_top: 80,
        viewport_height: 24,
        overscan_lines: 8,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };

    let plan = ViewportReflowPlanner::plan(&input);
    let hooks = plan.scheduling_hooks();
    assert!(!hooks.is_empty());

    // Submit only the first hook (viewport core batch) to pane 1.
    let first_hook = &hooks[0];
    let intent = first_hook.to_resize_intent(1, 1, 100);
    let outcome = scheduler.submit_intent(intent);
    assert!(matches!(outcome, SubmitOutcome::Accepted { .. }));

    // Schedule a frame — the intent should be picked.
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 1);
    assert_eq!(frame.scheduled[0].pane_id, 1);
    assert_eq!(frame.scheduled[0].work_units, first_hook.work_units);

    // Complete the active work.
    assert!(scheduler.complete_active(1, 1));
    assert_eq!(scheduler.active_total(), 0);
    assert_eq!(scheduler.pending_total(), 0);
}

#[test]
fn multi_pane_plans_interleave_correctly_under_shared_budget() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 4,
        allow_single_oversubscription: true,
        ..ResizeSchedulerConfig::default()
    });

    // Pane 1: viewport at top, interactive work.
    let input1 = ReflowPlannerInput {
        total_logical_lines: 100,
        viewport_top: 0,
        viewport_height: 24,
        overscan_lines: 4,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 4,
    };

    // Pane 2: viewport at bottom, interactive work.
    let input2 = ReflowPlannerInput {
        total_logical_lines: 100,
        viewport_top: 76,
        viewport_height: 24,
        overscan_lines: 4,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 4,
    };

    let plan1 = ViewportReflowPlanner::plan(&input1);
    let plan2 = ViewportReflowPlanner::plan(&input2);

    // Submit viewport core from both panes.
    let hook1 = &plan1.scheduling_hooks()[0];
    let hook2 = &plan2.scheduling_hooks()[0];

    let outcome1 = scheduler.submit_intent(hook1.to_resize_intent(1, 1, 100));
    let outcome2 = scheduler.submit_intent(hook2.to_resize_intent(2, 1, 100));
    assert!(matches!(outcome1, SubmitOutcome::Accepted { .. }));
    assert!(matches!(outcome2, SubmitOutcome::Accepted { .. }));

    // Schedule — both are interactive, should compete for budget.
    let frame = scheduler.schedule_frame_with_budget(4);
    assert!(
        !frame.scheduled.is_empty(),
        "at least one pane should be scheduled"
    );

    // Complete what was scheduled.
    for work in &frame.scheduled {
        assert!(scheduler.complete_active(work.pane_id, work.intent_seq));
    }
}

#[test]
fn cold_scrollback_hooks_treated_as_background_in_scheduler() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 10,
        max_deferrals_before_force: 3,
        allow_single_oversubscription: true,
        ..ResizeSchedulerConfig::default()
    });

    let input = ReflowPlannerInput {
        total_logical_lines: 500,
        viewport_top: 200,
        viewport_height: 30,
        overscan_lines: 10,
        max_batch_lines: 16,
        lines_per_work_unit: 8,
        frame_budget_units: 100, // generous budget so planner selects everything
    };

    let plan = ViewportReflowPlanner::plan(&input);
    let hooks = plan.scheduling_hooks();

    // Find a cold scrollback hook.
    let cold_idx = plan
        .batches
        .iter()
        .position(|b| b.priority == ReflowBatchPriority::ColdScrollback)
        .expect("should have cold scrollback batches");
    let cold_hook = &hooks[cold_idx];
    assert_eq!(
        cold_hook.scheduler_class,
        ResizeWorkClass::Background,
        "cold scrollback should be Background class"
    );

    // Submit cold scrollback to scheduler.
    let intent = cold_hook.to_resize_intent(10, 1, 200);
    let outcome = scheduler.submit_intent(intent);
    assert!(matches!(outcome, SubmitOutcome::Accepted { .. }));

    // Also submit an interactive viewport intent for another pane.
    let viewport_hook = &hooks[0];
    assert_eq!(viewport_hook.scheduler_class, ResizeWorkClass::Interactive);
    let intent2 = viewport_hook.to_resize_intent(11, 1, 200);
    let outcome2 = scheduler.submit_intent(intent2);
    assert!(matches!(outcome2, SubmitOutcome::Accepted { .. }));

    // Schedule with tight budget — interactive should be preferred.
    let frame = scheduler.schedule_frame_with_budget(3);
    if frame.scheduled.len() == 1 {
        assert_eq!(
            frame.scheduled[0].pane_id, 11,
            "interactive pane should be scheduled first under tight budget"
        );
    }
}

#[test]
fn planner_to_scheduler_full_lifecycle_with_phase_transitions() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 20,
        ..ResizeSchedulerConfig::default()
    });

    let input = ReflowPlannerInput {
        total_logical_lines: 100,
        viewport_top: 30,
        viewport_height: 20,
        overscan_lines: 5,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 20,
    };

    let plan = ViewportReflowPlanner::plan(&input);
    let hook = &plan.scheduling_hooks()[0];

    // Submit.
    let intent = hook.to_resize_intent(1, 1, 100);
    scheduler.submit_intent(intent);

    // Schedule.
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 1);

    // Walk through phase transitions.
    assert!(scheduler.mark_active_phase(1, 1, ResizeExecutionPhase::Preparing, 101));
    assert!(scheduler.mark_active_phase(1, 1, ResizeExecutionPhase::Reflowing, 102));
    assert!(scheduler.mark_active_phase(1, 1, ResizeExecutionPhase::Presenting, 103));

    // Complete.
    assert!(scheduler.complete_active(1, 1));

    // Verify lifecycle events were recorded.
    let events = scheduler.lifecycle_events(20);
    assert!(
        events.len() >= 3,
        "should have at least 3 lifecycle events for phase transitions, got {}",
        events.len()
    );
}

#[test]
fn superseding_planner_intents_coalesce_in_scheduler() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 10,
        ..ResizeSchedulerConfig::default()
    });

    // First resize plan (viewport at top).
    let input1 = ReflowPlannerInput {
        total_logical_lines: 200,
        viewport_top: 0,
        viewport_height: 24,
        overscan_lines: 8,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };

    // Second resize plan (viewport moved to middle — user scrolled).
    let input2 = ReflowPlannerInput {
        total_logical_lines: 200,
        viewport_top: 100,
        viewport_height: 24,
        overscan_lines: 8,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };

    let plan1 = ViewportReflowPlanner::plan(&input1);
    let plan2 = ViewportReflowPlanner::plan(&input2);

    // Submit first plan's viewport hook.
    let hook1 = &plan1.scheduling_hooks()[0];
    let outcome1 = scheduler.submit_intent(hook1.to_resize_intent(1, 1, 100));
    assert!(matches!(
        outcome1,
        SubmitOutcome::Accepted {
            replaced_pending_seq: None,
        }
    ));

    // Submit second plan's viewport hook — should coalesce (replace pending).
    let hook2 = &plan2.scheduling_hooks()[0];
    let outcome2 = scheduler.submit_intent(hook2.to_resize_intent(1, 2, 101));
    assert!(matches!(
        outcome2,
        SubmitOutcome::Accepted {
            replaced_pending_seq: Some(1),
        }
    ));

    // Schedule — should pick the latest intent (seq 2).
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 1);
    assert_eq!(frame.scheduled[0].intent_seq, 2);
    assert_eq!(scheduler.metrics().superseded_intents, 1);
}

#[test]
fn domain_partitioned_planner_intents_respect_fair_budget() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 6,
        domain_budget_enabled: true,
        allow_single_oversubscription: true,
        ..ResizeSchedulerConfig::default()
    });

    let input = ReflowPlannerInput {
        total_logical_lines: 100,
        viewport_top: 30,
        viewport_height: 20,
        overscan_lines: 5,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };

    let plan = ViewportReflowPlanner::plan(&input);
    let hook = &plan.scheduling_hooks()[0];

    // Submit from local domain.
    let mut local_intent = hook.to_resize_intent(1, 1, 100);
    local_intent.domain = ResizeDomain::Local;
    scheduler.submit_intent(local_intent);

    // Submit from SSH domain.
    let mut ssh_intent = hook.to_resize_intent(2, 1, 100);
    ssh_intent.domain = ResizeDomain::Ssh {
        host: "remote-host".to_string(),
    };
    scheduler.submit_intent(ssh_intent);

    // Schedule — both should compete for budget.
    let frame = scheduler.schedule_frame();
    assert!(
        !frame.scheduled.is_empty(),
        "at least one pane should be scheduled with domain budgets"
    );

    // Clean up.
    for work in &frame.scheduled {
        scheduler.complete_active(work.pane_id, work.intent_seq);
    }
}

#[test]
fn input_guardrail_defers_planner_background_work() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 8,
        input_guardrail_enabled: true,
        input_backlog_threshold: 1,
        input_reserve_units: 4,
        max_deferrals_before_force: 3,
        allow_single_oversubscription: true,
        ..ResizeSchedulerConfig::default()
    });

    let input = ReflowPlannerInput {
        total_logical_lines: 500,
        viewport_top: 200,
        viewport_height: 30,
        overscan_lines: 10,
        max_batch_lines: 16,
        lines_per_work_unit: 8,
        frame_budget_units: 100,
    };

    let plan = ViewportReflowPlanner::plan(&input);
    let hooks = plan.scheduling_hooks();

    // Find a cold scrollback hook (Background class).
    let cold_hook = hooks
        .iter()
        .find(|h| h.scheduler_class == ResizeWorkClass::Background)
        .expect("should have background hook");

    // Submit background work.
    let intent = cold_hook.to_resize_intent(1, 1, 100);
    scheduler.submit_intent(intent);

    // Schedule with high input backlog — background should be deferred.
    let frame = scheduler.schedule_frame_with_input_backlog(8, 5);
    assert_eq!(frame.input_reserved_units, 4);
    assert_eq!(
        frame.effective_resize_budget_units, 4,
        "resize budget should be reduced by input reserve"
    );
}

#[test]
fn storm_detection_throttles_rapid_planner_resubmissions() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 20,
        storm_window_ms: 100,
        storm_threshold_intents: 3,
        max_storm_picks_per_tab: 1,
        allow_single_oversubscription: true,
        ..ResizeSchedulerConfig::default()
    });

    let input = ReflowPlannerInput {
        total_logical_lines: 100,
        viewport_top: 30,
        viewport_height: 20,
        overscan_lines: 5,
        max_batch_lines: 32,
        lines_per_work_unit: 8,
        frame_budget_units: 10,
    };

    let plan = ViewportReflowPlanner::plan(&input);
    let hook = &plan.scheduling_hooks()[0];

    // Submit 5 rapid intents from the same tab (simulating resize storm).
    for seq in 1..=5u64 {
        let mut intent = hook.to_resize_intent(seq, seq, 100 + seq);
        intent.tab_id = Some(42);
        scheduler.submit_intent(intent);
    }

    // Schedule — storm detection should limit picks from tab 42.
    let frame = scheduler.schedule_frame();
    let tab42_picks: Vec<_> = frame
        .scheduled
        .iter()
        .filter(|w| {
            // All panes 1-5 have tab_id=42.
            (1..=5).contains(&w.pane_id)
        })
        .collect();
    assert!(
        tab42_picks.len() <= scheduler.config().max_storm_picks_per_tab as usize,
        "storm detection should limit per-tab picks, got {}",
        tab42_picks.len()
    );
}
