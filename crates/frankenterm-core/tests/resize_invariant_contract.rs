use frankenterm_core::resize_invariants::{
    DimensionTriple, ResizeInvariantReport, ResizeViolationKind, ScreenSnapshot,
    check_lifecycle_event_invariants, check_presentation_invariants,
    check_scheduler_snapshot_invariants, check_screen_invariants,
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

// ── DarkBadger wa-1u90p.7.1 ──────────────────────────────────────

// -- Screen invariant contract tests --

fn valid_screen() -> ScreenSnapshot {
    ScreenSnapshot {
        physical_rows: 24,
        physical_cols: 80,
        lines_len: 24,
        scrollback_size: 1000,
        cursor_x: 0,
        cursor_y: 0,
        cursor_phys_row: 0,
    }
}

#[test]
fn screen_contract_clean_for_nominal_viewport() {
    let mut report = ResizeInvariantReport::new();
    check_screen_invariants(&mut report, Some(1), &valid_screen());
    assert!(
        report.is_clean(),
        "nominal viewport should be clean: {:?}",
        report.violations
    );
}

#[test]
fn screen_contract_detects_insufficient_lines() {
    let mut report = ResizeInvariantReport::new();
    let snap = ScreenSnapshot {
        lines_len: 10, // less than physical_rows=24
        ..valid_screen()
    };
    check_screen_invariants(&mut report, Some(2), &snap);
    assert!(report.has_errors());
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::InsufficientLines)
    );
}

#[test]
fn screen_contract_detects_excessive_lines() {
    let mut report = ResizeInvariantReport::new();
    let snap = ScreenSnapshot {
        lines_len: 24 + 1001, // exceeds rows + scrollback
        ..valid_screen()
    };
    check_screen_invariants(&mut report, Some(3), &snap);
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::ExcessiveLines)
    );
}

#[test]
fn screen_contract_detects_cursor_phys_row_out_of_bounds() {
    let mut report = ResizeInvariantReport::new();
    let snap = ScreenSnapshot {
        cursor_phys_row: 24, // == lines_len, out of bounds (0-indexed)
        ..valid_screen()
    };
    check_screen_invariants(&mut report, Some(4), &snap);
    assert!(report.has_errors());
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::CursorOutOfBounds)
    );
}

#[test]
fn screen_contract_detects_cursor_y_below_zero() {
    let mut report = ResizeInvariantReport::new();
    let snap = ScreenSnapshot {
        cursor_y: -1,
        ..valid_screen()
    };
    check_screen_invariants(&mut report, Some(5), &snap);
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::CursorOutOfBounds)
    );
}

#[test]
fn screen_contract_detects_cursor_y_above_viewport() {
    let mut report = ResizeInvariantReport::new();
    let snap = ScreenSnapshot {
        cursor_y: 24, // == physical_rows, out of viewport
        ..valid_screen()
    };
    check_screen_invariants(&mut report, Some(6), &snap);
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::CursorOutOfBounds)
    );
}

#[test]
fn screen_contract_detects_cursor_x_past_right_margin() {
    let mut report = ResizeInvariantReport::new();
    let snap = ScreenSnapshot {
        cursor_x: 81, // > physical_cols=80
        ..valid_screen()
    };
    check_screen_invariants(&mut report, Some(7), &snap);
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::CursorOutOfBounds)
    );
}

#[test]
fn screen_contract_allows_cursor_x_at_wrap_position() {
    // cursor_x == physical_cols is valid (wrap_next state)
    let mut report = ResizeInvariantReport::new();
    let snap = ScreenSnapshot {
        cursor_x: 80, // == physical_cols, valid wrap_next position
        ..valid_screen()
    };
    check_screen_invariants(&mut report, Some(8), &snap);
    assert!(
        report.is_clean(),
        "cursor at wrap_next position should be clean: {:?}",
        report.violations
    );
}

#[test]
fn screen_contract_accepts_scrollback_filled_viewport() {
    let mut report = ResizeInvariantReport::new();
    let snap = ScreenSnapshot {
        lines_len: 24 + 1000, // exactly at capacity
        ..valid_screen()
    };
    check_screen_invariants(&mut report, Some(9), &snap);
    assert!(
        report.is_clean(),
        "viewport at exact capacity should be clean: {:?}",
        report.violations
    );
}

// -- Presentation invariant contract tests --

fn consistent_dims() -> DimensionTriple {
    DimensionTriple {
        pty_rows: 24,
        pty_cols: 80,
        terminal_rows: 24,
        terminal_cols: 80,
        screen_rows: 24,
        screen_cols: 80,
    }
}

#[test]
fn presentation_contract_clean_for_consistent_dimensions() {
    let mut report = ResizeInvariantReport::new();
    check_presentation_invariants(&mut report, Some(1), Some(1), &consistent_dims());
    assert!(
        report.is_clean(),
        "consistent dims should be clean: {:?}",
        report.violations
    );
}

#[test]
fn presentation_contract_detects_pty_terminal_row_mismatch() {
    let mut report = ResizeInvariantReport::new();
    let dims = DimensionTriple {
        pty_rows: 25, // mismatch with terminal_rows=24
        ..consistent_dims()
    };
    check_presentation_invariants(&mut report, Some(2), Some(1), &dims);
    assert!(report.has_errors());
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::PtyTerminalDimensionMismatch)
    );
}

#[test]
fn presentation_contract_detects_pty_terminal_col_mismatch() {
    let mut report = ResizeInvariantReport::new();
    let dims = DimensionTriple {
        pty_cols: 120, // mismatch with terminal_cols=80
        ..consistent_dims()
    };
    check_presentation_invariants(&mut report, Some(3), Some(1), &dims);
    assert!(report.has_errors());
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::PtyTerminalDimensionMismatch)
    );
}

#[test]
fn presentation_contract_detects_render_dimension_stale() {
    let mut report = ResizeInvariantReport::new();
    let dims = DimensionTriple {
        screen_rows: 20, // mismatch with terminal_rows=24
        ..consistent_dims()
    };
    check_presentation_invariants(&mut report, Some(4), Some(1), &dims);
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::RenderDimensionStale)
    );
}

#[test]
fn invariant_report_total_checks_accumulates() {
    let mut report = ResizeInvariantReport::new();
    check_screen_invariants(&mut report, Some(1), &valid_screen());
    let checks_after_screen = report.total_checks();
    assert!(
        checks_after_screen > 0,
        "screen check should register checks"
    );

    check_presentation_invariants(&mut report, Some(1), Some(1), &consistent_dims());
    assert!(
        report.total_checks() > checks_after_screen,
        "presentation check should add more checks"
    );
}

#[test]
fn invariant_report_has_critical_vs_error_distinction() {
    // Clean report has neither
    let report = ResizeInvariantReport::new();
    assert!(!report.has_critical());
    assert!(!report.has_errors());
    assert!(report.is_clean());
}
