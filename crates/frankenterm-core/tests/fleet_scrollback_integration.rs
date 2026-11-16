//! Integration tests: FleetMemoryController + TieredScrollback interaction.
//!
//! Validates that fleet pressure evaluation produces correct actions for
//! realistic 200+ pane swarm scenarios, and that TieredScrollback responds
//! correctly to those actions (eviction, enforce_warm_cap, etc.).
//!
//! Bead: ft-1memj.19

use frankenterm_core::backpressure::BackpressureTier;
use frankenterm_core::fleet_memory_controller::{
    FleetMemoryAction, FleetMemoryConfig, FleetMemoryController, FleetPressureTier,
    PaneScrollbackInfo, PressureSignals,
};
use frankenterm_core::fleet_scrollback_coordinator::{
    CoordinatorConfig, FleetScrollbackCoordinator, SnapshotPaneScrollbackAccess,
};
use frankenterm_core::memory_budget::BudgetLevel;
use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::scrollback_tiers::{ScrollbackConfig, TieredScrollback};

fn normal_signals(pane_count: usize) -> PressureSignals {
    PressureSignals {
        backpressure: BackpressureTier::Green,
        memory_pressure: MemoryPressureTier::Green,
        worst_budget: BudgetLevel::Normal,
        pane_count,
        paused_pane_count: 0,
    }
}

fn elevated_signals(pane_count: usize) -> PressureSignals {
    PressureSignals {
        backpressure: BackpressureTier::Yellow,
        memory_pressure: MemoryPressureTier::Green,
        worst_budget: BudgetLevel::Normal,
        pane_count,
        paused_pane_count: 0,
    }
}

fn critical_signals(pane_count: usize) -> PressureSignals {
    PressureSignals {
        backpressure: BackpressureTier::Red,
        memory_pressure: MemoryPressureTier::Orange,
        worst_budget: BudgetLevel::Normal,
        pane_count,
        paused_pane_count: 0,
    }
}

fn emergency_signals(pane_count: usize) -> PressureSignals {
    PressureSignals {
        backpressure: BackpressureTier::Black,
        memory_pressure: MemoryPressureTier::Red,
        worst_budget: BudgetLevel::OverBudget,
        pane_count,
        paused_pane_count: 0,
    }
}

// ---------------------------------------------------------------------------
// Fleet controller evaluation tests
// ---------------------------------------------------------------------------

#[test]
fn fleet_normal_with_few_panes_recommends_no_action() {
    let mut ctrl = FleetMemoryController::new(FleetMemoryConfig::default());
    let actions = ctrl.evaluate(&normal_signals(10));
    assert_eq!(ctrl.compound_tier(), FleetPressureTier::Normal);
    assert!(
        actions.iter().all(|a| matches!(a, FleetMemoryAction::None)),
        "Normal tier should produce no active actions: {actions:?}"
    );
}

#[test]
fn fleet_elevated_with_many_panes_recommends_eviction() {
    let mut ctrl = FleetMemoryController::new(FleetMemoryConfig::default());
    // Need consecutive evaluations to overcome hysteresis
    let hysteresis = ctrl.config().escalation_threshold;
    for _ in 0..hysteresis {
        ctrl.evaluate(&elevated_signals(200));
    }
    let actions = ctrl.evaluate(&elevated_signals(200));
    assert_eq!(ctrl.compound_tier(), FleetPressureTier::Elevated);
    assert!(
        actions.contains(&FleetMemoryAction::ThrottlePolling),
        "Elevated tier should throttle: {actions:?}"
    );
    // With 200 panes, eviction should also be recommended
    assert!(
        actions.contains(&FleetMemoryAction::EvictWarmScrollback),
        "Elevated tier with 200 panes should evict warm scrollback: {actions:?}"
    );
}

#[test]
fn fleet_elevated_with_few_panes_skips_eviction() {
    let mut ctrl = FleetMemoryController::new(FleetMemoryConfig::default());
    let hysteresis = ctrl.config().escalation_threshold;
    for _ in 0..hysteresis {
        ctrl.evaluate(&elevated_signals(50));
    }
    let actions = ctrl.evaluate(&elevated_signals(50));
    assert_eq!(ctrl.compound_tier(), FleetPressureTier::Elevated);
    assert!(
        actions.contains(&FleetMemoryAction::ThrottlePolling),
        "Elevated tier should throttle: {actions:?}"
    );
    assert!(
        !actions.contains(&FleetMemoryAction::EvictWarmScrollback),
        "Elevated tier with <= 100 panes should NOT evict: {actions:?}"
    );
}

#[test]
fn fleet_critical_always_evicts_and_pauses() {
    let mut ctrl = FleetMemoryController::new(FleetMemoryConfig::default());
    let hysteresis = ctrl.config().escalation_threshold;
    for _ in 0..hysteresis {
        ctrl.evaluate(&critical_signals(200));
    }
    let actions = ctrl.evaluate(&critical_signals(200));
    assert_eq!(ctrl.compound_tier(), FleetPressureTier::Critical);
    assert!(actions.contains(&FleetMemoryAction::EvictWarmScrollback));
    assert!(actions.contains(&FleetMemoryAction::PauseIdlePanes));
    assert!(actions.contains(&FleetMemoryAction::ThrottlePolling));
}

#[test]
fn fleet_emergency_triggers_cleanup() {
    let mut ctrl = FleetMemoryController::new(FleetMemoryConfig::default());
    let hysteresis = ctrl.config().escalation_threshold;
    for _ in 0..hysteresis {
        ctrl.evaluate(&emergency_signals(200));
    }
    let actions = ctrl.evaluate(&emergency_signals(200));
    assert_eq!(ctrl.compound_tier(), FleetPressureTier::Emergency);
    assert!(
        actions.contains(&FleetMemoryAction::EmergencyCleanup),
        "Emergency should trigger cleanup: {actions:?}"
    );
}

// ---------------------------------------------------------------------------
// Hysteresis behavior
// ---------------------------------------------------------------------------

#[test]
fn fleet_hysteresis_prevents_oscillation() {
    let mut ctrl = FleetMemoryController::new(FleetMemoryConfig::default());
    let escalation = ctrl.config().escalation_threshold;
    let deescalation = ctrl.config().deescalation_threshold;

    // Escalate to Elevated
    for _ in 0..=escalation {
        ctrl.evaluate(&elevated_signals(200));
    }
    assert_eq!(ctrl.compound_tier(), FleetPressureTier::Elevated);

    // Single Normal reading should NOT de-escalate
    ctrl.evaluate(&normal_signals(200));
    assert_eq!(
        ctrl.compound_tier(),
        FleetPressureTier::Elevated,
        "Single normal reading should not de-escalate due to hysteresis"
    );

    // Need `deescalation_threshold` consecutive Normal readings
    for _ in 0..deescalation {
        ctrl.evaluate(&normal_signals(200));
    }
    assert_eq!(
        ctrl.compound_tier(),
        FleetPressureTier::Normal,
        "Should de-escalate after {deescalation} consecutive normal readings"
    );
}

// ---------------------------------------------------------------------------
// Scrollback + fleet action integration
// ---------------------------------------------------------------------------

#[test]
fn evict_warm_scrollback_clears_warm_tier() {
    let config = ScrollbackConfig {
        hot_lines: 100,
        page_size: 50,
        warm_max_bytes: 10 * 1024 * 1024,
        ..ScrollbackConfig::default()
    };
    let mut scrollback = TieredScrollback::new(config);

    // Fill hot + overflow to warm
    for i in 0..500 {
        scrollback.push_line(format!(
            "Line {i}: some content that takes up space in the buffer"
        ));
    }

    let snap_before = scrollback.snapshot();
    assert!(
        snap_before.warm_pages > 0,
        "Should have warm pages after overflow"
    );
    assert!(snap_before.warm_bytes > 0, "Should have warm bytes");

    // Simulate EvictWarmScrollback action
    scrollback.evict_all_warm();

    let snap_after = scrollback.snapshot();
    assert_eq!(
        snap_after.warm_pages, 0,
        "Warm pages should be zero after eviction"
    );
    assert_eq!(
        snap_after.warm_bytes, 0,
        "Warm bytes should be zero after eviction"
    );
    assert!(
        snap_after.cold_pages > 0,
        "Cold pages should increase after eviction"
    );
    assert_eq!(
        snap_after.hot_lines, snap_before.hot_lines,
        "Hot lines should be unchanged"
    );
}

#[test]
fn fleet_driven_eviction_cycle_for_200_panes() {
    let config = ScrollbackConfig {
        hot_lines: 100,
        page_size: 50,
        warm_max_bytes: 1024 * 1024, // 1 MB per pane
        ..ScrollbackConfig::default()
    };

    // Simulate 200 panes with warm data
    let mut panes: Vec<TieredScrollback> = (0..200)
        .map(|_| TieredScrollback::new(config.clone()))
        .collect();

    // Fill each pane with enough lines to generate warm pages
    for pane in &mut panes {
        for i in 0..300 {
            pane.push_line(format!("Agent output line {i}: working on task..."));
        }
    }

    // Verify warm data exists
    let total_warm_before: usize = panes.iter().map(|p| p.snapshot().warm_bytes).sum();
    assert!(total_warm_before > 0, "Should have warm data across panes");

    // Fleet controller detects pressure and recommends eviction
    let mut ctrl = FleetMemoryController::new(FleetMemoryConfig::default());
    let hysteresis = ctrl.config().escalation_threshold;
    for _ in 0..=hysteresis {
        ctrl.evaluate(&critical_signals(200));
    }
    let actions = ctrl.evaluate(&critical_signals(200));
    assert!(actions.contains(&FleetMemoryAction::EvictWarmScrollback));

    // Execute eviction on all panes
    for pane in &mut panes {
        pane.evict_all_warm();
    }

    // Verify warm tier is empty across fleet
    let total_warm_after: usize = panes.iter().map(|p| p.snapshot().warm_bytes).sum();
    assert_eq!(total_warm_after, 0, "All warm data should be evicted");

    // Hot tier should be unaffected
    for pane in &panes {
        let snap = pane.snapshot();
        assert!(snap.hot_lines > 0, "Hot lines should be preserved");
    }
}

#[test]
fn enforce_warm_cap_respects_byte_limit() {
    let config = ScrollbackConfig {
        hot_lines: 100,
        page_size: 50,
        warm_max_bytes: 1024, // Very small warm cap
        ..ScrollbackConfig::default()
    };
    let mut scrollback = TieredScrollback::new(config);

    // Push enough to overflow warm cap
    for i in 0..1000 {
        scrollback.push_line(format!(
            "Line {i}: data that will overflow the warm tier cap"
        ));
    }

    let snap = scrollback.snapshot();
    // enforce_warm_cap evicts pages until warm_bytes <= warm_max_bytes,
    // so the invariant is exact (not approximate).
    assert!(
        snap.warm_bytes <= 1024,
        "Warm bytes ({}) should respect cap (1024) after enforce_warm_cap",
        snap.warm_bytes
    );
    // Cold tier should have received the evicted warm pages.
    assert!(
        snap.cold_pages > 0,
        "Cold pages should be non-zero after warm cap enforcement"
    );
}

#[test]
fn fleet_snapshot_captures_evaluation_state() {
    let mut ctrl = FleetMemoryController::new(FleetMemoryConfig::default());

    ctrl.evaluate(&normal_signals(10));
    let snap = ctrl.snapshot();
    assert_eq!(snap.compound_tier, FleetPressureTier::Normal);
    assert_eq!(snap.total_evaluations, 1);
    assert_eq!(snap.total_transitions, 0);

    // Escalate
    let hysteresis = ctrl.config().escalation_threshold;
    for _ in 0..=hysteresis {
        ctrl.evaluate(&emergency_signals(200));
    }
    let snap = ctrl.snapshot();
    assert_eq!(snap.compound_tier, FleetPressureTier::Emergency);
    assert!(snap.total_transitions > 0);
}

#[test]
fn scrollback_snapshot_serde_roundtrip() {
    let config = ScrollbackConfig::default();
    let mut scrollback = TieredScrollback::new(config);
    for i in 0..200 {
        scrollback.push_line(format!("Line {i}"));
    }

    let snap = scrollback.snapshot();
    let json = serde_json::to_string(&snap).expect("serialize snapshot");
    let deserialized: frankenterm_core::scrollback_tiers::ScrollbackTierSnapshot =
        serde_json::from_str(&json).expect("deserialize snapshot");
    assert_eq!(snap, deserialized);
}

#[test]
fn fleet_audit_trail_records_decisions() {
    let mut ctrl = FleetMemoryController::new(FleetMemoryConfig::default());

    // Multiple evaluations
    for _ in 0..5 {
        ctrl.evaluate(&normal_signals(10));
    }

    let trail = ctrl.audit_trail();
    assert_eq!(trail.len(), 5, "Should have 5 audit records");
    for record in trail {
        assert_eq!(record.compound_tier, FleetPressureTier::Normal);
    }
}

#[test]
fn memory_budget_over_budget_escalates_fleet_tier() {
    let mut ctrl = FleetMemoryController::new(FleetMemoryConfig::default());
    let signals = PressureSignals {
        backpressure: BackpressureTier::Green,
        memory_pressure: MemoryPressureTier::Green,
        worst_budget: BudgetLevel::OverBudget,
        pane_count: 200,
        paused_pane_count: 0,
    };

    let hysteresis = ctrl.config().escalation_threshold;
    for _ in 0..=hysteresis {
        ctrl.evaluate(&signals);
    }
    assert!(
        ctrl.compound_tier() >= FleetPressureTier::Critical,
        "OverBudget should escalate to at least Critical: {:?}",
        ctrl.compound_tier()
    );
}

#[test]
fn worst_of_semantics_takes_most_severe_signal() {
    let mut ctrl = FleetMemoryController::new(FleetMemoryConfig::default());
    let signals = PressureSignals {
        backpressure: BackpressureTier::Green,    // Normal
        memory_pressure: MemoryPressureTier::Red, // Emergency
        worst_budget: BudgetLevel::Normal,        // Normal
        pane_count: 50,
        paused_pane_count: 0,
    };

    let hysteresis = ctrl.config().escalation_threshold;
    for _ in 0..=hysteresis {
        ctrl.evaluate(&signals);
    }
    assert_eq!(
        ctrl.compound_tier(),
        FleetPressureTier::Emergency,
        "Worst-of should pick Emergency from Red memory pressure"
    );
}

// ---------------------------------------------------------------------------
// FleetScrollbackCoordinator integration tests
// ---------------------------------------------------------------------------

fn make_pane_map(
    count: usize,
    lines_per_pane: usize,
) -> std::collections::HashMap<u64, TieredScrollback> {
    let config = ScrollbackConfig {
        hot_lines: 100,
        page_size: 50,
        warm_max_bytes: 10 * 1024 * 1024, // 10 MB per pane
        ..ScrollbackConfig::default()
    };
    let mut map = std::collections::HashMap::new();
    for i in 0..count {
        let mut sb = TieredScrollback::new(config.clone());
        for j in 0..lines_per_pane {
            sb.push_line(format!("pane-{i} line-{j}: agent output content"));
        }
        map.insert(i as u64, sb);
    }
    map
}

fn pane_infos(map: &std::collections::HashMap<u64, TieredScrollback>) -> Vec<PaneScrollbackInfo> {
    map.iter()
        .map(|(&id, sb)| {
            let snap = sb.snapshot();
            PaneScrollbackInfo {
                pane_id: id,
                activity_counter: snap.activity_counter,
                warm_bytes: snap.warm_bytes,
                warm_pages: snap.warm_pages,
                estimated_memory_bytes: sb.estimated_memory_bytes(),
            }
        })
        .collect()
}

fn snapshot_pane_access(
    map: &std::collections::HashMap<u64, TieredScrollback>,
) -> SnapshotPaneScrollbackAccess {
    SnapshotPaneScrollbackAccess::new(map.iter().map(|(&id, sb)| (id, sb.snapshot())).collect())
}

#[test]
fn snapshot_adapter_observes_real_fleet_state_without_placeholder_emptiness() {
    let mut coord = FleetScrollbackCoordinator::new(
        CoordinatorConfig {
            min_fleet_warm_bytes_for_eviction: 0,
            ..CoordinatorConfig::default()
        },
        FleetMemoryConfig {
            escalation_threshold: 1,
            deescalation_threshold: 1,
            ..FleetMemoryConfig::default()
        },
    );

    let panes = make_pane_map(8, 500);
    let mut snapshot_access = snapshot_pane_access(&panes);
    let infos = FleetScrollbackCoordinator::collect_pane_infos(&snapshot_access);
    let repeated_infos = FleetScrollbackCoordinator::collect_pane_infos(&snapshot_access);

    assert_eq!(infos.len(), 8);
    assert_eq!(infos.len(), repeated_infos.len());
    assert!(
        infos
            .iter()
            .all(|info| info.warm_bytes > 0 && info.warm_pages > 0),
        "snapshot adapter should expose real warm scrollback data: {infos:?}"
    );

    let signals = PressureSignals {
        backpressure: BackpressureTier::Red,
        memory_pressure: MemoryPressureTier::Orange,
        worst_budget: BudgetLevel::Normal,
        pane_count: infos.len(),
        paused_pane_count: 0,
    };

    let result = coord.evaluate(&signals, &infos, &mut snapshot_access);

    assert_eq!(result.compound_tier, FleetPressureTier::Critical);
    assert!(
        result.eviction_plan.is_some(),
        "real snapshot data should produce an eviction plan instead of placeholder emptiness"
    );
    assert!(
        result.targets_applied > 0,
        "snapshot adapter should enforce eviction targets"
    );
    assert!(
        result.pages_evicted > 0,
        "snapshot adapter should evict warm pages"
    );
    assert!(
        result.bytes_reclaimed > 0,
        "snapshot adapter should report reclaimed bytes"
    );
    assert!(coord.telemetry().plans_produced > 0);
}

#[test]
fn coordinator_200_pane_emergency_evicts_all_warm() {
    let mut coord = FleetScrollbackCoordinator::new(
        CoordinatorConfig {
            emergency_evict_all: true,
            min_fleet_warm_bytes_for_eviction: 0,
            ..CoordinatorConfig::default()
        },
        FleetMemoryConfig {
            escalation_threshold: 1,
            deescalation_threshold: 1,
            ..FleetMemoryConfig::default()
        },
    );

    let mut panes = make_pane_map(200, 500);

    // Verify we have warm data across the fleet
    let total_warm_before: usize = panes.values().map(|sb| sb.snapshot().warm_bytes).sum();
    assert!(
        total_warm_before > 0,
        "Fleet should have warm data before eviction"
    );

    let signals = PressureSignals {
        backpressure: BackpressureTier::Black,
        memory_pressure: MemoryPressureTier::Red,
        worst_budget: BudgetLevel::OverBudget,
        pane_count: 200,
        paused_pane_count: 0,
    };

    let infos = pane_infos(&panes);
    let result = coord.evaluate(&signals, &infos, &mut panes);

    assert_eq!(result.compound_tier, FleetPressureTier::Emergency);

    // All warm data should be gone
    let total_warm_after: usize = panes.values().map(|sb| sb.snapshot().warm_bytes).sum();
    assert_eq!(total_warm_after, 0, "Emergency should clear all warm data");

    // Hot lines should be preserved
    for sb in panes.values() {
        assert!(sb.hot_len() > 0, "Hot tier should be preserved");
    }

    // Cold tier should have received evicted data
    let total_cold: u64 = panes.values().map(|sb| sb.cold_line_count()).sum();
    assert!(
        total_cold > 0,
        "Cold tier should have received evicted data"
    );

    // Telemetry should reflect the emergency cleanup
    assert!(coord.telemetry().emergency_cleanups > 0);
    assert!(coord.telemetry().pages_evicted > 0);
}

#[test]
fn coordinator_200_pane_critical_partial_eviction() {
    let mut coord = FleetScrollbackCoordinator::new(
        CoordinatorConfig {
            min_fleet_warm_bytes_for_eviction: 0,
            max_targets_per_cycle: 200, // allow targeting all panes
            ..CoordinatorConfig::default()
        },
        FleetMemoryConfig {
            escalation_threshold: 1,
            deescalation_threshold: 1,
            ..FleetMemoryConfig::default()
        },
    );

    let mut panes = make_pane_map(200, 500);

    let signals = PressureSignals {
        backpressure: BackpressureTier::Red,
        memory_pressure: MemoryPressureTier::Orange,
        worst_budget: BudgetLevel::Normal,
        pane_count: 200,
        paused_pane_count: 0,
    };

    let infos = pane_infos(&panes);
    let total_warm_before: usize = infos.iter().map(|i| i.warm_bytes).sum();

    let result = coord.evaluate(&signals, &infos, &mut panes);

    assert_eq!(result.compound_tier, FleetPressureTier::Critical);

    // Should have produced an eviction plan
    assert!(
        result.eviction_plan.is_some(),
        "Critical tier should produce an eviction plan"
    );

    // Some warm data should be evicted but not necessarily all
    let total_warm_after: usize = panes.values().map(|sb| sb.snapshot().warm_bytes).sum();
    assert!(
        total_warm_after < total_warm_before,
        "Warm bytes should decrease: before={total_warm_before}, after={total_warm_after}"
    );

    // Hot tier should be preserved for all panes
    for sb in panes.values() {
        assert!(sb.hot_len() > 0, "Hot tier should be preserved");
    }

    assert_eq!(coord.telemetry().emergency_cleanups, 0);
    assert!(coord.telemetry().plans_produced > 0);
}

#[test]
fn coordinator_normal_pressure_no_eviction() {
    let mut coord = FleetScrollbackCoordinator::default();
    let mut panes = make_pane_map(50, 300);

    let signals = FleetScrollbackCoordinator::default_signals(50);
    let infos = pane_infos(&panes);

    let result = coord.evaluate(&signals, &infos, &mut panes);

    assert_eq!(result.compound_tier, FleetPressureTier::Normal);
    assert!(result.eviction_plan.is_none());
    assert_eq!(result.pages_evicted, 0);

    // Warm data should be untouched
    let total_warm: usize = panes.values().map(|sb| sb.snapshot().warm_bytes).sum();
    assert!(total_warm > 0, "Warm data should be preserved under normal");
}

#[test]
fn coordinator_memory_stays_under_1gb_at_200_panes() {
    // This is the key acceptance criterion from ft-1memj.19:
    // 200 panes with 100k lines each should use < 1 GB.
    //
    // Here we use a smaller scale (200 panes x 5000 lines) to validate
    // the tiered model keeps memory bounded.
    let config = ScrollbackConfig {
        hot_lines: 500, // smaller hot for test speed
        page_size: 100,
        warm_max_bytes: 2 * 1024 * 1024, // 2 MB warm cap per pane
        ..ScrollbackConfig::default()
    };

    let mut panes = std::collections::HashMap::new();
    for i in 0..200u64 {
        let mut sb = TieredScrollback::new(config.clone());
        for j in 0..5000 {
            sb.push_line(format!("pane-{i} line-{j}: agent session output"));
        }
        panes.insert(i, sb);
    }

    // Calculate total memory footprint
    let total_memory: usize = panes.values().map(|sb| sb.estimated_memory_bytes()).sum();
    let total_memory_mb = total_memory / (1024 * 1024);

    // With 500 hot lines * ~40 bytes avg * 200 panes = ~4 MB hot
    // With 2 MB warm cap * 200 panes = ~400 MB warm max
    // Total should be well under 1 GB
    assert!(
        total_memory_mb < 1024,
        "Total memory should be under 1 GB: {total_memory_mb} MB"
    );

    // Verify each pane has capped warm tier
    for sb in panes.values() {
        let snap = sb.snapshot();
        assert!(
            snap.warm_bytes <= 2 * 1024 * 1024,
            "Per-pane warm should be under 2 MB cap: {} bytes",
            snap.warm_bytes
        );
    }

    // Verify data is not lost — total line count across tiers = 5000
    for sb in panes.values() {
        assert_eq!(
            sb.total_line_count(),
            5000,
            "All lines should be accounted for across tiers"
        );
    }
}
