//! Edge-case and adversarial tests for resize invariant enforcement.
//!
//! Covers boundary conditions, degenerate inputs, and multi-pane interaction
//! patterns that the happy-path contract tests do not exercise.
//!
//! Bead: wa-1u90p.7.1

use frankenterm_core::resize_invariants::{
    DimensionTriple, ResizeInvariantReport, ResizeInvariantTelemetry, ResizePhase,
    ResizeViolationKind, ScreenSnapshot, check_phase_transition, check_presentation_invariants,
    check_scheduler_invariants, check_screen_invariants,
};

// ---------------------------------------------------------------------------
// Scheduler edge cases
// ---------------------------------------------------------------------------

#[test]
fn scheduler_no_seqs_is_clean() {
    let mut report = ResizeInvariantReport::new();
    check_scheduler_invariants(&mut report, 1, None, None, 0, false);
    assert!(
        report.is_clean(),
        "no seqs should be clean: {:?}",
        report.violations
    );
}

#[test]
fn scheduler_active_only_no_latest_is_clean() {
    // When only active_seq exists (no latest), it implies the active intent
    // is the current latest. This is valid during early phase progression.
    let mut report = ResizeInvariantReport::new();
    check_scheduler_invariants(&mut report, 1, Some(1), None, 0, false);
    assert!(report.is_clean());
}

#[test]
fn scheduler_equal_active_latest_is_clean_even_at_max_u64() {
    let mut report = ResizeInvariantReport::new();
    check_scheduler_invariants(&mut report, 1, Some(u64::MAX), Some(u64::MAX), 0, true);
    assert!(report.is_clean());
}

#[test]
fn scheduler_commit_without_seqs_is_clean() {
    // Committing with no sequences is vacuously clean (no stale commit possible).
    let mut report = ResizeInvariantReport::new();
    check_scheduler_invariants(&mut report, 1, None, None, 0, true);
    assert!(report.is_clean());
}

#[test]
fn scheduler_queue_depth_exactly_one_is_clean() {
    let mut report = ResizeInvariantReport::new();
    check_scheduler_invariants(&mut report, 1, Some(1), Some(2), 1, false);
    assert!(report.is_clean());
}

#[test]
fn scheduler_multiple_panes_independent() {
    let mut report = ResizeInvariantReport::new();
    // Pane 1: normal flow
    check_scheduler_invariants(&mut report, 1, Some(5), Some(5), 0, true);
    // Pane 2: also normal flow with different sequence
    check_scheduler_invariants(&mut report, 2, Some(100), Some(100), 0, true);
    // Pane 3: has a queue overflow
    check_scheduler_invariants(&mut report, 3, Some(1), Some(2), 5, false);

    assert_eq!(report.checks_failed, 1, "only pane 3 should have violation");
    assert!(report.violations.iter().all(|v| v.pane_id == Some(3)));
}

// ---------------------------------------------------------------------------
// Screen edge cases
// ---------------------------------------------------------------------------

#[test]
fn screen_minimum_viable_1x1() {
    let mut report = ResizeInvariantReport::new();
    let snapshot = ScreenSnapshot {
        physical_rows: 1,
        physical_cols: 1,
        lines_len: 1,
        scrollback_size: 0,
        cursor_x: 0,
        cursor_y: 0,
        cursor_phys_row: 0,
    };
    check_screen_invariants(&mut report, Some(1), &snapshot);
    assert!(
        report.is_clean(),
        "1x1 should be valid: {:?}",
        report.violations
    );
}

#[test]
fn screen_cursor_at_right_edge_allowed() {
    // cursor_x == physical_cols is valid (wrap_next state)
    let mut report = ResizeInvariantReport::new();
    let snapshot = ScreenSnapshot {
        physical_rows: 24,
        physical_cols: 80,
        lines_len: 24,
        scrollback_size: 0,
        cursor_x: 80, // at the right edge, not past it
        cursor_y: 0,
        cursor_phys_row: 0,
    };
    check_screen_invariants(&mut report, Some(1), &snapshot);
    // cursor_x == physical_cols is allowed per the check
    assert!(
        !report.violations.iter().any(|v| {
            v.kind == ResizeViolationKind::CursorOutOfBounds
                && v.message.contains("past right margin")
        }),
        "cursor at right edge should be valid"
    );
}

#[test]
fn screen_cursor_past_right_edge_warns() {
    let mut report = ResizeInvariantReport::new();
    let snapshot = ScreenSnapshot {
        physical_rows: 24,
        physical_cols: 80,
        lines_len: 24,
        scrollback_size: 0,
        cursor_x: 81, // past right edge
        cursor_y: 0,
        cursor_phys_row: 0,
    };
    check_screen_invariants(&mut report, Some(1), &snapshot);
    assert!(report.violations.iter().any(|v| {
        v.kind == ResizeViolationKind::CursorOutOfBounds && v.message.contains("past right margin")
    }));
}

#[test]
fn screen_zero_rows_detected() {
    let mut report = ResizeInvariantReport::new();
    let snapshot = ScreenSnapshot {
        physical_rows: 0,
        physical_cols: 80,
        lines_len: 0,
        scrollback_size: 0,
        cursor_x: 0,
        cursor_y: 0,
        cursor_phys_row: 0,
    };
    check_screen_invariants(&mut report, Some(1), &snapshot);
    // cursor_phys_row=0 >= lines_len=0 should be out of bounds
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::CursorOutOfBounds)
    );
}

#[test]
fn screen_large_scrollback() {
    let mut report = ResizeInvariantReport::new();
    let snapshot = ScreenSnapshot {
        physical_rows: 24,
        physical_cols: 80,
        lines_len: 100_024,
        scrollback_size: 100_000,
        cursor_x: 0,
        cursor_y: 0,
        cursor_phys_row: 100_000,
    };
    check_screen_invariants(&mut report, Some(1), &snapshot);
    assert!(
        report.is_clean(),
        "large scrollback should be valid: {:?}",
        report.violations
    );
}

#[test]
fn screen_lines_exceed_capacity_warns() {
    let mut report = ResizeInvariantReport::new();
    let snapshot = ScreenSnapshot {
        physical_rows: 24,
        physical_cols: 80,
        lines_len: 2000,
        scrollback_size: 100,
        cursor_x: 0,
        cursor_y: 0,
        cursor_phys_row: 100,
    };
    check_screen_invariants(&mut report, Some(1), &snapshot);
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::ExcessiveLines)
    );
}

// ---------------------------------------------------------------------------
// Presentation edge cases
// ---------------------------------------------------------------------------

#[test]
fn presentation_all_zeros_is_consistent() {
    let mut report = ResizeInvariantReport::new();
    let dims = DimensionTriple {
        pty_rows: 0,
        pty_cols: 0,
        terminal_rows: 0,
        terminal_cols: 0,
        screen_rows: 0,
        screen_cols: 0,
    };
    check_presentation_invariants(&mut report, None, None, &dims);
    assert!(report.is_clean());
}

#[test]
fn presentation_pty_ahead_of_terminal_detected() {
    // This is the F1 race condition from the fault model
    let mut report = ResizeInvariantReport::new();
    let dims = DimensionTriple {
        pty_rows: 30,
        pty_cols: 120,
        terminal_rows: 24,
        terminal_cols: 80,
        screen_rows: 24,
        screen_cols: 80,
    };
    check_presentation_invariants(&mut report, Some(1), Some(5), &dims);
    assert!(report.has_errors());
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::PtyTerminalDimensionMismatch)
    );
}

#[test]
fn presentation_terminal_ahead_of_screen_detected() {
    let mut report = ResizeInvariantReport::new();
    let dims = DimensionTriple {
        pty_rows: 30,
        pty_cols: 120,
        terminal_rows: 30,
        terminal_cols: 120,
        screen_rows: 24,
        screen_cols: 80,
    };
    check_presentation_invariants(&mut report, Some(1), Some(5), &dims);
    assert!(report.has_errors());
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.kind == ResizeViolationKind::RenderDimensionStale)
    );
}

#[test]
fn presentation_only_rows_differ_detected() {
    let mut report = ResizeInvariantReport::new();
    let dims = DimensionTriple {
        pty_rows: 25,
        pty_cols: 80,
        terminal_rows: 24,
        terminal_cols: 80,
        screen_rows: 24,
        screen_cols: 80,
    };
    check_presentation_invariants(&mut report, Some(1), Some(1), &dims);
    assert!(report.has_errors());
}

// ---------------------------------------------------------------------------
// Phase transition exhaustive tests
// ---------------------------------------------------------------------------

#[test]
fn all_valid_transitions_pass() {
    let valid_pairs = [
        (ResizePhase::Idle, ResizePhase::Queued),
        (ResizePhase::Queued, ResizePhase::Preparing),
        (ResizePhase::Queued, ResizePhase::Cancelled),
        (ResizePhase::Preparing, ResizePhase::Reflowing),
        (ResizePhase::Preparing, ResizePhase::Cancelled),
        (ResizePhase::Preparing, ResizePhase::Failed),
        (ResizePhase::Reflowing, ResizePhase::Presenting),
        (ResizePhase::Reflowing, ResizePhase::Cancelled),
        (ResizePhase::Reflowing, ResizePhase::Failed),
        (ResizePhase::Presenting, ResizePhase::Committed),
        (ResizePhase::Presenting, ResizePhase::Cancelled),
        (ResizePhase::Presenting, ResizePhase::Failed),
        (ResizePhase::Committed, ResizePhase::Idle),
        (ResizePhase::Cancelled, ResizePhase::Queued),
        (ResizePhase::Cancelled, ResizePhase::Idle),
        (ResizePhase::Failed, ResizePhase::Idle),
    ];

    for (from, to) in valid_pairs {
        let mut report = ResizeInvariantReport::new();
        check_phase_transition(&mut report, Some(1), Some(1), from, to);
        assert!(
            report.is_clean(),
            "transition {:?} -> {:?} should be valid but got: {:?}",
            from,
            to,
            report.violations
        );
    }
}

#[test]
fn all_illegal_transitions_detected() {
    let illegal_pairs = [
        (ResizePhase::Idle, ResizePhase::Preparing),
        (ResizePhase::Idle, ResizePhase::Reflowing),
        (ResizePhase::Idle, ResizePhase::Presenting),
        (ResizePhase::Idle, ResizePhase::Committed),
        (ResizePhase::Queued, ResizePhase::Reflowing),
        (ResizePhase::Queued, ResizePhase::Presenting),
        (ResizePhase::Queued, ResizePhase::Committed),
        (ResizePhase::Preparing, ResizePhase::Presenting),
        (ResizePhase::Preparing, ResizePhase::Committed),
        (ResizePhase::Reflowing, ResizePhase::Committed),
        (ResizePhase::Committed, ResizePhase::Preparing),
    ];

    for (from, to) in illegal_pairs {
        let mut report = ResizeInvariantReport::new();
        check_phase_transition(&mut report, Some(1), Some(1), from, to);
        assert!(
            report.has_critical(),
            "transition {:?} -> {:?} should be illegal but passed",
            from,
            to
        );
    }
}

#[test]
fn self_transitions_are_illegal() {
    let phases = [
        ResizePhase::Idle,
        ResizePhase::Queued,
        ResizePhase::Preparing,
        ResizePhase::Reflowing,
        ResizePhase::Presenting,
        ResizePhase::Committed,
        ResizePhase::Failed,
    ];

    for phase in phases {
        let mut report = ResizeInvariantReport::new();
        check_phase_transition(&mut report, Some(1), Some(1), phase, phase);
        assert!(
            report.has_critical(),
            "self-transition {:?} -> {:?} should be illegal",
            phase,
            phase
        );
    }
}

// ---------------------------------------------------------------------------
// Telemetry aggregation
// ---------------------------------------------------------------------------

#[test]
fn telemetry_aggregates_across_multiple_reports() {
    let mut telemetry = ResizeInvariantTelemetry::default();

    // Report 1: clean
    let mut r1 = ResizeInvariantReport::new();
    check_scheduler_invariants(&mut r1, 1, Some(1), Some(1), 0, true);
    telemetry.absorb(&r1);

    // Report 2: has violations
    let mut r2 = ResizeInvariantReport::new();
    check_scheduler_invariants(&mut r2, 2, Some(5), Some(7), 3, true);
    telemetry.absorb(&r2);

    // Report 3: clean screen
    let mut r3 = ResizeInvariantReport::new();
    check_screen_invariants(
        &mut r3,
        Some(3),
        &ScreenSnapshot {
            physical_rows: 24,
            physical_cols: 80,
            lines_len: 1024,
            scrollback_size: 1000,
            cursor_x: 0,
            cursor_y: 0,
            cursor_phys_row: 1000,
        },
    );
    telemetry.absorb(&r3);

    assert!(telemetry.total_passes > 0);
    assert!(telemetry.total_failures > 0);
    assert!(telemetry.critical_count >= 1); // stale commit
    assert!(telemetry.error_count >= 1); // queue overflow
    assert_eq!(
        telemetry.total_checks,
        telemetry.total_passes + telemetry.total_failures
    );
}

#[test]
fn telemetry_severity_counts_correct() {
    let mut telemetry = ResizeInvariantTelemetry::default();

    // Create violations of each severity
    let mut report = ResizeInvariantReport::new();

    // Warning: cursor past viewport
    check_screen_invariants(
        &mut report,
        Some(1),
        &ScreenSnapshot {
            physical_rows: 24,
            physical_cols: 80,
            lines_len: 1024,
            scrollback_size: 1000,
            cursor_x: 0,
            cursor_y: 30,
            cursor_phys_row: 100,
        },
    );

    // Error: queue overflow
    check_scheduler_invariants(&mut report, 1, Some(1), Some(1), 5, false);

    // Critical: stale commit
    check_scheduler_invariants(&mut report, 2, Some(3), Some(5), 0, true);

    telemetry.absorb(&report);

    assert!(telemetry.warning_count >= 1);
    assert!(telemetry.error_count >= 1);
    assert!(telemetry.critical_count >= 1);
}
