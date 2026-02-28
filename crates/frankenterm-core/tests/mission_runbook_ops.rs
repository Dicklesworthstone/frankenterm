//! Runbook, incident response, and adoption guide validation tests.
//! [ft-1i2ge.6.6]
//!
//! Validates operational procedures for the mission system:
//! - Startup/launch checklist
//! - Failure injection and recovery playbooks
//! - Override management lifecycle (apply → inspect → clear)
//! - Configuration tuning patterns
//! - Diagnostic workflows (report generation, metric inspection)
//! - Incident response escalation paths

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::beads_types::{BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_events::{
    MissionEventBuilder, MissionEventKind, MissionEventLog, MissionEventLogConfig,
};
use frankenterm_core::mission_loop::{
    ConflictDetectionConfig, DeconflictionStrategy, KnownReservation, MissionLoop,
    MissionLoopConfig, MissionSafetyEnvelopeConfig, MissionTrigger, OperatorOverride,
    OperatorOverrideKind, format_operator_report_plain,
};
use frankenterm_core::plan::{MissionAgentAvailability, MissionAgentCapabilityProfile};
use frankenterm_core::planner_features::{PlannerExtractionContext, SolverConfig};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn agent(id: &str) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Ready,
    }
}

fn offline(id: &str) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Offline {
            reason_code: "runbook-test".to_string(),
        },
    }
}

fn issue(id: &str, priority: u8) -> BeadIssueDetail {
    BeadIssueDetail {
        id: id.to_string(),
        title: format!("Bead {id}"),
        status: BeadStatus::Open,
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

fn risky(id: &str) -> BeadIssueDetail {
    let mut b = issue(id, 1);
    b.labels = vec!["danger".to_string()];
    b
}

fn ctx() -> PlannerExtractionContext {
    PlannerExtractionContext::default()
}

fn elog() -> MissionEventLog {
    MissionEventLog::new(MissionEventLogConfig {
        max_events: 200,
        enabled: true,
    })
}

fn ovr(id: &str, kind: OperatorOverrideKind) -> OperatorOverride {
    OperatorOverride {
        override_id: id.to_string(),
        kind,
        activated_by: "runbook-test".to_string(),
        reason_code: "ops.runbook".to_string(),
        rationale: "Runbook procedure test".to_string(),
        activated_at_ms: 1000,
        expires_at_ms: None,
        correlation_id: Some("runbook".to_string()),
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// STARTUP / LAUNCH CHECKLIST
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn launch_default_config_produces_valid_initial_state() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    let state = ml.state();
    assert_eq!(state.cycle_count, 0);
    assert!(state.last_evaluation_ms.is_none());
    assert!(state.pending_triggers.is_empty());
    assert!(state.last_decision.is_none());
    assert_eq!(state.total_assignments_made, 0);
    assert_eq!(state.total_rejections, 0);
    assert!(state.metrics_history.is_empty());

    // Config is inspectable
    let config = ml.config();
    assert_eq!(config.cadence_ms, 30_000);
    assert_eq!(config.max_trigger_batch, 10);
    assert!(config.metrics.enabled);
    assert!(config.conflict_detection.enabled);
}

#[test]
fn launch_first_evaluation_always_proceeds() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    // Before any evaluation, should_evaluate is always true
    assert!(ml.should_evaluate(0));
    assert!(ml.should_evaluate(1_000_000));
}

#[test]
fn launch_config_serde_roundtrip() {
    let config = MissionLoopConfig {
        cadence_ms: 15_000,
        max_trigger_batch: 5,
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 3,
            max_risky_assignments_per_cycle: 1,
            max_consecutive_retries_per_bead: 2,
            risky_label_markers: vec!["danger".to_string()],
        },
        ..MissionLoopConfig::default()
    };

    let json = serde_json::to_string(&config).unwrap();
    let rt: MissionLoopConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(config.cadence_ms, rt.cadence_ms);
    assert_eq!(config.max_trigger_batch, rt.max_trigger_batch);
    assert_eq!(
        config.safety_envelope.max_assignments_per_cycle,
        rt.safety_envelope.max_assignments_per_cycle
    );
}

#[test]
fn launch_tick_returns_none_before_cadence() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    // First tick succeeds
    let first = ml.tick(0, &issues, &agents, &c);
    assert!(first.is_some());

    // Tick before cadence returns None
    let second = ml.tick(5_000, &issues, &agents, &c);
    assert!(second.is_none());
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// FAILURE INJECTION / RECOVERY PLAYBOOK
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn failure_inject_all_agents_offline_then_recover() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    // Step 1: Normal operation
    let healthy = vec![agent("a1"), agent("a2")];
    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &healthy, &c);
    assert!(ml.state().total_assignments_made > 0);

    // Step 2: Total fleet failure
    let failed = vec![offline("a1"), offline("a2")];
    let d = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &failed, &c);
    assert!(d.assignment_set.assignments.is_empty());

    // Step 3: Recovery — bring agents back
    let recovered = vec![agent("a1"), agent("a2")];
    let d = ml.evaluate(
        61_000,
        MissionTrigger::AgentAvailabilityChange {
            agent_id: "a1".to_string(),
        },
        &issues,
        &recovered,
        &c,
    );
    assert!(!d.assignment_set.assignments.is_empty());

    // Step 4: Verify report reflects recovery
    let report = ml.generate_operator_report(Some(&elog()), None);
    assert!(report.status.total_assignments > 0);
    assert_ne!(report.health.overall, "critical");
}

#[test]
fn failure_inject_empty_bead_list_graceful() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let empty_issues: Vec<BeadIssueDetail> = vec![];
    let c = ctx();

    // No beads available should not panic
    let d = ml.evaluate(
        1000,
        MissionTrigger::CadenceTick,
        &empty_issues,
        &agents,
        &c,
    );
    assert!(d.assignment_set.assignments.is_empty());
    assert_eq!(d.extraction_summary.total_candidates, 0);
}

#[test]
fn failure_inject_all_closed_beads_graceful() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let closed = vec![
        BeadIssueDetail {
            status: BeadStatus::Closed,
            ..issue("b1", 1)
        },
        BeadIssueDetail {
            status: BeadStatus::Closed,
            ..issue("b2", 2)
        },
    ];
    let c = ctx();

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &closed, &agents, &c);
    assert!(d.assignment_set.assignments.is_empty());
}

#[test]
fn failure_partial_agent_fleet_maintains_throughput() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    // 3 agents available
    let full_fleet = vec![agent("a1"), agent("a2"), agent("a3")];
    let d1 = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &full_fleet, &c);
    let _full_count = d1.assignment_set.assignments.len();

    // 1 agent goes offline — remaining agents should still get assignments
    let partial_fleet = vec![agent("a1"), offline("a2"), agent("a3")];
    let d2 = ml.evaluate(
        31_000,
        MissionTrigger::CadenceTick,
        &issues,
        &partial_fleet,
        &c,
    );
    assert!(!d2.assignment_set.assignments.is_empty());

    for a in &d2.assignment_set.assignments {
        assert_ne!(a.agent_id, "a2", "Offline agent should not get assignments");
    }
}

#[test]
fn failure_rapid_trigger_accumulation() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        cadence_ms: 300_000, // Very long cadence
        max_trigger_batch: 3,
        ..MissionLoopConfig::default()
    });
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    // First tick
    ml.evaluate(0, MissionTrigger::CadenceTick, &issues, &agents, &c);

    // Accumulate triggers below batch threshold
    ml.trigger(MissionTrigger::BeadStatusChange {
        bead_id: "b1".to_string(),
    });
    ml.trigger(MissionTrigger::AgentAvailabilityChange {
        agent_id: "a1".to_string(),
    });
    assert_eq!(ml.pending_trigger_count(), 2);
    assert!(!ml.should_evaluate(10_000)); // Below batch

    // One more trigger hits the batch threshold
    ml.trigger(MissionTrigger::ManualTrigger {
        reason: "escalation".to_string(),
    });
    assert_eq!(ml.pending_trigger_count(), 3);
    assert!(ml.should_evaluate(10_000)); // At batch threshold

    // Tick processes all pending triggers
    let d = ml.tick(10_000, &issues, &agents, &c);
    assert!(d.is_some());
    assert_eq!(ml.pending_trigger_count(), 0);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// OVERRIDE MANAGEMENT LIFECYCLE
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn override_full_lifecycle_apply_inspect_clear() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    // Step 1: No overrides initially
    assert!(ml.active_overrides().is_empty());

    // Step 2: Apply pin override
    ml.apply_override(ovr(
        "pin-b1",
        OperatorOverrideKind::Pin {
            bead_id: "b1".to_string(),
            target_agent: "a2".to_string(),
        },
    ))
    .unwrap();

    // Step 3: Inspect
    assert_eq!(ml.active_overrides().len(), 1);
    assert_eq!(ml.active_overrides()[0].override_id, "pin-b1");

    // Step 4: Clear
    let cleared = ml.clear_override("pin-b1", 5000);
    assert!(cleared);
    assert!(ml.active_overrides().is_empty());

    // Step 5: Clear non-existent returns false
    assert!(!ml.clear_override("nonexistent", 6000));
}

#[test]
fn override_duplicate_id_rejected() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    ml.apply_override(ovr(
        "excl-1",
        OperatorOverrideKind::Exclude {
            bead_id: "b1".to_string(),
        },
    ))
    .unwrap();

    // Duplicate ID should fail
    let result = ml.apply_override(ovr(
        "excl-1",
        OperatorOverrideKind::Exclude {
            bead_id: "b2".to_string(),
        },
    ));
    assert!(result.is_err());
}

#[test]
fn override_ttl_auto_eviction() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    let mut excl = ovr(
        "ttl-excl",
        OperatorOverrideKind::Exclude {
            bead_id: "b1".to_string(),
        },
    );
    excl.expires_at_ms = Some(10_000);
    ml.apply_override(excl).unwrap();

    // Before expiry — override is active
    assert_eq!(ml.active_overrides().len(), 1);
    let d1 = ml.evaluate(5000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    assert!(
        d1.assignment_set
            .assignments
            .iter()
            .all(|a| a.bead_id != "b1")
    );

    // After expiry — override evicted, bead assignable
    let d2 = ml.evaluate(35_000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    assert!(ml.active_overrides().is_empty());
    assert!(!d2.assignment_set.assignments.is_empty());
}

#[test]
fn override_summary_in_report_after_evaluation() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];

    ml.apply_override(ovr(
        "excl-b1",
        OperatorOverrideKind::Exclude {
            bead_id: "b1".to_string(),
        },
    ))
    .unwrap();

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let summary = ml.state().last_override_summary.as_ref().unwrap();
    assert!(summary.excluded_beads.contains(&"b1".to_string()));
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// CONFIGURATION TUNING
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn tuning_cadence_affects_evaluation_schedule() {
    let mut fast = MissionLoop::new(MissionLoopConfig {
        cadence_ms: 5_000,
        ..MissionLoopConfig::default()
    });
    let mut slow = MissionLoop::new(MissionLoopConfig {
        cadence_ms: 60_000,
        ..MissionLoopConfig::default()
    });
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    // First tick for both
    fast.evaluate(0, MissionTrigger::CadenceTick, &issues, &agents, &c);
    slow.evaluate(0, MissionTrigger::CadenceTick, &issues, &agents, &c);

    // At 10s: fast should be ready, slow should not
    assert!(fast.should_evaluate(10_000));
    assert!(!slow.should_evaluate(10_000));

    // At 65s: both should be ready
    assert!(fast.should_evaluate(65_000));
    assert!(slow.should_evaluate(65_000));
}

#[test]
fn tuning_safety_envelope_limits_assignment_count() {
    let restricted = MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 1,
            max_risky_assignments_per_cycle: 0,
            max_consecutive_retries_per_bead: 3,
            risky_label_markers: vec!["danger".to_string()],
        },
        ..MissionLoopConfig::default()
    };
    let mut ml = MissionLoop::new(restricted);
    let agents = vec![agent("a1"), agent("a2"), agent("a3")];
    let issues = vec![issue("b1", 1), issue("b2", 2), issue("b3", 3)];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(
        d.assignment_set.assignments.len() <= 1,
        "Safety envelope should cap at 1"
    );
}

#[test]
fn tuning_risky_label_markers_configurable() {
    let config = MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 10,
            max_risky_assignments_per_cycle: 0, // Zero risky allowed
            max_consecutive_retries_per_bead: 3,
            risky_label_markers: vec!["danger".to_string()],
        },
        ..MissionLoopConfig::default()
    };
    let mut ml = MissionLoop::new(config);
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![risky("r1"), risky("r2"), issue("safe", 2)];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let risky_count = d
        .assignment_set
        .assignments
        .iter()
        .filter(|a| a.bead_id.starts_with('r'))
        .count();
    assert_eq!(risky_count, 0, "No risky beads allowed with cap=0");
}

#[test]
fn tuning_solver_max_assignments_limits_total() {
    let config = MissionLoopConfig {
        solver_config: SolverConfig {
            max_assignments: 2,
            ..Default::default()
        },
        ..MissionLoopConfig::default()
    };
    let mut ml = MissionLoop::new(config);
    let agents = vec![agent("a1"), agent("a2"), agent("a3")];
    let issues = vec![
        issue("b1", 1),
        issue("b2", 2),
        issue("b3", 3),
        issue("b4", 4),
    ];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(
        d.assignment_set.assignments.len() <= 2,
        "Solver max_assignments should cap output"
    );
}

#[test]
fn tuning_conflict_detection_strategy_selectable() {
    for strategy in [
        DeconflictionStrategy::PriorityWins,
        DeconflictionStrategy::FirstClaimWins,
        DeconflictionStrategy::ManualResolution,
    ] {
        let config = MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                enabled: true,
                max_conflicts_per_cycle: 10,
                strategy,
                generate_messages: true,
            },
            ..MissionLoopConfig::default()
        };
        let ml = MissionLoop::new(config);
        assert!(ml.config().conflict_detection.enabled);
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// DIAGNOSTIC WORKFLOWS
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn diag_report_available_at_any_time() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    // Report should be available even before any evaluation
    let report = ml.generate_operator_report(None, None);
    assert_eq!(report.status.phase_label, "idle");
    assert_eq!(report.health.overall, "idle");
}

#[test]
fn diag_report_with_event_log_includes_events() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let mut event_log = elog();
    event_log.emit(
        MissionEventBuilder::new(MissionEventKind::CycleStarted, "diag.runbook")
            .cycle(1, 1000)
            .labels("ops", "runbook"),
    );
    event_log.emit(
        MissionEventBuilder::new(MissionEventKind::CycleCompleted, "diag.runbook")
            .cycle(1, 1050)
            .labels("ops", "runbook"),
    );

    let report = ml.generate_operator_report(Some(&event_log), None);
    assert_eq!(report.event_summary.total_emitted, 2);
}

#[test]
fn diag_plain_text_report_human_readable() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];

    for i in 0..3 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &ctx(),
        );
    }

    let report = ml.generate_operator_report(Some(&elog()), None);
    let text = format_operator_report_plain(&report);

    // Must have all standard sections for operator triage
    assert!(text.contains("=== Mission Status ==="));
    assert!(text.contains("=== Health ==="));
    assert!(text.contains("Phase:"));
    assert!(text.contains("Overall:"));
    assert!(text.contains("Throughput:"));
    assert!(text.contains("assign/min"));
    assert!(text.contains("Conflict rate:"));
    assert!(text.contains("Churn rate:"));
}

#[test]
fn diag_latest_metrics_accessible() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    assert!(ml.latest_metrics().is_none());

    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let metrics = ml.latest_metrics().unwrap();
    assert_eq!(metrics.cycle_id, 1);
    assert!(metrics.assignments > 0);
}

#[test]
fn diag_conflict_stats_reflect_detected_conflicts() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            enabled: true,
            max_conflicts_per_cycle: 20,
            strategy: DeconflictionStrategy::PriorityWins,
            generate_messages: true,
        },
        ..MissionLoopConfig::default()
    });
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    // Create overlapping file reservations → conflict
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
    let cr = ml.detect_conflicts(&d.assignment_set, &reservations, &[], 1000, &issues);
    assert!(!cr.conflicts.is_empty());

    let (total, _auto) = ml.conflict_stats();
    assert!(total > 0);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// INCIDENT RESPONSE ESCALATION
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn incident_manual_trigger_for_emergency_evaluation() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        cadence_ms: 300_000,
        max_trigger_batch: 1, // Single trigger forces evaluation
        ..MissionLoopConfig::default()
    });
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    ml.evaluate(0, MissionTrigger::CadenceTick, &issues, &agents, &c);

    // Not time for cadence
    assert!(!ml.should_evaluate(10_000));

    // Operator triggers manual evaluation for incident
    ml.trigger(MissionTrigger::ManualTrigger {
        reason: "incident-response".to_string(),
    });
    assert!(ml.should_evaluate(10_000));

    let d = ml.tick(10_000, &issues, &agents, &c);
    assert!(d.is_some());
}

#[test]
fn incident_emergency_exclude_bead_via_override() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    // Evaluate — both beads available
    let d1 = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    let has_b1 = d1
        .assignment_set
        .assignments
        .iter()
        .any(|a| a.bead_id == "b1");
    assert!(has_b1 || !d1.assignment_set.assignments.is_empty());

    // Incident: exclude b1 immediately
    ml.apply_override(ovr(
        "incident-excl",
        OperatorOverrideKind::Exclude {
            bead_id: "b1".to_string(),
        },
    ))
    .unwrap();

    let d2 = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    assert!(
        d2.assignment_set
            .assignments
            .iter()
            .all(|a| a.bead_id != "b1"),
        "b1 must be excluded after override"
    );
}

#[test]
fn incident_pin_to_specific_agent() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2"), agent("a3")];
    let issues = vec![issue("b1", 1)];

    ml.apply_override(ovr(
        "pin-to-a3",
        OperatorOverrideKind::Pin {
            bead_id: "b1".to_string(),
            target_agent: "a3".to_string(),
        },
    ))
    .unwrap();

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let b1_assignment = d
        .assignment_set
        .assignments
        .iter()
        .find(|a| a.bead_id == "b1");
    assert!(b1_assignment.is_some());
    assert_eq!(
        b1_assignment.unwrap().agent_id,
        "a3",
        "Pin override must force assignment to a3"
    );
}

#[test]
fn incident_exclude_agent_under_investigation() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1)];

    ml.apply_override(ovr(
        "exclude-suspect",
        OperatorOverrideKind::ExcludeAgent {
            agent_id: "a1".to_string(),
        },
    ))
    .unwrap();

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    for a in &d.assignment_set.assignments {
        assert_ne!(
            a.agent_id, "a1",
            "Excluded agent must not receive assignments"
        );
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// ADOPTION GUIDE VALIDATION
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn adoption_minimal_viable_setup() {
    // Simplest possible setup: default config, one agent, one bead
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let d = ml.evaluate(
        1000,
        MissionTrigger::CadenceTick,
        &[issue("first-task", 1)],
        &[agent("my-agent")],
        &PlannerExtractionContext::default(),
    );
    assert!(!d.assignment_set.assignments.is_empty());
    assert_eq!(d.assignment_set.assignments[0].agent_id, "my-agent");
}

#[test]
fn adoption_observe_before_modify_pattern() {
    // Adoption pattern: observe state → understand metrics → then tune
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    // Run several cycles to build metric history
    for i in 0..5 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    // Observe: generate report
    let report = ml.generate_operator_report(Some(&elog()), None);
    let text = format_operator_report_plain(&report);

    // Operator can inspect all metrics before making tuning decisions
    assert!(report.status.cycle_count >= 5);
    assert!(!text.is_empty());
    assert!(ml.latest_metrics().is_some());
}

#[test]
fn adoption_state_serde_for_persistence() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    // State can be serialized for persistence across restarts
    let state = ml.state();
    let json = serde_json::to_string(state).unwrap();
    let _rt: frankenterm_core::mission_loop::MissionLoopState =
        serde_json::from_str(&json).unwrap();
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// DETERMINISM
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn determinism_runbook_procedures_identical() {
    let run = || {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let agents = vec![agent("a1"), agent("a2")];
        let issues = vec![issue("b1", 1), issue("b2", 2)];
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

        let report = ml.generate_operator_report(Some(&elog()), None);
        serde_json::to_string(&report).unwrap()
    };

    assert_eq!(run(), run());
}
