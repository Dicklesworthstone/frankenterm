//! Integration coverage for the capacity governor and heavy-command policy path
//! (ft-3681t.7.3).

use frankenterm_core::build_coord::{is_heavy_cargo_command, requires_rch_offload};
use frankenterm_core::capacity_governor::{
    CapacityGovernor, CapacityGovernorConfig, GovernorDecision, PressureSignals, WorkloadCategory,
};
use frankenterm_core::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyEngine, PolicyInput,
};

#[test]
fn robot_heavy_cargo_without_rch_requires_policy_approval() {
    let command = "cargo test -p frankenterm-core -- --nocapture";
    assert!(is_heavy_cargo_command(command));
    assert!(requires_rch_offload(command));

    let mut engine = PolicyEngine::permissive();
    let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
        .with_pane(7)
        .with_capabilities(PaneCapabilities::prompt())
        .with_command_text(command);

    let decision = engine.authorize(&input);
    assert!(decision.requires_approval());
    assert!(
        decision
            .reason()
            .is_some_and(|reason| reason.contains("rch exec")),
        "reason should explain the rch requirement: {:?}",
        decision.reason()
    );
}

#[test]
fn robot_heavy_cargo_with_rch_prefix_is_allowed() {
    let command = "TMPDIR=/tmp rch exec -- cargo test -p frankenterm-core -- --nocapture";
    assert!(is_heavy_cargo_command(command));
    assert!(!requires_rch_offload(command));

    let mut engine = PolicyEngine::permissive();
    let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
        .with_pane(7)
        .with_capabilities(PaneCapabilities::prompt())
        .with_command_text(command);

    let decision = engine.authorize(&input);
    assert!(decision.is_allowed());
}

#[test]
fn zero_workers_prevent_capacity_governor_offload() {
    let mut governor = CapacityGovernor::new(CapacityGovernorConfig::default());
    let signals = PressureSignals {
        cpu_utilization: 0.35,
        memory_utilization: 0.40,
        active_heavy_workloads: 2,
        active_medium_workloads: 0,
        load_average_1m: 2.0,
        rch_available: true,
        rch_workers_available: 0,
        io_pressure: 0.10,
        timestamp_ms: 1_000,
    };

    assert!(!signals.rch_can_offload());
    let decision = governor.evaluate(WorkloadCategory::Heavy, &signals);
    assert!(matches!(decision, GovernorDecision::Throttle { .. }));
}

#[test]
fn available_workers_enable_capacity_governor_offload() {
    let mut governor = CapacityGovernor::new(CapacityGovernorConfig::default());
    let signals = PressureSignals {
        cpu_utilization: 0.35,
        memory_utilization: 0.40,
        active_heavy_workloads: 2,
        active_medium_workloads: 0,
        load_average_1m: 2.0,
        rch_available: true,
        rch_workers_available: 2,
        io_pressure: 0.10,
        timestamp_ms: 1_000,
    };

    assert!(signals.rch_can_offload());
    let decision = governor.evaluate(WorkloadCategory::Heavy, &signals);
    assert!(matches!(decision, GovernorDecision::Offload { .. }));
}
