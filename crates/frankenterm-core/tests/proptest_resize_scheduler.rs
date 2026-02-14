//! Property-based tests for resize scheduler sequencing invariants.
//!
//! Focus:
//! - randomized submit/schedule/cancel/phase-progression sequences
//! - stale-work cancellation behavior under supersession
//! - deterministic outcomes for identical input traces
//! - Unicode domain identifiers in resize intents

use std::collections::HashMap;

use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeExecutionPhase, ResizeIntent, ResizeLifecycleStage, ResizeScheduler,
    ResizeSchedulerConfig, ResizeSchedulerDebugSnapshot, ResizeWorkClass,
};
use proptest::prelude::*;

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
