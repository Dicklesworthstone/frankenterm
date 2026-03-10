//! Property-based tests for resize scheduler sequencing invariants.
//!
//! Focus:
//! - randomized submit/schedule/cancel/phase-progression sequences
//! - stale-work cancellation behavior under supersession
//! - deterministic outcomes for identical input traces
//! - Unicode domain identifiers in resize intents
//! - config serde roundtrip stability
//! - monotonic submission enforcement
//! - single-flight-per-pane guarantee
//! - budget respect after scheduling
//! - emergency disable suppression
//! - overload admission control
//! - starvation protection convergence
//! - snapshot/metric consistency
//! - lifecycle event ordering
//!
//! Bead: wa-1u90p.7.1

use std::collections::{HashMap, HashSet};

use frankenterm_core::resize_scheduler::{
    ResizeControlPlaneGateState, ResizeDomain, ResizeExecutionPhase, ResizeIntent,
    ResizeLifecycleStage, ResizeOverloadReason, ResizeScheduler, ResizeSchedulerConfig,
    ResizeSchedulerDebugSnapshot, ResizeSchedulerMetrics, ResizeSchedulerPaneSnapshot,
    ResizeSchedulerSnapshot, ResizeStalledTransaction, ResizeWorkClass, ScheduleFrameResult,
    ScheduledResizeWork, SubmitOutcome,
};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StepInput {
    pane_index: u8,
    submit_count: u8,
    interactive: bool,
    work_units: u8,
    budget: u8,
    backlog: u8,
    domain_selector: u8,
}

fn arb_step_input() -> impl Strategy<Value = StepInput> {
    (
        0u8..12,
        0u8..4,
        any::<bool>(),
        1u8..6,
        1u8..6,
        0u8..8,
        0u8..8,
    )
        .prop_map(
            |(
                pane_index,
                submit_count,
                interactive,
                work_units,
                budget,
                backlog,
                domain_selector,
            )| {
                StepInput {
                    pane_index,
                    submit_count,
                    interactive,
                    work_units,
                    budget,
                    backlog,
                    domain_selector,
                }
            },
        )
}

fn arb_steps(max_len: usize) -> impl Strategy<Value = Vec<StepInput>> {
    proptest::collection::vec(arb_step_input(), 1..max_len)
}

fn arb_work_class() -> impl Strategy<Value = ResizeWorkClass> {
    prop_oneof![
        Just(ResizeWorkClass::Interactive),
        Just(ResizeWorkClass::Background),
    ]
}

fn arb_domain() -> impl Strategy<Value = ResizeDomain> {
    prop_oneof![
        Just(ResizeDomain::Local),
        "[a-z]{3,12}".prop_map(|h| ResizeDomain::Ssh { host: h }),
        "[a-z]{3,12}".prop_map(|e| ResizeDomain::Mux { endpoint: e }),
    ]
}

fn domain_for(selector: u8, seq: u64) -> ResizeDomain {
    match selector % 5 {
        0 => ResizeDomain::Local,
        1 => ResizeDomain::Ssh {
            host: format!("edge-{seq}.example.com"),
        },
        2 => ResizeDomain::Ssh {
            host: format!("büro-{seq}-a\u{0301}"),
        },
        3 => ResizeDomain::Mux {
            endpoint: format!("東京/ノード/{seq}"),
        },
        _ => ResizeDomain::Mux {
            endpoint: format!("emoji-🧪-🌈-{seq}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn apply_step(
    scheduler: &mut ResizeScheduler,
    next_seq_by_pane: &mut HashMap<u64, u64>,
    now_ms: &mut u64,
    step: &StepInput,
) {
    let pane_id = u64::from(step.pane_index) + 1;
    let submit_total = usize::from(step.submit_count);

    for _ in 0..submit_total {
        let next_seq = next_seq_by_pane.entry(pane_id).or_insert(1);
        let seq = *next_seq;
        let intent = ResizeIntent {
            pane_id,
            intent_seq: seq,
            scheduler_class: if step.interactive {
                ResizeWorkClass::Interactive
            } else {
                ResizeWorkClass::Background
            },
            work_units: u32::from(step.work_units),
            submitted_at_ms: *now_ms,
            domain: domain_for(step.domain_selector, seq),
            tab_id: None,
        };
        let _ = scheduler.submit_intent(intent);
        *next_seq = next_seq.saturating_add(1);
        *now_ms = now_ms.saturating_add(1);
    }

    let _ = scheduler
        .schedule_frame_with_input_backlog(u32::from(step.budget), u32::from(step.backlog));
    advance_active_transactions(scheduler, now_ms);
}

fn advance_active_transactions(scheduler: &mut ResizeScheduler, now_ms: &mut u64) {
    let panes = scheduler.snapshot().panes;

    for pane in panes {
        let Some(active_seq) = pane.active_seq else {
            continue;
        };

        if scheduler.active_is_superseded(pane.pane_id) {
            let _ = scheduler.cancel_active_if_superseded(pane.pane_id);
            continue;
        }

        match pane.active_phase {
            Some(ResizeExecutionPhase::Preparing) => {
                *now_ms = now_ms.saturating_add(1);
                let _ = scheduler.mark_active_phase(
                    pane.pane_id,
                    active_seq,
                    ResizeExecutionPhase::Reflowing,
                    *now_ms,
                );
            }
            Some(ResizeExecutionPhase::Reflowing) => {
                *now_ms = now_ms.saturating_add(1);
                let _ = scheduler.mark_active_phase(
                    pane.pane_id,
                    active_seq,
                    ResizeExecutionPhase::Presenting,
                    *now_ms,
                );
            }
            Some(ResizeExecutionPhase::Presenting) => {
                let _ = scheduler.complete_active(pane.pane_id, active_seq);
            }
            None => {
                *now_ms = now_ms.saturating_add(1);
                let _ = scheduler.mark_active_phase(
                    pane.pane_id,
                    active_seq,
                    ResizeExecutionPhase::Reflowing,
                    *now_ms,
                );
            }
        }
    }
}

fn drive_to_quiescence(scheduler: &mut ResizeScheduler, now_ms: &mut u64) {
    for _ in 0..256 {
        if scheduler.pending_total() == 0 && scheduler.active_total() == 0 {
            break;
        }
        let _ = scheduler.schedule_frame();
        advance_active_transactions(scheduler, now_ms);
    }
}

fn execute_trace(steps: &[StepInput]) -> ResizeSchedulerDebugSnapshot {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 3,
        input_guardrail_enabled: true,
        input_backlog_threshold: 2,
        input_reserve_units: 1,
        max_deferrals_before_force: 2,
        max_lifecycle_events: 8_192,
        ..ResizeSchedulerConfig::default()
    });
    let mut next_seq_by_pane = HashMap::new();
    let mut now_ms = 1_000u64;

    for step in steps {
        apply_step(&mut scheduler, &mut next_seq_by_pane, &mut now_ms, step);
    }
    drive_to_quiescence(&mut scheduler, &mut now_ms);

    scheduler.debug_snapshot(2_048)
}

fn make_default_scheduler() -> ResizeScheduler {
    ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 3,
        input_guardrail_enabled: true,
        input_backlog_threshold: 2,
        input_reserve_units: 1,
        max_deferrals_before_force: 2,
        max_lifecycle_events: 8_192,
        ..ResizeSchedulerConfig::default()
    })
}

// ===========================================================================
// Original properties (preserved)
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(120))]

    #[test]
    fn prop_randomized_resize_sequences_preserve_invariants(steps in arb_steps(80)) {
        let debug = execute_trace(&steps);
        prop_assert!(
            debug.invariants.is_clean(),
            "resize invariant violations: {:?}",
            debug.invariants.violations
        );
        prop_assert_eq!(debug.invariant_telemetry.critical_count, 0);
        prop_assert_eq!(debug.invariant_telemetry.error_count, 0);

        let mut last_committed_seq: HashMap<u64, u64> = HashMap::new();
        for event in &debug.lifecycle_events {
            if event.stage != ResizeLifecycleStage::Committed {
                continue;
            }

            if let Some(previous) = last_committed_seq.insert(event.pane_id, event.intent_seq) {
                prop_assert!(
                    event.intent_seq > previous,
                    "committed sequence regressed for pane {}: {} -> {}",
                    event.pane_id,
                    previous,
                    event.intent_seq
                );
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn prop_resize_scheduler_execution_is_deterministic(steps in arb_steps(60)) {
        let first = execute_trace(&steps);
        let second = execute_trace(&steps);
        prop_assert_eq!(first, second);
    }
}

// ===========================================================================
// Config serde roundtrip
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_config_serde_roundtrip(
        frame_budget in 1u32..100,
        backlog_threshold in 0u32..20,
        reserve_units in 0u32..20,
        max_deferrals in 1u32..50,
        max_pending in 1usize..500,
        enabled in any::<bool>(),
        emergency in any::<bool>(),
        guardrail in any::<bool>(),
    ) {
        let config = ResizeSchedulerConfig {
            control_plane_enabled: enabled,
            emergency_disable: emergency,
            frame_budget_units: frame_budget,
            input_guardrail_enabled: guardrail,
            input_backlog_threshold: backlog_threshold,
            input_reserve_units: reserve_units,
            max_deferrals_before_force: max_deferrals,
            max_pending_panes: max_pending,
            ..ResizeSchedulerConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: ResizeSchedulerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }
}

// ===========================================================================
// Monotonic submission enforcement
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_non_monotonic_submit_rejected(
        pane_id in 1u64..100,
        first_seq in 5u64..100,
    ) {
        let mut scheduler = make_default_scheduler();
        let intent_first = ResizeIntent {
            pane_id,
            intent_seq: first_seq,
            scheduler_class: ResizeWorkClass::Interactive,
            work_units: 1,
            submitted_at_ms: 1000,
            domain: ResizeDomain::Local,
            tab_id: None,
        };
        let _ = scheduler.submit_intent(intent_first);

        // Submit with a lower sequence number — must be rejected.
        let stale_seq = first_seq.saturating_sub(1).max(1);
        if stale_seq < first_seq {
            let intent_stale = ResizeIntent {
                pane_id,
                intent_seq: stale_seq,
                scheduler_class: ResizeWorkClass::Interactive,
                work_units: 1,
                submitted_at_ms: 1001,
                domain: ResizeDomain::Local,
                tab_id: None,
            };
            let outcome = scheduler.submit_intent(intent_stale);
            prop_assert!(
                matches!(outcome, SubmitOutcome::RejectedNonMonotonic { .. }),
                "stale seq {} after {} should be rejected, got {:?}",
                stale_seq, first_seq, outcome
            );
        }
    }
}

// ===========================================================================
// Single-flight per pane: at most one active per pane
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_at_most_one_active_per_pane(steps in arb_steps(60)) {
        let mut scheduler = make_default_scheduler();
        let mut next_seq_by_pane = HashMap::new();
        let mut now_ms = 1_000u64;

        for step in &steps {
            apply_step(&mut scheduler, &mut next_seq_by_pane, &mut now_ms, step);

            // After each step, verify single-flight invariant.
            let snapshot = scheduler.snapshot();
            let mut active_panes = HashSet::new();
            for pane in &snapshot.panes {
                if pane.active_seq.is_some() {
                    prop_assert!(
                        active_panes.insert(pane.pane_id),
                        "pane {} has duplicate active entries",
                        pane.pane_id
                    );
                }
            }
        }
    }
}

// ===========================================================================
// Snapshot consistency: pending_total and active_total match pane rows
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_snapshot_counts_match_pane_rows(steps in arb_steps(60)) {
        let mut scheduler = make_default_scheduler();
        let mut next_seq_by_pane = HashMap::new();
        let mut now_ms = 1_000u64;

        for step in &steps {
            apply_step(&mut scheduler, &mut next_seq_by_pane, &mut now_ms, step);

            let snapshot = scheduler.snapshot();
            let actual_pending = snapshot.panes.iter()
                .filter(|p| p.pending_seq.is_some())
                .count();
            let actual_active = snapshot.panes.iter()
                .filter(|p| p.active_seq.is_some())
                .count();
            prop_assert_eq!(
                snapshot.pending_total, actual_pending,
                "pending_total mismatch: reported {} but {} pane rows have pending",
                snapshot.pending_total, actual_pending
            );
            prop_assert_eq!(
                snapshot.active_total, actual_active,
                "active_total mismatch: reported {} but {} pane rows have active",
                snapshot.active_total, actual_active
            );
        }
    }
}

// ===========================================================================
// Emergency disable suppresses all submissions
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn prop_emergency_disable_suppresses_submits(
        pane_id in 1u64..100,
        seq in 1u64..100,
        work_class in arb_work_class(),
    ) {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            emergency_disable: true,
            ..ResizeSchedulerConfig::default()
        });
        let intent = ResizeIntent {
            pane_id,
            intent_seq: seq,
            scheduler_class: work_class,
            work_units: 1,
            submitted_at_ms: 1000,
            domain: ResizeDomain::Local,
            tab_id: None,
        };
        let outcome = scheduler.submit_intent(intent);
        prop_assert!(
            matches!(outcome, SubmitOutcome::SuppressedByKillSwitch { .. }),
            "emergency disable should suppress, got {:?}",
            outcome
        );
        prop_assert_eq!(scheduler.pending_total(), 0);
    }
}

// ===========================================================================
// Quiescence: after draining, no pending or active remain
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_quiescence_drains_all_work(steps in arb_steps(40)) {
        let mut scheduler = make_default_scheduler();
        let mut next_seq_by_pane = HashMap::new();
        let mut now_ms = 1_000u64;

        for step in &steps {
            apply_step(&mut scheduler, &mut next_seq_by_pane, &mut now_ms, step);
        }
        drive_to_quiescence(&mut scheduler, &mut now_ms);

        prop_assert_eq!(
            scheduler.pending_total(), 0,
            "after quiescence, pending should be 0"
        );
        prop_assert_eq!(
            scheduler.active_total(), 0,
            "after quiescence, active should be 0"
        );
    }
}

// ===========================================================================
// Lifecycle event_seq is strictly monotonic
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_lifecycle_event_seq_monotonic(steps in arb_steps(60)) {
        let debug = execute_trace(&steps);
        let mut prev_seq = 0u64;
        for event in &debug.lifecycle_events {
            prop_assert!(
                event.event_seq > prev_seq || prev_seq == 0,
                "lifecycle event_seq not monotonic: prev={} current={}",
                prev_seq, event.event_seq
            );
            prev_seq = event.event_seq;
        }
    }
}

// ===========================================================================
// Domain key determinism
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_domain_key_deterministic(domain in arb_domain()) {
        let k1 = domain.key();
        let k2 = domain.key();
        prop_assert_eq!(&k1, &k2);
        prop_assert!(!k1.is_empty(), "domain key should be non-empty");
    }

    #[test]
    fn prop_domain_key_prefix_matches_variant(domain in arb_domain()) {
        let key = domain.key();
        match &domain {
            ResizeDomain::Local => prop_assert_eq!(key, "local"),
            ResizeDomain::Ssh { .. } => prop_assert!(
                key.starts_with("ssh:"),
                "SSH domain key should start with 'ssh:': {}",
                key
            ),
            ResizeDomain::Mux { .. } => prop_assert!(
                key.starts_with("mux:"),
                "Mux domain key should start with 'mux:': {}",
                key
            ),
        }
    }
}

// ===========================================================================
// Work class and domain serde roundtrip
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_work_class_serde_roundtrip(class in arb_work_class()) {
        let json = serde_json::to_string(&class).unwrap();
        let back: ResizeWorkClass = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(class, back);
    }

    #[test]
    fn prop_domain_serde_roundtrip(domain in arb_domain()) {
        let json = serde_json::to_string(&domain).unwrap();
        let back: ResizeDomain = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(domain, back);
    }
}

// ===========================================================================
// Budget respect: scheduled work doesn't wildly exceed budget
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_scheduled_budget_bounded(
        n_panes in 1u64..8,
        work_per in 1u32..4,
        budget in 1u32..10,
    ) {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: budget,
            input_guardrail_enabled: false,
            allow_single_oversubscription: true,
            ..ResizeSchedulerConfig::default()
        });
        // Submit several panes worth of work.
        for p in 1..=n_panes {
            let intent = ResizeIntent {
                pane_id: p,
                intent_seq: 1,
                scheduler_class: ResizeWorkClass::Interactive,
                work_units: work_per,
                submitted_at_ms: 1000,
                domain: ResizeDomain::Local,
                tab_id: None,
            };
            let _ = scheduler.submit_intent(intent);
        }
        let result = scheduler.schedule_frame_with_budget(budget);
        // With oversubscription allowed, first pick may exceed budget but
        // subsequent picks should not push spending past budget + one pick.
        let max_allowed = budget + work_per; // one oversubscription allowance
        prop_assert!(
            result.budget_spent_units <= max_allowed,
            "budget_spent={} exceeded max_allowed={} (budget={}, work_per={})",
            result.budget_spent_units, max_allowed, budget, work_per
        );
    }
}

// ===========================================================================
// No duplicate pane IDs in snapshot
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_no_duplicate_pane_ids_in_snapshot(steps in arb_steps(50)) {
        let debug = execute_trace(&steps);
        let mut seen = HashSet::new();
        for pane in &debug.scheduler.panes {
            prop_assert!(
                seen.insert(pane.pane_id),
                "duplicate pane_id {} in snapshot",
                pane.pane_id
            );
        }
    }
}

// ===========================================================================
// Intent serde roundtrip
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_intent_serde_roundtrip(
        pane_id in 1u64..1000,
        seq in 1u64..1000,
        class in arb_work_class(),
        units in 0u32..100,
        ts in 0u64..u64::MAX / 2,
        domain in arb_domain(),
        tab_id in proptest::option::of(1u64..100),
    ) {
        let intent = ResizeIntent {
            pane_id,
            intent_seq: seq,
            scheduler_class: class,
            work_units: units,
            submitted_at_ms: ts,
            domain,
            tab_id,
        };
        let json = serde_json::to_string(&intent).unwrap();
        let back: ResizeIntent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(intent, back);
    }
}

// ===========================================================================
// Metrics monotonicity: frame counter never decreases
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn prop_metrics_frames_monotonic(steps in arb_steps(40)) {
        let mut scheduler = make_default_scheduler();
        let mut next_seq_by_pane = HashMap::new();
        let mut now_ms = 1_000u64;
        let mut last_frames = 0u64;

        for step in &steps {
            apply_step(&mut scheduler, &mut next_seq_by_pane, &mut now_ms, step);
            let snap = scheduler.snapshot();
            prop_assert!(
                snap.metrics.frames >= last_frames,
                "frame counter decreased: {} -> {}",
                last_frames, snap.metrics.frames
            );
            last_frames = snap.metrics.frames;
        }
    }
}

// ===========================================================================
// Debug snapshot serde roundtrip
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn prop_debug_snapshot_serde_roundtrip(steps in arb_steps(30)) {
        let debug = execute_trace(&steps);
        let json = serde_json::to_string(&debug).unwrap();
        let back: ResizeSchedulerDebugSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(debug, back);
    }
}

// ===========================================================================
// Interactive always scheduled before background (priority ordering)
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_interactive_scheduled_before_background(
        n_bg in 1u64..4,
        n_int in 1u64..4,
    ) {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 1,
            input_guardrail_enabled: false,
            ..ResizeSchedulerConfig::default()
        });
        // Submit background work first
        for p in 1..=n_bg {
            let intent = ResizeIntent {
                pane_id: p,
                intent_seq: 1,
                scheduler_class: ResizeWorkClass::Background,
                work_units: 1,
                submitted_at_ms: 1000,
                domain: ResizeDomain::Local,
                tab_id: None,
            };
            let _ = scheduler.submit_intent(intent);
        }
        // Then submit interactive work
        for p in (n_bg + 1)..=(n_bg + n_int) {
            let intent = ResizeIntent {
                pane_id: p,
                intent_seq: 1,
                scheduler_class: ResizeWorkClass::Interactive,
                work_units: 1,
                submitted_at_ms: 1001,
                domain: ResizeDomain::Local,
                tab_id: None,
            };
            let _ = scheduler.submit_intent(intent);
        }
        // Schedule one frame with budget=1
        let result = scheduler.schedule_frame_with_budget(1);
        if !result.scheduled.is_empty() {
            let snap = scheduler.snapshot();
            let active_pane = snap.panes.iter().find(|p| p.active_seq.is_some());
            if let Some(active) = active_pane {
                prop_assert!(
                    active.pane_id > n_bg,
                    "interactive pane should be picked first, but got pane_id={} (bg range 1..={})",
                    active.pane_id, n_bg
                );
            }
        }
    }
}

// ===========================================================================
// Supersession: newer intent makes active stale
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_supersession_detected(
        pane_id in 1u64..100,
    ) {
        let mut scheduler = make_default_scheduler();
        let intent1 = ResizeIntent {
            pane_id,
            intent_seq: 1,
            scheduler_class: ResizeWorkClass::Interactive,
            work_units: 1,
            submitted_at_ms: 1000,
            domain: ResizeDomain::Local,
            tab_id: None,
        };
        let _ = scheduler.submit_intent(intent1);
        let _ = scheduler.schedule_frame();

        let intent2 = ResizeIntent {
            pane_id,
            intent_seq: 2,
            scheduler_class: ResizeWorkClass::Interactive,
            work_units: 1,
            submitted_at_ms: 1001,
            domain: ResizeDomain::Local,
            tab_id: None,
        };
        let outcome = scheduler.submit_intent(intent2);
        prop_assert!(
            !matches!(outcome, SubmitOutcome::RejectedNonMonotonic { .. }),
            "seq 2 after seq 1 should not be rejected, got {:?}", outcome
        );
        let is_superseded = scheduler.active_is_superseded(pane_id);
        prop_assert!(is_superseded,
            "active intent should be superseded after newer submit");
    }
}

// ===========================================================================
// Overload admission: max_pending_panes cap
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn prop_overload_rejects_beyond_cap(
        cap in 1usize..5,
    ) {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            max_pending_panes: cap,
            ..ResizeSchedulerConfig::default()
        });
        let mut accepted = 0usize;
        let mut rejected = 0usize;
        for p in 1..=(cap as u64 + 2) {
            let intent = ResizeIntent {
                pane_id: p,
                intent_seq: 1,
                scheduler_class: ResizeWorkClass::Background,
                work_units: 1,
                submitted_at_ms: 1000,
                domain: ResizeDomain::Local,
                tab_id: None,
            };
            let outcome = scheduler.submit_intent(intent);
            match outcome {
                SubmitOutcome::Accepted { .. } => accepted += 1,
                SubmitOutcome::DroppedOverload { .. } => rejected += 1,
                _ => {}
            }
        }
        prop_assert!(
            scheduler.pending_total() <= cap,
            "pending {} should not exceed cap {}", scheduler.pending_total(), cap
        );
        prop_assert!(
            rejected > 0,
            "some submits should be rejected when exceeding cap {}; accepted={}, rejected={}",
            cap, accepted, rejected
        );
    }
}

// ===========================================================================
// Lifecycle events: each pane's events start with Queued
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn prop_lifecycle_stage_ordering(steps in arb_steps(40)) {
        let debug = execute_trace(&steps);
        let mut per_intent: HashMap<(u64, u64), Vec<ResizeLifecycleStage>> = HashMap::new();
        for event in &debug.lifecycle_events {
            per_intent
                .entry((event.pane_id, event.intent_seq))
                .or_default()
                .push(event.stage);
        }
        for ((pane_id, intent_seq), stages) in &per_intent {
            if let Some(first) = stages.first() {
                prop_assert_eq!(
                    *first,
                    ResizeLifecycleStage::Queued,
                    "first event for pane {} seq {} should be Queued, got {:?}",
                    pane_id, intent_seq, first
                );
            }
        }
    }
}

// ===========================================================================
// Phase progression: mark_active_phase advances correctly
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_phase_progression(
        pane_id in 1u64..100,
    ) {
        let mut scheduler = make_default_scheduler();
        let intent = ResizeIntent {
            pane_id,
            intent_seq: 1,
            scheduler_class: ResizeWorkClass::Interactive,
            work_units: 1,
            submitted_at_ms: 1000,
            domain: ResizeDomain::Local,
            tab_id: None,
        };
        let _ = scheduler.submit_intent(intent);
        let _ = scheduler.schedule_frame();

        let snap = scheduler.snapshot();
        let pane = snap.panes.iter().find(|p| p.pane_id == pane_id);
        if let Some(p) = pane {
            if p.active_seq.is_some() {
                let ok = scheduler.mark_active_phase(
                    pane_id, 1, ResizeExecutionPhase::Reflowing, 1001);
                prop_assert!(ok, "mark Reflowing should succeed");

                let ok = scheduler.mark_active_phase(
                    pane_id, 1, ResizeExecutionPhase::Presenting, 1002);
                prop_assert!(ok, "mark Presenting should succeed");

                let ok = scheduler.complete_active(pane_id, 1);
                prop_assert!(ok, "complete should succeed");

                let snap2 = scheduler.snapshot();
                let pane2 = snap2.panes.iter().find(|p| p.pane_id == pane_id);
                if let Some(p2) = pane2 {
                    prop_assert!(p2.active_seq.is_none(),
                        "after complete, active_seq should be None");
                }
            }
        }
    }
}

// ===========================================================================
// Deferral aging: background work eventually gets scheduled
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn prop_background_eventually_scheduled(
        max_deferrals in 1u32..5,
    ) {
        let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
            frame_budget_units: 0,
            max_deferrals_before_force: max_deferrals,
            input_guardrail_enabled: false,
            ..ResizeSchedulerConfig::default()
        });
        let intent = ResizeIntent {
            pane_id: 1,
            intent_seq: 1,
            scheduler_class: ResizeWorkClass::Background,
            work_units: 1,
            submitted_at_ms: 1000,
            domain: ResizeDomain::Local,
            tab_id: None,
        };
        let _ = scheduler.submit_intent(intent);

        let mut was_scheduled = false;
        for _ in 0..(max_deferrals as usize + 10) {
            let _ = scheduler.schedule_frame();
            if scheduler.active_total() > 0 {
                was_scheduled = true;
                break;
            }
        }
        prop_assert!(was_scheduled,
            "background work should eventually be force-scheduled after {} deferrals",
            max_deferrals);
    }
}

// ===========================================================================
// Structural and trait tests
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// ResizeSchedulerConfig implements Clone correctly.
    #[test]
    fn prop_config_clone(
        frame_budget in 1u32..100,
        enabled in any::<bool>(),
        emergency in any::<bool>(),
    ) {
        let config = ResizeSchedulerConfig {
            frame_budget_units: frame_budget,
            control_plane_enabled: enabled,
            emergency_disable: emergency,
            ..ResizeSchedulerConfig::default()
        };
        let cloned = config.clone();
        prop_assert_eq!(config, cloned, "Config Clone should produce equal value");
    }

    /// ResizeSchedulerConfig Debug output is nonempty.
    #[test]
    fn prop_config_debug_nonempty(
        frame_budget in 1u32..100,
    ) {
        let config = ResizeSchedulerConfig {
            frame_budget_units: frame_budget,
            ..ResizeSchedulerConfig::default()
        };
        let debug = format!("{:?}", config);
        prop_assert!(!debug.is_empty(), "Config Debug should not be empty");
    }

    /// Config serde is deterministic — serialize twice yields same JSON.
    #[test]
    fn prop_config_serde_deterministic(
        frame_budget in 1u32..100,
        enabled in any::<bool>(),
    ) {
        let config = ResizeSchedulerConfig {
            frame_budget_units: frame_budget,
            control_plane_enabled: enabled,
            ..ResizeSchedulerConfig::default()
        };
        let json1 = serde_json::to_string(&config).unwrap();
        let json2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&json1, &json2, "Config serialization not deterministic");
    }

    /// ResizeIntent Debug output is nonempty.
    #[test]
    fn prop_intent_debug_nonempty(
        pane_id in 1u64..100,
        seq in 1u64..100,
    ) {
        let intent = ResizeIntent {
            pane_id,
            intent_seq: seq,
            scheduler_class: ResizeWorkClass::Interactive,
            work_units: 1,
            submitted_at_ms: 1000,
            domain: ResizeDomain::Local,
            tab_id: None,
        };
        let debug = format!("{:?}", intent);
        prop_assert!(!debug.is_empty(), "Intent Debug should not be empty");
    }

    /// ResizeDomain implements Clone correctly.
    #[test]
    fn prop_domain_clone(domain in arb_domain()) {
        let cloned = domain.clone();
        prop_assert_eq!(domain.key(), cloned.key(), "Domain Clone should produce same key");
    }

    /// ResizeWorkClass implements Clone correctly.
    #[test]
    fn prop_work_class_clone(class in arb_work_class()) {
        let cloned = class;
        prop_assert_eq!(class, cloned, "WorkClass Copy should produce equal value");
    }
}

// ---------------------------------------------------------------------------
// Strategies: additional resize_scheduler types
// ---------------------------------------------------------------------------

fn arb_overload_reason() -> impl Strategy<Value = ResizeOverloadReason> {
    prop_oneof![
        Just(ResizeOverloadReason::QueueCapacity),
        Just(ResizeOverloadReason::DeferralTimeout),
    ]
}

fn arb_lifecycle_stage() -> impl Strategy<Value = ResizeLifecycleStage> {
    prop_oneof![
        Just(ResizeLifecycleStage::Queued),
        Just(ResizeLifecycleStage::Scheduled),
        Just(ResizeLifecycleStage::Preparing),
        Just(ResizeLifecycleStage::Reflowing),
        Just(ResizeLifecycleStage::Presenting),
        Just(ResizeLifecycleStage::Cancelled),
        Just(ResizeLifecycleStage::Committed),
    ]
}

fn arb_execution_phase() -> impl Strategy<Value = ResizeExecutionPhase> {
    prop_oneof![
        Just(ResizeExecutionPhase::Preparing),
        Just(ResizeExecutionPhase::Reflowing),
        Just(ResizeExecutionPhase::Presenting),
    ]
}

fn arb_resize_intent() -> impl Strategy<Value = ResizeIntent> {
    (
        0u64..10_000,
        0u64..100_000,
        arb_work_class(),
        1u32..100,
        0u64..10_000_000,
        arb_domain(),
        proptest::option::of(0u64..100),
    )
        .prop_map(
            |(
                pane_id,
                intent_seq,
                scheduler_class,
                work_units,
                submitted_at_ms,
                domain,
                tab_id,
            )| {
                ResizeIntent {
                    pane_id,
                    intent_seq,
                    scheduler_class,
                    work_units,
                    submitted_at_ms,
                    domain,
                    tab_id,
                }
            },
        )
}

fn arb_scheduler_config() -> impl Strategy<Value = ResizeSchedulerConfig> {
    (
        proptest::bool::ANY,
        proptest::bool::ANY,
        proptest::bool::ANY,
        1u32..100,
        proptest::bool::ANY,
        0u32..20,
        0u32..50,
    )
        .prop_map(|(cp, ed, lf, fbu, ig, ibt, ibu)| {
            let mut cfg = ResizeSchedulerConfig::default();
            cfg.control_plane_enabled = cp;
            cfg.emergency_disable = ed;
            cfg.legacy_fallback_enabled = lf;
            cfg.frame_budget_units = fbu;
            cfg.input_guardrail_enabled = ig;
            cfg.input_backlog_threshold = ibt;
            cfg.input_reserve_units = ibu;
            cfg
        })
}

fn arb_scheduled_resize_work() -> impl Strategy<Value = ScheduledResizeWork> {
    (
        0u64..10_000,
        0u64..100_000,
        arb_work_class(),
        1u32..100,
        proptest::bool::ANY,
        proptest::bool::ANY,
    )
        .prop_map(
            |(pane_id, intent_seq, scheduler_class, work_units, over_budget, forced)| {
                ScheduledResizeWork {
                    pane_id,
                    intent_seq,
                    scheduler_class,
                    work_units,
                    over_budget,
                    forced_by_starvation: forced,
                }
            },
        )
}

fn arb_resize_metrics() -> impl Strategy<Value = ResizeSchedulerMetrics> {
    (
        0u64..100_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
        0u64..10_000,
    )
        .prop_map(
            |(frames, superseded, rejected, forced, over, overload_rej, overload_ev)| {
                let mut m = ResizeSchedulerMetrics::default();
                m.frames = frames;
                m.superseded_intents = superseded;
                m.rejected_non_monotonic = rejected;
                m.forced_background_runs = forced;
                m.over_budget_runs = over;
                m.overload_rejected = overload_rej;
                m.overload_evicted = overload_ev;
                m
            },
        )
}

fn arb_gate_state() -> impl Strategy<Value = ResizeControlPlaneGateState> {
    (
        proptest::bool::ANY,
        proptest::bool::ANY,
        proptest::bool::ANY,
    )
        .prop_map(|(cp, ed, lf)| ResizeControlPlaneGateState {
            control_plane_enabled: cp,
            emergency_disable: ed,
            legacy_fallback_enabled: lf,
            active: cp && !ed,
        })
}

fn arb_stalled_tx() -> impl Strategy<Value = ResizeStalledTransaction> {
    (
        0u64..10_000,
        0u64..100_000,
        proptest::option::of(arb_execution_phase()),
        0u64..60_000,
        proptest::option::of(0u64..100_000),
    )
        .prop_map(|(pane_id, intent_seq, active_phase, age_ms, latest_seq)| {
            ResizeStalledTransaction {
                pane_id,
                intent_seq,
                active_phase,
                age_ms,
                latest_seq,
            }
        })
}

// ---------------------------------------------------------------------------
// Enum serde roundtrips
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// ResizeOverloadReason serde roundtrip.
    #[test]
    fn prop_overload_reason_serde(reason in arb_overload_reason()) {
        let json = serde_json::to_string(&reason).unwrap();
        let back: ResizeOverloadReason = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, reason);
    }

    /// ResizeOverloadReason serializes to snake_case.
    #[test]
    fn prop_overload_reason_snake_case(reason in arb_overload_reason()) {
        let json = serde_json::to_string(&reason).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized overload reason should be snake_case, got '{}'", inner
        );
    }

    /// ResizeLifecycleStage serde roundtrip.
    #[test]
    fn prop_lifecycle_stage_serde(stage in arb_lifecycle_stage()) {
        let json = serde_json::to_string(&stage).unwrap();
        let back: ResizeLifecycleStage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, stage);
    }

    /// ResizeLifecycleStage serializes to snake_case.
    #[test]
    fn prop_lifecycle_stage_snake_case(stage in arb_lifecycle_stage()) {
        let json = serde_json::to_string(&stage).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized lifecycle stage should be snake_case, got '{}'", inner
        );
    }

    /// ResizeExecutionPhase serde roundtrip.
    #[test]
    fn prop_execution_phase_serde(phase in arb_execution_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let back: ResizeExecutionPhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, phase);
    }

    /// ResizeExecutionPhase serializes to snake_case.
    #[test]
    fn prop_execution_phase_snake_case(phase in arb_execution_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized execution phase should be snake_case, got '{}'", inner
        );
    }
}

// ---------------------------------------------------------------------------
// Struct serde roundtrips
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ResizeIntent serde roundtrip.
    #[test]
    fn prop_resize_intent_serde(intent in arb_resize_intent()) {
        let json = serde_json::to_string(&intent).unwrap();
        let back: ResizeIntent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, intent.pane_id);
        prop_assert_eq!(back.intent_seq, intent.intent_seq);
        prop_assert_eq!(back.scheduler_class, intent.scheduler_class);
        prop_assert_eq!(back.work_units, intent.work_units);
        prop_assert_eq!(back.submitted_at_ms, intent.submitted_at_ms);
    }

    /// ResizeSchedulerConfig serde roundtrip.
    #[test]
    fn prop_scheduler_config_serde(config in arb_scheduler_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ResizeSchedulerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.control_plane_enabled, config.control_plane_enabled);
        prop_assert_eq!(back.emergency_disable, config.emergency_disable);
        prop_assert_eq!(back.legacy_fallback_enabled, config.legacy_fallback_enabled);
        prop_assert_eq!(back.frame_budget_units, config.frame_budget_units);
    }

    /// ScheduledResizeWork serde roundtrip.
    #[test]
    fn prop_scheduled_work_serde(work in arb_scheduled_resize_work()) {
        let json = serde_json::to_string(&work).unwrap();
        let back: ScheduledResizeWork = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, work);
    }

    /// ResizeSchedulerMetrics serde roundtrip.
    #[test]
    fn prop_scheduler_metrics_serde(metrics in arb_resize_metrics()) {
        let json = serde_json::to_string(&metrics).unwrap();
        let back: ResizeSchedulerMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.frames, metrics.frames);
        prop_assert_eq!(back.superseded_intents, metrics.superseded_intents);
        prop_assert_eq!(back.rejected_non_monotonic, metrics.rejected_non_monotonic);
        prop_assert_eq!(back.forced_background_runs, metrics.forced_background_runs);
        prop_assert_eq!(back.over_budget_runs, metrics.over_budget_runs);
    }

    /// ScheduleFrameResult default roundtrip.
    #[test]
    fn prop_frame_result_default_serde(_dummy in 0..1_u8) {
        let result = ScheduleFrameResult::default();
        let json = serde_json::to_string(&result).unwrap();
        let back: ScheduleFrameResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, result);
    }

    /// ResizeControlPlaneGateState serde roundtrip.
    #[test]
    fn prop_gate_state_serde(gate in arb_gate_state()) {
        let json = serde_json::to_string(&gate).unwrap();
        let back: ResizeControlPlaneGateState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, gate);
    }

    /// ResizeControlPlaneGateState active field consistency.
    #[test]
    fn prop_gate_state_active_consistency(gate in arb_gate_state()) {
        let expected = gate.control_plane_enabled && !gate.emergency_disable;
        prop_assert_eq!(gate.active, expected,
            "active should be control_plane_enabled && !emergency_disable");
    }

    /// ResizeStalledTransaction serde roundtrip.
    #[test]
    fn prop_stalled_tx_serde(tx in arb_stalled_tx()) {
        let json = serde_json::to_string(&tx).unwrap();
        let back: ResizeStalledTransaction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, tx.pane_id);
        prop_assert_eq!(back.intent_seq, tx.intent_seq);
        prop_assert_eq!(back.active_phase, tx.active_phase);
        prop_assert_eq!(back.age_ms, tx.age_ms);
        prop_assert_eq!(back.latest_seq, tx.latest_seq);
    }

    /// ResizeSchedulerMetrics default is all zeros.
    #[test]
    fn prop_metrics_default_zeros(_dummy in 0..1_u8) {
        let m = ResizeSchedulerMetrics::default();
        prop_assert_eq!(m.frames, 0);
        prop_assert_eq!(m.superseded_intents, 0);
        prop_assert_eq!(m.rejected_non_monotonic, 0);
        prop_assert_eq!(m.forced_background_runs, 0);
        prop_assert_eq!(m.over_budget_runs, 0);
        prop_assert_eq!(m.overload_rejected, 0);
        prop_assert_eq!(m.overload_evicted, 0);
    }
}

// =============================================================================
// SubmitOutcome serde roundtrips
// =============================================================================

fn arb_submit_outcome() -> impl Strategy<Value = SubmitOutcome> {
    prop_oneof![
        proptest::option::of(0_u64..10_000).prop_map(|replaced| SubmitOutcome::Accepted {
            replaced_pending_seq: replaced
        }),
        (0_u64..10_000).prop_map(|latest_seq| SubmitOutcome::RejectedNonMonotonic { latest_seq }),
        (
            0_usize..100,
            proptest::option::of((0_u64..10_000, 0_u64..10_000))
        )
            .prop_map(|(pending_total, evicted)| SubmitOutcome::DroppedOverload {
                pending_total,
                evicted_pending: evicted,
            }),
        any::<bool>().prop_map(|fb| SubmitOutcome::SuppressedByKillSwitch {
            legacy_fallback: fb
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_submit_outcome_serde(outcome in arb_submit_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: SubmitOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, outcome);
    }

    #[test]
    fn prop_submit_outcome_tagged(outcome in arb_submit_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        prop_assert!(json.contains("\"status\""), "tagged enum should contain status key: {}", json);
    }
}

// =============================================================================
// ResizeSchedulerPaneSnapshot serde roundtrip
// =============================================================================

fn arb_pane_snapshot() -> impl Strategy<Value = ResizeSchedulerPaneSnapshot> {
    (
        0_u64..10_000,
        proptest::option::of(0_u64..10_000),
        proptest::option::of(0_u64..10_000),
        proptest::option::of(arb_work_class()),
        proptest::option::of(0_u64..10_000),
        proptest::option::of(arb_execution_phase()),
        proptest::option::of(0_u64..2_000_000_000_000),
        0_u32..100,
        0_u32..100,
    )
        .prop_map(
            |(pane_id, latest, pending, p_class, active, a_phase, a_started, deferrals, aging)| {
                ResizeSchedulerPaneSnapshot {
                    pane_id,
                    latest_seq: latest,
                    pending_seq: pending,
                    pending_class: p_class,
                    active_seq: active,
                    active_phase: a_phase,
                    active_phase_started_at_ms: a_started,
                    deferrals,
                    aging_credit: aging,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_pane_snapshot_serde(snap in arb_pane_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: ResizeSchedulerPaneSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, snap);
    }
}

// =============================================================================
// ResizeSchedulerSnapshot serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_scheduler_snapshot_serde(
        config in arb_scheduler_config(),
        metrics in arb_resize_metrics(),
        pending in 0_usize..50,
        active in 0_usize..10,
        panes in proptest::collection::vec(arb_pane_snapshot(), 0..3),
    ) {
        let snap = ResizeSchedulerSnapshot {
            config,
            metrics,
            pending_total: pending,
            active_total: active,
            panes,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ResizeSchedulerSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, snap);
    }

    #[test]
    fn prop_scheduler_snapshot_json_keys(
        config in arb_scheduler_config(),
        metrics in arb_resize_metrics(),
    ) {
        let snap = ResizeSchedulerSnapshot {
            config,
            metrics,
            pending_total: 5,
            active_total: 2,
            panes: vec![],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = val.as_object().unwrap();
        prop_assert!(obj.contains_key("config"));
        prop_assert!(obj.contains_key("metrics"));
        prop_assert!(obj.contains_key("panes"));
    }
}
