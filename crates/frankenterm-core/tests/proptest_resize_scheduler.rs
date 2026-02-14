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
    ResizeDomain, ResizeExecutionPhase, ResizeIntent, ResizeLifecycleStage, ResizeScheduler,
    ResizeSchedulerConfig, ResizeSchedulerDebugSnapshot, ResizeWorkClass, SubmitOutcome,
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
