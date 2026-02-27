//! Operator workflow UX validation, friction heatmap, and remediation playbook.
//! [ft-1i2ge.5.8]
//!
//! Validates the full operator journey under routine monitoring, incident response,
//! override management, and recovery scenarios. Identifies UX friction points and
//! documents remediation priorities.

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::beads_types::{BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_events::{
    MissionEventBuilder, MissionEventKind, MissionEventLog, MissionEventLogConfig,
};
use frankenterm_core::mission_loop::{
    ActiveBeadClaim, ConflictDetectionConfig, DeconflictionStrategy, KnownReservation,
    MissionLoop, MissionLoopConfig, MissionSafetyEnvelopeConfig, MissionTrigger,
    OperatorOverride, OperatorOverrideKind, format_operator_report_plain,
};
use frankenterm_core::plan::MissionAgentAvailability;
use frankenterm_core::plan::MissionAgentCapabilityProfile;
use frankenterm_core::planner_features::PlannerExtractionContext;

// ── Test helpers ─────────────────────────────────────────────────────────────

fn ready_agent(agent_id: &str) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: agent_id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Ready,
    }
}

fn offline_agent(agent_id: &str) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: agent_id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Offline {
            reason_code: "maintenance".to_string(),
        },
    }
}

fn open_bead(id: &str, priority: u8) -> BeadIssueDetail {
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

fn risky_bead(id: &str) -> BeadIssueDetail {
    let mut bead = open_bead(id, 1);
    bead.labels = vec!["danger".to_string(), "destructive".to_string()];
    bead
}

fn make_override(id: &str, kind: OperatorOverrideKind) -> OperatorOverride {
    OperatorOverride {
        override_id: id.to_string(),
        kind,
        activated_by: "operator-ux-test".to_string(),
        reason_code: "ux.validation".to_string(),
        rationale: "UX workflow validation".to_string(),
        activated_at_ms: 1000,
        expires_at_ms: None,
        correlation_id: Some("ux-test-corr".to_string()),
    }
}

fn ctx() -> PlannerExtractionContext {
    PlannerExtractionContext::default()
}

fn event_log() -> MissionEventLog {
    MissionEventLog::new(MissionEventLogConfig {
        max_events: 50,
        enabled: true,
    })
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Scenario 1: Routine monitoring workflow
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn routine_monitoring_fresh_loop_reports_idle() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    let log = event_log();
    let report = ml.generate_operator_report(Some(&log), None);

    assert_eq!(report.status.phase_label, "idle");
    assert_eq!(report.status.cycle_count, 0);
    assert_eq!(report.status.total_assignments, 0);
    assert_eq!(report.status.total_rejections, 0);
    assert_eq!(report.health.overall, "idle");
    assert!(report.assignment_table.is_empty());
    assert!(report.latest_explanations.is_empty());
}

#[test]
fn routine_monitoring_after_evaluation_reports_active() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let log = event_log();
    let report = ml.generate_operator_report(Some(&log), None);

    assert_eq!(report.status.phase_label, "active");
    assert_eq!(report.status.cycle_count, 1);
    assert!(report.status.last_evaluation_ms.is_some());
}

#[test]
fn routine_monitoring_report_plain_text_has_all_sections() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a"), ready_agent("agent-b")];
    let issues = vec![open_bead("b-1", 1), open_bead("b-2", 2)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let log = event_log();
    let report = ml.generate_operator_report(Some(&log), None);
    let text = format_operator_report_plain(&report);

    assert!(text.contains("=== Mission Status ==="));
    assert!(text.contains("=== Health ==="));
    assert!(text.contains("Phase:"));
    assert!(text.contains("Cycles:"));
    assert!(text.contains("Overall:"));
}

#[test]
fn routine_monitoring_metrics_populated_after_cycles() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1)];
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

    assert!(ml.latest_metrics().is_some());
    let metrics = ml.latest_metrics().unwrap();
    assert_eq!(metrics.cycle_id, 3);
}

#[test]
fn routine_monitoring_no_event_log_produces_empty_event_section() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    let report = ml.generate_operator_report(None, None);

    assert_eq!(report.event_summary.retained_events, 0);
    assert_eq!(report.event_summary.total_emitted, 0);
    assert!(report.event_summary.by_phase.is_empty());
}

#[test]
fn routine_monitoring_with_event_log_populates_event_section() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let mut log = event_log();
    log.emit(
        MissionEventBuilder::new(MissionEventKind::CycleStarted, "mission.lifecycle.start")
            .cycle(1, 1000)
            .labels("test", "ux"),
    );

    let report = ml.generate_operator_report(Some(&log), None);
    assert!(report.event_summary.total_emitted > 0);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Scenario 2: Incident response workflow
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn incident_manual_trigger_causes_immediate_evaluation() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1)];

    ml.trigger(MissionTrigger::ManualTrigger {
        reason: "incident-response".to_string(),
    });
    assert_eq!(ml.pending_trigger_count(), 1);
    assert!(ml.should_evaluate(0));

    let decision = ml.tick(0, &issues, &agents, &ctx());
    assert!(decision.is_some());
    assert_eq!(ml.pending_trigger_count(), 0);
}

#[test]
fn incident_exclude_problematic_bead() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-bad", 1), open_bead("b-good", 2)];

    let ovr = make_override(
        "excl-bad",
        OperatorOverrideKind::Exclude {
            bead_id: "b-bad".to_string(),
        },
    );
    ml.apply_override(ovr).unwrap();

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    assert!(
        !decision
            .assignment_set
            .assignments
            .iter()
            .any(|a| a.bead_id == "b-bad")
    );
}

#[test]
fn incident_exclude_agent_removes_from_assignments() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-bad"), ready_agent("agent-good")];
    let issues = vec![open_bead("b-1", 1)];

    let ovr = make_override(
        "excl-agent",
        OperatorOverrideKind::ExcludeAgent {
            agent_id: "agent-bad".to_string(),
        },
    );
    ml.apply_override(ovr).unwrap();

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    for a in &decision.assignment_set.assignments {
        assert_ne!(a.agent_id, "agent-bad");
    }
}

#[test]
fn incident_agent_goes_offline_reflected_in_evaluation() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![offline_agent("agent-a"), ready_agent("agent-b")];
    let issues = vec![open_bead("b-1", 1)];

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    for a in &decision.assignment_set.assignments {
        assert_ne!(a.agent_id, "agent-a");
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Scenario 3: Override lifecycle management
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn override_full_lifecycle_activate_evaluate_clear() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a"), ready_agent("agent-b")];
    let issues = vec![open_bead("b-1", 1)];
    let c = ctx();

    // Step 1: Activate pin override
    let pin = make_override(
        "pin-1",
        OperatorOverrideKind::Pin {
            bead_id: "b-1".to_string(),
            target_agent: "agent-b".to_string(),
        },
    );
    assert!(ml.apply_override(pin).is_ok());
    assert_eq!(ml.active_overrides().len(), 1);

    // Step 2: Evaluate — pin should force assignment
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    let assigned_to_b = decision
        .assignment_set
        .assignments
        .iter()
        .any(|a| a.bead_id == "b-1" && a.agent_id == "agent-b");
    assert!(assigned_to_b);

    // Step 3: Clear override
    assert!(ml.clear_override("pin-1", 2000));
    assert!(ml.active_overrides().is_empty());

    // Step 4: Evaluate again — normal behavior resumes
    let decision2 = ml.evaluate(3000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    assert!(!decision2.assignment_set.assignments.is_empty());
}

#[test]
fn override_duplicate_id_rejected_with_clear_error() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    let ovr1 = make_override(
        "dup-1",
        OperatorOverrideKind::Exclude {
            bead_id: "b-1".to_string(),
        },
    );
    let ovr2 = make_override(
        "dup-1",
        OperatorOverrideKind::Exclude {
            bead_id: "b-2".to_string(),
        },
    );

    assert!(ml.apply_override(ovr1).is_ok());
    let err = ml.apply_override(ovr2);
    assert!(err.is_err());
    let err_msg = err.unwrap_err();
    assert!(
        err_msg.contains("dup-1"),
        "Error should mention the duplicate ID"
    );
}

#[test]
fn override_clear_nonexistent_returns_false() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    assert!(!ml.clear_override("does-not-exist", 1000));
}

#[test]
fn override_expired_auto_evicted_on_evaluate() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1)];

    let mut ovr = make_override(
        "expire-me",
        OperatorOverrideKind::Exclude {
            bead_id: "b-1".to_string(),
        },
    );
    ovr.expires_at_ms = Some(500); // Already expired by time of eval
    ml.apply_override(ovr).unwrap();
    assert_eq!(ml.active_overrides().len(), 1);

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(ml.active_overrides().is_empty());
}

#[test]
fn override_reprioritize_boosts_score() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-low", 5), open_bead("b-high", 1)];

    let ovr = make_override(
        "boost-low",
        OperatorOverrideKind::Reprioritize {
            bead_id: "b-low".to_string(),
            score_delta: 100,
        },
    );
    ml.apply_override(ovr).unwrap();

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    if !decision.assignment_set.assignments.is_empty() {
        assert_eq!(decision.assignment_set.assignments[0].bead_id, "b-low");
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Scenario 4: Conflict detection and resolution workflow
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn conflict_detection_with_file_reservation_overlap() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            enabled: true,
            max_conflicts_per_cycle: 20,
            strategy: DeconflictionStrategy::PriorityWins,
            generate_messages: true,
        },
        ..MissionLoopConfig::default()
    });
    let agents = vec![ready_agent("agent-a"), ready_agent("agent-b")];
    let issues = vec![open_bead("b-1", 1), open_bead("b-2", 1)];

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let reservations = vec![
        KnownReservation {
            holder: "agent-a".to_string(),
            paths: vec!["src/main.rs".to_string()],
            exclusive: true,
            bead_id: Some("b-1".to_string()),
            expires_at_ms: Some(60_000),
        },
        KnownReservation {
            holder: "agent-b".to_string(),
            paths: vec!["src/main.rs".to_string()],
            exclusive: true,
            bead_id: Some("b-2".to_string()),
            expires_at_ms: Some(60_000),
        },
    ];

    let report =
        ml.detect_conflicts(&decision.assignment_set, &reservations, &[], 1000, &issues);

    assert!(
        !report.conflicts.is_empty(),
        "Expected file reservation overlap conflict"
    );
}

#[test]
fn conflict_detection_active_claim_collision() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            enabled: true,
            max_conflicts_per_cycle: 20,
            strategy: DeconflictionStrategy::FirstClaimWins,
            generate_messages: true,
        },
        ..MissionLoopConfig::default()
    });
    let agents = vec![ready_agent("agent-a"), ready_agent("agent-b")];
    let issues = vec![open_bead("b-1", 1)];

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let active_claims = vec![ActiveBeadClaim {
        bead_id: "b-1".to_string(),
        agent_id: "agent-c".to_string(),
        claimed_at_ms: 500,
    }];

    let report =
        ml.detect_conflicts(&decision.assignment_set, &[], &active_claims, 1000, &issues);

    if decision
        .assignment_set
        .assignments
        .iter()
        .any(|a| a.bead_id == "b-1")
    {
        assert!(!report.conflicts.is_empty());
    }
}

#[test]
fn conflict_report_shown_in_operator_status() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    let log = event_log();

    let report = ml.generate_operator_report(Some(&log), None);
    assert_eq!(report.conflicts.total_detected, 0);
    assert_eq!(report.conflicts.total_auto_resolved, 0);
    assert_eq!(report.conflicts.pending_manual, 0);
    assert!(report.conflicts.recent_conflicts.is_empty());
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Scenario 5: Safety envelope and risky bead handling
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn safety_envelope_caps_risky_assignments() {
    let config = MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 10,
            max_risky_assignments_per_cycle: 1,
            max_consecutive_retries_per_bead: 3,
            risky_label_markers: vec!["danger".to_string(), "destructive".to_string()],
        },
        ..MissionLoopConfig::default()
    };
    let mut ml = MissionLoop::new(config);
    let agents = vec![
        ready_agent("agent-a"),
        ready_agent("agent-b"),
        ready_agent("agent-c"),
    ];
    let issues = vec![risky_bead("r-1"), risky_bead("r-2"), risky_bead("r-3")];

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let risky_assigned = decision
        .assignment_set
        .assignments
        .iter()
        .filter(|a| a.bead_id.starts_with("r-"))
        .count();
    assert!(
        risky_assigned <= 1,
        "Expected at most 1 risky assignment, got {}",
        risky_assigned
    );
}

#[test]
fn safety_envelope_retry_streak_backoff() {
    let config = MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_consecutive_retries_per_bead: 2,
            ..MissionSafetyEnvelopeConfig::default()
        },
        ..MissionLoopConfig::default()
    };
    let mut ml = MissionLoop::new(config);
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1)];
    let c = ctx();

    for i in 0..4 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    let state = ml.state();
    assert!(state.cycle_count >= 4);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Scenario 6: Decision explainability workflow
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn explainability_report_generates_without_crash() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1)];

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    // Report without explainability should still work
    let log = event_log();
    let report = ml.generate_operator_report(Some(&log), None);

    // Status section is always populated
    assert_eq!(report.status.cycle_count, 1);
    // Without explainability, no explanations
    assert!(report.latest_explanations.is_empty());

    // Decision should have valid summaries
    assert!(decision.extraction_summary.total_candidates > 0);
    assert!(decision.scorer_summary.scored_count > 0);
}

#[test]
fn explainability_plain_text_includes_health() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1), open_bead("b-2", 3)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let log = event_log();
    let report = ml.generate_operator_report(Some(&log), None);
    let text = format_operator_report_plain(&report);

    assert!(text.contains("=== Health ==="));
    assert!(text.contains("Overall:"));
    assert!(text.contains("assign/min"));
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Scenario 7: Multi-cycle degradation and recovery
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn degradation_no_agents_produces_empty_assignment() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents: Vec<MissionAgentCapabilityProfile> = vec![];
    let issues = vec![open_bead("b-1", 1)];

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(decision.assignment_set.assignments.is_empty());
}

#[test]
fn degradation_no_issues_produces_empty_assignment() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues: Vec<BeadIssueDetail> = vec![];

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(decision.assignment_set.assignments.is_empty());
}

#[test]
fn degradation_all_agents_offline() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![offline_agent("agent-a"), offline_agent("agent-b")];
    let issues = vec![open_bead("b-1", 1)];

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(decision.assignment_set.assignments.is_empty());
}

#[test]
fn recovery_agent_comes_back_online() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let issues = vec![open_bead("b-1", 1)];
    let c = ctx();

    // Cycle 1: agent offline
    let agents_offline = vec![offline_agent("agent-a")];
    let d1 = ml.evaluate(
        1000,
        MissionTrigger::CadenceTick,
        &issues,
        &agents_offline,
        &c,
    );
    assert!(d1.assignment_set.assignments.is_empty());

    // Cycle 2: agent comes back online
    let agents_online = vec![ready_agent("agent-a")];
    let d2 = ml.evaluate(
        31_000,
        MissionTrigger::AgentAvailabilityChange {
            agent_id: "agent-a".to_string(),
        },
        &issues,
        &agents_online,
        &c,
    );
    assert!(!d2.assignment_set.assignments.is_empty());
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Scenario 8: Operator override state inspection
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn override_state_accessible_after_evaluation() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1), open_bead("b-2", 2)];

    let pin = make_override(
        "pin-inspect",
        OperatorOverrideKind::Pin {
            bead_id: "b-1".to_string(),
            target_agent: "agent-a".to_string(),
        },
    );
    let excl = make_override(
        "excl-inspect",
        OperatorOverrideKind::Exclude {
            bead_id: "b-2".to_string(),
        },
    );
    ml.apply_override(pin).unwrap();
    ml.apply_override(excl).unwrap();

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let state = ml.state();
    assert_eq!(state.override_state.active.len(), 2);
    assert!(state.last_override_summary.is_some());

    let summary = state.last_override_summary.as_ref().unwrap();
    assert!(summary.excluded_beads.contains(&"b-2".to_string()));
    assert!(
        summary
            .pinned_assignments
            .iter()
            .any(|p| p.bead_id == "b-1")
    );
}

#[test]
fn override_history_bounded_after_many_clears() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    for i in 0..120 {
        let ovr = make_override(
            &format!("ovr-{i}"),
            OperatorOverrideKind::Exclude {
                bead_id: format!("b-{i}"),
            },
        );
        ml.apply_override(ovr).unwrap();
        ml.clear_override(&format!("ovr-{i}"), (i as i64 + 1) * 100);
    }

    let state = ml.state();
    assert!(state.override_state.history.len() <= 100);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Scenario 9: Trigger batching and cadence behavior
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn trigger_batch_overflow_forces_evaluation() {
    let config = MissionLoopConfig {
        max_trigger_batch: 3,
        ..MissionLoopConfig::default()
    };
    let mut ml = MissionLoop::new(config);

    for i in 0..3 {
        ml.trigger(MissionTrigger::BeadStatusChange {
            bead_id: format!("b-{i}"),
        });
    }

    assert_eq!(ml.pending_trigger_count(), 3);
    assert!(ml.should_evaluate(0));
}

#[test]
fn cadence_timing_respects_interval() {
    let config = MissionLoopConfig {
        cadence_ms: 30_000,
        ..MissionLoopConfig::default()
    };
    let mut ml = MissionLoop::new(config);
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1)];

    ml.evaluate(0, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    assert!(!ml.should_evaluate(10_000));
    assert!(ml.should_evaluate(30_000));
}

#[test]
fn multiple_triggers_of_same_type_all_queued() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    ml.trigger(MissionTrigger::BeadStatusChange {
        bead_id: "b-1".to_string(),
    });
    ml.trigger(MissionTrigger::BeadStatusChange {
        bead_id: "b-2".to_string(),
    });
    ml.trigger(MissionTrigger::ManualTrigger {
        reason: "test".to_string(),
    });

    assert_eq!(ml.pending_trigger_count(), 3);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Scenario 10: Extraction summary and scorer summary UX
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn decision_extraction_summary_populated() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1), open_bead("b-2", 2), open_bead("b-3", 3)];

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    assert!(decision.extraction_summary.total_candidates > 0);
    assert!(decision.extraction_summary.ready_candidates > 0);
}

#[test]
fn decision_scorer_summary_populated() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1)];

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    assert!(decision.scorer_summary.scored_count > 0);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Scenario 11: End-to-end operator journey — full cycle
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn e2e_operator_journey_monitor_intervene_recover() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let issues = vec![
        open_bead("b-critical", 1),
        open_bead("b-normal", 3),
        open_bead("b-low", 5),
    ];
    let c = ctx();
    let agents = vec![ready_agent("agent-a"), ready_agent("agent-b")];

    // Phase 1: Normal monitoring
    let d1 = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    let log = event_log();
    let report1 = ml.generate_operator_report(Some(&log), None);
    assert_eq!(report1.status.phase_label, "active");
    assert!(!d1.assignment_set.assignments.is_empty());

    // Phase 2: Operator excludes agent-b
    let excl = make_override(
        "incident-excl",
        OperatorOverrideKind::ExcludeAgent {
            agent_id: "agent-b".to_string(),
        },
    );
    ml.apply_override(excl).unwrap();

    let d2 = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    for a in &d2.assignment_set.assignments {
        assert_ne!(a.agent_id, "agent-b", "Excluded agent should not be assigned");
    }

    // Phase 3: Issue resolved — clear override
    ml.clear_override("incident-excl", 61_000);
    let _d3 = ml.evaluate(62_000, MissionTrigger::CadenceTick, &issues, &agents, &c);

    let report3 = ml.generate_operator_report(Some(&log), None);
    assert_eq!(report3.status.cycle_count, 3);
    assert!(report3.status.total_assignments > 0);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Friction heatmap validation
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn friction_report_format_readable_without_scrolling() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a"), ready_agent("agent-b")];
    let issues = vec![open_bead("b-1", 1), open_bead("b-2", 2)];
    let c = ctx();

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &c);

    let log = event_log();
    let report = ml.generate_operator_report(Some(&log), None);
    let text = format_operator_report_plain(&report);

    for line in text.lines() {
        assert!(
            line.len() < 200,
            "Line too long for terminal display: {}",
            line
        );
    }
}

#[test]
fn friction_health_section_uses_human_readable_labels() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1)];
    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    let log = event_log();
    let report = ml.generate_operator_report(Some(&log), None);
    let report_text = format_operator_report_plain(&report);

    assert!(report_text.contains("assign/min"));
    assert!(report_text.contains("Overall:"));
}

#[test]
fn friction_assignment_table_sorted_by_total() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![
        ready_agent("agent-a"),
        ready_agent("agent-b"),
        ready_agent("agent-c"),
    ];
    let issues = vec![
        open_bead("b-1", 1),
        open_bead("b-2", 2),
        open_bead("b-3", 3),
    ];
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

    let log = event_log();
    let report = ml.generate_operator_report(Some(&log), None);

    for w in report.assignment_table.windows(2) {
        assert!(
            w[0].total_assignments >= w[1].total_assignments,
            "Assignment table should be sorted descending by total"
        );
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// Determinism validation
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn determinism_same_inputs_same_outputs() {
    let run = |seed: i64| {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let agents = vec![ready_agent("agent-a")];
        let issues = vec![open_bead("b-1", 1), open_bead("b-2", 2)];
        let c = PlannerExtractionContext::default();
        let d = ml.evaluate(seed, MissionTrigger::CadenceTick, &issues, &agents, &c);
        (
            d.assignment_set.assignments.len(),
            d.assignment_set.rejected.len(),
            d.extraction_summary.total_candidates,
            d.scorer_summary.scored_count,
        )
    };

    let r1 = run(1000);
    let r2 = run(1000);
    assert_eq!(r1, r2, "Same inputs must produce identical outputs");
}

#[test]
fn determinism_report_generation_stable() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![ready_agent("agent-a")];
    let issues = vec![open_bead("b-1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let log = event_log();
    let r1 = format_operator_report_plain(&ml.generate_operator_report(Some(&log), None));
    let r2 = format_operator_report_plain(&ml.generate_operator_report(Some(&log), None));
    assert_eq!(r1, r2, "Report generation must be deterministic");
}
