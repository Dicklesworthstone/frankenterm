//! Integration test: VOI scheduler → backpressure → resize scheduler.
//!
//! Exercises the adaptive scheduling pipeline:
//!
//!   VoiScheduler.schedule(now_ms)
//!     → ScheduleResult (panes sorted by information value)
//!       → BackpressureManager.evaluate(depths) → BackpressureTier
//!         → ResizeScheduler.schedule_frame_with_budget(budget) → FrameSchedule
//!
//! The VOI scheduler decides which panes to poll based on entropy and
//! information value. Backpressure tier modulates VOI cost multipliers
//! and resize budgets. The resize scheduler allocates frame budgets
//! constrained by system load.

use frankenterm_core::backpressure::{
    BackpressureConfig, BackpressureManager, BackpressureTier, QueueDepths,
};
use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeIntent, ResizeScheduler, ResizeSchedulerConfig, ResizeWorkClass,
};
use frankenterm_core::voi::{BackpressureTierInput, VoiConfig, VoiScheduler};

// ── Helpers ─────────────────────────────────────────────────────────────

fn make_intent(pane_id: u64, seq: u64, work_units: u32, at_ms: u64) -> ResizeIntent {
    ResizeIntent {
        pane_id,
        intent_seq: seq,
        scheduler_class: ResizeWorkClass::Interactive,
        work_units,
        submitted_at_ms: at_ms,
        domain: ResizeDomain::Local,
        tab_id: Some(1),
    }
}

fn empty_depths() -> QueueDepths {
    QueueDepths {
        capture_depth: 0,
        capture_capacity: 1000,
        write_depth: 0,
        write_capacity: 1000,
    }
}

/// Map BackpressureTier to VoI's BackpressureTierInput.
fn bp_tier_to_voi(tier: BackpressureTier) -> BackpressureTierInput {
    match tier {
        BackpressureTier::Green => BackpressureTierInput::Green,
        BackpressureTier::Yellow => BackpressureTierInput::Yellow,
        BackpressureTier::Red | BackpressureTier::Black => BackpressureTierInput::Red,
    }
}

/// Choose resize frame budget based on backpressure tier.
fn budget_for_tier(tier: BackpressureTier, base_budget: u32) -> u32 {
    match tier {
        BackpressureTier::Green => base_budget,
        BackpressureTier::Yellow => base_budget / 2,
        BackpressureTier::Red => base_budget / 4,
        BackpressureTier::Black => 1,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

/// VOI scheduler orders panes by information value; backpressure
/// increases polling cost, reducing VOI scores and frame budgets.
#[test]
fn voi_ordering_responds_to_backpressure() {
    let mut voi = VoiScheduler::new(VoiConfig::default());

    // Register 3 panes at t=0.
    for pane_id in 1..=3u64 {
        voi.register_pane(pane_id, 0);
    }

    // Pane 1 gets high importance.
    voi.set_importance(1, 5.0);
    // Pane 3 gets low importance.
    voi.set_importance(3, 0.5);

    // Schedule at t=1000 (1 second of entropy drift).
    let result_green = voi.schedule(1000);
    assert_eq!(result_green.schedule.len(), 3);

    // Under green backpressure, pane 1 should have highest VOI.
    assert_eq!(result_green.schedule[0].pane_id, 1);

    // Now set red backpressure — costs increase 5x.
    voi.set_backpressure(BackpressureTierInput::Red);
    let result_red = voi.schedule(1000);

    // VOI scores should all decrease under red (higher cost).
    for (green, red) in result_green.schedule.iter().zip(result_red.schedule.iter()) {
        if green.pane_id == red.pane_id {
            assert!(
                red.effective_cost > green.effective_cost,
                "red effective cost should exceed green for pane {}",
                green.pane_id
            );
        }
    }

    // But relative ordering should be preserved (importance-driven).
    assert_eq!(result_red.schedule[0].pane_id, 1);
}

/// Backpressure tier drives both VOI cost multiplier and resize frame
/// budget, creating coordinated load shedding.
#[test]
fn backpressure_tier_coordinates_voi_and_resize() {
    let bp = BackpressureManager::new(BackpressureConfig::default());
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
    let mut voi = VoiScheduler::new(VoiConfig::default());

    // Register panes in VOI.
    for id in 1..=4u64 {
        voi.register_pane(id, 0);
    }

    // Get initial tier from backpressure (should be Green with empty queues).
    let depths = empty_depths();
    let snap = bp.snapshot(&depths);
    let tier = snap.tier;
    assert_eq!(tier, BackpressureTier::Green);

    // Apply tier to both subsystems.
    voi.set_backpressure(bp_tier_to_voi(tier));
    let base_budget = scheduler.snapshot().config.frame_budget_units;
    let budget = budget_for_tier(tier, base_budget);

    // Submit resize intents.
    for id in 1..=4u64 {
        let intent = make_intent(id, 1, 5, 1000);
        scheduler.submit_intent(intent);
    }

    let frame = scheduler.schedule_frame_with_budget(budget);
    assert_eq!(frame.requested_frame_budget_units, base_budget);

    // VOI schedule should have all panes after 1s drift.
    let voi_result = voi.schedule(1000);
    assert_eq!(voi_result.schedule.len(), 4);
    assert!(voi_result.total_entropy > 0.0);
}

/// Entropy drift makes stale panes more urgent; backpressure modulates
/// the urgency-to-action mapping.
#[test]
fn entropy_drift_and_backpressure_modulate_scheduling() {
    let mut voi = VoiScheduler::new(VoiConfig {
        entropy_drift_rate: 0.5, // Fast drift for testing
        min_voi_threshold: 0.001,
        ..VoiConfig::default()
    });

    voi.register_pane(1, 0);
    voi.register_pane(2, 0);

    // Update pane 1 belief at t=500 (recent observation).
    let active_ll = [0.0, -1.0, -2.0, -3.0, -1.5, -2.5, -3.5];
    voi.update_belief(1, &active_ll, 500);

    // Pane 2 has no observations (maximum staleness).

    // Schedule at t=5000 (5 seconds later).
    voi.apply_drift(5000);
    let result = voi.schedule(5000);

    assert_eq!(result.schedule.len(), 2);

    // Pane 2 should be staler (no observations since registration).
    let pane1 = result.schedule.iter().find(|d| d.pane_id == 1).unwrap();
    let pane2 = result.schedule.iter().find(|d| d.pane_id == 2).unwrap();
    assert!(
        pane2.staleness_ms > pane1.staleness_ms,
        "pane 2 should be staler: {} vs {}",
        pane2.staleness_ms,
        pane1.staleness_ms
    );

    // Suggested intervals should be within configured bounds.
    let interval_1 = voi.suggested_interval_ms(1, 5000);
    let interval_2 = voi.suggested_interval_ms(2, 5000);
    assert!(interval_1 >= 50); // min_poll_interval_ms
    assert!(interval_2 >= 50);
    assert!(interval_1 <= 30_000); // max_poll_interval_ms
    assert!(interval_2 <= 30_000);
}

/// Full pipeline: register panes → accumulate entropy → apply backpressure
/// → schedule VOI → submit resize intents → allocate frame budget.
#[test]
fn full_pipeline_voi_backpressure_resize() {
    let mut voi = VoiScheduler::new(VoiConfig {
        entropy_drift_rate: 0.2,
        min_voi_threshold: 0.001,
        ..VoiConfig::default()
    });
    let bp = BackpressureManager::new(BackpressureConfig::default());
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        storm_threshold_intents: 100,
        ..ResizeSchedulerConfig::default()
    });

    // Phase 1: register 5 panes.
    for id in 1..=5u64 {
        voi.register_pane(id, 0);
    }
    assert_eq!(voi.pane_count(), 5);

    // Phase 2: simulate 3 seconds of operation.
    for tick in 1..=30u64 {
        let now_ms = tick * 100;

        // Apply entropy drift every second.
        if tick % 10 == 0 {
            voi.apply_drift(now_ms);
        }

        // Submit resize intents for some panes.
        if tick % 5 == 0 {
            let intent = make_intent(tick % 5 + 1, tick, 3, now_ms);
            scheduler.submit_intent(intent);
        }
    }

    // Check current backpressure with empty queues.
    let depths = empty_depths();
    let tier = bp.snapshot(&depths).tier;
    voi.set_backpressure(bp_tier_to_voi(tier));

    // Compute VOI schedule.
    let voi_result = voi.schedule(3000);
    assert_eq!(voi_result.schedule.len(), 5);
    assert!(voi_result.total_entropy > 0.0);
    assert!(voi_result.above_threshold > 0);

    // Use backpressure tier to choose resize budget.
    let base_budget = scheduler.snapshot().config.frame_budget_units;
    let budget = budget_for_tier(tier, base_budget);
    let frame = scheduler.schedule_frame_with_budget(budget);
    assert!(frame.requested_frame_budget_units > 0);

    // Phase 3: simulate elevated queue depths → yellow backpressure.
    let heavy_depths = QueueDepths {
        capture_depth: 800,
        capture_capacity: 1000,
        write_depth: 600,
        write_capacity: 1000,
    };
    bp.evaluate(&heavy_depths);
    let yellow_tier = bp.current_tier();
    voi.set_backpressure(bp_tier_to_voi(yellow_tier));

    // Re-schedule with elevated backpressure.
    let voi_yellow = voi.schedule(4000);

    // VOI effective costs should be higher under yellow.
    if yellow_tier != BackpressureTier::Green {
        let green_costs: Vec<f64> = voi_result
            .schedule
            .iter()
            .map(|d| d.effective_cost)
            .collect();
        let yellow_costs: Vec<f64> = voi_yellow
            .schedule
            .iter()
            .map(|d| d.effective_cost)
            .collect();
        let any_increased = green_costs
            .iter()
            .zip(yellow_costs.iter())
            .any(|(g, y)| *y > *g);
        assert!(
            any_increased,
            "some effective costs should increase under backpressure"
        );
    }

    // Reduced frame budget under pressure.
    let yellow_budget = budget_for_tier(yellow_tier, base_budget);
    assert!(yellow_budget <= budget);

    // Verify telemetry coherence.
    let voi_telem = voi.telemetry().snapshot();
    assert_eq!(voi_telem.panes_registered, 5);
    assert!(voi_telem.schedules_computed >= 2);
    assert!(voi_telem.drift_applications >= 3);

    // Snapshot coherence.
    let snap = voi.snapshot(4000);
    assert_eq!(snap.pane_count, 5);
    assert!(snap.total_entropy > 0.0);

    let resize_metrics = scheduler.metrics();
    assert!(resize_metrics.frames >= 1);
}
