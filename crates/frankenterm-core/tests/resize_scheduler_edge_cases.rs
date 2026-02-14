//! Edge-case and boundary-condition tests for ResizeScheduler.
//!
//! Covers: zero work-units normalization, multi-pane independent sequencing,
//! completion rejection, phase marking, metrics accumulation, cancellation,
//! gate state, pending/active totals, snapshot state, lifecycle event limits,
//! aging credit, overload policy, domain keys, debug snapshot, and last-frame
//! metrics.

use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeExecutionPhase, ResizeIntent, ResizeScheduler, ResizeSchedulerConfig,
    ResizeWorkClass, SubmitOutcome,
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

// ── zero work-units normalization ──────────────────────────

#[test]
fn zero_work_units_normalized_to_one() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 5,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 0, 100));
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 1);
    assert_eq!(
        frame.scheduled[0].work_units, 1,
        "zero work_units should be normalized to 1"
    );
    assert!(scheduler.complete_active(1, 1));
}

// ── multi-pane independent sequencing ──────────────────────

#[test]
fn separate_panes_have_independent_sequence_spaces() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

    let o1 = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
    let o2 = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Interactive, 1, 100));
    assert!(matches!(o1, SubmitOutcome::Accepted { .. }));
    assert!(matches!(o2, SubmitOutcome::Accepted { .. }));

    let o3 = scheduler.submit_intent(intent(1, 5, ResizeWorkClass::Interactive, 1, 101));
    assert!(matches!(
        o3,
        SubmitOutcome::Accepted {
            replaced_pending_seq: Some(1)
        }
    ));

    let o4 = scheduler.submit_intent(intent(2, 2, ResizeWorkClass::Interactive, 1, 102));
    assert!(matches!(
        o4,
        SubmitOutcome::Accepted {
            replaced_pending_seq: Some(1)
        }
    ));
}

// ── complete_active rejection cases ────────────────────────

#[test]
fn complete_active_wrong_seq_is_rejected() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
    let _ = scheduler.schedule_frame();

    assert!(!scheduler.complete_active(1, 99));
    assert_eq!(scheduler.metrics().completion_rejected, 1);
    assert!(scheduler.complete_active(1, 1));
}

#[test]
fn complete_active_for_nonexistent_pane_is_rejected() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
    assert!(!scheduler.complete_active(999, 1));
}

// ── mark_active_phase with stale seq ───────────────────────

#[test]
fn mark_active_phase_with_stale_seq_returns_false() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
    let _ = scheduler.schedule_frame();

    assert!(!scheduler.mark_active_phase(1, 99, ResizeExecutionPhase::Preparing, 101));
    assert!(scheduler.mark_active_phase(1, 1, ResizeExecutionPhase::Preparing, 101));
}

// ── metrics accumulation ───────────────────────────────────

#[test]
fn metrics_track_frames_and_completions_correctly() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 10,
        ..ResizeSchedulerConfig::default()
    });

    for seq in 1..=3u64 {
        let _ = scheduler.submit_intent(intent(1, seq, ResizeWorkClass::Interactive, 1, 100 + seq));
        let _ = scheduler.schedule_frame();
        assert!(scheduler.complete_active(1, seq));
    }

    assert_eq!(scheduler.metrics().frames, 3);
    assert_eq!(scheduler.metrics().completed_active, 3);
    assert_eq!(scheduler.metrics().cancelled_active, 0);
}

// ── cancel_active_if_superseded ────────────────────────────

#[test]
fn cancel_active_if_superseded_with_no_pending_returns_false() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
    let _ = scheduler.schedule_frame();

    assert!(!scheduler.cancel_active_if_superseded(1));
}

#[test]
fn cancel_active_if_superseded_with_pending_returns_true() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
    let _ = scheduler.schedule_frame();
    let _ = scheduler.submit_intent(intent(1, 2, ResizeWorkClass::Interactive, 1, 101));

    assert!(scheduler.active_is_superseded(1));
    assert!(scheduler.cancel_active_if_superseded(1));
    assert_eq!(scheduler.metrics().cancelled_active, 1);
}

// ── gate state and control plane ───────────────────────────

#[test]
fn gate_state_reflects_combined_flags() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

    let state = scheduler.gate_state();
    assert!(state.control_plane_enabled);
    assert!(!state.emergency_disable);
    assert!(state.active);

    scheduler.set_emergency_disable(true);
    let state = scheduler.gate_state();
    assert!(state.control_plane_enabled);
    assert!(state.emergency_disable);
    assert!(!state.active);

    scheduler.set_emergency_disable(false);
    scheduler.set_control_plane_enabled(false);
    let state = scheduler.gate_state();
    assert!(!state.control_plane_enabled);
    assert!(!state.active);
}

// ── schedule_frame with no pending ─────────────────────────

#[test]
fn schedule_frame_with_no_pending_returns_empty_result() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

    let frame = scheduler.schedule_frame();
    assert!(frame.scheduled.is_empty());
    assert_eq!(frame.budget_spent_units, 0);
    assert_eq!(frame.pending_after, 0);
}

// ── pending_total and active_total lifecycle ────────────────

#[test]
fn pending_and_active_totals_update_through_lifecycle() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 10,
        ..ResizeSchedulerConfig::default()
    });

    assert_eq!(scheduler.pending_total(), 0);
    assert_eq!(scheduler.active_total(), 0);

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
    assert_eq!(scheduler.pending_total(), 1);
    assert_eq!(scheduler.active_total(), 0);

    let _ = scheduler.schedule_frame();
    assert_eq!(scheduler.pending_total(), 0);
    assert_eq!(scheduler.active_total(), 1);

    assert!(scheduler.complete_active(1, 1));
    assert_eq!(scheduler.pending_total(), 0);
    assert_eq!(scheduler.active_total(), 0);
}

// ── snapshot includes all pane state ───────────────────────

#[test]
fn snapshot_includes_all_pane_state() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 10,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 2, 100));
    let _ = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Background, 3, 100));
    let _ = scheduler.schedule_frame();

    let snapshot = scheduler.snapshot();
    assert_eq!(snapshot.panes.len(), 2);

    let pane1 = snapshot.panes.iter().find(|p| p.pane_id == 1).unwrap();
    assert_eq!(pane1.active_seq, Some(1));
}

// ── lifecycle_events limit ─────────────────────────────────

#[test]
fn lifecycle_events_limit_returns_at_most_requested() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

    for seq in 1..=10u64 {
        let _ = scheduler.submit_intent(intent(1, seq, ResizeWorkClass::Interactive, 1, 100 + seq));
    }

    let events_5 = scheduler.lifecycle_events(5);
    assert!(events_5.len() <= 5);

    let events_100 = scheduler.lifecycle_events(100);
    assert!(events_100.len() >= 10);
}

// ── aging credit and priority scoring ──────────────────────

#[test]
fn aging_credit_promotes_deferred_pane_to_next_frame() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 1,
        aging_credit_per_frame: 10,
        max_aging_credit: 50,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
    let _ = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Interactive, 1, 100));

    let frame1 = scheduler.schedule_frame();
    assert_eq!(frame1.scheduled.len(), 1);
    let picked = frame1.scheduled[0].pane_id;
    assert!(scheduler.complete_active(picked, 1));

    let frame2 = scheduler.schedule_frame();
    assert_eq!(frame2.scheduled.len(), 1);
    assert_ne!(
        frame2.scheduled[0].pane_id, picked,
        "deferred pane should be picked after aging"
    );
}

// ── overload policy ────────────────────────────────────────

#[test]
fn overload_rejects_when_max_pending_reached() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 10,
        max_pending_panes: 2,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Background, 1, 100));
    let _ = scheduler.submit_intent(intent(2, 1, ResizeWorkClass::Background, 1, 100));

    let outcome = scheduler.submit_intent(intent(3, 1, ResizeWorkClass::Background, 1, 100));
    assert!(
        matches!(outcome, SubmitOutcome::DroppedOverload { .. }),
        "should reject when max_pending_panes reached, got {outcome:?}",
    );
}

// ── domain key formatting ──────────────────────────────────

#[test]
fn domain_keys_are_distinct_and_well_formed() {
    let local = ResizeDomain::Local;
    assert_eq!(local.key(), "local");

    let ssh = ResizeDomain::Ssh {
        host: "dev.example.com".to_string(),
    };
    assert_eq!(ssh.key(), "ssh:dev.example.com");

    let mux = ResizeDomain::Mux {
        endpoint: "ws://mux:8080".to_string(),
    };
    assert_eq!(mux.key(), "mux:ws://mux:8080");

    assert_ne!(local.key(), ssh.key());
    assert_ne!(ssh.key(), mux.key());
}

// ── debug snapshot ─────────────────────────────────────────

#[test]
fn debug_snapshot_lifecycle_limit_zero_means_all_events() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));

    // limit=0 means "return all events" per the implementation.
    let debug = scheduler.debug_snapshot(0);
    assert!(
        !debug.lifecycle_events.is_empty(),
        "limit=0 should return all events, not zero"
    );
    assert!(!debug.scheduler.panes.is_empty());

    // Contrast with limit=1 which returns at most 1.
    let debug1 = scheduler.debug_snapshot(1);
    assert!(debug1.lifecycle_events.len() <= 1);
}

// ── last_frame metrics ─────────────────────────────────────

#[test]
fn last_frame_metrics_update_after_schedule() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 10,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 3, 100));
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 1);

    assert_eq!(scheduler.metrics().last_frame_budget_units, 10);
    assert_eq!(scheduler.metrics().last_frame_scheduled, 1);
    assert_eq!(scheduler.metrics().last_frame_spent_units, 3);
}

// ── disabled scheduler returns empty frames ────────────────

#[test]
fn disabled_scheduler_suppresses_all_operations() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
    scheduler.set_emergency_disable(true);

    let outcome = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 1, 100));
    // When disabled, submission is suppressed.
    assert!(
        matches!(outcome, SubmitOutcome::SuppressedByKillSwitch { .. }),
        "disabled scheduler should suppress submissions, got {outcome:?}"
    );

    let frame = scheduler.schedule_frame();
    assert!(frame.scheduled.is_empty());
    assert_eq!(
        scheduler.metrics().suppressed_frames,
        1,
        "disabled scheduler should increment suppressed_frames"
    );
}

// ── phase transitions produce lifecycle events ─────────────

#[test]
fn full_phase_progression_recorded_in_lifecycle() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 10,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(1, 1, ResizeWorkClass::Interactive, 2, 100));
    let _ = scheduler.schedule_frame();

    assert!(scheduler.mark_active_phase(1, 1, ResizeExecutionPhase::Preparing, 101));
    assert!(scheduler.mark_active_phase(1, 1, ResizeExecutionPhase::Reflowing, 102));
    assert!(scheduler.mark_active_phase(1, 1, ResizeExecutionPhase::Presenting, 103));
    assert!(scheduler.complete_active(1, 1));

    let events = scheduler.lifecycle_events(50);
    // Should have: submit, schedule, 3 phases, complete = at least 6 events.
    assert!(
        events.len() >= 6,
        "expected at least 6 lifecycle events, got {}",
        events.len()
    );
}

// ── many panes stress test ─────────────────────────────────

#[test]
fn many_panes_schedule_without_panic() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 50,
        max_pending_panes: 200,
        ..ResizeSchedulerConfig::default()
    });

    for pane_id in 1..=100u64 {
        let _ = scheduler.submit_intent(intent(
            pane_id,
            1,
            if pane_id % 3 == 0 {
                ResizeWorkClass::Background
            } else {
                ResizeWorkClass::Interactive
            },
            1,
            100,
        ));
    }

    assert_eq!(scheduler.pending_total(), 100);

    let frame = scheduler.schedule_frame();
    assert!(!frame.scheduled.is_empty());
    assert!(
        frame.budget_spent_units <= 50,
        "should not exceed frame budget of 50"
    );

    for work in &frame.scheduled {
        assert!(scheduler.complete_active(work.pane_id, work.intent_seq));
    }
}

// ── rapid supersession coalesces correctly ──────────────────

#[test]
fn rapid_supersession_coalesces_to_latest() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

    for seq in 1..=20u64 {
        let _ = scheduler.submit_intent(intent(1, seq, ResizeWorkClass::Interactive, 1, 100 + seq));
    }

    // Only one pending intent should exist (the latest).
    assert_eq!(scheduler.pending_total(), 1);
    assert_eq!(scheduler.metrics().superseded_intents, 19);

    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 1);
    assert_eq!(frame.scheduled[0].intent_seq, 20);
}
