//! Property-based tests for resize_invariants module.
//!
//! Validates invariant checker correctness under randomized inputs:
//! - Phase transition FSM: valid vs illegal transitions
//! - Scheduler invariant detection: monotonicity, stale commit, queue depth
//! - Screen invariant detection: cursor bounds, line counts
//! - Presentation invariant detection: dimension consistency
//! - Report/telemetry accumulation correctness
//! - Serde roundtrip stability
//!
//! Bead: wa-1u90p.7 (Validation Program)

use proptest::prelude::*;

use frankenterm_core::resize_invariants::{
    DimensionTriple, ResizeInvariantReport, ResizeInvariantTelemetry, ResizePhase,
    ResizeViolationKind, ResizeViolationSeverity, ScreenSnapshot, check_phase_transition,
    check_presentation_invariants, check_scheduler_invariants, check_scheduler_snapshot_invariants,
    check_scheduler_snapshot_row_invariants, check_screen_invariants,
};
use frankenterm_core::resize_scheduler::{
    ResizeExecutionPhase, ResizeSchedulerConfig, ResizeSchedulerMetrics,
    ResizeSchedulerPaneSnapshot, ResizeSchedulerSnapshot, ResizeWorkClass,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_phase() -> impl Strategy<Value = ResizePhase> {
    prop_oneof![
        Just(ResizePhase::Idle),
        Just(ResizePhase::Queued),
        Just(ResizePhase::Preparing),
        Just(ResizePhase::Reflowing),
        Just(ResizePhase::Presenting),
        Just(ResizePhase::Committed),
        Just(ResizePhase::Cancelled),
        Just(ResizePhase::Failed),
    ]
}

fn arb_severity() -> impl Strategy<Value = ResizeViolationSeverity> {
    prop_oneof![
        Just(ResizeViolationSeverity::Warning),
        Just(ResizeViolationSeverity::Error),
        Just(ResizeViolationSeverity::Critical),
    ]
}

fn arb_screen_snapshot() -> impl Strategy<Value = ScreenSnapshot> {
    (1_usize..200, 1_usize..500, 0_usize..10_000, 0_usize..100).prop_flat_map(
        |(rows, cols, scrollback, _cursor_y_offset)| {
            let max_lines = rows + scrollback;
            let lines_len = rows..=max_lines;
            let cursor_x = 0..=cols;
            let cursor_y = 0..rows;
            lines_len.prop_flat_map(move |lines_len| {
                let cursor_phys = if lines_len > 0 { 0..lines_len } else { 0..1 };
                (
                    Just(lines_len),
                    cursor_x.clone(),
                    cursor_y.clone(),
                    cursor_phys,
                )
                    .prop_map(move |(lines_len, cx, cy, cpr)| ScreenSnapshot {
                        physical_rows: rows,
                        physical_cols: cols,
                        lines_len,
                        scrollback_size: scrollback,
                        cursor_x: cx,
                        cursor_y: cy as i64,
                        cursor_phys_row: cpr,
                    })
            })
        },
    )
}

fn arb_screen_snapshot_invalid() -> impl Strategy<Value = ScreenSnapshot> {
    prop_oneof![
        // lines_len < physical_rows (insufficient lines)
        (2_usize..100, 1_usize..200).prop_map(|(rows, cols)| {
            ScreenSnapshot {
                physical_rows: rows,
                physical_cols: cols,
                lines_len: rows - 1,
                scrollback_size: 1000,
                cursor_x: 0,
                cursor_y: 0,
                cursor_phys_row: 0,
            }
        }),
        // cursor_phys_row >= lines_len (out of bounds)
        (1_usize..100, 1_usize..200, 1_usize..500).prop_map(|(rows, cols, extra)| {
            let lines_len = rows + 100;
            ScreenSnapshot {
                physical_rows: rows,
                physical_cols: cols,
                lines_len,
                scrollback_size: 200,
                cursor_x: 0,
                cursor_y: 0,
                cursor_phys_row: lines_len + extra,
            }
        }),
        // cursor_x > physical_cols (past right margin)
        (1_usize..100, 1_usize..200, 1_usize..50).prop_map(|(rows, cols, extra)| {
            ScreenSnapshot {
                physical_rows: rows,
                physical_cols: cols,
                lines_len: rows + 100,
                scrollback_size: 200,
                cursor_x: cols + extra,
                cursor_y: 0,
                cursor_phys_row: 0,
            }
        }),
    ]
}

fn arb_dimension_triple_consistent() -> impl Strategy<Value = DimensionTriple> {
    (1_usize..500, 1_usize..500).prop_map(|(rows, cols)| DimensionTriple {
        pty_rows: rows,
        pty_cols: cols,
        terminal_rows: rows,
        terminal_cols: cols,
        screen_rows: rows,
        screen_cols: cols,
    })
}

fn arb_dimension_triple_inconsistent() -> impl Strategy<Value = DimensionTriple> {
    (
        1_usize..500,
        1_usize..500,
        1_usize..500,
        1_usize..500,
        1_usize..500,
        1_usize..500,
    )
        .prop_filter("must be inconsistent", |(pr, pc, tr, tc, sr, sc)| {
            !(*pr == *tr && *pc == *tc && *tr == *sr && *tc == *sc)
        })
        .prop_map(|(pr, pc, tr, tc, sr, sc)| DimensionTriple {
            pty_rows: pr,
            pty_cols: pc,
            terminal_rows: tr,
            terminal_cols: tc,
            screen_rows: sr,
            screen_cols: sc,
        })
}

#[allow(dead_code)]
fn arb_pane_snapshot_valid() -> impl Strategy<Value = ResizeSchedulerPaneSnapshot> {
    (
        1_u64..1000,
        1_u64..1000,
        proptest::option::of(any::<bool>()),
    )
        .prop_map(|(pane_id, seq, has_active)| {
            let active_seq = if has_active.unwrap_or(false) {
                Some(seq)
            } else {
                None
            };
            let active_phase = if active_seq.is_some() {
                Some(ResizeExecutionPhase::Preparing)
            } else {
                None
            };
            ResizeSchedulerPaneSnapshot {
                pane_id,
                latest_seq: Some(seq),
                pending_seq: None,
                pending_class: None,
                active_seq,
                active_phase,
                active_phase_started_at_ms: None,
                deferrals: 0,
                aging_credit: 0,
            }
        })
}

// ---------------------------------------------------------------------------
// Phase transition FSM properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn phase_valid_transitions_are_accepted(from in arb_phase()) {
        for &to in from.valid_transitions() {
            let mut report = ResizeInvariantReport::new();
            check_phase_transition(&mut report, Some(1), Some(1), from, to);
            prop_assert!(
                report.is_clean(),
                "valid transition {:?} -> {:?} should produce clean report, got {:?}",
                from, to, report.violations
            );
        }
    }

    #[test]
    fn self_transitions_always_illegal(phase in arb_phase()) {
        let mut report = ResizeInvariantReport::new();
        check_phase_transition(&mut report, Some(1), Some(1), phase, phase);
        prop_assert!(
            report.has_critical(),
            "self-transition {:?} -> {:?} should be critical violation",
            phase, phase
        );
        prop_assert!(
            report.violations.iter().any(|v| v.kind == ResizeViolationKind::IllegalPhaseTransition),
            "expected IllegalPhaseTransition violation for {:?} -> {:?}",
            phase, phase
        );
    }

    #[test]
    fn illegal_transitions_detected(from in arb_phase(), to in arb_phase()) {
        let is_valid = from.valid_transitions().contains(&to);
        let mut report = ResizeInvariantReport::new();
        check_phase_transition(&mut report, Some(1), Some(1), from, to);

        if is_valid {
            prop_assert!(report.is_clean(),
                "valid transition {:?} -> {:?} should be clean",
                from, to
            );
        } else {
            prop_assert!(report.has_critical(),
                "illegal transition {:?} -> {:?} should be critical",
                from, to
            );
        }
    }

    #[test]
    fn phase_transition_check_count_always_one(from in arb_phase(), to in arb_phase()) {
        let mut report = ResizeInvariantReport::new();
        check_phase_transition(&mut report, Some(1), Some(1), from, to);
        prop_assert_eq!(report.total_checks(), 1, "phase transition should produce exactly 1 check");
    }
}

// ---------------------------------------------------------------------------
// Scheduler invariant properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn scheduler_clean_when_active_equals_latest(seq in 1_u64..10000, queue in 0_usize..2) {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(&mut report, 1, Some(seq), Some(seq), queue.min(1), false);
        if queue <= 1 {
            prop_assert!(report.is_clean(),
                "seq={} queue={} should be clean, violations: {:?}",
                seq, queue.min(1), report.violations
            );
        }
    }

    #[test]
    fn scheduler_detects_stale_commit(
        active in 1_u64..5000,
        gap in 1_u64..100
    ) {
        let latest = active + gap;
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(&mut report, 1, Some(active), Some(latest), 0, true);
        prop_assert!(
            report.has_critical(),
            "committing active={} with latest={} should be critical stale commit",
            active, latest
        );
        prop_assert!(
            report.violations.iter().any(|v| v.kind == ResizeViolationKind::StaleCommit),
            "expected StaleCommit violation"
        );
    }

    #[test]
    fn scheduler_detects_queue_overflow(
        depth in 2_usize..100,
        active_seq in 1_u64..1000,
        latest_seq in 1_u64..1000
    ) {
        let latest = active_seq.max(latest_seq);
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(&mut report, 1, Some(active_seq.min(latest)), Some(latest), depth, false);
        prop_assert!(
            report.violations.iter().any(|v| v.kind == ResizeViolationKind::QueueDepthOverflow),
            "queue_depth={} > 1 should trigger overflow, violations: {:?}",
            depth, report.violations
        );
    }

    #[test]
    fn scheduler_detects_sequence_regression(
        active in 2_u64..10000,
        gap in 1_u64..100
    ) {
        let latest = active - gap.min(active - 1);
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(&mut report, 1, Some(active), Some(latest), 0, false);
        if latest < active {
            prop_assert!(
                report.violations.iter().any(|v| v.kind == ResizeViolationKind::IntentSequenceRegression),
                "active={} > latest={} should trigger regression",
                active, latest
            );
        }
    }

    #[test]
    fn valid_commit_is_clean(seq in 1_u64..10000) {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(&mut report, 1, Some(seq), Some(seq), 0, true);
        prop_assert!(
            report.is_clean(),
            "valid commit seq={} should be clean, violations: {:?}",
            seq, report.violations
        );
    }
}

// ---------------------------------------------------------------------------
// Scheduler snapshot row invariant properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn snapshot_row_clean_when_all_seqs_consistent(seq in 1_u64..10000) {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_snapshot_row_invariants(
            &mut report, 1, Some(seq), None, Some(seq), true,
        );
        prop_assert!(report.is_clean(),
            "consistent snapshot row should be clean, violations: {:?}",
            report.violations
        );
    }

    #[test]
    fn snapshot_row_pending_must_be_greater_than_active(
        active in 1_u64..5000,
        pending_delta in 0_u64..100
    ) {
        let pending = active + pending_delta;
        let latest = pending;
        let mut report = ResizeInvariantReport::new();
        check_scheduler_snapshot_row_invariants(
            &mut report, 1, Some(latest), Some(pending), Some(active), true,
        );
        if pending_delta == 0 {
            // pending == active: ConcurrentPaneTransaction
            prop_assert!(
                report.violations.iter().any(|v| v.kind == ResizeViolationKind::ConcurrentPaneTransaction),
                "pending={} == active={} should trigger concurrent pane error",
                pending, active
            );
        } else {
            prop_assert!(report.is_clean(),
                "pending={} > active={} should be clean, violations: {:?}",
                pending, active, report.violations
            );
        }
    }

    #[test]
    fn snapshot_row_phase_without_active_is_critical(
        latest in 1_u64..10000
    ) {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_snapshot_row_invariants(
            &mut report, 1, Some(latest), None, None, true,
        );
        prop_assert!(
            report.has_critical(),
            "active_phase without active_seq should be critical"
        );
        prop_assert!(
            report.violations.iter().any(|v| v.kind == ResizeViolationKind::IllegalPhaseTransition),
            "expected IllegalPhaseTransition violation"
        );
    }

    #[test]
    fn snapshot_row_all_none_is_clean(pane_id in 1_u64..100) {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_snapshot_row_invariants(
            &mut report, pane_id, None, None, None, false,
        );
        prop_assert!(report.is_clean(),
            "all-None snapshot row should be clean");
    }
}

// ---------------------------------------------------------------------------
// Aggregate snapshot invariant properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn snapshot_with_unique_panes_is_clean(
        pane_count in 1_usize..20
    ) {
        let mut panes = Vec::new();
        for i in 0..pane_count {
            panes.push(ResizeSchedulerPaneSnapshot {
                pane_id: i as u64 + 1,
                latest_seq: Some(10),
                pending_seq: None,
                pending_class: None,
                active_seq: None,
                active_phase: None,
                active_phase_started_at_ms: None,
                deferrals: 0,
                aging_credit: 0,
            });
        }

        let snapshot = ResizeSchedulerSnapshot {
            config: ResizeSchedulerConfig::default(),
            metrics: ResizeSchedulerMetrics::default(),
            pending_total: 0,
            active_total: 0,
            panes,
        };

        let mut report = ResizeInvariantReport::new();
        check_scheduler_snapshot_invariants(&mut report, &snapshot);
        prop_assert!(report.is_clean(),
            "snapshot with unique panes should be clean, violations: {:?}",
            report.violations
        );
    }

    #[test]
    fn snapshot_count_mismatch_detected(
        real_pending in 0_usize..10,
        claimed_pending in 0_usize..10,
        real_active in 0_usize..10,
        claimed_active in 0_usize..10
    ) {
        let total = real_pending + real_active;
        let idle = 5_usize;
        let mut panes = Vec::new();

        // Create panes with pending
        for i in 0..real_pending {
            panes.push(ResizeSchedulerPaneSnapshot {
                pane_id: i as u64,
                latest_seq: Some(10),
                pending_seq: Some(10),
                pending_class: Some(ResizeWorkClass::Interactive),
                active_seq: None,
                active_phase: None,
                active_phase_started_at_ms: None,
                deferrals: 0,
                aging_credit: 0,
            });
        }
        // Create panes with active
        for i in 0..real_active {
            panes.push(ResizeSchedulerPaneSnapshot {
                pane_id: (real_pending + i) as u64,
                latest_seq: Some(10),
                pending_seq: None,
                pending_class: None,
                active_seq: Some(10),
                active_phase: Some(ResizeExecutionPhase::Reflowing),
                active_phase_started_at_ms: None,
                deferrals: 0,
                aging_credit: 0,
            });
        }
        // Create idle panes
        for i in 0..idle {
            panes.push(ResizeSchedulerPaneSnapshot {
                pane_id: (total + i) as u64,
                latest_seq: Some(10),
                pending_seq: None,
                pending_class: None,
                active_seq: None,
                active_phase: None,
                active_phase_started_at_ms: None,
                deferrals: 0,
                aging_credit: 0,
            });
        }

        let snapshot = ResizeSchedulerSnapshot {
            config: ResizeSchedulerConfig::default(),
            metrics: ResizeSchedulerMetrics::default(),
            pending_total: claimed_pending,
            active_total: claimed_active,
            panes,
        };

        let mut report = ResizeInvariantReport::new();
        check_scheduler_snapshot_invariants(&mut report, &snapshot);

        if claimed_pending != real_pending {
            prop_assert!(
                report.violations.iter().any(|v| v.kind == ResizeViolationKind::SnapshotPendingCountMismatch),
                "claimed_pending={} != real_pending={} should trigger mismatch",
                claimed_pending, real_pending
            );
        }
        if claimed_active != real_active {
            prop_assert!(
                report.violations.iter().any(|v| v.kind == ResizeViolationKind::SnapshotActiveCountMismatch),
                "claimed_active={} != real_active={} should trigger mismatch",
                claimed_active, real_active
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Screen invariant properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn valid_screen_snapshot_is_clean(snapshot in arb_screen_snapshot()) {
        let mut report = ResizeInvariantReport::new();
        check_screen_invariants(&mut report, Some(1), &snapshot);
        prop_assert!(report.is_clean(),
            "valid screen snapshot should be clean: {:?}, violations: {:?}",
            snapshot, report.violations
        );
    }

    #[test]
    fn invalid_screen_snapshot_has_violations(snapshot in arb_screen_snapshot_invalid()) {
        let mut report = ResizeInvariantReport::new();
        check_screen_invariants(&mut report, Some(1), &snapshot);
        prop_assert!(!report.is_clean(),
            "invalid screen snapshot should have violations: {:?}",
            snapshot
        );
    }

    #[test]
    fn screen_checks_always_count_five(
        rows in 1_usize..100,
        cols in 1_usize..200,
    ) {
        let snapshot = ScreenSnapshot {
            physical_rows: rows,
            physical_cols: cols,
            lines_len: rows + 100,
            scrollback_size: 200,
            cursor_x: 0,
            cursor_y: 0,
            cursor_phys_row: 0,
        };
        let mut report = ResizeInvariantReport::new();
        check_screen_invariants(&mut report, Some(1), &snapshot);
        prop_assert_eq!(
            report.total_checks(), 5,
            "screen invariants should always run exactly 5 checks"
        );
    }
}

// ---------------------------------------------------------------------------
// Presentation invariant properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn consistent_dimensions_are_clean(dims in arb_dimension_triple_consistent()) {
        let mut report = ResizeInvariantReport::new();
        check_presentation_invariants(&mut report, Some(1), Some(1), &dims);
        prop_assert!(report.is_clean(),
            "consistent dimensions should be clean: {:?}",
            dims
        );
    }

    #[test]
    fn inconsistent_dimensions_have_violations(dims in arb_dimension_triple_inconsistent()) {
        let mut report = ResizeInvariantReport::new();
        check_presentation_invariants(&mut report, Some(1), Some(1), &dims);
        // At least one of PTY/terminal or terminal/screen should mismatch
        let pty_match = dims.pty_rows == dims.terminal_rows && dims.pty_cols == dims.terminal_cols;
        let screen_match = dims.terminal_rows == dims.screen_rows && dims.terminal_cols == dims.screen_cols;

        if !pty_match {
            prop_assert!(
                report.violations.iter().any(|v| v.kind == ResizeViolationKind::PtyTerminalDimensionMismatch),
                "PTY != terminal should trigger mismatch: {:?}", dims
            );
        }
        if !screen_match {
            prop_assert!(
                report.violations.iter().any(|v| v.kind == ResizeViolationKind::RenderDimensionStale),
                "terminal != screen should trigger stale: {:?}", dims
            );
        }
    }

    #[test]
    fn presentation_checks_always_count_two(
        pr in 1_usize..100, pc in 1_usize..100,
        tr in 1_usize..100, tc in 1_usize..100,
        sr in 1_usize..100, sc in 1_usize..100
    ) {
        let dims = DimensionTriple {
            pty_rows: pr, pty_cols: pc,
            terminal_rows: tr, terminal_cols: tc,
            screen_rows: sr, screen_cols: sc,
        };
        let mut report = ResizeInvariantReport::new();
        check_presentation_invariants(&mut report, Some(1), Some(1), &dims);
        prop_assert_eq!(report.total_checks(), 2,
            "presentation invariants should always run exactly 2 checks"
        );
    }
}

// ---------------------------------------------------------------------------
// Report and telemetry properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn report_total_is_pass_plus_fail(
        active_seq in proptest::option::of(1_u64..100),
        latest_seq in proptest::option::of(1_u64..100),
        queue_depth in 0_usize..5,
        is_committing in any::<bool>()
    ) {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(
            &mut report, 1, active_seq, latest_seq, queue_depth, is_committing,
        );
        prop_assert_eq!(
            report.total_checks(),
            report.checks_passed + report.checks_failed,
            "total_checks should equal checks_passed + checks_failed"
        );
    }

    #[test]
    fn telemetry_absorb_preserves_counts(
        n_green in 0_u32..5,
        n_yellow in 0_u32..5,
    ) {
        let mut telemetry = ResizeInvariantTelemetry::default();

        for _ in 0..n_green {
            let mut report = ResizeInvariantReport::new();
            check_scheduler_invariants(&mut report, 1, Some(1), Some(1), 0, false);
            telemetry.absorb(&report);
        }

        for _ in 0..n_yellow {
            let mut report = ResizeInvariantReport::new();
            check_scheduler_invariants(&mut report, 1, Some(1), Some(5), 3, true);
            telemetry.absorb(&report);
        }

        prop_assert_eq!(
            telemetry.total_checks,
            telemetry.total_passes + telemetry.total_failures,
            "telemetry total_checks should equal passes + failures"
        );
        prop_assert_eq!(
            telemetry.total_failures,
            telemetry.critical_count + telemetry.error_count + telemetry.warning_count,
            "failure count should equal sum of severity counts"
        );
    }
}

// ---------------------------------------------------------------------------
// Serde roundtrip properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn violation_severity_serde_roundtrip(sev in arb_severity()) {
        let json = serde_json::to_string(&sev).expect("serialize severity");
        let rt: ResizeViolationSeverity = serde_json::from_str(&json).expect("deserialize severity");
        prop_assert_eq!(sev, rt, "severity roundtrip should be stable");
    }

    #[test]
    fn phase_serde_roundtrip(phase in arb_phase()) {
        let json = serde_json::to_string(&phase).expect("serialize phase");
        let rt: ResizePhase = serde_json::from_str(&json).expect("deserialize phase");
        prop_assert_eq!(phase, rt, "phase roundtrip should be stable");
    }

    #[test]
    fn dimension_triple_serde_roundtrip(dims in arb_dimension_triple_consistent()) {
        let json = serde_json::to_string(&dims).expect("serialize dims");
        let rt: DimensionTriple = serde_json::from_str(&json).expect("deserialize dims");
        prop_assert_eq!(dims, rt, "dimension triple roundtrip should be stable");
    }

    #[test]
    fn screen_snapshot_serde_roundtrip(snap in arb_screen_snapshot()) {
        let json = serde_json::to_string(&snap).expect("serialize snapshot");
        let rt: ScreenSnapshot = serde_json::from_str(&json).expect("deserialize snapshot");
        prop_assert_eq!(snap.physical_rows, rt.physical_rows);
        prop_assert_eq!(snap.physical_cols, rt.physical_cols);
        prop_assert_eq!(snap.lines_len, rt.lines_len);
        prop_assert_eq!(snap.cursor_x, rt.cursor_x);
        prop_assert_eq!(snap.cursor_y, rt.cursor_y);
    }

    #[test]
    fn report_serde_roundtrip(
        active in proptest::option::of(1_u64..100),
        latest in proptest::option::of(1_u64..100),
    ) {
        let mut report = ResizeInvariantReport::new();
        check_scheduler_invariants(&mut report, 1, active, latest, 0, false);
        let json = serde_json::to_string(&report).expect("serialize report");
        let rt: ResizeInvariantReport = serde_json::from_str(&json).expect("deserialize report");
        prop_assert_eq!(report.checks_passed, rt.checks_passed);
        prop_assert_eq!(report.checks_failed, rt.checks_failed);
        prop_assert_eq!(report.violations.len(), rt.violations.len());
    }
}

// ---------------------------------------------------------------------------
// Phase FSM graph completeness property
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn all_phases_have_valid_transitions(_dummy in 0..1_u8) {
        let all_phases = [
            ResizePhase::Idle,
            ResizePhase::Queued,
            ResizePhase::Preparing,
            ResizePhase::Reflowing,
            ResizePhase::Presenting,
            ResizePhase::Committed,
            ResizePhase::Cancelled,
            ResizePhase::Failed,
        ];
        for phase in all_phases {
            let transitions = phase.valid_transitions();
            prop_assert!(
                !transitions.is_empty(),
                "{:?} must have at least one valid transition",
                phase
            );
            // No self-transitions
            prop_assert!(
                !transitions.contains(&phase),
                "{:?} must not contain self-transition",
                phase
            );
        }
    }

    #[test]
    fn happy_path_reaches_committed(_dummy in 0..1_u8) {
        let path = [
            ResizePhase::Idle,
            ResizePhase::Queued,
            ResizePhase::Preparing,
            ResizePhase::Reflowing,
            ResizePhase::Presenting,
            ResizePhase::Committed,
            ResizePhase::Idle,
        ];
        let mut report = ResizeInvariantReport::new();
        for pair in path.windows(2) {
            check_phase_transition(&mut report, Some(1), Some(1), pair[0], pair[1]);
        }
        prop_assert!(report.is_clean(),
            "happy path should be entirely valid, violations: {:?}",
            report.violations
        );
        prop_assert_eq!(report.checks_passed, 6, "happy path has 6 transitions");
    }
}
