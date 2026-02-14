use frankenterm_core::resize_scheduler::{
    ResizeIntent, ResizeScheduler, ResizeSchedulerConfig, ResizeWorkClass,
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
