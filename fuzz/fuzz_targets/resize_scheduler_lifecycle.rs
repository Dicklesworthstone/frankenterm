#![no_main]

use std::collections::HashMap;

use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeExecutionPhase, ResizeIntent, ResizeScheduler, ResizeSchedulerConfig,
    ResizeWorkClass,
};
use libfuzzer_sys::fuzz_target;

fn domain_for(tag: u8, pane_id: u64, seq: u64) -> ResizeDomain {
    match tag % 5 {
        0 => ResizeDomain::Local,
        1 => ResizeDomain::Ssh {
            host: format!("edge-{pane_id}-{seq}.example.com"),
        },
        2 => ResizeDomain::Ssh {
            host: format!("büro-{pane_id}-{seq}-a\u{0301}"),
        },
        3 => ResizeDomain::Mux {
            endpoint: format!("東京/ノード/{pane_id}/{seq}"),
        },
        _ => ResizeDomain::Mux {
            endpoint: format!("emoji-🧪-🌈-{pane_id}-{seq}"),
        },
    }
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

fuzz_target!(|data: &[u8]| {
    if data.len() > 65_536 {
        return;
    }

    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 3,
        input_guardrail_enabled: true,
        input_backlog_threshold: 2,
        input_reserve_units: 1,
        max_deferrals_before_force: 2,
        max_lifecycle_events: 65_536,
        ..ResizeSchedulerConfig::default()
    });

    let mut next_seq_by_pane: HashMap<u64, u64> = HashMap::new();
    let mut now_ms = 1_000u64;

    for chunk in data.chunks(6) {
        let [
            op_tag,
            pane_raw,
            work_raw,
            budget_raw,
            backlog_raw,
            domain_tag,
        ] = match chunk {
            [a, b, c, d, e, f] => [*a, *b, *c, *d, *e, *f],
            _ => break,
        };

        let pane_id = u64::from(pane_raw % 16) + 1;

        match op_tag % 5 {
            0 => {
                let submit_count = usize::from((work_raw % 3) + 1);
                for _ in 0..submit_count {
                    let next_seq = next_seq_by_pane.entry(pane_id).or_insert(1);
                    let seq = *next_seq;
                    let intent = ResizeIntent {
                        pane_id,
                        intent_seq: seq,
                        scheduler_class: if work_raw & 1 == 0 {
                            ResizeWorkClass::Interactive
                        } else {
                            ResizeWorkClass::Background
                        },
                        work_units: u32::from((work_raw % 5) + 1),
                        submitted_at_ms: now_ms,
                        domain: domain_for(domain_tag, pane_id, seq),
                        tab_id: Some(u64::from(domain_tag % 4)),
                    };
                    let _ = scheduler.submit_intent(intent);
                    *next_seq = next_seq.saturating_add(1);
                    now_ms = now_ms.saturating_add(1);
                }
            }
            1 => {
                let budget = u32::from((budget_raw % 6) + 1);
                let backlog = u32::from(backlog_raw % 8);
                let _ = scheduler.schedule_frame_with_input_backlog(budget, backlog);
            }
            2 => advance_active_transactions(&mut scheduler, &mut now_ms),
            3 => {
                if scheduler.active_is_superseded(pane_id) {
                    let _ = scheduler.cancel_active_if_superseded(pane_id);
                }
            }
            _ => {
                let budget = u32::from((budget_raw % 6) + 1);
                let backlog = u32::from(backlog_raw % 8);
                let _ = scheduler.schedule_frame_with_input_backlog(budget, backlog);
                advance_active_transactions(&mut scheduler, &mut now_ms);
            }
        }
    }

    for _ in 0..256 {
        if scheduler.pending_total() == 0 && scheduler.active_total() == 0 {
            break;
        }
        let _ = scheduler.schedule_frame();
        advance_active_transactions(&mut scheduler, &mut now_ms);
    }

    let debug = scheduler.debug_snapshot(65_536);
    assert!(
        debug.invariants.is_clean(),
        "resize invariant violation under fuzz input: {:?}",
        debug.invariants.violations
    );
});
