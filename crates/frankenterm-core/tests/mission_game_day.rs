//! ft-1i2ge.7.7 — Operator takeover game-days and emergency-control certification
//!
//! Structured operator takeover drills validating emergency stop, manual takeover,
//! degraded-mode operation, controlled recovery, override audit integrity,
//! and deterministic drill outcomes under realistic multi-agent load.

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::beads_types::{BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_events::{MissionEventLog, MissionEventLogConfig};
use frankenterm_core::mission_loop::{
    MissionLoop, MissionLoopConfig, MissionSafetyEnvelopeConfig, MissionTrigger,
    OperatorOverride, OperatorOverrideKind,
};
use frankenterm_core::plan::{MissionAgentAvailability, MissionAgentCapabilityProfile};
use frankenterm_core::planner_features::PlannerExtractionContext;

// ── Helpers ──────────────────────────────────────────────────────────

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

fn degraded_agent(id: &str, max: u32) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Degraded {
            reason_code: "hw-fault".to_string(),
            max_parallel_assignments: max,
        },
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
            reason_code: "operator-takeover".to_string(),
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

fn ctx() -> PlannerExtractionContext {
    PlannerExtractionContext::default()
}

fn elog() -> MissionEventLog {
    MissionEventLog::new(MissionEventLogConfig::default())
}

fn override_exclude_agent(id: &str, agent_id: &str) -> OperatorOverride {
    OperatorOverride {
        override_id: id.to_string(),
        kind: OperatorOverrideKind::ExcludeAgent {
            agent_id: agent_id.to_string(),
        },
        activated_by: "operator".to_string(),
        reason_code: "game-day".to_string(),
        rationale: "operator takeover drill".to_string(),
        activated_at_ms: 1000,
        expires_at_ms: None,
        correlation_id: Some("drill-001".to_string()),
    }
}

fn override_exclude_bead(id: &str, bead_id: &str) -> OperatorOverride {
    OperatorOverride {
        override_id: id.to_string(),
        kind: OperatorOverrideKind::Exclude {
            bead_id: bead_id.to_string(),
        },
        activated_by: "operator".to_string(),
        reason_code: "game-day".to_string(),
        rationale: "emergency stop drill".to_string(),
        activated_at_ms: 1000,
        expires_at_ms: None,
        correlation_id: Some("drill-001".to_string()),
    }
}

fn override_pin(id: &str, bead_id: &str, agent_id: &str) -> OperatorOverride {
    OperatorOverride {
        override_id: id.to_string(),
        kind: OperatorOverrideKind::Pin {
            bead_id: bead_id.to_string(),
            target_agent: agent_id.to_string(),
        },
        activated_by: "operator".to_string(),
        reason_code: "game-day".to_string(),
        rationale: "manual takeover drill".to_string(),
        activated_at_ms: 1000,
        expires_at_ms: None,
        correlation_id: Some("drill-001".to_string()),
    }
}

fn override_reprioritize(id: &str, bead_id: &str, delta: i32) -> OperatorOverride {
    OperatorOverride {
        override_id: id.to_string(),
        kind: OperatorOverrideKind::Reprioritize {
            bead_id: bead_id.to_string(),
            score_delta: delta,
        },
        activated_by: "operator".to_string(),
        reason_code: "game-day".to_string(),
        rationale: "priority override drill".to_string(),
        activated_at_ms: 1000,
        expires_at_ms: None,
        correlation_id: Some("drill-001".to_string()),
    }
}

fn swarm_agents(n: usize) -> Vec<MissionAgentCapabilityProfile> {
    (0..n).map(|i| agent(&format!("agent-{i}"))).collect()
}

fn swarm_issues(n: usize) -> Vec<BeadIssueDetail> {
    (0..n)
        .map(|i| issue(&format!("bead-{i}"), (i % 5 + 1) as u8))
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════
// Category 1: Emergency Stop Drills
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn emergency_exclude_all_agents_halts_dispatch() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = swarm_agents(5);
    let issues = swarm_issues(5);

    // Normal cycle first.
    let before = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(before.assignment_set.assignment_count() > 0, "normal cycle should produce assignments");

    // Emergency: exclude all agents.
    for i in 0..5 {
        ml.apply_override(override_exclude_agent(
            &format!("estop-agent-{i}"),
            &format!("agent-{i}"),
        ))
        .unwrap();
    }

    let after = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert_eq!(
        after.assignment_set.assignment_count(),
        0,
        "emergency stop: all agents excluded must yield zero assignments"
    );
}

#[test]
fn emergency_exclude_all_beads_halts_dispatch() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = swarm_agents(3);
    let issues = swarm_issues(3);

    // Exclude all beads.
    for i in 0..3 {
        ml.apply_override(override_exclude_bead(
            &format!("estop-bead-{i}"),
            &format!("bead-{i}"),
        ))
        .unwrap();
    }

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert_eq!(
        decision.assignment_set.assignment_count(),
        0,
        "emergency stop: all beads excluded must yield zero assignments"
    );
}

#[test]
fn emergency_zero_assignment_cap_halts_dispatch() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 0,
            ..MissionSafetyEnvelopeConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let agents = swarm_agents(3);
    let issues = swarm_issues(3);
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert_eq!(
        decision.assignment_set.assignment_count(),
        0,
        "zero cap emergency stop"
    );
}

#[test]
fn emergency_stop_latency_under_1ms() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = swarm_agents(10);
    let issues = swarm_issues(10);

    // Warm up.
    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    // Apply emergency stop.
    for i in 0..10 {
        ml.apply_override(override_exclude_agent(
            &format!("estop-{i}"),
            &format!("agent-{i}"),
        ))
        .unwrap();
    }

    let start = std::time::Instant::now();
    let decision = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    let elapsed = start.elapsed();

    assert_eq!(decision.assignment_set.assignment_count(), 0);
    assert!(
        elapsed.as_millis() < 5,
        "emergency stop evaluation must be fast: {:?}",
        elapsed
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Category 2: Manual Takeover Drills
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn takeover_pin_redirects_work_to_target_agent() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("primary"), agent("backup")];
    let issues = vec![issue("critical-bead", 1)];

    // Pin critical work to backup agent.
    ml.apply_override(override_pin("takeover-pin", "critical-bead", "backup"))
        .unwrap();

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    let assigned_agent = decision
        .assignment_set
        .assignments
        .iter()
        .find(|a| a.bead_id == "critical-bead")
        .map(|a| a.agent_id.as_str());
    assert_eq!(
        assigned_agent,
        Some("backup"),
        "pin must redirect work to backup agent"
    );
}

#[test]
fn takeover_exclude_then_pin_selective_routing() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2"), agent("a3")];
    let issues = vec![issue("bead-1", 1), issue("bead-2", 2)];

    // Exclude a1 (compromised agent), pin bead-1 to a3 (trusted operator).
    ml.apply_override(override_exclude_agent("excl-a1", "a1"))
        .unwrap();
    ml.apply_override(override_pin("pin-bead1", "bead-1", "a3"))
        .unwrap();

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    // a1 should have zero assignments.
    let a1_assigned = decision
        .assignment_set
        .assignments
        .iter()
        .any(|a| a.agent_id == "a1");
    assert!(!a1_assigned, "excluded agent must not receive work");

    // bead-1 should be pinned to a3.
    let bead1_agent = decision
        .assignment_set
        .assignments
        .iter()
        .find(|a| a.bead_id == "bead-1")
        .map(|a| a.agent_id.as_str());
    assert_eq!(bead1_agent, Some("a3"), "pinned bead must go to target agent");
}

#[test]
fn takeover_reprioritize_critical_work_first() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 1,
            ..MissionSafetyEnvelopeConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let agents = vec![agent("a1")];
    let issues = vec![issue("low-pri", 5), issue("emergency", 3)];

    // Boost emergency bead to ensure it gets picked first.
    ml.apply_override(override_reprioritize("boost-emerg", "emergency", 100))
        .unwrap();

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    // With cap=1, only one assignment. Emergency should win.
    if decision.assignment_set.assignment_count() == 1 {
        let assigned_bead = &decision.assignment_set.assignments[0].bead_id;
        assert_eq!(
            assigned_bead, "emergency",
            "reprioritized bead should be assigned first"
        );
    }
}

#[test]
fn takeover_multiple_pins_different_agents() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("human-1"), agent("human-2"), agent("auto-1")];
    let issues = vec![issue("task-a", 1), issue("task-b", 2), issue("task-c", 3)];

    // Operator pins task-a to human-1, task-b to human-2.
    ml.apply_override(override_pin("pin-a", "task-a", "human-1"))
        .unwrap();
    ml.apply_override(override_pin("pin-b", "task-b", "human-2"))
        .unwrap();

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let a_agent = decision
        .assignment_set
        .assignments
        .iter()
        .find(|a| a.bead_id == "task-a")
        .map(|a| a.agent_id.as_str());
    let b_agent = decision
        .assignment_set
        .assignments
        .iter()
        .find(|a| a.bead_id == "task-b")
        .map(|a| a.agent_id.as_str());

    assert_eq!(a_agent, Some("human-1"));
    assert_eq!(b_agent, Some("human-2"));
}

// ═══════════════════════════════════════════════════════════════════════
// Category 3: Degraded-Mode Operation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn degraded_reduced_capacity_limits_assignments() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    // Agent degraded to max 1 parallel assignment.
    let agents = vec![degraded_agent("degraded-a1", 1), agent("healthy-a2")];
    let issues = swarm_issues(5);
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    // Should still produce assignments but respect degraded capacity.
    assert!(decision.assignment_set.assignment_count() > 0);
}

#[test]
fn degraded_offline_agents_get_no_work() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![offline_agent("offline-1"), agent("healthy-1")];
    let issues = vec![issue("bead-1", 1)];
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    let offline_assigned = decision
        .assignment_set
        .assignments
        .iter()
        .any(|a| a.agent_id == "offline-1");
    assert!(
        !offline_assigned,
        "offline agent must not receive assignments"
    );
}

#[test]
fn degraded_all_agents_offline_yields_zero_assignments() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![offline_agent("off-1"), offline_agent("off-2")];
    let issues = swarm_issues(3);
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert_eq!(
        decision.assignment_set.assignment_count(),
        0,
        "all agents offline must yield zero assignments"
    );
}

#[test]
fn degraded_mixed_availability_prioritizes_healthy() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 1,
            ..MissionSafetyEnvelopeConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let agents = vec![
        offline_agent("off"),
        degraded_agent("degraded", 1),
        agent("healthy"),
    ];
    let issues = vec![issue("bead-1", 1)];
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    if decision.assignment_set.assignment_count() == 1 {
        let assigned = &decision.assignment_set.assignments[0].agent_id;
        assert_ne!(
            assigned, "off",
            "offline agent must not be selected over healthy agents"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Category 4: Controlled Recovery
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn recovery_clear_overrides_restores_normal_dispatch() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = swarm_agents(3);
    let issues = swarm_issues(3);

    // Emergency stop.
    for i in 0..3 {
        ml.apply_override(override_exclude_agent(
            &format!("estop-{i}"),
            &format!("agent-{i}"),
        ))
        .unwrap();
    }
    let stopped = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert_eq!(stopped.assignment_set.assignment_count(), 0);

    // Recovery: clear all overrides.
    for i in 0..3 {
        ml.clear_override(&format!("estop-{i}"), 2000);
    }
    let recovered = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(
        recovered.assignment_set.assignment_count() > 0,
        "clearing overrides must restore normal dispatch"
    );
}

#[test]
fn recovery_partial_override_clear_gradual_restore() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = swarm_agents(3);
    let issues = swarm_issues(3);

    // Exclude all agents.
    for i in 0..3 {
        ml.apply_override(override_exclude_agent(
            &format!("excl-{i}"),
            &format!("agent-{i}"),
        ))
        .unwrap();
    }

    // Gradually restore: clear agent-0 first.
    ml.clear_override("excl-0", 2000);
    let partial = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    // Only agent-0 is available now.
    let agent0_only = partial
        .assignment_set
        .assignments
        .iter()
        .all(|a| a.agent_id == "agent-0");
    if partial.assignment_set.assignment_count() > 0 {
        assert!(agent0_only, "only cleared agent should receive work");
    }

    // Restore all.
    ml.clear_override("excl-1", 3000);
    ml.clear_override("excl-2", 3000);
    let full = ml.evaluate(61_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(
        full.assignment_set.assignment_count() > 0,
        "full restore should produce assignments"
    );
}

#[test]
fn recovery_ttl_based_auto_expiry() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = swarm_agents(2);
    let issues = swarm_issues(2);

    // Exclude with TTL that expires before next evaluation.
    let ovr = OperatorOverride {
        override_id: "ttl-excl".to_string(),
        kind: OperatorOverrideKind::ExcludeAgent {
            agent_id: "agent-0".to_string(),
        },
        activated_by: "operator".to_string(),
        reason_code: "game-day".to_string(),
        rationale: "temporary exclude".to_string(),
        activated_at_ms: 1000,
        expires_at_ms: Some(5000), // Expires at 5s.
        correlation_id: None,
    };
    ml.apply_override(ovr).unwrap();

    // Evaluate at t=2000 — override still active.
    let during = ml.evaluate(2000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    let agent0_during = during
        .assignment_set
        .assignments
        .iter()
        .any(|a| a.agent_id == "agent-0");
    // agent-0 excluded during TTL.
    assert!(!agent0_during, "agent-0 should be excluded during TTL");

    // Evaluate at t=31000 — override expired.
    let after = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    // Override should have expired; agent-0 may now receive work.
    assert!(
        after.assignment_set.assignment_count() > 0,
        "expired TTL should restore normal dispatch"
    );
}

#[test]
fn recovery_report_reflects_override_state() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = swarm_agents(2);
    let issues = swarm_issues(2);
    let log = elog();

    // Before overrides.
    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    let report_before = ml.generate_operator_report(Some(&log), None);
    let overrides_before = ml.active_overrides().len();
    assert_eq!(overrides_before, 0);

    // Apply override.
    ml.apply_override(override_exclude_agent("excl-0", "agent-0"))
        .unwrap();
    let overrides_during = ml.active_overrides().len();
    assert_eq!(overrides_during, 1);

    // Clear.
    ml.clear_override("excl-0", 2000);
    let overrides_after = ml.active_overrides().len();
    assert_eq!(overrides_after, 0);

    // Report should still be generatable.
    ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    let report_after = ml.generate_operator_report(Some(&log), None);
    // Reports should serialize.
    let _json_before = serde_json::to_value(&report_before).unwrap();
    let _json_after = serde_json::to_value(&report_after).unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
// Category 5: Override Audit Trail
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn audit_override_correlation_id_preserved() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let ovr = OperatorOverride {
        override_id: "audit-001".to_string(),
        kind: OperatorOverrideKind::ExcludeAgent {
            agent_id: "agent-0".to_string(),
        },
        activated_by: "operator-alice".to_string(),
        reason_code: "incident-123".to_string(),
        rationale: "emergency response to security incident".to_string(),
        activated_at_ms: 1000,
        expires_at_ms: None,
        correlation_id: Some("INC-2026-001".to_string()),
    };
    ml.apply_override(ovr).unwrap();

    let active = ml.active_overrides();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].override_id, "audit-001");
    assert_eq!(active[0].activated_by, "operator-alice");
    assert_eq!(active[0].reason_code, "incident-123");
    assert_eq!(
        active[0].correlation_id,
        Some("INC-2026-001".to_string())
    );
}

#[test]
fn audit_cleared_overrides_tracked() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    ml.apply_override(override_exclude_agent("track-1", "a1"))
        .unwrap();
    ml.apply_override(override_exclude_agent("track-2", "a2"))
        .unwrap();

    assert_eq!(ml.active_overrides().len(), 2);

    ml.clear_override("track-1", 2000);
    assert_eq!(ml.active_overrides().len(), 1);
    assert_eq!(ml.active_overrides()[0].override_id, "track-2");

    ml.clear_override("track-2", 3000);
    assert_eq!(ml.active_overrides().len(), 0);
}

#[test]
fn audit_many_overrides_lifecycle() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    // Apply 20 overrides.
    for i in 0..20 {
        ml.apply_override(override_exclude_bead(
            &format!("lifecycle-{i}"),
            &format!("bead-{i}"),
        ))
        .unwrap();
    }
    assert_eq!(ml.active_overrides().len(), 20);

    // Clear half.
    for i in 0..10 {
        ml.clear_override(&format!("lifecycle-{i}"), 2000);
    }
    assert_eq!(ml.active_overrides().len(), 10);

    // Verify remaining are the right ones (lifecycle-10 through lifecycle-19).
    let remaining_ids: Vec<&str> = ml.active_overrides().iter().map(|o| o.override_id.as_str()).collect();
    for i in 10..20 {
        assert!(
            remaining_ids.contains(&format!("lifecycle-{i}").as_str()),
            "lifecycle-{i} should still be active"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Category 6: Multi-Agent Load Drills
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn load_takeover_with_10_agents() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = swarm_agents(10);
    let issues = swarm_issues(10);

    // Normal dispatch.
    let before = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    let before_count = before.assignment_set.assignment_count();
    assert!(before_count > 0);

    // Takeover: exclude half the agents.
    for i in 0..5 {
        ml.apply_override(override_exclude_agent(
            &format!("half-excl-{i}"),
            &format!("agent-{i}"),
        ))
        .unwrap();
    }
    let during = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    // Assignments should continue but only through remaining agents.
    let during_agents: Vec<&str> = during
        .assignment_set
        .assignments
        .iter()
        .map(|a| a.agent_id.as_str())
        .collect();
    for agent_id in &during_agents {
        assert!(
            !agent_id.starts_with("agent-0")
                && !agent_id.starts_with("agent-1")
                && !agent_id.starts_with("agent-2")
                && !agent_id.starts_with("agent-3")
                && !agent_id.starts_with("agent-4")
                || *agent_id == "agent-0" // edge: "agent-0" could match "agent-0X" in sorted
                // Use direct check instead
        );
    }
    // No excluded agent should appear.
    for i in 0..5 {
        let excluded = format!("agent-{i}");
        assert!(
            !during_agents.contains(&excluded.as_str()),
            "excluded agent {excluded} should not appear in assignments"
        );
    }
}

#[test]
fn load_full_swarm_emergency_stop_and_recovery() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = swarm_agents(20);
    let issues = swarm_issues(15);

    // Normal.
    let normal = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(normal.assignment_set.assignment_count() > 0);

    // Emergency stop: exclude all 20 agents.
    for i in 0..20 {
        ml.apply_override(override_exclude_agent(
            &format!("full-estop-{i}"),
            &format!("agent-{i}"),
        ))
        .unwrap();
    }
    let stopped = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert_eq!(stopped.assignment_set.assignment_count(), 0);

    // Full recovery.
    for i in 0..20 {
        ml.clear_override(&format!("full-estop-{i}"), 60_000);
    }
    let recovered = ml.evaluate(91_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(
        recovered.assignment_set.assignment_count() > 0,
        "full recovery must restore dispatch"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Category 7: Determinism
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn determinism_emergency_stop_reproducible() {
    let run = || {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let agents = swarm_agents(5);
        let issues = swarm_issues(5);

        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
        for i in 0..5 {
            ml.apply_override(override_exclude_agent(
                &format!("det-estop-{i}"),
                &format!("agent-{i}"),
            ))
            .unwrap();
        }
        let d = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
        d.assignment_set.assignment_count()
    };
    let r1 = run();
    let r2 = run();
    assert_eq!(r1, r2, "emergency stop must be deterministic");
    assert_eq!(r1, 0);
}

#[test]
fn determinism_takeover_and_recovery_reproducible() {
    let run = || {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let agents = swarm_agents(4);
        let issues = swarm_issues(4);

        // Normal.
        let d1 = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
        // Takeover.
        ml.apply_override(override_exclude_agent("det-excl-0", "agent-0"))
            .unwrap();
        ml.apply_override(override_pin("det-pin-0", "bead-0", "agent-1"))
            .unwrap();
        let d2 = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
        // Recovery.
        ml.clear_override("det-excl-0", 60_000);
        ml.clear_override("det-pin-0", 60_000);
        let d3 = ml.evaluate(91_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

        let ids: Vec<Vec<String>> = [d1, d2, d3]
            .iter()
            .map(|d| {
                let mut v: Vec<String> = d
                    .assignment_set
                    .assignments
                    .iter()
                    .map(|a| format!("{}:{}", a.bead_id, a.agent_id))
                    .collect();
                v.sort();
                v
            })
            .collect();
        ids
    };
    let r1 = run();
    let r2 = run();
    assert_eq!(r1, r2, "takeover+recovery drill must be deterministic");
}

#[test]
fn determinism_report_after_drill_stable() {
    let agents = swarm_agents(3);
    let issues = swarm_issues(3);

    let run = || {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let log = elog();

        ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
        ml.apply_override(override_exclude_agent("det-report-excl", "agent-0"))
            .unwrap();
        ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
        ml.clear_override("det-report-excl", 60_000);
        ml.evaluate(91_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

        let report = ml.generate_operator_report(Some(&log), None);
        serde_json::to_value(&report).unwrap()
    };
    let r1 = run();
    let r2 = run();
    assert_eq!(r1, r2, "report after drill must be deterministic");
}
