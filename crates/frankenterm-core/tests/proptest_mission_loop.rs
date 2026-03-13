//! Property-based tests for mission_loop safety envelope, conflict detection,
//! and serde invariants (ft-1i2ge.4.7 supplement).
//!
//! Tests the following invariants through the public API:
//!
//! - Safety envelope caps are never exceeded
//! - Conflict count never exceeds `max_conflicts_per_cycle`
//! - Conflict detection is deterministic (same inputs → same outputs)
//! - No false positives when assignments and reservations are disjoint
//! - All three conflict types are correctly classified
//! - PriorityWins strategy respects bead priority ordering
//! - Message generation produces exactly one message per involved agent
//! - Serde roundtrip preserves all conflict detection types
//! - State accumulation is monotonically non-decreasing
//! - History bounding holds across arbitrary cycle counts

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::beads_types::{BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_loop::*;
use frankenterm_core::plan::MissionAgentAvailability;
use frankenterm_core::plan::MissionAgentCapabilityProfile;
use frankenterm_core::planner_features::{
    Assignment as PlannerAssignment, AssignmentSet, PlannerExtractionContext, SolverConfig,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_assignment_set(assignments: Vec<PlannerAssignment>) -> AssignmentSet {
    AssignmentSet {
        assignments,
        rejected: Vec::new(),
        solver_config: SolverConfig::default(),
    }
}

fn make_issue(id: &str, priority: u8) -> BeadIssueDetail {
    BeadIssueDetail {
        id: id.to_string(),
        title: format!("Bead {}", id),
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

// ── ML-P01: Safety envelope never exceeds max_assignments_per_cycle ─────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn ml_p01_envelope_never_exceeds_cap(
        cap in 1usize..10,
        num_issues in 1usize..20,
        num_agents in 1usize..8,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            safety_envelope: MissionSafetyEnvelopeConfig {
                max_assignments_per_cycle: cap,
                max_risky_assignments_per_cycle: cap,
                max_consecutive_retries_per_bead: 100,
                ..MissionSafetyEnvelopeConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let issues: Vec<BeadIssueDetail> = (0..num_issues)
            .map(|i| make_issue(&format!("b{}", i), (i % 5) as u8))
            .collect();
        let agents: Vec<MissionAgentCapabilityProfile> = (0..num_agents)
            .map(|i| MissionAgentCapabilityProfile {
                agent_id: format!("a{}", i),
                capabilities: vec!["robot.send".to_string()],
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 10,
                availability: MissionAgentAvailability::Ready,
            })
            .collect();
        let ctx = PlannerExtractionContext::default();
        let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
        prop_assert!(
            decision.assignment_set.assignment_count() <= cap,
            "assignments {} exceeded cap {}",
            decision.assignment_set.assignment_count(),
            cap
        );
    }

    // ── ML-P02: Conflict count bounded by max_conflicts_per_cycle ───────────

    #[test]
    fn ml_p02_conflict_count_bounded(
        max_conflicts in 1usize..10,
        num_claims in 2usize..15,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                max_conflicts_per_cycle: max_conflicts,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let aset = make_assignment_set(
            (0..num_claims)
                .map(|i| PlannerAssignment {
                    bead_id: "shared".to_string(),
                    agent_id: format!("agent{}", i),
                    score: (i as f64).mul_add(-0.05, 1.0),
                    rank: 1,
                })
                .collect(),
        );
        let issues = vec![make_issue("shared", 0)];
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);
        prop_assert!(
            report.conflicts.len() <= max_conflicts,
            "conflicts {} exceeded max {}",
            report.conflicts.len(),
            max_conflicts
        );
    }

    // ── ML-P03: Conflict detection is deterministic ─────────────────────────

    #[test]
    fn ml_p03_detection_deterministic(
        num_agents in 2usize..6,
    ) {
        let issues = vec![make_issue("x", 0)];
        let aset = make_assignment_set(
            (0..num_agents)
                .map(|i| PlannerAssignment {
                    bead_id: "x".to_string(),
                    agent_id: format!("a{}", i),
                    score: 1.0 / (i as f64 + 1.0),
                    rank: 1,
                })
                .collect(),
        );

        let mut ml1 = MissionLoop::new(MissionLoopConfig::default());
        let r1 = ml1.detect_conflicts(&aset, &[], &[], 5000, &issues);

        let mut ml2 = MissionLoop::new(MissionLoopConfig::default());
        let r2 = ml2.detect_conflicts(&aset, &[], &[], 5000, &issues);

        prop_assert_eq!(r1.conflicts.len(), r2.conflicts.len());
        for (c1, c2) in r1.conflicts.iter().zip(r2.conflicts.iter()) {
            prop_assert_eq!(&c1.conflict_type, &c2.conflict_type);
            prop_assert_eq!(&c1.involved_agents, &c2.involved_agents);
            prop_assert_eq!(&c1.resolution, &c2.resolution);
            prop_assert_eq!(&c1.error_code, &c2.error_code);
        }
    }

    // ── ML-P04: Disjoint assignments produce no conflicts ───────────────────

    #[test]
    fn ml_p04_disjoint_assignments_no_conflicts(
        num_beads in 1usize..10,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues: Vec<BeadIssueDetail> = (0..num_beads)
            .map(|i| make_issue(&format!("b{}", i), 0))
            .collect();
        let aset = make_assignment_set(
            (0..num_beads)
                .map(|i| PlannerAssignment {
                    bead_id: format!("b{}", i),
                    agent_id: format!("a{}", i),
                    score: 0.5,
                    rank: 1,
                })
                .collect(),
        );
        // Disjoint reservations (different paths per agent).
        let reservations: Vec<KnownReservation> = (0..num_beads)
            .map(|i| KnownReservation {
                holder: format!("a{}", i),
                paths: vec![format!("src/mod{}.rs", i)],
                exclusive: true,
                bead_id: Some(format!("b{}", i)),
                expires_at_ms: Some(999_999),
            })
            .collect();
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        prop_assert!(
            report.conflicts.is_empty(),
            "Expected no conflicts for disjoint assignments, got {}",
            report.conflicts.len()
        );
    }

    // ── ML-P05: ConcurrentBeadClaim correctly classified ────────────────────

    #[test]
    fn ml_p05_concurrent_bead_claim_classified(
        num_agents in 2usize..6,
        priority in 0u8..5,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![make_issue("shared-bead", priority)];
        let aset = make_assignment_set(
            (0..num_agents)
                .map(|i| PlannerAssignment {
                    bead_id: "shared-bead".to_string(),
                    agent_id: format!("a{}", i),
                    score: (i as f64).mul_add(-0.1, 1.0),
                    rank: 1,
                })
                .collect(),
        );
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);
        for conflict in &report.conflicts {
            prop_assert_eq!(
                &conflict.conflict_type,
                &ConflictType::ConcurrentBeadClaim,
                "Expected ConcurrentBeadClaim, got {:?}",
                conflict.conflict_type
            );
            prop_assert_eq!(&conflict.error_code, "FTM2002");
            prop_assert_eq!(&conflict.reason_code, "concurrent_bead_claim");
        }
        // Should have num_agents - 1 conflicts (one per loser).
        prop_assert_eq!(report.conflicts.len(), num_agents - 1);
    }

    // ── ML-P06: ActiveClaimCollision correctly classified ───────────────────

    #[test]
    fn ml_p06_active_claim_collision_classified(
        num_claims in 1usize..8,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                max_conflicts_per_cycle: 20,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let issues: Vec<BeadIssueDetail> = (0..num_claims)
            .map(|i| make_issue(&format!("b{}", i), 0))
            .collect();
        let aset = make_assignment_set(
            (0..num_claims)
                .map(|i| PlannerAssignment {
                    bead_id: format!("b{}", i),
                    agent_id: format!("new-a{}", i),
                    score: 0.5,
                    rank: 1,
                })
                .collect(),
        );
        let active: Vec<ActiveBeadClaim> = (0..num_claims)
            .map(|i| ActiveBeadClaim {
                bead_id: format!("b{}", i),
                agent_id: format!("old-a{}", i),
                claimed_at_ms: 500,
            })
            .collect();
        let report = ml.detect_conflicts(&aset, &[], &active, 5000, &issues);
        prop_assert_eq!(report.conflicts.len(), num_claims);
        for conflict in &report.conflicts {
            prop_assert_eq!(
                &conflict.conflict_type,
                &ConflictType::ActiveClaimCollision,
                "Expected ActiveClaimCollision, got {:?}",
                conflict.conflict_type
            );
            prop_assert_eq!(&conflict.error_code, "FTM2003");
        }
    }

    // ── ML-P07: PriorityWins respects bead priority ordering ────────────────

    #[test]
    fn ml_p07_priority_wins_respects_ordering(
        pri_a in 0u8..5,
        pri_b in 0u8..5,
        score_a in 0.0f64..1.0,
        _score_b in 0.0f64..1.0,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                strategy: DeconflictionStrategy::PriorityWins,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![
            make_issue("bead-a", pri_a),
            make_issue("bead-b", pri_b),
        ];
        let aset = make_assignment_set(vec![PlannerAssignment {
            bead_id: "bead-a".to_string(),
            agent_id: "agent-alpha".to_string(),
            score: score_a,
            rank: 1,
        }]);
        let reservations = vec![
            KnownReservation {
                holder: "agent-alpha".to_string(),
                paths: vec!["src/shared.rs".to_string()],
                exclusive: true,
                bead_id: Some("bead-a".to_string()),
                expires_at_ms: Some(999_999),
            },
            KnownReservation {
                holder: "agent-beta".to_string(),
                paths: vec!["src/shared.rs".to_string()],
                exclusive: true,
                bead_id: Some("bead-b".to_string()),
                expires_at_ms: Some(999_999),
            },
        ];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        prop_assert_eq!(report.conflicts.len(), 1);

        match &report.conflicts[0].resolution {
            ConflictResolution::AutoResolved {
                winner_agent,
                loser_agent,
                ..
            } => {
                match pri_a.cmp(&pri_b) {
                    std::cmp::Ordering::Less => {
                        prop_assert_eq!(winner_agent, "agent-alpha");
                        prop_assert_eq!(loser_agent, "agent-beta");
                    }
                    std::cmp::Ordering::Greater => {
                        prop_assert_eq!(winner_agent, "agent-beta");
                        prop_assert_eq!(loser_agent, "agent-alpha");
                    }
                    std::cmp::Ordering::Equal => {
                        // Equal priority: score_a >= score_b → alpha wins.
                        // score_a is the new assignment's score, score_b=0.0 for reservations.
                        // In resolve_conflict, agent_a=alpha with score_a, agent_b=beta with 0.0.
                        if score_a >= 0.0 {
                            prop_assert_eq!(winner_agent, "agent-alpha");
                        }
                    }
                }
            }
            other => {
                prop_assert!(false, "Expected AutoResolved, got {:?}", other);
            }
        }
    }

    // ── ML-P08: Messages generated for all involved agents ──────────────────

    #[test]
    fn ml_p08_messages_for_all_involved_agents(
        num_agents in 2usize..5,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![make_issue("x", 0)];
        let aset = make_assignment_set(
            (0..num_agents)
                .map(|i| PlannerAssignment {
                    bead_id: "x".to_string(),
                    agent_id: format!("a{}", i),
                    score: (i as f64).mul_add(-0.1, 1.0),
                    rank: 1,
                })
                .collect(),
        );
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);

        // Each conflict involves 2 agents, messages sent to both.
        for conflict in &report.conflicts {
            for agent in &conflict.involved_agents {
                let agent_msgs: Vec<_> = report
                    .messages
                    .iter()
                    .filter(|m| m.recipient == *agent && m.conflict_id == conflict.conflict_id)
                    .collect();
                prop_assert_eq!(
                    agent_msgs.len(),
                    1,
                    "Agent {} should get exactly 1 message for conflict {}, got {}",
                    agent,
                    conflict.conflict_id,
                    agent_msgs.len()
                );
            }
        }
    }

    // ── ML-P09: Serde roundtrip for ConflictDetectionReport ─────────────────

    #[test]
    fn ml_p09_report_serde_roundtrip(
        num_agents in 2usize..4,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![make_issue("x", 0)];
        let aset = make_assignment_set(
            (0..num_agents)
                .map(|i| PlannerAssignment {
                    bead_id: "x".to_string(),
                    agent_id: format!("a{}", i),
                    score: 0.5,
                    rank: 1,
                })
                .collect(),
        );
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);

        let json = serde_json::to_string(&report).unwrap();
        let back: ConflictDetectionReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report.conflicts.len(), back.conflicts.len());
        prop_assert_eq!(report.messages.len(), back.messages.len());
        prop_assert_eq!(report.auto_resolved_count, back.auto_resolved_count);
        prop_assert_eq!(report.pending_resolution_count, back.pending_resolution_count);
    }

    // ── ML-P10: State accumulation is monotonically non-decreasing ──────────

    #[test]
    fn ml_p10_state_accumulation_monotonic(
        num_cycles in 1usize..10,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![make_issue("x", 0)];
        let mut prev_detected = 0u64;
        let mut prev_resolved = 0u64;

        for i in 0..num_cycles {
            let aset = make_assignment_set(vec![
                PlannerAssignment {
                    bead_id: "x".to_string(),
                    agent_id: "a1".to_string(),
                    score: 1.0,
                    rank: 1,
                },
                PlannerAssignment {
                    bead_id: "x".to_string(),
                    agent_id: "a2".to_string(),
                    score: 0.5,
                    rank: 1,
                },
            ]);
            ml.detect_conflicts(&aset, &[], &[], (i * 1000) as i64, &issues);
            let (detected, resolved) = ml.conflict_stats();
            prop_assert!(
                detected >= prev_detected,
                "detected went from {} to {} at cycle {}",
                prev_detected, detected, i
            );
            prop_assert!(
                resolved >= prev_resolved,
                "resolved went from {} to {} at cycle {}",
                prev_resolved, resolved, i
            );
            prev_detected = detected;
            prev_resolved = resolved;
        }
    }

    // ── ML-P11: History bounding holds across arbitrary cycles ───────────────

    #[test]
    fn ml_p11_history_bounded(
        max_conflicts in 1usize..10,
        num_cycles in 1usize..30,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                max_conflicts_per_cycle: max_conflicts,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![make_issue("x", 0)];
        let history_max = max_conflicts * 4;

        for i in 0..num_cycles {
            let aset = make_assignment_set(vec![
                PlannerAssignment {
                    bead_id: "x".to_string(),
                    agent_id: "a1".to_string(),
                    score: 1.0,
                    rank: 1,
                },
                PlannerAssignment {
                    bead_id: "x".to_string(),
                    agent_id: "a2".to_string(),
                    score: 0.5,
                    rank: 1,
                },
            ]);
            ml.detect_conflicts(&aset, &[], &[], (i * 1000) as i64, &issues);
            prop_assert!(
                ml.state().conflict_history.len() <= history_max,
                "history {} exceeded max {} at cycle {}",
                ml.state().conflict_history.len(),
                history_max,
                i
            );
        }
    }

    // ── ML-P12: Disabled detection produces no side effects ─────────────────

    #[test]
    fn ml_p12_disabled_no_side_effects(
        num_agents in 2usize..6,
        num_claims in 1usize..5,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                enabled: false,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let issues: Vec<BeadIssueDetail> = (0..num_claims)
            .map(|i| make_issue(&format!("b{}", i), 0))
            .collect();
        let aset = make_assignment_set(
            (0..num_agents)
                .map(|i| PlannerAssignment {
                    bead_id: format!("b{}", i % num_claims),
                    agent_id: format!("a{}", i),
                    score: 0.5,
                    rank: 1,
                })
                .collect(),
        );
        let active: Vec<ActiveBeadClaim> = (0..num_claims)
            .map(|i| ActiveBeadClaim {
                bead_id: format!("b{}", i),
                agent_id: format!("other{}", i),
                claimed_at_ms: 100,
            })
            .collect();
        let report = ml.detect_conflicts(&aset, &[], &active, 5000, &issues);
        prop_assert!(report.conflicts.is_empty());
        prop_assert!(report.messages.is_empty());
        prop_assert_eq!(ml.state().total_conflicts_detected, 0);
        prop_assert_eq!(ml.state().total_conflicts_auto_resolved, 0);
    }

    // ── ML-P13: ManualResolution strategy always defers ─────────────────────

    #[test]
    fn ml_p13_manual_resolution_always_defers(
        num_agents in 2usize..5,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                strategy: DeconflictionStrategy::ManualResolution,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![make_issue("x", 0)];
        let aset = make_assignment_set(
            (0..num_agents)
                .map(|i| PlannerAssignment {
                    bead_id: "x".to_string(),
                    agent_id: format!("a{}", i),
                    score: (i as f64).mul_add(-0.1, 1.0),
                    rank: 1,
                })
                .collect(),
        );
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);
        for conflict in &report.conflicts {
            prop_assert_eq!(
                &conflict.resolution,
                &ConflictResolution::PendingManualResolution,
                "ManualResolution strategy should never auto-resolve"
            );
        }
        prop_assert_eq!(report.auto_resolved_count, 0);
        prop_assert_eq!(report.pending_resolution_count, report.conflicts.len());
    }

    // ── ML-P14: FirstClaimWins always favors existing holder ────────────────

    #[test]
    fn ml_p14_first_claim_wins_favors_existing(
        score_a in 0.0f64..1.0,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                strategy: DeconflictionStrategy::FirstClaimWins,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![make_issue("bead-a", 0), make_issue("bead-b", 0)];
        let aset = make_assignment_set(vec![PlannerAssignment {
            bead_id: "bead-a".to_string(),
            agent_id: "new-agent".to_string(),
            score: score_a,
            rank: 1,
        }]);
        let reservations = vec![
            KnownReservation {
                holder: "new-agent".to_string(),
                paths: vec!["src/shared.rs".to_string()],
                exclusive: true,
                bead_id: Some("bead-a".to_string()),
                expires_at_ms: Some(999_999),
            },
            KnownReservation {
                holder: "existing-agent".to_string(),
                paths: vec!["src/shared.rs".to_string()],
                exclusive: true,
                bead_id: Some("bead-b".to_string()),
                expires_at_ms: Some(999_999),
            },
        ];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        prop_assert_eq!(report.conflicts.len(), 1);
        match &report.conflicts[0].resolution {
            ConflictResolution::AutoResolved { winner_agent, .. } => {
                prop_assert_eq!(
                    winner_agent,
                    "existing-agent",
                    "FirstClaimWins must always favor existing holder"
                );
            }
            other => {
                prop_assert!(false, "Expected AutoResolved, got {:?}", other);
            }
        }
    }

    // ── ML-P15: Config serde roundtrip preserves all fields ─────────────────

    #[test]
    fn ml_p15_config_serde_roundtrip(
        cap in 1usize..100,
        max_risky in 1usize..50,
        max_conflicts in 1usize..100,
    ) {
        let config = MissionLoopConfig {
            safety_envelope: MissionSafetyEnvelopeConfig {
                max_assignments_per_cycle: cap,
                max_risky_assignments_per_cycle: max_risky,
                max_consecutive_retries_per_bead: 5,
                ..MissionSafetyEnvelopeConfig::default()
            },
            conflict_detection: ConflictDetectionConfig {
                max_conflicts_per_cycle: max_conflicts,
                strategy: DeconflictionStrategy::PriorityWins,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: MissionLoopConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            back.safety_envelope.max_assignments_per_cycle,
            cap
        );
        prop_assert_eq!(
            back.safety_envelope.max_risky_assignments_per_cycle,
            max_risky
        );
        prop_assert_eq!(
            back.conflict_detection.max_conflicts_per_cycle,
            max_conflicts
        );
        prop_assert_eq!(
            back.conflict_detection.strategy,
            DeconflictionStrategy::PriorityWins
        );
    }

    // ── ML-P16: Expired reservations don't trigger conflicts ────────────────

    #[test]
    fn ml_p16_expired_reservations_ignored(
        expiry_ms in 0i64..5000,
        current_ms in 5000i64..10000,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![make_issue("a", 0), make_issue("b", 1)];
        let aset = make_assignment_set(vec![PlannerAssignment {
            bead_id: "a".to_string(),
            agent_id: "agent1".to_string(),
            score: 0.5,
            rank: 1,
        }]);
        let reservations = vec![
            KnownReservation {
                holder: "agent1".to_string(),
                paths: vec!["src/x.rs".to_string()],
                exclusive: true,
                bead_id: Some("a".to_string()),
                expires_at_ms: Some(999_999),
            },
            KnownReservation {
                holder: "agent2".to_string(),
                paths: vec!["src/x.rs".to_string()],
                exclusive: true,
                bead_id: Some("b".to_string()),
                expires_at_ms: Some(expiry_ms), // expired before current_ms
            },
        ];
        let report = ml.detect_conflicts(&aset, &reservations, &[], current_ms, &issues);
        // The reservation from agent2 should be expired and ignored.
        prop_assert!(
            report.conflicts.is_empty(),
            "Expired reservation should not trigger conflict, got {} conflicts",
            report.conflicts.len()
        );
    }

    // ── ML-P17: Non-exclusive reservations don't trigger conflicts ──────────

    #[test]
    fn ml_p17_non_exclusive_reservations_ignored(
        num_reservations in 1usize..5,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![make_issue("a", 0)];
        let aset = make_assignment_set(vec![PlannerAssignment {
            bead_id: "a".to_string(),
            agent_id: "agent1".to_string(),
            score: 0.5,
            rank: 1,
        }]);
        let mut reservations = vec![KnownReservation {
            holder: "agent1".to_string(),
            paths: vec!["src/x.rs".to_string()],
            exclusive: true,
            bead_id: Some("a".to_string()),
            expires_at_ms: Some(999_999),
        }];
        for i in 0..num_reservations {
            reservations.push(KnownReservation {
                holder: format!("other{}", i),
                paths: vec!["src/x.rs".to_string()],
                exclusive: false, // non-exclusive
                bead_id: Some(format!("other-bead{}", i)),
                expires_at_ms: Some(999_999),
            });
        }
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        prop_assert!(
            report.conflicts.is_empty(),
            "Non-exclusive reservations should not trigger conflict"
        );
    }

    // ── ML-P18: Same-agent reservations don't self-conflict ─────────────────

    #[test]
    fn ml_p18_same_agent_no_self_conflict(
        num_paths in 1usize..5,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![make_issue("a", 0)];
        let paths: Vec<String> = (0..num_paths).map(|i| format!("src/m{}.rs", i)).collect();
        let aset = make_assignment_set(vec![PlannerAssignment {
            bead_id: "a".to_string(),
            agent_id: "agent1".to_string(),
            score: 0.5,
            rank: 1,
        }]);
        // Both reservations are from agent1 — same agent.
        let reservations = vec![
            KnownReservation {
                holder: "agent1".to_string(),
                paths: paths.clone(),
                exclusive: true,
                bead_id: Some("a".to_string()),
                expires_at_ms: Some(999_999),
            },
            KnownReservation {
                holder: "agent1".to_string(),
                paths,
                exclusive: true,
                bead_id: Some("b".to_string()),
                expires_at_ms: Some(999_999),
            },
        ];
        let report = ml.detect_conflicts(&aset, &reservations, &[], 5000, &issues);
        prop_assert!(
            report.conflicts.is_empty(),
            "Same-agent reservations should never self-conflict"
        );
    }

    // ── ML-P19: State serde roundtrip preserves conflict counters ────────────

    #[test]
    fn ml_p19_state_serde_preserves_counters(
        num_cycles in 1usize..5,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let issues = vec![make_issue("x", 0)];
        for i in 0..num_cycles {
            let aset = make_assignment_set(vec![
                PlannerAssignment {
                    bead_id: "x".to_string(),
                    agent_id: "a1".to_string(),
                    score: 1.0,
                    rank: 1,
                },
                PlannerAssignment {
                    bead_id: "x".to_string(),
                    agent_id: "a2".to_string(),
                    score: 0.5,
                    rank: 1,
                },
            ]);
            ml.detect_conflicts(&aset, &[], &[], (i * 1000) as i64, &issues);
        }
        let state = ml.state().clone();
        let json = serde_json::to_string(&state).unwrap();
        let back: MissionLoopState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_conflicts_detected, state.total_conflicts_detected);
        prop_assert_eq!(back.total_conflicts_auto_resolved, state.total_conflicts_auto_resolved);
        prop_assert_eq!(back.conflict_history.len(), state.conflict_history.len());
    }

    // ── ML-P20: Messages disabled produces empty message list ───────────────

    #[test]
    fn ml_p20_messages_disabled_empty(
        num_agents in 2usize..5,
    ) {
        let mut ml = MissionLoop::new(MissionLoopConfig {
            conflict_detection: ConflictDetectionConfig {
                generate_messages: false,
                ..ConflictDetectionConfig::default()
            },
            ..MissionLoopConfig::default()
        });
        let issues = vec![make_issue("x", 0)];
        let aset = make_assignment_set(
            (0..num_agents)
                .map(|i| PlannerAssignment {
                    bead_id: "x".to_string(),
                    agent_id: format!("a{}", i),
                    score: 0.5,
                    rank: 1,
                })
                .collect(),
        );
        let report = ml.detect_conflicts(&aset, &[], &[], 5000, &issues);
        prop_assert!(
            !report.conflicts.is_empty(),
            "Should have conflicts"
        );
        prop_assert!(
            report.messages.is_empty(),
            "Messages should be empty when generate_messages=false"
        );
    }
}

// ============================================================================
// Serde roundtrip tests for 26 uncovered data types (PinkForge session 16)
// ============================================================================

fn arb_ml_str() -> impl Strategy<Value = String> {
    "[a-z0-9_]{1,15}"
}

fn arb_operator_override_kind() -> impl Strategy<Value = OperatorOverrideKind> {
    prop_oneof![
        (arb_ml_str(), arb_ml_str()).prop_map(|(bead, agent)| {
            OperatorOverrideKind::Pin { bead_id: bead, target_agent: agent }
        }),
        arb_ml_str().prop_map(|b| OperatorOverrideKind::Exclude { bead_id: b }),
        arb_ml_str().prop_map(|a| OperatorOverrideKind::ExcludeAgent { agent_id: a }),
        (arb_ml_str(), -100i32..100).prop_map(|(b, d)| {
            OperatorOverrideKind::Reprioritize { bead_id: b, score_delta: d }
        }),
    ]
}

fn arb_operator_override() -> impl Strategy<Value = OperatorOverride> {
    (arb_ml_str(), arb_operator_override_kind(), arb_ml_str(), arb_ml_str(), arb_ml_str(), 0i64..2_000_000_000_000)
        .prop_map(|(id, kind, by, rc, rationale, at)| OperatorOverride {
            override_id: id, kind, activated_by: by, reason_code: rc,
            rationale, activated_at_ms: at, expires_at_ms: None, correlation_id: None,
        })
}

fn arb_conflict_type() -> impl Strategy<Value = ConflictType> {
    prop_oneof![
        Just(ConflictType::FileReservationOverlap),
        Just(ConflictType::ResourceReservationOverlap),
        Just(ConflictType::ConcurrentBeadClaim),
        Just(ConflictType::ActiveClaimCollision),
    ]
}

fn arb_conflict_resolution() -> impl Strategy<Value = ConflictResolution> {
    prop_oneof![
        (arb_ml_str(), arb_ml_str()).prop_map(|(w, l)| ConflictResolution::AutoResolved {
            winner_agent: w, loser_agent: l, strategy: DeconflictionStrategy::PriorityWins,
        }),
        (0i64..60_000).prop_map(|ms| ConflictResolution::Deferred { retry_after_ms: ms }),
        Just(ConflictResolution::PendingManualResolution),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // --- Operator override types ---

    #[test]
    fn ml_s01_operator_override_kind_serde(val in arb_operator_override_kind()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: OperatorOverrideKind = serde_json::from_str(&json).unwrap();
        // Compare discriminant
        let check = matches!(
            (&val, &back),
            (OperatorOverrideKind::Pin { .. }, OperatorOverrideKind::Pin { .. })
            | (OperatorOverrideKind::Exclude { .. }, OperatorOverrideKind::Exclude { .. })
            | (OperatorOverrideKind::ExcludeAgent { .. }, OperatorOverrideKind::ExcludeAgent { .. })
            | (OperatorOverrideKind::Reprioritize { .. }, OperatorOverrideKind::Reprioritize { .. })
        );
        prop_assert!(check);
    }

    #[test]
    fn ml_s02_operator_override_serde(val in arb_operator_override()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: OperatorOverride = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.override_id, &val.override_id);
        prop_assert_eq!(&back.activated_by, &val.activated_by);
        prop_assert_eq!(back.activated_at_ms, val.activated_at_ms);
    }

    #[test]
    fn ml_s03_operator_override_state_serde(val in arb_operator_override()) {
        let state = OperatorOverrideState {
            active: vec![val.clone()],
            history: vec![],
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: OperatorOverrideState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.active.len(), 1);
        prop_assert_eq!(&back.active[0].override_id, &val.override_id);
    }

    #[test]
    fn ml_s04_override_application_summary_serde(expired in 0usize..10) {
        let summary = OverrideApplicationSummary {
            excluded_beads: vec!["b1".to_string()],
            excluded_agents: vec!["a1".to_string()],
            pinned_assignments: vec![PinnedAssignmentRecord {
                bead_id: "b2".to_string(), agent_id: "a2".to_string(),
                override_id: "ov-1".to_string(),
            }],
            reprioritized_beads: vec![],
            expired_overrides: expired,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: OverrideApplicationSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.expired_overrides, expired);
        prop_assert_eq!(back.excluded_beads.len(), 1);
    }

    #[test]
    fn ml_s05_pinned_assignment_record_serde(bead in arb_ml_str(), agent in arb_ml_str()) {
        let rec = PinnedAssignmentRecord {
            bead_id: bead.clone(), agent_id: agent.clone(), override_id: "ov-1".to_string(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: PinnedAssignmentRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.bead_id, &bead);
        prop_assert_eq!(&back.agent_id, &agent);
    }

    #[test]
    fn ml_s06_reprioritized_bead_record_serde(bead in arb_ml_str(), delta in -100i32..100) {
        let rec = ReprioritizedBeadRecord {
            bead_id: bead.clone(), original_score: 0.5, adjusted_score: 0.5 + delta as f64,
            delta,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: ReprioritizedBeadRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.bead_id, &bead);
        prop_assert_eq!(back.delta, delta);
    }

    // --- Conflict detection detail types ---

    #[test]
    fn ml_s07_assignment_conflict_serde(ct in arb_conflict_type(), cr in arb_conflict_resolution()) {
        let val = AssignmentConflict {
            conflict_id: "c-1".to_string(), conflict_type: ct,
            involved_agents: vec!["a1".to_string(), "a2".to_string()],
            involved_beads: vec!["b1".to_string()],
            conflicting_paths: vec!["/src/main.rs".to_string()],
            detected_at_ms: 1_700_000_000_000, resolution: cr,
            reason_code: "dup".to_string(), error_code: "ML-100".to_string(),
        };
        let json = serde_json::to_string(&val).unwrap();
        let back: AssignmentConflict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.conflict_id, "c-1");
        prop_assert_eq!(back.involved_agents.len(), 2);
    }

    #[test]
    fn ml_s08_deconfliction_message_serde(recipient in arb_ml_str(), subject in arb_ml_str()) {
        let msg = DeconflictionMessage {
            recipient: recipient.clone(), subject: subject.clone(),
            body: "please release".to_string(), thread_id: "t-1".to_string(),
            importance: "high".to_string(), conflict_id: "c-1".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: DeconflictionMessage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.recipient, &recipient);
        prop_assert_eq!(&back.subject, &subject);
    }

    #[test]
    fn ml_s09_known_resource_reservation_serde(holder in arb_ml_str(), exclusive in proptest::bool::ANY) {
        let res = KnownResourceReservation {
            holder: holder.clone(), resources: vec!["/path".to_string()],
            exclusive, bead_id: Some("b1".to_string()), expires_at_ms: Some(9999),
        };
        let json = serde_json::to_string(&res).unwrap();
        let back: KnownResourceReservation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.holder, &holder);
        prop_assert_eq!(back.exclusive, exclusive);
    }

    // --- Mission metrics types ---

    #[test]
    fn ml_s10_mission_metrics_labels_serde(ws in arb_ml_str(), track in arb_ml_str()) {
        let labels = MissionMetricsLabels { workspace: ws.clone(), track: track.clone() };
        let json = serde_json::to_string(&labels).unwrap();
        let back: MissionMetricsLabels = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.workspace, &ws);
        prop_assert_eq!(&back.track, &track);
    }

    #[test]
    fn ml_s11_extraction_summary_serde(total in 0usize..100, ready in 0usize..100) {
        let es = ExtractionSummary { total_candidates: total, ready_candidates: ready, top_impact_bead: None };
        let json = serde_json::to_string(&es).unwrap();
        let back: ExtractionSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_candidates, total);
        prop_assert_eq!(back.ready_candidates, ready);
    }

    #[test]
    fn ml_s12_scorer_summary_serde(scored in 0usize..100, above in 0usize..100) {
        let ss = ScorerSummary { scored_count: scored, above_threshold_count: above, top_scored_bead: None };
        let json = serde_json::to_string(&ss).unwrap();
        let back: ScorerSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.scored_count, scored);
        prop_assert_eq!(back.above_threshold_count, above);
    }

    #[test]
    fn ml_s13_mission_metrics_totals_serde(cycles in 0u64..10000, assignments in 0u64..10000) {
        let t = MissionMetricsTotals {
            cycles, assignments, rejections: 0, conflict_rejections: 0,
            policy_denials: 0, unblocked_transitions: 0, planner_churn_events: 0,
            assignments_by_agent: HashMap::new(),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: MissionMetricsTotals = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.cycles, cycles);
        prop_assert_eq!(back.assignments, assignments);
    }

    #[test]
    fn ml_s14_mission_cycle_metrics_sample_serde(
        cycle_id in 0u64..10000,
        ts in 0i64..2_000_000_000_000,
        latency in 0u64..10000,
    ) {
        let sample = MissionCycleMetricsSample {
            cycle_id, timestamp_ms: ts, evaluation_latency_ms: latency,
            assignments: 5, rejections: 1, conflict_rejections: 0,
            policy_denials: 0, unblocked_transitions: 2, planner_churn_events: 0,
            throughput_assignments_per_minute: 10.0, unblock_velocity_per_minute: 4.0,
            conflict_rate: 0.0, planner_churn_rate: 0.0, policy_deny_rate: 0.0,
            assignments_by_agent: HashMap::new(),
            workspace_label: "default".to_string(), track_label: "main".to_string(),
        };
        let json = serde_json::to_string(&sample).unwrap();
        let back: MissionCycleMetricsSample = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.cycle_id, cycle_id);
        prop_assert_eq!(back.timestamp_ms, ts);
        prop_assert_eq!(back.evaluation_latency_ms, latency);
    }

    // --- Operator report types ---

    #[test]
    fn ml_s15_operator_status_section_serde(cycles in 0u64..10000, total_a in 0u64..10000) {
        let sec = OperatorStatusSection {
            cycle_count: cycles, last_evaluation_ms: Some(5000),
            total_assignments: total_a, total_rejections: 0,
            pending_trigger_count: 0, phase_label: "running".to_string(),
        };
        let json = serde_json::to_string(&sec).unwrap();
        let back: OperatorStatusSection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.cycle_count, cycles);
        prop_assert_eq!(back.total_assignments, total_a);
    }

    #[test]
    fn ml_s16_agent_assignment_row_serde(agent in arb_ml_str(), total in 0u64..100) {
        let row = AgentAssignmentRow {
            agent_id: agent.clone(), total_assignments: total,
            active_beads: 2, active_bead_ids: vec!["b1".to_string(), "b2".to_string()],
        };
        let json = serde_json::to_string(&row).unwrap();
        let back: AgentAssignmentRow = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.agent_id, &agent);
        prop_assert_eq!(back.total_assignments, total);
    }

    #[test]
    fn ml_s17_operator_health_section_serde(overall in arb_ml_str()) {
        let sec = OperatorHealthSection {
            throughput_assignments_per_minute: 10.0, unblock_velocity_per_minute: 5.0,
            conflict_rate: 0.01, planner_churn_rate: 0.0, policy_deny_rate: 0.0,
            avg_evaluation_latency_ms: 42.5, overall: overall.clone(),
        };
        let json = serde_json::to_string(&sec).unwrap();
        let back: OperatorHealthSection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.overall, &overall);
        // f64 tolerance
        prop_assert!((back.avg_evaluation_latency_ms - 42.5).abs() < 1e-10);
    }

    #[test]
    fn ml_s18_operator_conflict_section_serde(detected in 0u64..100, auto_res in 0u64..100) {
        let sec = OperatorConflictSection {
            total_detected: detected, total_auto_resolved: auto_res,
            pending_manual: 0, recent_conflicts: vec![],
        };
        let json = serde_json::to_string(&sec).unwrap();
        let back: OperatorConflictSection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_detected, detected);
        prop_assert_eq!(back.total_auto_resolved, auto_res);
    }

    #[test]
    fn ml_s19_operator_conflict_summary_serde(cid in arb_ml_str()) {
        let s = OperatorConflictSummary {
            conflict_id: cid.clone(), conflict_type: "duplicate".to_string(),
            agents: vec!["a1".to_string()], beads: vec!["b1".to_string()],
            resolution: "auto".to_string(), reason_code: "dup".to_string(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: OperatorConflictSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.conflict_id, &cid);
    }

    #[test]
    fn ml_s20_operator_event_section_serde(retained in 0usize..100, emitted in 0u64..10000) {
        let sec = OperatorEventSection {
            retained_events: retained, total_emitted: emitted,
            by_phase: HashMap::new(), by_kind: HashMap::new(),
        };
        let json = serde_json::to_string(&sec).unwrap();
        let back: OperatorEventSection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.retained_events, retained);
        prop_assert_eq!(back.total_emitted, emitted);
    }

    #[test]
    fn ml_s21_operator_decision_summary_serde(bead in arb_ml_str(), outcome in arb_ml_str()) {
        let ds = OperatorDecisionSummary {
            bead_id: bead.clone(), outcome: outcome.clone(),
            summary: "assigned".to_string(),
            top_factors: vec![OperatorFactorSummary {
                dimension: "urgency".to_string(), value: 0.8,
                polarity: "positive".to_string(), description: "High urgency".to_string(),
            }],
        };
        let json = serde_json::to_string(&ds).unwrap();
        let back: OperatorDecisionSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.bead_id, &bead);
        prop_assert_eq!(&back.outcome, &outcome);
        prop_assert_eq!(back.top_factors.len(), 1);
    }

    #[test]
    fn ml_s22_operator_factor_summary_serde(dim in arb_ml_str(), polarity in arb_ml_str()) {
        let fs = OperatorFactorSummary {
            dimension: dim.clone(), value: 0.75,
            polarity: polarity.clone(), description: "test factor".to_string(),
        };
        let json = serde_json::to_string(&fs).unwrap();
        let back: OperatorFactorSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.dimension, &dim);
        prop_assert_eq!(&back.polarity, &polarity);
        prop_assert!((back.value - 0.75).abs() < 1e-10);
    }

    #[test]
    fn ml_s23_operator_status_report_serde(cycles in 0u64..10000) {
        let report = OperatorStatusReport {
            status: OperatorStatusSection {
                cycle_count: cycles, last_evaluation_ms: None,
                total_assignments: 0, total_rejections: 0,
                pending_trigger_count: 0, phase_label: "idle".to_string(),
            },
            assignment_table: vec![],
            health: OperatorHealthSection {
                throughput_assignments_per_minute: 0.0, unblock_velocity_per_minute: 0.0,
                conflict_rate: 0.0, planner_churn_rate: 0.0, policy_deny_rate: 0.0,
                avg_evaluation_latency_ms: 0.0, overall: "healthy".to_string(),
            },
            conflicts: OperatorConflictSection {
                total_detected: 0, total_auto_resolved: 0,
                pending_manual: 0, recent_conflicts: vec![],
            },
            event_summary: OperatorEventSection {
                retained_events: 0, total_emitted: 0,
                by_phase: HashMap::new(), by_kind: HashMap::new(),
            },
            latest_explanations: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: OperatorStatusReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.status.cycle_count, cycles);
    }

    // --- MissionDecision (compound type) ---

    #[test]
    fn ml_s24_mission_decision_serde(cycle_id in 0u64..10000, ts in 0i64..2_000_000_000_000) {
        let d = MissionDecision {
            cycle_id, timestamp_ms: ts,
            trigger: MissionTrigger::CadenceTick,
            assignment_set: AssignmentSet {
                assignments: vec![], rejected: vec![],
                solver_config: SolverConfig::default(),
            },
            extraction_summary: ExtractionSummary {
                total_candidates: 0, ready_candidates: 0, top_impact_bead: None,
            },
            scorer_summary: ScorerSummary {
                scored_count: 0, above_threshold_count: 0, top_scored_bead: None,
            },
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: MissionDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.cycle_id, cycle_id);
        prop_assert_eq!(back.timestamp_ms, ts);
    }
}
