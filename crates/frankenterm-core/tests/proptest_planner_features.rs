#![cfg(feature = "subprocess-bridge")]
#![allow(clippy::manual_range_contains)]

//! Property-based tests for the planner feature extraction, multi-factor scoring,
//! assignment solver, anti-thrash governor, mission profiles, and utility policy tuner.

use proptest::prelude::*;

use frankenterm_core::beads_types::{
    BeadReadinessReport, BeadReadyCandidate, BeadResolverReasonCode, BeadStatus,
};
use frankenterm_core::plan::{MissionAgentAvailability, MissionAgentCapabilityProfile};
use frankenterm_core::planner_features::{
    ConflictPair, EffortBucket, GovernorAction, GovernorConfig, MissionProfile, MissionProfileKind,
    PlannerExtractionConfig, PlannerExtractionContext, PlannerFeatureVector, PlannerWeights,
    SafetyGate, ScoredCandidate, ScorerConfig, ScorerInput, SolverConfig, ThrashGovernor,
    UtilityPolicyTuner, extract_planner_features, extract_planner_features_all, score_candidates,
    solve_assignments,
};

// ── Strategies ────────────────────────────────────────────────────────────────

fn arb_bead_id() -> impl Strategy<Value = String> {
    "[a-z]{2,6}-[0-9]{1,4}".prop_map(|s| s)
}

fn arb_bead_status() -> impl Strategy<Value = BeadStatus> {
    prop_oneof![
        Just(BeadStatus::Open),
        Just(BeadStatus::InProgress),
        Just(BeadStatus::Blocked),
        Just(BeadStatus::Deferred),
        Just(BeadStatus::Closed),
    ]
}

fn arb_reason_code() -> impl Strategy<Value = BeadResolverReasonCode> {
    prop_oneof![
        Just(BeadResolverReasonCode::MissingDependencyNode),
        Just(BeadResolverReasonCode::CyclicDependencyGraph),
        Just(BeadResolverReasonCode::PartialGraphData),
    ]
}

fn arb_candidate(id: String) -> impl Strategy<Value = BeadReadyCandidate> {
    (
        arb_bead_status(),
        0u8..5,
        0usize..5,
        0usize..20,
        0usize..10,
        any::<bool>(),
        proptest::collection::vec(arb_reason_code(), 0..3),
    )
        .prop_map(
            move |(status, priority, blocker_count, unblock_count, depth, ready, degraded)| {
                BeadReadyCandidate {
                    id: id.clone(),
                    title: format!("Bead {}", id),
                    status,
                    priority,
                    blocker_count,
                    blocker_ids: Vec::new(),
                    transitive_unblock_count: unblock_count,
                    critical_path_depth_hint: depth,
                    ready,
                    degraded_reasons: degraded,
                }
            },
        )
}

fn arb_readiness_report(n: usize) -> impl Strategy<Value = BeadReadinessReport> {
    proptest::collection::vec(arb_bead_id(), n)
        .prop_flat_map(|ids| {
            // Deduplicate
            let unique: Vec<String> = {
                let mut seen = std::collections::HashSet::new();
                ids.into_iter()
                    .filter(|id| seen.insert(id.clone()))
                    .collect()
            };
            let candidates_strat: Vec<_> =
                unique.iter().map(|id| arb_candidate(id.clone())).collect();
            candidates_strat
        })
        .prop_map(|candidates| {
            let ready_ids: Vec<String> = candidates
                .iter()
                .filter(|c| c.ready)
                .map(|c| c.id.clone())
                .collect();
            BeadReadinessReport {
                candidates,
                ready_ids,
                degraded_reason_codes: Vec::new(),
            }
        })
}

fn arb_extraction_config() -> impl Strategy<Value = PlannerExtractionConfig> {
    (
        1usize..20,
        1usize..15,
        1.0f64..500.0,
        0.0f64..1.0,
        0.0f64..1.0,
        0.0f64..1.0,
        0.0f64..1.0,
    )
        .prop_map(|(max_unblock, max_depth, max_stale, iuw, idw, upw, usw)| {
            PlannerExtractionConfig {
                max_unblock_count: max_unblock,
                max_critical_depth: max_depth,
                max_staleness_hours: max_stale,
                impact_unblock_weight: iuw,
                impact_depth_weight: idw,
                urgency_priority_weight: upw,
                urgency_staleness_weight: usw,
            }
        })
}

fn arb_planner_weights() -> impl Strategy<Value = PlannerWeights> {
    (
        0.0f64..1.0,
        0.0f64..1.0,
        0.0f64..1.0,
        0.0f64..1.0,
        0.0f64..1.0,
    )
        .prop_map(|(impact, urgency, risk, fit, confidence)| PlannerWeights {
            impact,
            urgency,
            risk,
            fit,
            confidence,
        })
}

fn arb_feature_vector() -> impl Strategy<Value = PlannerFeatureVector> {
    (
        arb_bead_id(),
        0.0f64..=1.0,
        0.0f64..=1.0,
        0.0f64..=1.0,
        0.0f64..=1.0,
        0.0f64..=1.0,
    )
        .prop_map(
            |(id, impact, urgency, risk, fit, confidence)| PlannerFeatureVector {
                bead_id: id,
                impact,
                urgency,
                risk,
                fit,
                confidence,
            },
        )
}

fn arb_effort_bucket() -> impl Strategy<Value = EffortBucket> {
    prop_oneof![
        Just(EffortBucket::Trivial),
        Just(EffortBucket::Small),
        Just(EffortBucket::Medium),
        Just(EffortBucket::Large),
        Just(EffortBucket::Epic),
    ]
}

fn arb_scorer_input() -> impl Strategy<Value = ScorerInput> {
    (
        arb_feature_vector(),
        proptest::option::of(arb_effort_bucket()),
        proptest::collection::vec(
            prop_oneof![
                Just("safety".to_string()),
                Just("regression".to_string()),
                Just("bug".to_string()),
                Just("feature".to_string()),
                Just("policy".to_string()),
            ],
            0..3,
        ),
    )
        .prop_map(|(features, effort, tags)| ScorerInput {
            features,
            effort,
            tags,
        })
}

fn arb_scorer_config() -> impl Strategy<Value = ScorerConfig> {
    (
        arb_planner_weights(),
        0.0f64..0.5,
        1.0f64..2.0,
        1.0f64..2.0,
        0.0f64..0.5,
        0.0001f64..0.01,
    )
        .prop_map(|(weights, ew, sb, rb, mct, tbe)| ScorerConfig {
            weights,
            effort_weight: ew,
            safety_bonus: sb,
            regression_bonus: rb,
            min_confidence_threshold: mct,
            tie_break_epsilon: tbe,
        })
}

fn arb_profile_kind() -> impl Strategy<Value = MissionProfileKind> {
    prop_oneof![
        Just(MissionProfileKind::Balanced),
        Just(MissionProfileKind::SafetyFirst),
        Just(MissionProfileKind::Throughput),
        Just(MissionProfileKind::UrgencyDriven),
        Just(MissionProfileKind::Conservative),
    ]
}

// ── Tests: Feature vector composite scoring ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Composite score is always in [0.0, 1.0].
    #[test]
    fn prop_composite_score_bounded(
        fv in arb_feature_vector(),
        weights in arb_planner_weights(),
    ) {
        let score = fv.composite_score_with_weights(&weights);
        prop_assert!(score >= 0.0 && score <= 1.0,
            "composite score {} out of bounds", score);
    }

    /// With zero weights, composite score is 0.
    #[test]
    fn prop_zero_weights_zero_score(fv in arb_feature_vector()) {
        let zero_w = PlannerWeights {
            impact: 0.0,
            urgency: 0.0,
            risk: 0.0,
            fit: 0.0,
            confidence: 0.0,
        };
        let score = fv.composite_score_with_weights(&zero_w);
        prop_assert!((score - 0.0).abs() < 1e-10,
            "expected 0 with zero weights, got {}", score);
    }

    /// Default weights sum to 1.0.
    #[test]
    fn prop_default_weights_sum_one(_dummy in 0u8..1) {
        let w = PlannerWeights::default();
        let total = w.total();
        prop_assert!((total - 1.0).abs() < 1e-10,
            "default weights total {} != 1.0", total);
    }
}

// ── Tests: Feature extraction ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Extracted features only include ready candidates.
    #[test]
    fn prop_extraction_only_ready(
        report in arb_readiness_report(8),
        config in arb_extraction_config(),
    ) {
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "agent-Alpha".to_string(),
            capabilities: vec!["general".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 5,
            availability: MissionAgentAvailability::Ready,
        }];
        let bead_ids: Vec<String> = report.candidates.iter().map(|c| c.id.clone()).collect();
        let context = PlannerExtractionContext {
            staleness_hours: bead_ids.iter().map(|id| (id.clone(), 24.0)).collect(),
        };

        let result = extract_planner_features(&report, &agents, &context, &config);

        let ready_count = report.candidates.iter().filter(|c| c.ready).count();
        prop_assert_eq!(result.features.len(), ready_count,
            "features count {} != ready count {}", result.features.len(), ready_count);
    }

    /// extract_planner_features_all includes ALL candidates (ready and blocked).
    #[test]
    fn prop_extraction_all_includes_all(
        report in arb_readiness_report(8),
        config in arb_extraction_config(),
    ) {
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "agent-Alpha".to_string(),
            capabilities: vec!["general".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 5,
            availability: MissionAgentAvailability::Ready,
        }];
        let bead_ids: Vec<String> = report.candidates.iter().map(|c| c.id.clone()).collect();
        let context = PlannerExtractionContext {
            staleness_hours: bead_ids.iter().map(|id| (id.clone(), 24.0)).collect(),
        };

        let result = extract_planner_features_all(&report, &agents, &context, &config);

        prop_assert_eq!(result.features.len(), report.candidates.len(),
            "features_all count {} != candidates count {}", result.features.len(), report.candidates.len());
    }

    /// All extracted feature values are in [0, 1].
    #[test]
    fn prop_extracted_features_bounded(
        report in arb_readiness_report(8),
        config in arb_extraction_config(),
    ) {
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "agent-Alpha".to_string(),
            capabilities: vec!["general".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 5,
            availability: MissionAgentAvailability::Ready,
        }];
        let bead_ids: Vec<String> = report.candidates.iter().map(|c| c.id.clone()).collect();
        let context = PlannerExtractionContext {
            staleness_hours: bead_ids.iter().map(|id| (id.clone(), 24.0)).collect(),
        };

        let result = extract_planner_features_all(&report, &agents, &context, &config);

        for fv in &result.features {
            prop_assert!(fv.impact >= 0.0 && fv.impact <= 1.0,
                "impact {} out of bounds for {}", fv.impact, fv.bead_id);
            prop_assert!(fv.urgency >= 0.0 && fv.urgency <= 1.0,
                "urgency {} out of bounds for {}", fv.urgency, fv.bead_id);
            prop_assert!(fv.risk >= 0.0 && fv.risk <= 1.0,
                "risk {} out of bounds for {}", fv.risk, fv.bead_id);
            prop_assert!(fv.fit >= 0.0 && fv.fit <= 1.0,
                "fit {} out of bounds for {}", fv.fit, fv.bead_id);
            prop_assert!(fv.confidence >= 0.0 && fv.confidence <= 1.0,
                "confidence {} out of bounds for {}", fv.confidence, fv.bead_id);
        }
    }

    /// Ranked IDs match features order.
    #[test]
    fn prop_ranked_ids_match_features_order(
        report in arb_readiness_report(8),
        config in arb_extraction_config(),
    ) {
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "agent-Alpha".to_string(),
            capabilities: vec!["general".to_string()],
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 5,
            availability: MissionAgentAvailability::Ready,
        }];
        let bead_ids: Vec<String> = report.candidates.iter().map(|c| c.id.clone()).collect();
        let context = PlannerExtractionContext {
            staleness_hours: bead_ids.iter().map(|id| (id.clone(), 24.0)).collect(),
        };

        let result = extract_planner_features(&report, &agents, &context, &config);

        let feature_ids: Vec<&str> = result.features.iter().map(|f| f.bead_id.as_str()).collect();
        let ranked: Vec<&str> = result.ranked_ids.iter().map(|s| s.as_str()).collect();
        prop_assert_eq!(feature_ids, ranked,
            "ranked_ids don't match features ordering");
    }
}

// ── Tests: Multi-factor scorer ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Scored candidates have correct rank assignments (1-based, sequential).
    #[test]
    fn prop_scorer_ranks_sequential(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..15),
        config in arb_scorer_config(),
    ) {
        let report = score_candidates(&inputs, &config);
        for (i, scored) in report.scored.iter().enumerate() {
            prop_assert_eq!(scored.rank, i + 1,
                "rank mismatch at position {}: expected {}, got {}", i, i + 1, scored.rank);
        }
    }

    /// Scored candidates are sorted by final_score descending (with tie-break).
    #[test]
    fn prop_scorer_sorted_descending(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..15),
        config in arb_scorer_config(),
    ) {
        let report = score_candidates(&inputs, &config);
        for pair in report.scored.windows(2) {
            let diff = (pair[0].final_score - pair[1].final_score).abs();
            if diff >= config.tie_break_epsilon {
                prop_assert!(pair[0].final_score >= pair[1].final_score,
                    "not sorted: {} < {} (diff {})", pair[0].final_score, pair[1].final_score, diff);
            }
        }
    }

    /// Candidates below confidence threshold get score = 0.
    #[test]
    fn prop_low_confidence_zero_score(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..10),
        config in arb_scorer_config(),
    ) {
        let report = score_candidates(&inputs, &config);
        for scored in &report.scored {
            if scored.below_confidence_threshold {
                prop_assert!((scored.final_score - 0.0).abs() < 1e-10,
                    "below-threshold candidate {} has non-zero score {}", scored.bead_id, scored.final_score);
            }
        }
    }

    /// All final_scores are in [0.0, 1.0].
    #[test]
    fn prop_scorer_output_bounded(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..15),
        config in arb_scorer_config(),
    ) {
        let report = score_candidates(&inputs, &config);
        for scored in &report.scored {
            prop_assert!(scored.final_score >= 0.0 && scored.final_score <= 1.0,
                "score {} out of bounds for {}", scored.final_score, scored.bead_id);
        }
    }

    /// Safety/regression tags produce tag_multiplier >= 1.0.
    #[test]
    fn prop_safety_tag_multiplier_at_least_one(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..10),
        config in arb_scorer_config(),
    ) {
        let report = score_candidates(&inputs, &config);
        for scored in &report.scored {
            prop_assert!(scored.tag_multiplier >= 1.0,
                "tag_multiplier {} < 1.0 for {}", scored.tag_multiplier, scored.bead_id);
        }
    }

    /// Scored output count matches input count.
    #[test]
    fn prop_scorer_count_matches(
        inputs in proptest::collection::vec(arb_scorer_input(), 0..15),
        config in arb_scorer_config(),
    ) {
        let report = score_candidates(&inputs, &config);
        prop_assert_eq!(report.scored.len(), inputs.len());
        prop_assert_eq!(report.ranked_ids.len(), inputs.len());
    }

    /// Ranked IDs are in same order as scored candidates.
    #[test]
    fn prop_scorer_ranked_ids_order(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..10),
        config in arb_scorer_config(),
    ) {
        let report = score_candidates(&inputs, &config);
        let scored_ids: Vec<&str> = report.scored.iter().map(|s| s.bead_id.as_str()).collect();
        let ranked: Vec<&str> = report.ranked_ids.iter().map(|s| s.as_str()).collect();
        prop_assert_eq!(scored_ids, ranked);
    }
}

// ── Tests: EffortBucket scoring ──────────────────────────────────────────────

proptest! {
    /// Effort scores are monotonically non-decreasing from Trivial to Epic.
    #[test]
    fn prop_effort_bucket_monotonic(_dummy in 0u8..1) {
        let buckets = [
            EffortBucket::Trivial,
            EffortBucket::Small,
            EffortBucket::Medium,
            EffortBucket::Large,
            EffortBucket::Epic,
        ];
        for pair in buckets.windows(2) {
            prop_assert!(pair[0].score() <= pair[1].score(),
                "effort not monotonic: {:?}({}) > {:?}({})",
                pair[0], pair[0].score(), pair[1], pair[1].score());
        }
    }

    /// All effort scores are in [0.0, 1.0].
    #[test]
    fn prop_effort_score_bounded(bucket in arb_effort_bucket()) {
        let score = bucket.score();
        prop_assert!(score >= 0.0 && score <= 1.0,
            "effort score {} out of bounds for {:?}", score, bucket);
    }
}

// ── Tests: Assignment solver ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Assignment count never exceeds max_assignments.
    #[test]
    fn prop_solver_respects_max_assignments(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..15),
        max_assignments in 1usize..10,
    ) {
        let config = ScorerConfig::default();
        let scored = score_candidates(&inputs, &config);
        let agents = vec![
            MissionAgentCapabilityProfile {
                agent_id: "agent-A".to_string(),
                capabilities: Vec::new(),
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 20,
                availability: MissionAgentAvailability::Ready,
            },
        ];
        let solver_config = SolverConfig {
            min_score: 0.0,
            max_assignments,
            safety_gates: Vec::new(),
            conflicts: Vec::new(),
        };

        let result = solve_assignments(&scored, &agents, &solver_config);

        prop_assert!(result.assignments.len() <= max_assignments,
            "assignments {} > max {}", result.assignments.len(), max_assignments);
    }

    /// No bead appears in both assigned and rejected.
    #[test]
    fn prop_solver_no_double_booking(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..15),
    ) {
        let config = ScorerConfig::default();
        let scored = score_candidates(&inputs, &config);
        let agents = vec![
            MissionAgentCapabilityProfile {
                agent_id: "agent-A".to_string(),
                capabilities: Vec::new(),
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 10,
                availability: MissionAgentAvailability::Ready,
            },
        ];
        let solver_config = SolverConfig::default();
        let result = solve_assignments(&scored, &agents, &solver_config);

        let assigned_ids: std::collections::HashSet<&str> =
            result.assignments.iter().map(|a| a.bead_id.as_str()).collect();
        for rej in &result.rejected {
            prop_assert!(!assigned_ids.contains(rej.bead_id.as_str()),
                "bead {} appears in both assignments and rejected", rej.bead_id);
        }
    }

    /// Safety gate denials are enforced: denied beads are rejected.
    #[test]
    fn prop_solver_safety_gate_enforced(
        inputs in proptest::collection::vec(arb_scorer_input(), 2..10),
    ) {
        let config = ScorerConfig::default();
        let scored = score_candidates(&inputs, &config);

        // Deny the first bead
        let denied_id = scored.scored.first().map(|s| s.bead_id.clone()).unwrap_or_default();
        if denied_id.is_empty() {
            return Ok(());
        }

        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "agent-A".to_string(),
            capabilities: Vec::new(),
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 20,
            availability: MissionAgentAvailability::Ready,
        }];
        let solver_config = SolverConfig {
            min_score: 0.0,
            max_assignments: 20,
            safety_gates: vec![SafetyGate {
                name: "test-gate".to_string(),
                denied_bead_ids: vec![denied_id.clone()],
            }],
            conflicts: Vec::new(),
        };

        let result = solve_assignments(&scored, &agents, &solver_config);
        let assigned_ids: std::collections::HashSet<&str> =
            result.assignments.iter().map(|a| a.bead_id.as_str()).collect();

        prop_assert!(!assigned_ids.contains(denied_id.as_str()),
            "safety-gate-denied bead {} was assigned", denied_id);
    }

    /// Conflict pairs: if both beads scored, at most one is assigned.
    #[test]
    fn prop_solver_conflict_enforced(
        inputs in proptest::collection::vec(arb_scorer_input(), 3..10),
    ) {
        let config = ScorerConfig::default();
        let scored = score_candidates(&inputs, &config);

        if scored.scored.len() < 2 {
            return Ok(());
        }

        let bead_a = scored.scored[0].bead_id.clone();
        let bead_b = scored.scored[1].bead_id.clone();

        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "agent-A".to_string(),
            capabilities: Vec::new(),
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 20,
            availability: MissionAgentAvailability::Ready,
        }];
        let solver_config = SolverConfig {
            min_score: 0.0,
            max_assignments: 20,
            safety_gates: Vec::new(),
            conflicts: vec![ConflictPair {
                bead_a: bead_a.clone(),
                bead_b: bead_b.clone(),
            }],
        };

        let result = solve_assignments(&scored, &agents, &solver_config);
        let assigned_ids: std::collections::HashSet<&str> =
            result.assignments.iter().map(|a| a.bead_id.as_str()).collect();

        let both_assigned = assigned_ids.contains(bead_a.as_str()) && assigned_ids.contains(bead_b.as_str());
        prop_assert!(!both_assigned,
            "conflicting beads {} and {} were both assigned", bead_a, bead_b);
    }

    /// With zero-capacity agents, nothing gets assigned.
    #[test]
    fn prop_solver_zero_capacity_no_assignments(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..10),
    ) {
        let config = ScorerConfig::default();
        let scored = score_candidates(&inputs, &config);
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "agent-Full".to_string(),
            capabilities: Vec::new(),
            lane_affinity: Vec::new(),
            current_load: 5,
            max_parallel_assignments: 5, // at capacity
            availability: MissionAgentAvailability::Ready,
        }];
        let solver_config = SolverConfig {
            min_score: 0.0,
            max_assignments: 20,
            safety_gates: Vec::new(),
            conflicts: Vec::new(),
        };

        let result = solve_assignments(&scored, &agents, &solver_config);

        prop_assert_eq!(result.assignments.len(), 0,
            "expected 0 assignments with zero capacity, got {}", result.assignments.len());
    }

    /// Offline agents get no assignments.
    #[test]
    fn prop_solver_offline_agents_skipped(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..10),
    ) {
        let config = ScorerConfig::default();
        let scored = score_candidates(&inputs, &config);
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "agent-Offline".to_string(),
            capabilities: Vec::new(),
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 10,
            availability: MissionAgentAvailability::Offline {
                reason_code: "test".to_string(),
            },
        }];
        let solver_config = SolverConfig {
            min_score: 0.0,
            max_assignments: 20,
            safety_gates: Vec::new(),
            conflicts: Vec::new(),
        };

        let result = solve_assignments(&scored, &agents, &solver_config);

        prop_assert_eq!(result.assignments.len(), 0,
            "expected 0 assignments with offline agents, got {}", result.assignments.len());
    }
}

// ── Tests: Anti-thrash governor ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Governor with no prior state allows all candidates.
    #[test]
    fn prop_governor_fresh_allows_all(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..10),
    ) {
        let config = ScorerConfig::default();
        let scored = score_candidates(&inputs, &config);
        let governor = ThrashGovernor::new(GovernorConfig::default());

        let report = governor.evaluate(&scored.scored);

        for verdict in &report.verdicts {
            let is_allow = matches!(verdict.action, GovernorAction::Allow);
            prop_assert!(is_allow,
                "fresh governor should Allow all, got {:?} for {}", verdict.action, verdict.bead_id);
        }
    }

    /// Cooldown blocks reassignment within the cooldown window.
    #[test]
    fn prop_governor_cooldown_blocks(
        cooldown_cycles in 2u64..8,
    ) {
        let config = GovernorConfig {
            reassignment_cooldown_cycles: cooldown_cycles,
            ..GovernorConfig::default()
        };
        let mut governor = ThrashGovernor::new(config);

        // Record a cycle with bead assigned
        governor.record_cycle(&["bead-1".to_string()]);

        // Immediately evaluate: should be blocked
        let candidate = ScoredCandidate {
            bead_id: "bead-1".to_string(),
            final_score: 0.8,
            feature_composite: 0.8,
            effort_penalty: 0.0,
            tag_multiplier: 1.0,
            below_confidence_threshold: false,
            rank: 1,
        };

        let report = governor.evaluate(&[candidate]);
        let verdict = &report.verdicts[0];
        let is_blocked = matches!(verdict.action, GovernorAction::BlockReassignment { .. });
        prop_assert!(is_blocked,
            "expected BlockReassignment, got {:?}", verdict.action);
    }

    /// Starvation boost is applied after enough skipped cycles.
    #[test]
    fn prop_governor_starvation_boost(
        threshold in 2u64..6,
        extra_cycles in 1u64..5,
    ) {
        let boost_per = 0.05;
        let config = GovernorConfig {
            reassignment_cooldown_cycles: 0,
            starvation_threshold_cycles: threshold,
            starvation_boost_per_cycle: boost_per,
            starvation_max_boost: 0.5,
            history_window: 20,
            thrash_flip_threshold: 100, // disable thrash detection
            thrash_penalty: 0.5,
        };
        let mut governor = ThrashGovernor::new(config);

        // Register bead
        governor.register_bead("bead-starved");

        // Skip it for threshold + extra_cycles
        for _ in 0..(threshold + extra_cycles) {
            governor.record_cycle(&[]); // not assigned
        }

        let candidate = ScoredCandidate {
            bead_id: "bead-starved".to_string(),
            final_score: 0.5,
            feature_composite: 0.5,
            effort_penalty: 0.0,
            tag_multiplier: 1.0,
            below_confidence_threshold: false,
            rank: 1,
        };

        let report = governor.evaluate(&[candidate]);
        let verdict = &report.verdicts[0];
        let is_boost = matches!(verdict.action, GovernorAction::BoostScore { .. });
        prop_assert!(is_boost,
            "expected BoostScore after {} skipped cycles, got {:?}", threshold + extra_cycles, verdict.action);
        prop_assert!(verdict.adjusted_score > verdict.original_score,
            "boost should increase score: {} vs {}", verdict.adjusted_score, verdict.original_score);
    }

    /// Thrash detection fires when flips exceed threshold.
    #[test]
    fn prop_governor_thrash_detection(
        flip_threshold in 2u32..5,
    ) {
        let config = GovernorConfig {
            reassignment_cooldown_cycles: 0,
            starvation_threshold_cycles: 100, // disable starvation
            starvation_boost_per_cycle: 0.0,
            starvation_max_boost: 0.0,
            history_window: 20,
            thrash_flip_threshold: flip_threshold,
            thrash_penalty: 0.4,
        };
        let mut governor = ThrashGovernor::new(config);

        // Register bead
        governor.register_bead("bead-thrash");

        // Create enough flips: alternate assigned/not-assigned
        let cycles_needed = (flip_threshold as usize) + 2;
        for i in 0..cycles_needed {
            if i % 2 == 0 {
                governor.record_cycle(&["bead-thrash".to_string()]);
            } else {
                governor.record_cycle(&[]);
            }
        }

        let candidate = ScoredCandidate {
            bead_id: "bead-thrash".to_string(),
            final_score: 0.8,
            feature_composite: 0.8,
            effort_penalty: 0.0,
            tag_multiplier: 1.0,
            below_confidence_threshold: false,
            rank: 1,
        };

        let report = governor.evaluate(&[candidate]);
        let verdict = &report.verdicts[0];
        // Should detect thrashing (penalty) or cooldown (block) — both are governor interventions
        let is_intervened = !matches!(verdict.action, GovernorAction::Allow);
        prop_assert!(is_intervened,
            "expected governor intervention after {} flips, got {:?}", flip_threshold, verdict.action);
    }

    /// Governor history window is bounded.
    #[test]
    fn prop_governor_history_bounded(
        window_size in 3usize..12,
        cycles in 1usize..30,
    ) {
        let config = GovernorConfig {
            history_window: window_size,
            reassignment_cooldown_cycles: 0,
            ..GovernorConfig::default()
        };
        let mut governor = ThrashGovernor::new(config);
        governor.register_bead("bead-window");

        for _ in 0..cycles {
            governor.record_cycle(&["bead-window".to_string()]);
        }

        let state = governor.bead_states.get("bead-window").unwrap();
        prop_assert!(state.assignment_history.len() <= window_size,
            "history {} > window {}", state.assignment_history.len(), window_size);
    }
}

// ── Tests: Mission profiles ──────────────────────────────────────────────────

proptest! {
    /// All mission profiles produce valid configs with bounded weights.
    #[test]
    fn prop_mission_profile_weights_bounded(kind in arb_profile_kind()) {
        let profile = MissionProfile::from_kind(kind);
        let w = &profile.scorer_config.weights;
        prop_assert!(w.impact >= 0.0 && w.impact <= 1.0);
        prop_assert!(w.urgency >= 0.0 && w.urgency <= 1.0);
        prop_assert!(w.risk >= 0.0 && w.risk <= 1.0);
        prop_assert!(w.fit >= 0.0 && w.fit <= 1.0);
        prop_assert!(w.confidence >= 0.0 && w.confidence <= 1.0);
    }

    /// Profile kind roundtrips through serde.
    #[test]
    fn prop_profile_kind_serde(kind in arb_profile_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: MissionProfileKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }

    /// MissionProfile serde roundtrip preserves kind.
    #[test]
    fn prop_mission_profile_serde_roundtrip(kind in arb_profile_kind()) {
        let profile = MissionProfile::from_kind(kind);
        let json = serde_json::to_string(&profile).unwrap();
        let back: MissionProfile = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(profile.kind, back.kind);
        prop_assert_eq!(profile.name, back.name);
    }
}

// ── Tests: Utility policy tuner ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Switch to same profile is no-op (no history entry).
    #[test]
    fn prop_tuner_same_profile_noop(kind in arb_profile_kind()) {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::from_kind(kind));
        let history_before = tuner.switch_history.len();
        tuner.switch_profile(1, kind, "no reason");
        prop_assert_eq!(tuner.switch_history.len(), history_before,
            "switching to same profile should not add history");
    }

    /// Switching profiles records history and changes active profile.
    #[test]
    fn prop_tuner_switch_records_history(
        from in arb_profile_kind(),
        to in arb_profile_kind(),
    ) {
        prop_assume!(from != to);
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::from_kind(from));
        tuner.switch_profile(1, to, "test switch");
        prop_assert_eq!(tuner.active_profile.kind, to);
        prop_assert_eq!(tuner.switch_history.len(), 1);
        prop_assert_eq!(tuner.switch_history[0].from, from);
        prop_assert_eq!(tuner.switch_history[0].to, to);
    }

    /// Weight overrides produce clamped effective config values.
    #[test]
    fn prop_tuner_override_clamped(
        kind in arb_profile_kind(),
        adj in -2.0f64..2.0,
    ) {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::from_kind(kind));
        tuner.set_override("impact", adj);
        let effective = tuner.effective_scorer_config();
        prop_assert!(effective.weights.impact >= 0.0 && effective.weights.impact <= 1.0,
            "impact {} out of bounds after override {}", effective.weights.impact, adj);
    }

    /// Clearing overrides returns to base profile values.
    #[test]
    fn prop_tuner_clear_override_restores(kind in arb_profile_kind()) {
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::from_kind(kind));
        let base = tuner.effective_scorer_config();
        tuner.set_override("impact", 0.5);
        tuner.clear_override("impact");
        let restored = tuner.effective_scorer_config();
        prop_assert!((base.weights.impact - restored.weights.impact).abs() < 1e-10,
            "override clear didn't restore: {} vs {}", base.weights.impact, restored.weights.impact);
    }

    /// Switching profiles clears overrides.
    #[test]
    fn prop_tuner_switch_clears_overrides(
        from in arb_profile_kind(),
        to in arb_profile_kind(),
    ) {
        prop_assume!(from != to);
        let mut tuner = UtilityPolicyTuner::new(MissionProfile::from_kind(from));
        tuner.set_override("impact", 0.3);
        tuner.set_override("urgency", -0.1);
        tuner.switch_profile(1, to, "clear test");
        prop_assert!(tuner.weight_overrides.is_empty(),
            "overrides should be cleared after profile switch");
    }
}

// ── Tests: Serde roundtrip ───────────────────────────────────────────────────

proptest! {
    /// PlannerFeatureVector serde roundtrip.
    #[test]
    fn prop_feature_vector_serde(fv in arb_feature_vector()) {
        let json = serde_json::to_string(&fv).unwrap();
        let back: PlannerFeatureVector = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&fv.bead_id, &back.bead_id);
        prop_assert!((fv.impact - back.impact).abs() < 1e-10);
        prop_assert!((fv.urgency - back.urgency).abs() < 1e-10);
        prop_assert!((fv.risk - back.risk).abs() < 1e-10);
        prop_assert!((fv.fit - back.fit).abs() < 1e-10);
        prop_assert!((fv.confidence - back.confidence).abs() < 1e-10);
    }

    /// PlannerWeights serde roundtrip.
    #[test]
    fn prop_weights_serde(w in arb_planner_weights()) {
        let json = serde_json::to_string(&w).unwrap();
        let back: PlannerWeights = serde_json::from_str(&json).unwrap();
        prop_assert!((w.impact - back.impact).abs() < 1e-10);
        prop_assert!((w.urgency - back.urgency).abs() < 1e-10);
        prop_assert!((w.risk - back.risk).abs() < 1e-10);
        prop_assert!((w.fit - back.fit).abs() < 1e-10);
        prop_assert!((w.confidence - back.confidence).abs() < 1e-10);
    }

    /// ScorerConfig serde roundtrip.
    #[test]
    fn prop_scorer_config_serde(config in arb_scorer_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ScorerConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((config.effort_weight - back.effort_weight).abs() < 1e-10);
        prop_assert!((config.safety_bonus - back.safety_bonus).abs() < 1e-10);
    }

    /// EffortBucket serde roundtrip.
    #[test]
    fn prop_effort_bucket_serde(bucket in arb_effort_bucket()) {
        let json = serde_json::to_string(&bucket).unwrap();
        let back: EffortBucket = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(bucket, back);
    }
}

// ── Tests: Determinism ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Scoring is deterministic: same inputs produce same output.
    #[test]
    fn prop_scoring_deterministic(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..10),
        config in arb_scorer_config(),
    ) {
        let r1 = score_candidates(&inputs, &config);
        let r2 = score_candidates(&inputs, &config);
        prop_assert_eq!(r1.ranked_ids, r2.ranked_ids,
            "scoring produced different rankings on same input");
    }

    /// Solver is deterministic: same inputs produce same assignments.
    #[test]
    fn prop_solver_deterministic(
        inputs in proptest::collection::vec(arb_scorer_input(), 1..10),
    ) {
        let config = ScorerConfig::default();
        let scored = score_candidates(&inputs, &config);
        let agents = vec![MissionAgentCapabilityProfile {
            agent_id: "agent-Det".to_string(),
            capabilities: Vec::new(),
            lane_affinity: Vec::new(),
            current_load: 0,
            max_parallel_assignments: 5,
            availability: MissionAgentAvailability::Ready,
        }];
        let solver_config = SolverConfig::default();

        let r1 = solve_assignments(&scored, &agents, &solver_config);
        let r2 = solve_assignments(&scored, &agents, &solver_config);

        let ids1: Vec<&str> = r1.assignments.iter().map(|a| a.bead_id.as_str()).collect();
        let ids2: Vec<&str> = r2.assignments.iter().map(|a| a.bead_id.as_str()).collect();
        prop_assert_eq!(ids1, ids2, "solver produced different assignments on same input");
    }
}
