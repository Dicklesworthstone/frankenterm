use frankenterm_core::resize_invariants::{
    ResizeInvariantReport, ResizeViolationKind, check_lifecycle_event_invariants,
    check_scheduler_snapshot_invariants,
};
use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeExecutionPhase, ResizeIntent, ResizeLifecycleDetail, ResizeLifecycleStage,
    ResizeScheduler, ResizeSchedulerConfig, ResizeWorkClass,
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
fn scheduler_snapshot_contract_is_clean_for_nominal_single_flight_flow() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
    let _ = scheduler.submit_intent(intent(7, 1, ResizeWorkClass::Interactive, 1, 100));
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 1);

    // Superseding pending intent while sequence 1 is active.
    let _ = scheduler.submit_intent(intent(7, 2, ResizeWorkClass::Interactive, 1, 101));

    let snapshot = scheduler.snapshot();
    let mut report = ResizeInvariantReport::new();
    check_scheduler_snapshot_invariants(&mut report, &snapshot);

    assert!(
        report.is_clean(),
        "expected clean scheduler snapshot invariant report, got: {:?}",
        report.violations
    );
}

#[test]
fn scheduler_snapshot_contract_detects_aggregate_count_mismatch() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
    let _ = scheduler.submit_intent(intent(9, 1, ResizeWorkClass::Background, 1, 100));
    let mut snapshot = scheduler.snapshot();

    // Corrupt aggregate metadata to emulate broken status surface wiring.
    snapshot.pending_total = snapshot.pending_total.saturating_add(1);

    let mut report = ResizeInvariantReport::new();
    check_scheduler_snapshot_invariants(&mut report, &snapshot);

    assert!(report.has_errors());
    assert!(report.violations.iter().any(|v| {
        matches!(
            v.kind,
            ResizeViolationKind::SnapshotPendingCountMismatch
                | ResizeViolationKind::SnapshotActiveCountMismatch
        )
    }));
}

#[test]
fn lifecycle_contract_is_clean_for_nominal_phase_progression() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
    let _ = scheduler.submit_intent(intent(5, 1, ResizeWorkClass::Interactive, 1, 1_000));
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 1);
    assert!(scheduler.mark_active_phase(5, 1, ResizeExecutionPhase::Reflowing, 1_010));
    assert!(scheduler.mark_active_phase(5, 1, ResizeExecutionPhase::Presenting, 1_020));
    assert!(scheduler.complete_active(5, 1));

    let events = scheduler.lifecycle_events(0);
    let mut report = ResizeInvariantReport::new();
    check_lifecycle_event_invariants(&mut report, &events);

    assert!(
        report.is_clean(),
        "expected clean lifecycle invariant report, got: {:?}",
        report.violations
    );
}

#[test]
fn lifecycle_contract_detects_stage_detail_mismatch() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
    let _ = scheduler.submit_intent(intent(11, 1, ResizeWorkClass::Interactive, 1, 2_000));
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 1);
    assert!(scheduler.mark_active_phase(11, 1, ResizeExecutionPhase::Reflowing, 2_010));
    assert!(scheduler.mark_active_phase(11, 1, ResizeExecutionPhase::Presenting, 2_020));
    assert!(scheduler.complete_active(11, 1));

    let mut events = scheduler.lifecycle_events(0);
    let completion = events
        .iter_mut()
        .find(|event| matches!(event.detail, ResizeLifecycleDetail::ActiveCompleted))
        .expect("ActiveCompleted event should exist");
    completion.stage = ResizeLifecycleStage::Failed;

    let mut report = ResizeInvariantReport::new();
    check_lifecycle_event_invariants(&mut report, &events);

    assert!(report.has_errors());
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::LifecycleDetailStageMismatch)
    );
}

#[test]
fn lifecycle_contract_detects_event_sequence_regression() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
    let _ = scheduler.submit_intent(intent(21, 1, ResizeWorkClass::Interactive, 1, 3_000));
    let _ = scheduler.schedule_frame();

    let mut events = scheduler.lifecycle_events(0);
    assert!(events.len() >= 2, "expected at least two lifecycle events");
    events[1].event_seq = events[0].event_seq;

    let mut report = ResizeInvariantReport::new();
    check_lifecycle_event_invariants(&mut report, &events);

    assert!(report.has_errors());
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::LifecycleEventSequenceRegression)
    );
}
