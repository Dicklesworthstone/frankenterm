use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeIntent, ResizeScheduler, ResizeSchedulerConfig, ResizeWorkClass,
};

fn intent(
    pane_id: u64,
    intent_seq: u64,
    scheduler_class: ResizeWorkClass,
    work_units: u32,
    submitted_at_ms: u64,
) -> ResizeIntent {
    ResizeIntent {
        pane_id,
        intent_seq,
        scheduler_class,
        work_units,
        submitted_at_ms,
        domain: ResizeDomain::default(),
        tab_id: None,
    }
}

#[test]
fn background_starvation_recovers_after_input_guardrail_pressure() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 3,
        input_guardrail_enabled: true,
        input_backlog_threshold: 1,
        input_reserve_units: 2,
        max_deferrals_before_force: 2,
        allow_single_oversubscription: true,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Background, 2, 100));
    let _ = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Interactive, 1, 100));

    let frame1 = scheduler.schedule_frame_with_input_backlog(3, 3);
    assert_eq!(frame1.frame_budget_units, 3);
    assert_eq!(frame1.effective_resize_budget_units, 1);
    assert_eq!(frame1.input_reserved_units, 2);
    assert_eq!(frame1.scheduled.len(), 1);
    assert_eq!(frame1.scheduled[0].pane_id, 2);
    assert!(scheduler.complete_active(2, 1));

    let _ = scheduler.submit_intent(intent(2, 2, ResizeWorkClass::Interactive, 1, 101));
    let frame2 = scheduler.schedule_frame_with_input_backlog(3, 3);
    assert_eq!(frame2.input_reserved_units, 2);
    assert_eq!(frame2.scheduled.len(), 1);
    assert_eq!(frame2.scheduled[0].pane_id, 2);
    assert!(scheduler.complete_active(2, 2));

    let _ = scheduler.submit_intent(intent(2, 3, ResizeWorkClass::Interactive, 1, 102));
    let frame3 = scheduler.schedule_frame_with_input_backlog(3, 0);
    assert_eq!(frame3.input_reserved_units, 0);
    assert_eq!(frame3.scheduled.len(), 2);

    let forced_background = frame3
        .scheduled
        .iter()
        .find(|work| work.pane_id == 1)
        .expect("background pane should be scheduled once backlog pressure clears");
    assert!(forced_background.forced_by_starvation);
    assert!(frame3.scheduled.iter().any(|work| work.pane_id == 2));

    assert!(scheduler.complete_active(1, 1));
    assert!(scheduler.complete_active(2, 3));
    assert_eq!(scheduler.metrics().forced_background_runs, 1);
    assert!(scheduler.metrics().input_guardrail_deferrals >= 2);
}

#[test]
fn debug_snapshot_reports_guardrail_budget_split_and_deferral_signals() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 4,
        input_guardrail_enabled: true,
        input_backlog_threshold: 1,
        input_reserve_units: 2,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(7, 1, ResizeWorkClass::Interactive, 4, 200));
    let frame = scheduler.schedule_frame_with_input_backlog(4, 2);

    assert!(frame.scheduled.is_empty());
    assert_eq!(frame.frame_budget_units, 4);
    assert_eq!(frame.effective_resize_budget_units, 2);
    assert_eq!(frame.input_reserved_units, 2);

    let debug = scheduler.debug_snapshot(8);
    assert_eq!(debug.scheduler.metrics.last_frame_budget_units, 4);
    assert_eq!(
        debug.scheduler.metrics.last_effective_resize_budget_units,
        2
    );
    assert_eq!(debug.scheduler.metrics.last_input_backlog, 2);
    assert_eq!(debug.scheduler.metrics.input_guardrail_frames, 1);
    assert_eq!(debug.scheduler.metrics.input_guardrail_deferrals, 1);

    let pane = debug
        .scheduler
        .panes
        .iter()
        .find(|row| row.pane_id == 7)
        .expect("pane should remain pending after guardrail deferral");
    assert_eq!(pane.pending_seq, Some(1));
    assert_eq!(pane.deferrals, 1);
}

// ── DarkBadger wa-1u90p.7.1 ──────────────────────────────────────

#[test]
fn guardrail_minimum_budget_leaves_at_least_one_unit() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 2,
        input_guardrail_enabled: true,
        input_backlog_threshold: 1,
        input_reserve_units: 10, // much larger than budget
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
    let frame = scheduler.schedule_frame_with_input_backlog(2, 5);

    // Reserve can't exceed budget-1, so effective_resize_budget >= 1
    assert!(
        frame.effective_resize_budget_units >= 1,
        "guardrail should leave at least 1 unit for resize work"
    );
    assert!(
        frame.input_reserved_units < frame.frame_budget_units,
        "reserve should be less than total budget"
    );
}

#[test]
fn guardrail_budget_one_no_reserve() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 1,
        input_guardrail_enabled: true,
        input_backlog_threshold: 1,
        input_reserve_units: 2,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
    let frame = scheduler.schedule_frame_with_input_backlog(1, 10);

    // Budget of 1 should not be reduced further
    assert_eq!(frame.input_reserved_units, 0);
    assert_eq!(frame.effective_resize_budget_units, 1);
    // The 1-unit intent should still be schedulable
    assert_eq!(frame.scheduled.len(), 1);
}

#[test]
fn guardrail_multiple_interactive_panes_under_pressure() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 6,
        input_guardrail_enabled: true,
        input_backlog_threshold: 1,
        input_reserve_units: 2,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    // Three interactive panes, each needing 2 units
    for i in 1..=3 {
        let _ = scheduler.submit_intent(intent(i, 1, ResizeWorkClass::Interactive, 2, 100));
    }

    let frame = scheduler.schedule_frame_with_input_backlog(6, 3);
    // effective = 6 - 2 = 4, so only 2 of 3 panes fit (2*2=4)
    assert_eq!(frame.effective_resize_budget_units, 4);
    assert_eq!(frame.scheduled.len(), 2);
    assert_eq!(frame.pending_after, 1);
}

#[test]
fn guardrail_deferred_intent_ages_and_eventually_schedules() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 4,
        input_guardrail_enabled: true,
        input_backlog_threshold: 1,
        input_reserve_units: 2,
        max_deferrals_before_force: 2,
        allow_single_oversubscription: true,
        ..ResizeSchedulerConfig::default()
    });

    // Background pane needs 3 units, effective budget under pressure is 2
    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Background, 3, 100));

    // Frame 1: deferred (3 > 2 effective)
    let f1 = scheduler.schedule_frame_with_input_backlog(4, 5);
    assert_eq!(f1.scheduled.len(), 0);

    // Frame 2: deferred again
    let f2 = scheduler.schedule_frame_with_input_backlog(4, 5);
    assert_eq!(f2.scheduled.len(), 0);

    // Frame 3: no backlog pressure, starvation forcing kicks in
    let f3 = scheduler.schedule_frame_with_input_backlog(4, 0);
    assert_eq!(f3.scheduled.len(), 1);
    assert!(f3.scheduled[0].forced_by_starvation);
    assert_eq!(f3.scheduled[0].pane_id, 1);
}

#[test]
fn guardrail_metrics_accumulate_correctly() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 4,
        input_guardrail_enabled: true,
        input_backlog_threshold: 1,
        input_reserve_units: 2,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    // Submit intent that fits in budget but not in effective budget
    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Background, 3, 100));

    // 3 frames with input pressure → 3 deferral frames
    for _ in 0..3 {
        scheduler.schedule_frame_with_input_backlog(4, 5);
    }

    assert_eq!(scheduler.metrics().input_guardrail_frames, 3);
    assert_eq!(scheduler.metrics().input_guardrail_deferrals, 3);
}
