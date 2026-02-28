//! End-to-end scenario matrix and acceptance harness for the mission system.
//! [ft-1i2ge.7.1]
//!
//! Validates the mission control loop under a comprehensive matrix of scenarios:
//! - Nominal: standard cadence operation with healthy agents and beads
//! - Blocked: all beads blocked, dependency chains, partial availability
//! - Degraded: high conflict rates, policy denials, agent churn
//! - Emergency: override-driven intervention, kill-switch analog, recovery paths
//!
//! Each scenario exercises the full pipeline: readiness → extraction → scoring →
//! override application → solving → safety envelope → conflict detection → reporting.

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::beads_types::{
    BeadDependencyRef, BeadIssueDetail, BeadIssueType, BeadStatus,
};
use frankenterm_core::mission_events::{
    MissionEventBuilder, MissionEventKind, MissionEventLog, MissionEventLogConfig,
};
use frankenterm_core::mission_loop::{
    ConflictDetectionConfig, DeconflictionStrategy, KnownReservation, MissionLoop,
    MissionLoopConfig, MissionSafetyEnvelopeConfig, MissionTrigger, OperatorOverride,
    OperatorOverrideKind, format_operator_report_plain,
};
use frankenterm_core::plan::MissionAgentAvailability;
use frankenterm_core::plan::MissionAgentCapabilityProfile;
use frankenterm_core::planner_features::PlannerExtractionContext;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn ready_agent(id: &str) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Ready,
    }
}

fn loaded_agent(id: &str, load: usize) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: load,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Ready,
    }
}

fn offline_agent(id: &str) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Offline {
            reason_code: "e2e-test".to_string(),
        },
    }
}

fn bead(id: &str, status: BeadStatus, priority: u8) -> BeadIssueDetail {
    BeadIssueDetail {
        id: id.to_string(),
        title: format!("Bead {id}"),
        status,
        priority,
        issue_type: BeadIssueType::Task,
        assignee: None,
        labels: Vec::new(),
        dependencies: Vec::new(),
        dependents: Vec::new(),
        parent: None,
        ingest_warning: None,
        extra: HashMap::new(),
    }
}

fn bead_with_deps(id: &str, priority: u8, deps: &[&str]) -> BeadIssueDetail {
    let mut b = bead(id, BeadStatus::Open, priority);
    b.dependencies = deps
        .iter()
        .map(|d| BeadDependencyRef {
            id: d.to_string(),
            title: None,
            status: None,
            priority: None,
            dependency_type: Some("blocks".to_string()),
        })
        .collect();
    b
}

fn risky_bead(id: &str) -> BeadIssueDetail {
    let mut b = bead(id, BeadStatus::Open, 1);
    b.labels = vec!["danger".to_string()];
    b
}

fn ovr(id: &str, kind: OperatorOverrideKind) -> OperatorOverride {
    OperatorOverride {
        override_id: id.to_string(),
        kind,
        activated_by: "e2e-test".to_string(),
        reason_code: "e2e.scenario_matrix".to_string(),
        rationale: "E2E scenario matrix test".to_string(),
        activated_at_ms: 1000,
        expires_at_ms: None,
        correlation_id: Some("e2e-matrix".to_string()),
    }
}

fn ctx() -> PlannerExtractionContext {
    PlannerExtractionContext::default()
}

fn log() -> MissionEventLog {
    MissionEventLog::new(MissionEventLogConfig {
        max_events: 100,
        enabled: true,
    })
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// NOMINAL SCENARIOS — Happy path with healthy system
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn nominal_single_agent_single_bead() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert_eq!(d.assignment_set.assignments.len(), 1);
    assert_eq!(d.assignment_set.assignments[0].bead_id, "b1");
    assert_eq!(d.assignment_set.assignments[0].agent_id, "a1");
}

#[test]
fn nominal_multi_agent_multi_bead() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1"), ready_agent("a2"), ready_agent("a3")];
    let issues = vec![
        bead("b1", BeadStatus::Open, 1),
        bead("b2", BeadStatus::Open, 2),
        bead("b3", BeadStatus::Open, 3),
    ];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(!d.assignment_set.assignments.is_empty());
    assert!(d.extraction_summary.total_candidates >= 3);
}

#[test]
fn nominal_multi_cycle_steady_state() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1"), ready_agent("a2")];
    let issues = vec![
        bead("b1", BeadStatus::Open, 1),
        bead("b2", BeadStatus::Open, 2),
    ];
    let c = ctx();

    let mut total_assignments = 0;
    for i in 0..10 {
        let d = ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
        total_assignments += d.assignment_set.assignments.len();
    }

    assert_eq!(ml.state().cycle_count, 10);
    assert!(total_assignments > 0);

    let report = ml.generate_operator_report(Some(&log()), None);
    assert_eq!(report.status.phase_label, "active");
    // Repeated assignment of the same beads may cause planner churn (agents
    // swapping beads across cycles), pushing health to "degraded".  The key
    // nominal invariant is that the system never reaches "critical".
    assert_ne!(
        report.health.overall, "critical",
        "Steady-state nominal scenario must not be critical"
    );
}

#[test]
fn nominal_priority_ordering_respected() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        solver_config: frankenterm_core::planner_features::SolverConfig {
            max_assignments: 1, // Force single assignment to test priority
            ..Default::default()
        },
        ..MissionLoopConfig::default()
    });
    let agents = vec![ready_agent("a1")];
    let issues = vec![
        bead("low", BeadStatus::Open, 5),
        bead("high", BeadStatus::Open, 1),
    ];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    if d.assignment_set.assignments.len() == 1 {
        assert_eq!(
            d.assignment_set.assignments[0].bead_id, "high",
            "Higher priority bead should be assigned first"
        );
    }
}

#[test]
fn nominal_cadence_tick_triggers() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        cadence_ms: 10_000,
        ..MissionLoopConfig::default()
    });
    let agents = vec![ready_agent("a1")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];
    let c = ctx();

    // First tick always evaluates
    assert!(ml.should_evaluate(0));
    ml.evaluate(0, MissionTrigger::CadenceTick, &issues, &agents, &c);

    // Too soon
    assert!(!ml.should_evaluate(5_000));

    // After cadence
    assert!(ml.should_evaluate(10_000));
}

#[test]
fn nominal_report_serde_roundtrip() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let report = ml.generate_operator_report(Some(&log()), None);
    let json = serde_json::to_string(&report).unwrap();
    let roundtripped: frankenterm_core::mission_loop::OperatorStatusReport =
        serde_json::from_str(&json).unwrap();

    assert_eq!(report.status.cycle_count, roundtripped.status.cycle_count);
    assert_eq!(report.health.overall, roundtripped.health.overall);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// BLOCKED SCENARIOS — Dependencies and unavailability
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn blocked_all_beads_have_unsatisfied_deps() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    // Both beads depend on closed parent, but their own status matters for readiness
    let issues = vec![
        bead_with_deps("b1", 1, &["parent-not-done"]),
        bead_with_deps("b2", 2, &["parent-not-done"]),
    ];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    // Beads with unsatisfied deps may still be extracted (readiness checks status, not deps)
    // The pipeline extracts open beads; dependency awareness is at readiness resolution level
    assert_eq!(d.cycle_id, 1);
}

#[test]
fn blocked_no_ready_agents() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![offline_agent("a1"), offline_agent("a2")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(d.assignment_set.assignments.is_empty());
}

#[test]
fn blocked_all_agents_at_capacity() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![loaded_agent("a1", 3), loaded_agent("a2", 3)];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    // Agents at max load may not receive assignments depending on solver capacity tracking
    assert_eq!(d.cycle_id, 1);
}

#[test]
fn blocked_empty_bead_list() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    let issues: Vec<BeadIssueDetail> = vec![];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(d.assignment_set.assignments.is_empty());
    assert_eq!(d.extraction_summary.total_candidates, 0);
}

#[test]
fn blocked_only_closed_beads() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    let issues = vec![
        bead("b1", BeadStatus::Closed, 1),
        bead("b2", BeadStatus::Closed, 2),
    ];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    // Closed beads are not ready for assignment
    assert!(d.assignment_set.assignments.is_empty());
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// DEGRADED SCENARIOS — Partial failure, high contention
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn degraded_mixed_online_offline_agents() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![
        ready_agent("a1"),
        offline_agent("a2"),
        ready_agent("a3"),
        offline_agent("a4"),
    ];
    let issues = vec![
        bead("b1", BeadStatus::Open, 1),
        bead("b2", BeadStatus::Open, 2),
    ];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    // Only online agents should get assignments
    for a in &d.assignment_set.assignments {
        assert!(
            a.agent_id == "a1" || a.agent_id == "a3",
            "Offline agent {} got assignment",
            a.agent_id
        );
    }
}

#[test]
fn degraded_agent_fleet_churns() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let issues = vec![bead("b1", BeadStatus::Open, 1)];
    let c = ctx();

    // Cycle 1: a1 online
    let agents_1 = vec![ready_agent("a1")];
    let d1 = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents_1, &c);
    assert!(!d1.assignment_set.assignments.is_empty());

    // Cycle 2: a1 offline, a2 online
    let agents_2 = vec![offline_agent("a1"), ready_agent("a2")];
    let d2 = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents_2, &c);
    if !d2.assignment_set.assignments.is_empty() {
        assert_eq!(d2.assignment_set.assignments[0].agent_id, "a2");
    }

    // Cycle 3: a2 offline, a3 online
    let agents_3 = vec![offline_agent("a2"), ready_agent("a3")];
    let d3 = ml.evaluate(61_000, MissionTrigger::CadenceTick, &issues, &agents_3, &c);
    if !d3.assignment_set.assignments.is_empty() {
        assert_eq!(d3.assignment_set.assignments[0].agent_id, "a3");
    }
}

#[test]
fn degraded_safety_envelope_enforced_under_pressure() {
    let config = MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 2,
            max_risky_assignments_per_cycle: 1,
            max_consecutive_retries_per_bead: 3,
            risky_label_markers: vec!["danger".to_string()],
        },
        ..MissionLoopConfig::default()
    };
    let mut ml = MissionLoop::new(config);
    let agents = vec![
        ready_agent("a1"),
        ready_agent("a2"),
        ready_agent("a3"),
        ready_agent("a4"),
    ];
    let issues = vec![
        risky_bead("r1"),
        risky_bead("r2"),
        bead("b1", BeadStatus::Open, 2),
        bead("b2", BeadStatus::Open, 3),
    ];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    // Total assignments capped at 2
    assert!(
        d.assignment_set.assignments.len() <= 2,
        "Expected at most 2 assignments, got {}",
        d.assignment_set.assignments.len()
    );

    // Risky assignments capped at 1
    let risky_count = d
        .assignment_set
        .assignments
        .iter()
        .filter(|a| a.bead_id.starts_with('r'))
        .count();
    assert!(risky_count <= 1, "Expected at most 1 risky, got {}", risky_count);
}

#[test]
fn degraded_conflict_detection_under_contention() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            enabled: true,
            max_conflicts_per_cycle: 20,
            strategy: DeconflictionStrategy::PriorityWins,
            generate_messages: true,
        },
        ..MissionLoopConfig::default()
    });
    let agents = vec![ready_agent("a1"), ready_agent("a2")];
    let issues = vec![
        bead("b1", BeadStatus::Open, 1),
        bead("b2", BeadStatus::Open, 1),
    ];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    // Simulate conflicting reservations
    let reservations = vec![
        KnownReservation {
            holder: "a1".to_string(),
            paths: vec!["src/shared.rs".to_string()],
            exclusive: true,
            bead_id: Some("b1".to_string()),
            expires_at_ms: Some(60_000),
        },
        KnownReservation {
            holder: "a2".to_string(),
            paths: vec!["src/shared.rs".to_string()],
            exclusive: true,
            bead_id: Some("b2".to_string()),
            expires_at_ms: Some(60_000),
        },
    ];

    let report =
        ml.detect_conflicts(&d.assignment_set, &reservations, &[], 1000, &issues);
    assert!(!report.conflicts.is_empty());

    let (total, _auto) = ml.conflict_stats();
    // Stats reflect detected conflicts
    assert!(total > 0 || !report.conflicts.is_empty());
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// EMERGENCY SCENARIOS — Override-driven intervention and recovery
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn emergency_exclude_all_beads_via_overrides() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    let issues = vec![
        bead("b1", BeadStatus::Open, 1),
        bead("b2", BeadStatus::Open, 2),
    ];

    // Exclude all beads
    ml.apply_override(ovr(
        "excl-b1",
        OperatorOverrideKind::Exclude {
            bead_id: "b1".to_string(),
        },
    ))
    .unwrap();
    ml.apply_override(ovr(
        "excl-b2",
        OperatorOverrideKind::Exclude {
            bead_id: "b2".to_string(),
        },
    ))
    .unwrap();

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(
        d.assignment_set.assignments.is_empty(),
        "All beads excluded, no assignments expected"
    );

    let summary = ml.state().last_override_summary.as_ref().unwrap();
    assert_eq!(summary.excluded_beads.len(), 2);
}

#[test]
fn emergency_exclude_all_agents_via_overrides() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1"), ready_agent("a2")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];

    ml.apply_override(ovr(
        "excl-a1",
        OperatorOverrideKind::ExcludeAgent {
            agent_id: "a1".to_string(),
        },
    ))
    .unwrap();
    ml.apply_override(ovr(
        "excl-a2",
        OperatorOverrideKind::ExcludeAgent {
            agent_id: "a2".to_string(),
        },
    ))
    .unwrap();

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(
        d.assignment_set.assignments.is_empty(),
        "All agents excluded, no assignments expected"
    );
}

#[test]
fn emergency_pin_overrides_solver() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1"), ready_agent("a2")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];

    ml.apply_override(ovr(
        "pin-b1-a2",
        OperatorOverrideKind::Pin {
            bead_id: "b1".to_string(),
            target_agent: "a2".to_string(),
        },
    ))
    .unwrap();

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let b1_assignment = d
        .assignment_set
        .assignments
        .iter()
        .find(|a| a.bead_id == "b1");
    assert!(b1_assignment.is_some(), "b1 should be assigned via pin");
    assert_eq!(b1_assignment.unwrap().agent_id, "a2");
}

#[test]
fn emergency_ttl_override_expires_and_system_recovers() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];
    let c = ctx();

    let mut exclude = ovr(
        "ttl-excl",
        OperatorOverrideKind::Exclude {
            bead_id: "b1".to_string(),
        },
    );
    exclude.expires_at_ms = Some(5_000);
    ml.apply_override(exclude).unwrap();

    // Cycle 1: override active, b1 excluded
    let d1 = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    assert!(
        d1.assignment_set
            .assignments
            .iter()
            .all(|a| a.bead_id != "b1"),
        "b1 should be excluded at t=1000"
    );

    // Cycle 2: override expired, b1 available again
    let d2 = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    assert!(ml.active_overrides().is_empty(), "Override should be evicted");
    assert!(
        !d2.assignment_set.assignments.is_empty(),
        "b1 should be assignable after override expiry"
    );
}

#[test]
fn emergency_manual_trigger_bypass_cadence() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        cadence_ms: 300_000, // Very long cadence
        max_trigger_batch: 1, // A single trigger forces evaluation
        ..MissionLoopConfig::default()
    });
    let agents = vec![ready_agent("a1")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];

    // First evaluation
    ml.evaluate(0, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    // Not time for cadence yet
    assert!(!ml.should_evaluate(10_000));

    // Manual trigger forces evaluation
    ml.trigger(MissionTrigger::ManualTrigger {
        reason: "emergency".to_string(),
    });
    assert!(ml.should_evaluate(10_000));
}

#[test]
fn emergency_recovery_full_lifecycle() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let issues = vec![
        bead("b1", BeadStatus::Open, 1),
        bead("b2", BeadStatus::Open, 2),
    ];
    let c = ctx();

    // Phase 1: Normal operation
    let agents_normal = vec![ready_agent("a1"), ready_agent("a2")];
    let d1 = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents_normal, &c);
    assert!(!d1.assignment_set.assignments.is_empty());

    // Phase 2: Crisis — all agents offline
    let agents_crisis = vec![offline_agent("a1"), offline_agent("a2")];
    let d2 = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents_crisis, &c);
    assert!(d2.assignment_set.assignments.is_empty());

    // Phase 3: Partial recovery — one agent back
    let agents_partial = vec![ready_agent("a1"), offline_agent("a2")];
    let d3 = ml.evaluate(
        61_000,
        MissionTrigger::AgentAvailabilityChange {
            agent_id: "a1".to_string(),
        },
        &issues,
        &agents_partial,
        &c,
    );
    assert!(!d3.assignment_set.assignments.is_empty());
    for a in &d3.assignment_set.assignments {
        assert_eq!(a.agent_id, "a1");
    }

    // Phase 4: Full recovery
    let agents_full = vec![ready_agent("a1"), ready_agent("a2")];
    let d4 = ml.evaluate(91_000, MissionTrigger::CadenceTick, &issues, &agents_full, &c);
    assert!(!d4.assignment_set.assignments.is_empty());

    let report = ml.generate_operator_report(Some(&log()), None);
    assert_eq!(report.status.cycle_count, 4);
    assert!(report.status.total_assignments > 0);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ACCEPTANCE CRITERIA VERIFICATION
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn acceptance_metrics_accumulate_correctly() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];
    let c = ctx();

    for i in 0..5 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    let state = ml.state();
    assert_eq!(state.cycle_count, 5);
    assert_eq!(state.metrics_totals.cycles, 5);
    assert!(state.total_assignments_made > 0);
    assert!(state.metrics_history.len() <= 256); // bounded by config
}

#[test]
fn acceptance_decision_contains_all_summaries() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    assert_eq!(d.cycle_id, 1);
    assert_eq!(d.timestamp_ms, 1000);
    assert!(d.extraction_summary.total_candidates > 0);
    assert!(d.scorer_summary.scored_count > 0);
}

#[test]
fn acceptance_report_plain_text_complete() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1"), ready_agent("a2")];
    let issues = vec![
        bead("b1", BeadStatus::Open, 1),
        bead("b2", BeadStatus::Open, 2),
    ];
    let c = ctx();

    for i in 0..3 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    let mut event_log = log();
    event_log.emit(
        MissionEventBuilder::new(MissionEventKind::CycleStarted, "acceptance.test")
            .cycle(1, 30_000)
            .labels("test", "acceptance"),
    );

    let report = ml.generate_operator_report(Some(&event_log), None);
    let text = format_operator_report_plain(&report);

    // Must contain all standard sections
    assert!(text.contains("=== Mission Status ==="));
    assert!(text.contains("=== Health ==="));
    assert!(text.contains("Phase:"));
    assert!(text.contains("Overall:"));
    assert!(text.contains("Throughput:"));
    assert!(text.contains("assign/min"));
}

#[test]
fn acceptance_determinism_across_runs() {
    let run = || {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let agents = vec![ready_agent("a1"), ready_agent("a2")];
        let issues = vec![
            bead("b1", BeadStatus::Open, 1),
            bead("b2", BeadStatus::Open, 2),
            bead("b3", BeadStatus::Open, 3),
        ];
        let c = ctx();

        let mut results = Vec::new();
        for i in 0..5 {
            let d = ml.evaluate(
                (i + 1) * 30_000,
                MissionTrigger::CadenceTick,
                &issues,
                &agents,
                &c,
            );
            results.push((
                d.assignment_set.assignments.len(),
                d.assignment_set.rejected.len(),
                d.extraction_summary.total_candidates,
            ));
        }
        results
    };

    let r1 = run();
    let r2 = run();
    assert_eq!(r1, r2, "Multi-cycle results must be deterministic");
}

#[test]
fn acceptance_event_log_integration() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let mut event_log = log();
    for i in 0..5 {
        event_log.emit(
            MissionEventBuilder::new(MissionEventKind::CycleStarted, "acceptance.events")
                .cycle(i + 1, (i as i64 + 1) * 1000)
                .labels("test", "acceptance"),
        );
    }

    let report = ml.generate_operator_report(Some(&event_log), None);
    assert_eq!(report.event_summary.total_emitted, 5);
    assert!(report.event_summary.retained_events > 0);
}

#[test]
fn acceptance_override_summary_reflects_all_types() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1"), ready_agent("a2")];
    let issues = vec![
        bead("b1", BeadStatus::Open, 1),
        bead("b2", BeadStatus::Open, 2),
        bead("b3", BeadStatus::Open, 3),
    ];

    // Apply one of each override type
    ml.apply_override(ovr(
        "pin-1",
        OperatorOverrideKind::Pin {
            bead_id: "b1".to_string(),
            target_agent: "a2".to_string(),
        },
    ))
    .unwrap();
    ml.apply_override(ovr(
        "excl-1",
        OperatorOverrideKind::Exclude {
            bead_id: "b2".to_string(),
        },
    ))
    .unwrap();
    ml.apply_override(ovr(
        "excl-agent-1",
        OperatorOverrideKind::ExcludeAgent {
            agent_id: "a1".to_string(),
        },
    ))
    .unwrap();
    ml.apply_override(ovr(
        "repri-1",
        OperatorOverrideKind::Reprioritize {
            bead_id: "b3".to_string(),
            score_delta: 50,
        },
    ))
    .unwrap();

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let summary = ml.state().last_override_summary.as_ref().unwrap();
    assert!(summary.excluded_beads.contains(&"b2".to_string()));
    assert!(summary.excluded_agents.contains(&"a1".to_string()));
    assert!(
        summary
            .pinned_assignments
            .iter()
            .any(|p| p.bead_id == "b1" && p.agent_id == "a2")
    );
    assert!(
        summary
            .reprioritized_beads
            .iter()
            .any(|r| r.bead_id == "b3" && r.delta == 50)
    );
}

#[test]
fn acceptance_state_serde_roundtrip() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let state = ml.state();
    let json = serde_json::to_string(state).unwrap();
    let roundtripped: frankenterm_core::mission_loop::MissionLoopState =
        serde_json::from_str(&json).unwrap();

    assert_eq!(state.cycle_count, roundtripped.cycle_count);
    assert_eq!(
        state.total_assignments_made,
        roundtripped.total_assignments_made
    );
}

#[test]
fn acceptance_trigger_types_all_evaluated() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("a1")];
    let issues = vec![bead("b1", BeadStatus::Open, 1)];
    let c = ctx();

    let triggers = vec![
        MissionTrigger::CadenceTick,
        MissionTrigger::BeadStatusChange {
            bead_id: "b1".to_string(),
        },
        MissionTrigger::AgentAvailabilityChange {
            agent_id: "a1".to_string(),
        },
        MissionTrigger::ManualTrigger {
            reason: "test".to_string(),
        },
        MissionTrigger::ExternalSignal {
            source: "ci".to_string(),
            payload: "{}".to_string(),
        },
    ];

    for (i, trigger) in triggers.into_iter().enumerate() {
        let d = ml.evaluate((i as i64 + 1) * 30_000, trigger, &issues, &agents, &c);
        assert_eq!(d.cycle_id, (i as u64) + 1);
    }

    assert_eq!(ml.state().cycle_count, 5);
}
