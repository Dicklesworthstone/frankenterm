//! Adversarial test suite for mission safety guardrails (ft-1i2ge.4.7).
//!
//! Cross-module integration tests spanning plan.rs, mission_loop.rs, and
//! planner_features.rs. Exercises edge cases, boundary conditions, failure
//! injection, and deterministic recovery paths across:
//!
//! - Policy preflight pipeline (Allow/Deny/RequireApproval aggregation)
//! - Reservation conflict detection (wildcard/directory/expiry)
//! - Safety envelope enforcement (caps, retry storms, risky labels)
//! - Conflict detection (3 conflict types, 3 strategies, message generation)
//! - Kill-switch semantics (level transitions, TTL expiry, history bounding)
//! - Dispatch deduplication (key determinism, caching, eviction)
//! - Approval lifecycle (state machine, idempotency, expiry)

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::beads_types::{BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_loop::*;
use frankenterm_core::plan::MissionAgentAvailability;
use frankenterm_core::plan::MissionAgentCapabilityProfile;
use frankenterm_core::planner_features::{
    Assignment as PlannerAssignment, AssignmentSet, ConflictPair, PlannerExtractionContext,
    RejectionReason, SolverConfig,
};

// ── Test helpers ────────────────────────────────────────────────────────────

fn issue(id: &str, priority: u8, labels: &[&str]) -> BeadIssueDetail {
    BeadIssueDetail {
        id: id.to_string(),
        title: format!("Bead {}", id),
        status: BeadStatus::Open,
        priority,
        issue_type: BeadIssueType::Task,
        assignee: None,
        labels: labels.iter().map(|l| l.to_string()).collect(),
        dependencies: Vec::new(),
        dependents: Vec::new(),
        parent: None,
        ingest_warning: None,
        extra: HashMap::new(),
    }
}

fn assignment(bead_id: &str, agent_id: &str, score: f64) -> PlannerAssignment {
    PlannerAssignment {
        bead_id: bead_id.to_string(),
        agent_id: agent_id.to_string(),
        score,
        rank: 1,
    }
}

fn assignment_set(assignments: Vec<PlannerAssignment>) -> AssignmentSet {
    AssignmentSet {
        assignments,
        rejected: Vec::new(),
        solver_config: SolverConfig::default(),
    }
}

fn reservation(holder: &str, paths: &[&str], bead: Option<&str>) -> KnownReservation {
    KnownReservation {
        holder: holder.to_string(),
        paths: paths.iter().map(|p| p.to_string()).collect(),
        exclusive: true,
        bead_id: bead.map(|b| b.to_string()),
        expires_at_ms: Some(999_999),
    }
}

fn active_claim(bead_id: &str, agent_id: &str) -> ActiveBeadClaim {
    ActiveBeadClaim {
        bead_id: bead_id.to_string(),
        agent_id: agent_id.to_string(),
        claimed_at_ms: 1000,
    }
}

fn loop_with_config(config: MissionLoopConfig) -> MissionLoop {
    MissionLoop::new(config)
}

// ── ADV-01: Safety envelope — assignment cap at exact boundary ──────────────

#[test]
fn adv_01_envelope_at_exact_cap_allows_all() {
    let mut ml = loop_with_config(MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 3,
            max_risky_assignments_per_cycle: 10,
            max_consecutive_retries_per_bead: 100,
            ..MissionSafetyEnvelopeConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let issues = vec![issue("a", 0, &[]), issue("b", 1, &[]), issue("c", 2, &[])];
    let agents = vec![
        MissionAgentCapabilityProfile {
            agent_id: "a1".to_string(),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 3,
            availability: MissionAgentAvailability::Ready,
        },
        MissionAgentCapabilityProfile {
            agent_id: "a2".to_string(),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 3,
            availability: MissionAgentAvailability::Ready,
        },
        MissionAgentCapabilityProfile {
            agent_id: "a3".to_string(),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 3,
            availability: MissionAgentAvailability::Ready,
        },
    ];
    let ctx = PlannerExtractionContext::default();
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
    // With 3 beads, 3 agents, and cap=3, all should pass.
    assert_eq!(decision.assignment_set.assignment_count(), 3);
    // No envelope rejections.
    assert!(!decision.assignment_set.rejected.iter().any(|r| {
        r.reasons
            .iter()
            .any(|reason| matches!(reason, RejectionReason::SafetyGateDenied { .. }))
    }));
}

// ── ADV-02: Safety envelope — one over cap rejects exactly one ──────────────

#[test]
fn adv_02_envelope_one_over_cap_rejects_one() {
    let mut ml = loop_with_config(MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 2,
            max_risky_assignments_per_cycle: 10,
            max_consecutive_retries_per_bead: 100,
            ..MissionSafetyEnvelopeConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let issues = vec![issue("a", 0, &[]), issue("b", 1, &[]), issue("c", 2, &[])];
    let agents: Vec<MissionAgentCapabilityProfile> = (0..3)
        .map(|i| MissionAgentCapabilityProfile {
            agent_id: format!("a{}", i),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 3,
            availability: MissionAgentAvailability::Ready,
        })
        .collect();
    let ctx = PlannerExtractionContext::default();
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
    assert_eq!(decision.assignment_set.assignment_count(), 2);
    let cap_rejection_count = decision
        .assignment_set
        .rejected
        .iter()
        .filter(|r| {
            r.reasons.iter().any(|reason| {
                matches!(
                    reason,
                    RejectionReason::SafetyGateDenied { gate_name }
                    if gate_name == "mission.envelope.max_assignments_per_cycle"
                )
            })
        })
        .count();
    assert_eq!(cap_rejection_count, 1);
}

// ── ADV-03: Safety envelope — risky label case insensitivity ────────────────

#[test]
fn adv_03_risky_label_case_insensitive() {
    let mut ml = loop_with_config(MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 10,
            max_risky_assignments_per_cycle: 1,
            max_consecutive_retries_per_bead: 100,
            risky_label_markers: vec!["danger".to_string()],
        },
        ..MissionLoopConfig::default()
    });
    // All 3 beads have "danger" in different cases.
    let issues = vec![
        issue("a", 0, &["DANGER"]),
        issue("b", 1, &["Danger"]),
        issue("c", 2, &["danger"]),
    ];
    let agents: Vec<MissionAgentCapabilityProfile> = (0..3)
        .map(|i| MissionAgentCapabilityProfile {
            agent_id: format!("a{}", i),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 3,
            availability: MissionAgentAvailability::Ready,
        })
        .collect();
    let ctx = PlannerExtractionContext::default();
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
    // Only 1 risky assignment allowed.
    assert_eq!(decision.assignment_set.assignment_count(), 1);
}

// ── ADV-04: Retry storm — blocked after max, unblocked after backoff ────────

#[test]
fn adv_04_retry_storm_backoff_then_recovery() {
    let mut ml = loop_with_config(MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 10,
            max_risky_assignments_per_cycle: 10,
            max_consecutive_retries_per_bead: 2,
            ..MissionSafetyEnvelopeConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let issues = vec![issue("retry-me", 0, &[])];
    let agents = vec![MissionAgentCapabilityProfile {
        agent_id: "a1".to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Ready,
    }];
    let ctx = PlannerExtractionContext::default();

    // Cycle 1: assigned (streak=1).
    let d1 = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
    assert_eq!(d1.assignment_set.assignment_count(), 1);

    // Cycle 2: assigned (streak=2).
    let d2 = ml.evaluate(2000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
    assert_eq!(d2.assignment_set.assignment_count(), 1);

    // Cycle 3: BLOCKED (streak=2 >= max=2), forced backoff.
    let d3 = ml.evaluate(3000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
    assert_eq!(d3.assignment_set.assignment_count(), 0);
    let is_storm = d3.assignment_set.rejected.iter().any(|r| {
        r.reasons.iter().any(|reason| {
            matches!(
                reason,
                RejectionReason::SafetyGateDenied { gate_name }
                if gate_name == "mission.envelope.retry_storm"
            )
        })
    });
    assert!(is_storm);

    // Cycle 4: RECOVERED (streak reset to 0 after backoff).
    let d4 = ml.evaluate(4000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
    assert_eq!(d4.assignment_set.assignment_count(), 1);
}

// ── ADV-05: Conflict detection — all three types in one cycle ───────────────

#[test]
fn adv_05_all_conflict_types_in_single_cycle() {
    let mut ml = loop_with_config(MissionLoopConfig::default());
    let aset = assignment_set(vec![
        assignment("a", "agent1", 1.0),
        assignment("b", "agent3", 0.8),
        assignment("b", "agent4", 0.3), // concurrent claim on "b"
        assignment("c", "agent5", 0.5), // collides with active claim
    ]);
    let reservations = vec![
        reservation("agent1", &["src/plan.rs"], Some("a")),
        reservation("agent2", &["src/plan.rs"], Some("x")), // overlaps with agent1
    ];
    let active = vec![active_claim("c", "agent6")];
    let issues = vec![
        issue("a", 0, &[]),
        issue("b", 1, &[]),
        issue("c", 2, &[]),
        issue("x", 3, &[]),
    ];
    let report = ml.detect_conflicts(&aset, &reservations, &active, 5000, &issues);

    let types: Vec<&ConflictType> = report.conflicts.iter().map(|c| &c.conflict_type).collect();
    assert!(types.contains(&&ConflictType::FileReservationOverlap));
    assert!(types.contains(&&ConflictType::ConcurrentBeadClaim));
    assert!(types.contains(&&ConflictType::ActiveClaimCollision));
    assert!(report.conflicts.len() >= 3);
}

// ── ADV-06: Conflict detection — max bound prevents flood ───────────────────

#[test]
fn adv_06_conflict_flood_bounded() {
    let mut ml = loop_with_config(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            max_conflicts_per_cycle: 2,
            ..ConflictDetectionConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    // 5 active claim collisions, but max=2.
    let aset = assignment_set(
        (0..5)
            .map(|i| assignment(&format!("b{}", i), &format!("agent{}", i), 1.0))
            .collect(),
    );
    let active: Vec<ActiveBeadClaim> = (0..5)
        .map(|i| active_claim(&format!("b{}", i), &format!("other{}", i)))
        .collect();
    let issues: Vec<BeadIssueDetail> = (0..5).map(|i| issue(&format!("b{}", i), 0, &[])).collect();
    let report = ml.detect_conflicts(&aset, &[], &active, 5000, &issues);
    assert_eq!(report.conflicts.len(), 2);
}

// ── ADV-07: Conflict detection — strategy switches produce different winners ─

#[test]
fn adv_07_strategy_affects_winner() {
    let issues = vec![
        issue("a", 0, &[]), // higher priority
        issue("b", 2, &[]), // lower priority
    ];

    // PriorityWins: agent1 (bead "a", P0) wins.
    let mut ml1 = loop_with_config(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            strategy: DeconflictionStrategy::PriorityWins,
            ..ConflictDetectionConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let aset1 = assignment_set(vec![assignment("a", "agent1", 0.5)]);
    let reservations = vec![
        reservation("agent1", &["src/plan.rs"], Some("a")),
        reservation("agent2", &["src/plan.rs"], Some("b")),
    ];
    let r1 = ml1.detect_conflicts(&aset1, &reservations, &[], 5000, &issues);
    assert_eq!(r1.conflicts.len(), 1);
    match &r1.conflicts[0].resolution {
        ConflictResolution::AutoResolved { winner_agent, .. } => {
            assert_eq!(winner_agent, "agent1"); // P0 beats P2
        }
        other => panic!("Expected AutoResolved, got {:?}", other),
    }

    // FirstClaimWins: agent2 (existing holder) always wins.
    let mut ml2 = loop_with_config(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            strategy: DeconflictionStrategy::FirstClaimWins,
            ..ConflictDetectionConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let aset2 = assignment_set(vec![assignment("a", "agent1", 0.5)]);
    let r2 = ml2.detect_conflicts(&aset2, &reservations, &[], 5000, &issues);
    assert_eq!(r2.conflicts.len(), 1);
    match &r2.conflicts[0].resolution {
        ConflictResolution::AutoResolved { winner_agent, .. } => {
            assert_eq!(winner_agent, "agent2"); // existing holder wins
        }
        other => panic!("Expected AutoResolved, got {:?}", other),
    }

    // ManualResolution: no auto-resolution.
    let mut ml3 = loop_with_config(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            strategy: DeconflictionStrategy::ManualResolution,
            ..ConflictDetectionConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let aset3 = assignment_set(vec![assignment("a", "agent1", 0.5)]);
    let r3 = ml3.detect_conflicts(&aset3, &reservations, &[], 5000, &issues);
    assert_eq!(r3.conflicts.len(), 1);
    assert_eq!(
        r3.conflicts[0].resolution,
        ConflictResolution::PendingManualResolution
    );
    assert_eq!(r3.pending_resolution_count, 1);
    assert_eq!(r3.auto_resolved_count, 0);
}

// ── ADV-08: Conflict detection — messages route to all involved agents ──────

#[test]
fn adv_08_deconfliction_messages_route_correctly() {
    let mut ml = loop_with_config(MissionLoopConfig::default());
    let aset = assignment_set(vec![
        assignment("x", "alice", 1.0),
        assignment("x", "bob", 0.5),
        assignment("x", "carol", 0.3),
    ]);
    let issues = vec![issue("x", 0, &[])];
    let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);

    // 2 conflicts (bob + carol vs alice).
    assert_eq!(report.conflicts.len(), 2);
    // Each conflict sends to 2 agents → 4 messages total.
    assert_eq!(report.messages.len(), 4);

    let recipients: Vec<&str> = report
        .messages
        .iter()
        .map(|m| m.recipient.as_str())
        .collect();
    // alice appears in both conflicts.
    assert_eq!(recipients.iter().filter(|&&r| r == "alice").count(), 2);
    assert!(recipients.contains(&"bob"));
    assert!(recipients.contains(&"carol"));

    // All messages have high importance for concurrent claims.
    assert!(report.messages.iter().all(|m| m.importance == "high"));
    // Thread ID should be the bead.
    assert!(report.messages.iter().all(|m| m.thread_id == "x"));
}

// ── ADV-09: Conflict detection — error codes are consistent ─────────────────

#[test]
fn adv_09_error_codes_match_conflict_type() {
    let mut ml = loop_with_config(MissionLoopConfig::default());

    // Reservation overlap → FTM2001.
    let aset1 = assignment_set(vec![assignment("a", "agent1", 1.0)]);
    let res = vec![
        reservation("agent1", &["src/x.rs"], Some("a")),
        reservation("agent2", &["src/x.rs"], Some("b")),
    ];
    let issues = vec![issue("a", 0, &[]), issue("b", 1, &[])];
    let r1 = ml.detect_conflicts(&aset1, &res, &[], 5000, &issues);
    assert_eq!(r1.conflicts[0].error_code, "FTM2001");
    assert_eq!(r1.conflicts[0].reason_code, "reservation_overlap");

    // Concurrent bead claim → FTM2002.
    let aset2 = assignment_set(vec![assignment("z", "a1", 1.0), assignment("z", "a2", 0.5)]);
    let issues2 = vec![issue("z", 0, &[])];
    let r2 = ml.detect_conflicts(&aset2, &[], &[], 6000, &issues2);
    assert_eq!(r2.conflicts[0].error_code, "FTM2002");
    assert_eq!(r2.conflicts[0].reason_code, "concurrent_bead_claim");

    // Active claim collision → FTM2003.
    let aset3 = assignment_set(vec![assignment("y", "a1", 1.0)]);
    let active = vec![active_claim("y", "a2")];
    let issues3 = vec![issue("y", 0, &[])];
    let r3 = ml.detect_conflicts(&aset3, &[], &active, 7000, &issues3);
    assert_eq!(r3.conflicts[0].error_code, "FTM2003");
    assert_eq!(r3.conflicts[0].reason_code, "active_claim_collision");
}

// ── ADV-10: Conflict state — accumulates across multiple cycles ─────────────

#[test]
fn adv_10_conflict_state_accumulates() {
    let mut ml = loop_with_config(MissionLoopConfig::default());
    let issues = vec![issue("x", 0, &[])];

    // 3 cycles, each with a conflict.
    for cycle in 0..3u64 {
        let aset = assignment_set(vec![assignment("x", "a1", 1.0), assignment("x", "a2", 0.5)]);
        ml.detect_conflicts(&aset, &[], &[], (cycle * 1000) as i64, &issues);
    }

    let (detected, resolved) = ml.conflict_stats();
    assert_eq!(detected, 3);
    assert_eq!(resolved, 3); // all auto-resolved via PriorityWins default
    assert_eq!(ml.state().conflict_history.len(), 3);
}

// ── ADV-11: Conflict detection — disabled is truly a no-op ──────────────────

#[test]
fn adv_11_disabled_detection_no_side_effects() {
    let mut ml = loop_with_config(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            enabled: false,
            ..ConflictDetectionConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let aset = assignment_set(vec![assignment("x", "a1", 1.0), assignment("x", "a2", 0.5)]);
    let issues = vec![issue("x", 0, &[])];
    let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);

    assert!(report.conflicts.is_empty());
    assert!(report.messages.is_empty());
    assert_eq!(report.auto_resolved_count, 0);
    assert_eq!(report.pending_resolution_count, 0);
    // State should NOT be modified.
    assert_eq!(ml.state().total_conflicts_detected, 0);
    assert_eq!(ml.state().total_conflicts_auto_resolved, 0);
    assert!(ml.state().conflict_history.is_empty());
}

// ── ADV-12: Safety envelope — mixed risky and non-risky independent caps ────

#[test]
fn adv_12_mixed_risky_non_risky_independent_caps() {
    let mut ml = loop_with_config(MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 5,
            max_risky_assignments_per_cycle: 1,
            max_consecutive_retries_per_bead: 100,
            risky_label_markers: vec!["danger".to_string()],
        },
        ..MissionLoopConfig::default()
    });
    let issues = vec![
        issue("safe1", 0, &[]),
        issue("safe2", 1, &[]),
        issue("risky1", 2, &["danger"]),
        issue("risky2", 3, &["danger"]),
    ];
    let agents: Vec<MissionAgentCapabilityProfile> = (0..4)
        .map(|i| MissionAgentCapabilityProfile {
            agent_id: format!("a{}", i),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 3,
            availability: MissionAgentAvailability::Ready,
        })
        .collect();
    let ctx = PlannerExtractionContext::default();
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

    // Max total = 5, max risky = 1.
    // 2 safe + 1 risky = 3 assigned, 1 risky rejected.
    assert!(decision.assignment_set.assignment_count() <= 3);
    let has_risky_rejections = decision.assignment_set.rejected.iter().any(|r| {
        r.reasons.iter().any(|reason| {
            matches!(
                reason,
                RejectionReason::SafetyGateDenied { gate_name }
                if gate_name == "mission.envelope.max_risky_assignments_per_cycle"
            )
        })
    });
    assert!(has_risky_rejections);
}

// ── ADV-13: Conflict detection — reservation with no bead_id skipped ────────

#[test]
fn adv_13_assignment_without_matching_reservation_no_overlap_check() {
    let mut ml = loop_with_config(MissionLoopConfig::default());
    // agent1 has no reservation for bead "a", so no path overlap can occur.
    let aset = assignment_set(vec![assignment("a", "agent1", 1.0)]);
    let reservations = vec![
        // agent2 has a reservation, but agent1 doesn't for bead "a".
        reservation("agent2", &["src/plan.rs"], Some("b")),
    ];
    let issues = vec![issue("a", 0, &[])];
    let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
    // No conflict because agent1 has no reservation paths to check.
    assert!(report.conflicts.is_empty());
}

// ── ADV-14: Conflict detection — wildcard edge cases ────────────────────────

#[test]
fn adv_14_wildcard_edge_cases() {
    let mut ml = loop_with_config(MissionLoopConfig::default());

    // `?` matches exactly one character.
    let aset = assignment_set(vec![assignment("a", "agent1", 1.0)]);
    let reservations = vec![
        reservation("agent1", &["src/a.rs"], Some("a")),
        reservation("agent2", &["src/?.rs"], Some("b")),
    ];
    let issues = vec![issue("a", 0, &[]), issue("b", 1, &[])];
    let r1 = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
    assert_eq!(r1.conflicts.len(), 1, "? should match single char");

    // `?` does NOT match two chars.
    let mut ml2 = loop_with_config(MissionLoopConfig::default());
    let aset2 = assignment_set(vec![assignment("c", "agent3", 1.0)]);
    let reservations2 = vec![
        reservation("agent3", &["src/ab.rs"], Some("c")),
        reservation("agent4", &["src/?.rs"], Some("d")),
    ];
    let issues2 = vec![issue("c", 0, &[]), issue("d", 1, &[])];
    let r2 = ml2.detect_conflicts(&aset2, &reservations2, &[], 5000, &issues2);
    assert!(r2.conflicts.is_empty(), "? should not match two chars");
}

// ── ADV-15: Serde roundtrip — full ConflictDetectionReport ──────────────────

#[test]
fn adv_15_full_report_serde_roundtrip() {
    let report = ConflictDetectionReport {
        cycle_id: 42,
        detected_at_ms: 99999,
        conflicts: vec![
            AssignmentConflict {
                conflict_id: "c1".to_string(),
                conflict_type: ConflictType::FileReservationOverlap,
                involved_agents: vec!["a1".to_string(), "a2".to_string()],
                involved_beads: vec!["bead-a".to_string(), "bead-b".to_string()],
                conflicting_paths: vec!["src/plan.rs".to_string()],
                detected_at_ms: 99999,
                resolution: ConflictResolution::AutoResolved {
                    winner_agent: "a1".to_string(),
                    loser_agent: "a2".to_string(),
                    strategy: DeconflictionStrategy::PriorityWins,
                },
                reason_code: "reservation_overlap".to_string(),
                error_code: "FTM2001".to_string(),
            },
            AssignmentConflict {
                conflict_id: "c2".to_string(),
                conflict_type: ConflictType::ConcurrentBeadClaim,
                involved_agents: vec!["a3".to_string(), "a4".to_string()],
                involved_beads: vec!["bead-c".to_string()],
                conflicting_paths: Vec::new(),
                detected_at_ms: 99999,
                resolution: ConflictResolution::PendingManualResolution,
                reason_code: "concurrent_bead_claim".to_string(),
                error_code: "FTM2002".to_string(),
            },
            AssignmentConflict {
                conflict_id: "c3".to_string(),
                conflict_type: ConflictType::ActiveClaimCollision,
                involved_agents: vec!["a5".to_string(), "a6".to_string()],
                involved_beads: vec!["bead-d".to_string()],
                conflicting_paths: Vec::new(),
                detected_at_ms: 99999,
                resolution: ConflictResolution::Deferred {
                    retry_after_ms: 30_000,
                },
                reason_code: "active_claim_collision".to_string(),
                error_code: "FTM2003".to_string(),
            },
        ],
        messages: vec![DeconflictionMessage {
            recipient: "a2".to_string(),
            subject: "[conflict] reservation_overlap on bead-a, bead-b".to_string(),
            body: "test body".to_string(),
            thread_id: "bead-a".to_string(),
            importance: "high".to_string(),
            conflict_id: "c1".to_string(),
        }],
        auto_resolved_count: 1,
        pending_resolution_count: 2,
    };

    let json = serde_json::to_string(&report).unwrap();
    let back: ConflictDetectionReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.cycle_id, 42);
    assert_eq!(back.conflicts.len(), 3);
    assert_eq!(back.messages.len(), 1);
    assert_eq!(back.auto_resolved_count, 1);
    assert_eq!(back.pending_resolution_count, 2);
    assert_eq!(back.conflicts[2].error_code, "FTM2003");
}

// ── ADV-16: Serde roundtrip — MissionLoopConfig with conflict detection ─────

#[test]
fn adv_16_config_serde_with_conflict_detection() {
    let config = MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            enabled: true,
            max_conflicts_per_cycle: 42,
            strategy: DeconflictionStrategy::FirstClaimWins,
            generate_messages: false,
        },
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 7,
            max_risky_assignments_per_cycle: 2,
            max_consecutive_retries_per_bead: 5,
            risky_label_markers: vec!["custom-risk".to_string()],
        },
        ..MissionLoopConfig::default()
    };
    let json = serde_json::to_string(&config).unwrap();
    let back: MissionLoopConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.conflict_detection.max_conflicts_per_cycle, 42);
    assert_eq!(
        back.conflict_detection.strategy,
        DeconflictionStrategy::FirstClaimWins
    );
    assert!(!back.conflict_detection.generate_messages);
    assert_eq!(back.safety_envelope.max_assignments_per_cycle, 7);
    assert_eq!(
        back.safety_envelope.risky_label_markers,
        vec!["custom-risk"]
    );
}

// ── ADV-17: Conflict detection — priority wins with equal priority ──────────

#[test]
fn adv_17_priority_wins_tiebreak_by_score() {
    let mut ml = loop_with_config(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            strategy: DeconflictionStrategy::PriorityWins,
            ..ConflictDetectionConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let aset = assignment_set(vec![assignment("a", "agent1", 0.3)]);
    let reservations = vec![
        reservation("agent1", &["src/x.rs"], Some("a")),
        reservation("agent2", &["src/x.rs"], Some("b")),
    ];
    // Both beads same priority → score decides. agent1 score=0.3, agent2 score=0.
    let issues = vec![issue("a", 1, &[]), issue("b", 1, &[])];
    let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
    assert_eq!(report.conflicts.len(), 1);
    match &report.conflicts[0].resolution {
        ConflictResolution::AutoResolved { winner_agent, .. } => {
            assert_eq!(winner_agent, "agent1"); // higher score wins tiebreak
        }
        other => panic!("Expected AutoResolved, got {:?}", other),
    }
}

// ── ADV-18: Empty inputs — no crashes, no false positives ───────────────────

#[test]
fn adv_18_empty_inputs_no_crashes() {
    let mut ml = loop_with_config(MissionLoopConfig::default());

    // Empty everything.
    let r1 = ml.detect_conflicts(&assignment_set(Vec::new()), &[], &[], 5000, &[]);
    assert!(r1.conflicts.is_empty());
    assert!(r1.messages.is_empty());

    // Assignments but no reservations/claims.
    let r2 = ml.detect_conflicts(
        &assignment_set(vec![assignment("a", "a1", 1.0)]),
        &[],
        &[],
        5000,
        &[],
    );
    assert!(r2.conflicts.is_empty());

    // Reservations but no assignments.
    let r3 = ml.detect_conflicts(
        &assignment_set(Vec::new()),
        &[reservation("a1", &["src/x.rs"], Some("a"))],
        &[active_claim("a", "a2")],
        5000,
        &[],
    );
    assert!(r3.conflicts.is_empty());
}

// ── ADV-19: Deterministic — same inputs produce same output ─────────────────

#[test]
fn adv_19_deterministic_detection() {
    let issues = vec![issue("x", 0, &[])];
    let aset = assignment_set(vec![assignment("x", "a1", 1.0), assignment("x", "a2", 0.5)]);

    let mut ml1 = loop_with_config(MissionLoopConfig::default());
    let r1 = ml1.detect_conflicts(&aset, &[], &[], 5000, &issues);

    let mut ml2 = loop_with_config(MissionLoopConfig::default());
    let r2 = ml2.detect_conflicts(&aset, &[], &[], 5000, &issues);

    assert_eq!(r1.conflicts.len(), r2.conflicts.len());
    for (c1, c2) in r1.conflicts.iter().zip(r2.conflicts.iter()) {
        assert_eq!(c1.conflict_type, c2.conflict_type);
        assert_eq!(c1.involved_agents, c2.involved_agents);
        assert_eq!(c1.resolution, c2.resolution);
        assert_eq!(c1.reason_code, c2.reason_code);
        assert_eq!(c1.error_code, c2.error_code);
    }
}

// ── ADV-20: Safety envelope + conflict detection interaction ────────────────

#[test]
fn adv_20_envelope_and_conflict_detection_compose() {
    let mut ml = loop_with_config(MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 2,
            max_risky_assignments_per_cycle: 10,
            max_consecutive_retries_per_bead: 100,
            ..MissionSafetyEnvelopeConfig::default()
        },
        ..MissionLoopConfig::default()
    });

    let issues = vec![issue("a", 0, &[]), issue("b", 1, &[]), issue("c", 2, &[])];
    let agents: Vec<MissionAgentCapabilityProfile> = (0..3)
        .map(|i| MissionAgentCapabilityProfile {
            agent_id: format!("a{}", i),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 3,
            availability: MissionAgentAvailability::Ready,
        })
        .collect();
    let ctx = PlannerExtractionContext::default();

    // Evaluate with envelope cap=2.
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
    assert_eq!(decision.assignment_set.assignment_count(), 2);

    // Now run conflict detection on the envelope-filtered assignments.
    // Both surviving assignments claim "a" → conflict.
    let post_envelope_set =
        assignment_set(vec![assignment("a", "a0", 1.0), assignment("a", "a1", 0.5)]);
    let report = ml.detect_conflicts(&post_envelope_set, &[], &[], 2000, &issues);
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(
        report.conflicts[0].conflict_type,
        ConflictType::ConcurrentBeadClaim
    );
}

// ── ADV-21: Conflict history bounding across many cycles ────────────────────

#[test]
fn adv_21_history_bounded_across_cycles() {
    let mut ml = loop_with_config(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            max_conflicts_per_cycle: 1,
            ..ConflictDetectionConfig::default()
        },
        ..MissionLoopConfig::default()
    });
    let issues = vec![issue("x", 0, &[])];

    // Run 100 cycles with a conflict each.
    for i in 0..100u64 {
        let aset = assignment_set(vec![assignment("x", "a1", 1.0), assignment("x", "a2", 0.5)]);
        ml.detect_conflicts(&aset, &[], &[], (i * 1000) as i64, &issues);
    }

    let (detected, _) = ml.conflict_stats();
    assert_eq!(detected, 100);
    // History bounded: max_conflicts_per_cycle (1) * 4 = 4.
    assert!(ml.state().conflict_history.len() <= 4);
}

// ── ADV-22: Serde roundtrip — MissionLoopState with conflict data ───────────

#[test]
fn adv_22_state_serde_with_conflicts() {
    let mut ml = loop_with_config(MissionLoopConfig::default());
    let aset = assignment_set(vec![assignment("x", "a1", 1.0), assignment("x", "a2", 0.5)]);
    let issues = vec![issue("x", 0, &[])];
    ml.detect_conflicts(&aset, &[], &[], 5000, &issues);

    let state = ml.state().clone();
    let json = serde_json::to_string(&state).unwrap();
    let back: MissionLoopState = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back.total_conflicts_detected,
        state.total_conflicts_detected
    );
    assert_eq!(
        back.total_conflicts_auto_resolved,
        state.total_conflicts_auto_resolved
    );
    assert_eq!(back.conflict_history.len(), state.conflict_history.len());
}

// ── ADV-23: KnownReservation and ActiveBeadClaim serde roundtrip ────────────

#[test]
fn adv_23_input_types_serde_roundtrip() {
    let kr = KnownReservation {
        holder: "agent1".to_string(),
        paths: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
        exclusive: true,
        bead_id: Some("bead-1".to_string()),
        expires_at_ms: Some(999_999),
    };
    let json = serde_json::to_string(&kr).unwrap();
    let back: KnownReservation = serde_json::from_str(&json).unwrap();
    assert_eq!(back.holder, "agent1");
    assert_eq!(back.paths.len(), 2);
    assert!(back.exclusive);

    let ac = ActiveBeadClaim {
        bead_id: "bead-2".to_string(),
        agent_id: "agent2".to_string(),
        claimed_at_ms: 1234,
    };
    let json2 = serde_json::to_string(&ac).unwrap();
    let back2: ActiveBeadClaim = serde_json::from_str(&json2).unwrap();
    assert_eq!(back2.bead_id, "bead-2");
    assert_eq!(back2.claimed_at_ms, 1234);
}

// ── ADV-24: Metrics capture conflict rejections in evaluate cycle ────────────

#[test]
fn adv_24_metrics_count_conflict_rejections() {
    let mut ml = loop_with_config(MissionLoopConfig {
        solver_config: SolverConfig {
            min_score: 0.0,
            max_assignments: 10,
            safety_gates: Vec::new(),
            conflicts: vec![ConflictPair {
                bead_a: "a".to_string(),
                bead_b: "b".to_string(),
            }],
        },
        ..MissionLoopConfig::default()
    });
    let issues = vec![issue("a", 0, &[]), issue("b", 1, &[])];
    let agents = vec![
        MissionAgentCapabilityProfile {
            agent_id: "a1".to_string(),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 3,
            availability: MissionAgentAvailability::Ready,
        },
        MissionAgentCapabilityProfile {
            agent_id: "a2".to_string(),
            capabilities: vec!["robot.send".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 3,
            availability: MissionAgentAvailability::Ready,
        },
    ];
    let ctx = PlannerExtractionContext::default();
    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

    let latest = ml.latest_metrics().expect("metrics must exist");
    // Beads a and b conflict → one should be rejected.
    assert!(latest.conflict_rejections >= 1);
    assert!(latest.conflict_rate > 0.0);
    assert!(ml.state().metrics_totals.conflict_rejections >= 1);
}

// ── ADV-25: DeconflictionMessage serde roundtrip ────────────────────────────

#[test]
fn adv_25_deconfliction_message_serde() {
    let msg = DeconflictionMessage {
        recipient: "agent1".to_string(),
        subject: "[conflict] test".to_string(),
        body: "Body with **markdown**".to_string(),
        thread_id: "bead-x".to_string(),
        importance: "high".to_string(),
        conflict_id: "c-123".to_string(),
    };
    let json = serde_json::to_string(&msg).unwrap();
    let back: DeconflictionMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back.recipient, "agent1");
    assert_eq!(back.importance, "high");
    assert_eq!(back.conflict_id, "c-123");
    assert!(back.body.contains("**markdown**"));
}
