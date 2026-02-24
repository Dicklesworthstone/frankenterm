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
                    score: 1.0 - (i as f64 * 0.05),
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
                    score: 1.0 - (i as f64 * 0.1),
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
                if pri_a < pri_b {
                    prop_assert_eq!(winner_agent, "agent-alpha");
                    prop_assert_eq!(loser_agent, "agent-beta");
                } else if pri_a > pri_b {
                    prop_assert_eq!(winner_agent, "agent-beta");
                    prop_assert_eq!(loser_agent, "agent-alpha");
                } else {
                    // Equal priority: score_a >= score_b → alpha wins.
                    // score_a is the new assignment's score, score_b=0.0 for reservations.
                    // In resolve_conflict, agent_a=alpha with score_a, agent_b=beta with 0.0.
                    if score_a >= 0.0 {
                        prop_assert_eq!(winner_agent, "agent-alpha");
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
                    score: 1.0 - (i as f64 * 0.1),
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
                    score: 1.0 - (i as f64 * 0.1),
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
