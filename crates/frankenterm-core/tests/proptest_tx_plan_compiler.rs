//! Property-based tests for tx_plan_compiler.
//!
//! Covers: compile_tx_plan pipeline, topological sort correctness,
//! parallel level computation, risk classification, deterministic hashing,
//! serde roundtrip, DAG invariants, rejected edges, precondition injection,
//! and compensation injection.

#![cfg(feature = "subprocess-bridge")]

use frankenterm_core::tx_plan_compiler::*;
use proptest::prelude::*;
use std::collections::{HashMap, HashSet};

// ── Strategies ───────────────────────────────────────────────────────────────

fn arb_bead_id() -> impl Strategy<Value = String> {
    "[a-z]{2,6}-[0-9]{1,3}".prop_map(|s| s)
}

fn arb_agent_id() -> impl Strategy<Value = String> {
    "agent-[a-z]{2,5}".prop_map(|s| s)
}

fn arb_score() -> impl Strategy<Value = f64> {
    0.0..=1.0_f64
}

fn arb_tag() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("critical".to_string()),
        Just("destructive".to_string()),
        Just("risky".to_string()),
        Just("unsafe".to_string()),
        Just("safe".to_string()),
        Just("idempotent".to_string()),
        Just("readonly".to_string()),
    ]
}

fn arb_tags() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(arb_tag(), 0..4)
}

fn arb_compensation_kind() -> impl Strategy<Value = CompensationKind> {
    prop_oneof![
        Just(CompensationKind::Rollback),
        Just(CompensationKind::NotifyOperator),
        (1u32..10).prop_map(|n| CompensationKind::RetryWithBackoff { max_retries: n }),
        Just(CompensationKind::SkipAndContinue),
        arb_bead_id().prop_map(|id| CompensationKind::Alternative {
            alternative_step_id: id
        }),
    ]
}

fn arb_compiler_config() -> impl Strategy<Value = CompilerConfig> {
    (
        any::<bool>(),
        any::<bool>(),
        arb_compensation_kind(),
        0.0..=1.0_f64,
        1000u64..120_000,
    )
        .prop_map(
            |(
                require_policy_preflight,
                auto_compensate_high_risk,
                default_compensation,
                context_freshness_threshold,
                context_freshness_max_age_ms,
            )| {
                CompilerConfig {
                    require_policy_preflight,
                    auto_compensate_high_risk,
                    default_compensation,
                    context_freshness_threshold,
                    context_freshness_max_age_ms,
                }
            },
        )
}

/// Generate a set of assignments with valid (acyclic) dependency structure.
/// Dependencies only reference earlier beads to prevent cycles.
fn arb_assignments(max_count: usize) -> impl Strategy<Value = Vec<PlannerAssignment>> {
    (1..=max_count).prop_flat_map(|count| {
        let bead_ids: Vec<String> = (0..count).map(|i| format!("b{i}")).collect();
        let bead_ids_clone = bead_ids.clone();

        prop::collection::vec((arb_agent_id(), arb_score(), arb_tags()), count..=count).prop_map(
            move |agent_score_tags| {
                agent_score_tags
                    .into_iter()
                    .enumerate()
                    .map(|(i, (agent_id, score, tags))| {
                        // Dependencies only reference earlier beads (ensures acyclicity).
                        let dep_candidates: Vec<String> = bead_ids_clone[..i].to_vec();
                        let dep_count = if dep_candidates.is_empty() {
                            0
                        } else {
                            // Take a subset of earlier beads as deps.
                            (i % 3).min(dep_candidates.len())
                        };
                        let dependency_bead_ids = dep_candidates[..dep_count].to_vec();

                        PlannerAssignment {
                            bead_id: bead_ids_clone[i].clone(),
                            agent_id,
                            score,
                            tags,
                            dependency_bead_ids,
                        }
                    })
                    .collect::<Vec<_>>()
            },
        )
    })
}

/// Generate assignments where some dependencies reference beads not in the plan.
fn arb_assignments_with_external_deps(
    max_count: usize,
) -> impl Strategy<Value = Vec<PlannerAssignment>> {
    arb_assignments(max_count).prop_map(|mut assignments| {
        // Add an external dep to every other assignment.
        for (i, a) in assignments.iter_mut().enumerate() {
            if i % 2 == 0 {
                a.dependency_bead_ids.push(format!("external-{i}"));
            }
        }
        assignments
    })
}

// ── Helper functions ─────────────────────────────────────────────────────────

fn verify_topological_order(plan: &TxPlan) {
    let order_position: HashMap<&str, usize> = plan
        .execution_order
        .iter()
        .enumerate()
        .map(|(pos, id)| (id.as_str(), pos))
        .collect();

    for step in &plan.steps {
        let step_pos = order_position
            .get(step.id.as_str())
            .expect("step must be in execution order");

        for dep in &step.depends_on {
            let dep_pos = order_position
                .get(dep.as_str())
                .expect("dependency must be in execution order");
            assert!(
                dep_pos < step_pos,
                "dependency {} (pos {}) must come before {} (pos {})",
                dep,
                dep_pos,
                step.id,
                step_pos
            );
        }
    }
}

fn verify_parallel_levels(plan: &TxPlan) {
    let step_level: HashMap<&str, usize> = plan
        .parallel_levels
        .iter()
        .enumerate()
        .flat_map(|(level, ids)| ids.iter().map(move |id| (id.as_str(), level)))
        .collect();

    let _step_map: HashMap<&str, &TxStep> = plan.steps.iter().map(|s| (s.id.as_str(), s)).collect();

    for step in &plan.steps {
        let my_level = step_level
            .get(step.id.as_str())
            .expect("step must be in a parallel level");

        for dep in &step.depends_on {
            let dep_level = step_level
                .get(dep.as_str())
                .expect("dependency must be in a parallel level");
            assert!(
                dep_level < my_level,
                "dependency {} (level {}) must be in earlier level than {} (level {})",
                dep,
                dep_level,
                step.id,
                my_level
            );
        }
    }

    // Every step must appear in exactly one level.
    let all_in_levels: Vec<&str> = plan
        .parallel_levels
        .iter()
        .flat_map(|level| level.iter().map(|s| s.as_str()))
        .collect();
    let unique: HashSet<&str> = all_in_levels.iter().copied().collect();
    assert_eq!(
        all_in_levels.len(),
        unique.len(),
        "no step should appear in multiple levels"
    );
    assert_eq!(
        unique.len(),
        plan.steps.len(),
        "every step must appear in exactly one level"
    );
}

// ── Property tests ───────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // ── Topological sort invariants ──────────────────────────────────────

    #[test]
    fn prop_execution_order_is_valid_topological_sort(
        assignments in arb_assignments(12),
        config in arb_compiler_config(),
    ) {
        let plan = compile_tx_plan("prop-plan", &assignments, &config);
        // All steps must appear in execution order.
        let order_set: HashSet<&str> = plan.execution_order.iter().map(|s| s.as_str()).collect();
        for step in &plan.steps {
            prop_assert!(order_set.contains(step.id.as_str()),
                "step {} missing from execution_order", step.id);
        }
        prop_assert_eq!(plan.execution_order.len(), plan.steps.len());
        verify_topological_order(&plan);
    }

    #[test]
    fn prop_execution_order_is_deterministic(
        assignments in arb_assignments(10),
        config in arb_compiler_config(),
    ) {
        let plan1 = compile_tx_plan("p", &assignments, &config);
        let plan2 = compile_tx_plan("p", &assignments, &config);
        prop_assert_eq!(&plan1.execution_order, &plan2.execution_order);
        prop_assert_eq!(plan1.plan_hash, plan2.plan_hash);
    }

    // ── Parallel level invariants ────────────────────────────────────────

    #[test]
    fn prop_parallel_levels_respect_dependencies(
        assignments in arb_assignments(12),
        config in arb_compiler_config(),
    ) {
        let plan = compile_tx_plan("prop-plan", &assignments, &config);
        verify_parallel_levels(&plan);
    }

    #[test]
    fn prop_independent_steps_share_level(
        count in 2usize..8,
        config in arb_compiler_config(),
    ) {
        // All independent (no deps) → single level.
        let assignments: Vec<PlannerAssignment> = (0..count)
            .map(|i| PlannerAssignment {
                bead_id: format!("b{i}"),
                agent_id: "agent-x".to_string(),
                score: 0.9,
                tags: Vec::new(),
                dependency_bead_ids: Vec::new(),
            })
            .collect();
        let plan = compile_tx_plan("p", &assignments, &config);
        prop_assert_eq!(plan.parallel_levels.len(), 1,
            "all independent steps should be in 1 level, got {}", plan.parallel_levels.len());
        prop_assert_eq!(plan.parallel_levels[0].len(), count);
    }

    #[test]
    fn prop_linear_chain_has_n_levels(
        count in 2usize..8,
    ) {
        let assignments: Vec<PlannerAssignment> = (0..count)
            .map(|i| PlannerAssignment {
                bead_id: format!("b{i}"),
                agent_id: "agent-x".to_string(),
                score: 0.9,
                tags: Vec::new(),
                dependency_bead_ids: if i > 0 {
                    vec![format!("b{}", i - 1)]
                } else {
                    Vec::new()
                },
            })
            .collect();
        let plan = compile_tx_plan("p", &assignments, &CompilerConfig::default());
        prop_assert_eq!(plan.parallel_levels.len(), count,
            "linear chain of {} should have {} levels", count, count);
        for (level_idx, level) in plan.parallel_levels.iter().enumerate() {
            prop_assert_eq!(level.len(), 1,
                "level {} should have exactly 1 step", level_idx);
        }
    }

    // ── Risk classification ──────────────────────────────────────────────

    #[test]
    fn prop_critical_tag_always_critical(
        score in arb_score(),
        other_tags in arb_tags(),
    ) {
        let mut tags = other_tags;
        tags.push("critical".to_string());
        let assignments = vec![PlannerAssignment {
            bead_id: "b0".to_string(),
            agent_id: "a0".to_string(),
            score,
            tags,
            dependency_bead_ids: Vec::new(),
        }];
        let plan = compile_tx_plan("p", &assignments, &CompilerConfig::default());
        prop_assert_eq!(plan.steps[0].risk, StepRisk::Critical);
    }

    #[test]
    fn prop_destructive_tag_always_critical(
        score in arb_score(),
    ) {
        let assignments = vec![PlannerAssignment {
            bead_id: "b0".to_string(),
            agent_id: "a0".to_string(),
            score,
            tags: vec!["destructive".to_string()],
            dependency_bead_ids: Vec::new(),
        }];
        let plan = compile_tx_plan("p", &assignments, &CompilerConfig::default());
        prop_assert_eq!(plan.steps[0].risk, StepRisk::Critical);
    }

    #[test]
    fn prop_risk_monotonic_with_score(
        score_low in 0.0..0.3_f64,
        score_high in 0.6..1.0_f64,
    ) {
        let low_a = vec![PlannerAssignment {
            bead_id: "bl".to_string(),
            agent_id: "a".to_string(),
            score: score_low,
            tags: Vec::new(),
            dependency_bead_ids: Vec::new(),
        }];
        let high_a = vec![PlannerAssignment {
            bead_id: "bh".to_string(),
            agent_id: "a".to_string(),
            score: score_high,
            tags: Vec::new(),
            dependency_bead_ids: Vec::new(),
        }];
        let cfg = CompilerConfig::default();
        let plan_low = compile_tx_plan("p", &low_a, &cfg);
        let plan_high = compile_tx_plan("p", &high_a, &cfg);
        prop_assert!(plan_low.steps[0].risk >= plan_high.steps[0].risk,
            "lower score ({}) should have >= risk than higher score ({})",
            score_low, score_high);
    }

    // ── Risk summary invariants ──────────────────────────────────────────

    #[test]
    fn prop_risk_summary_counts_correct(
        assignments in arb_assignments(10),
        config in arb_compiler_config(),
    ) {
        let plan = compile_tx_plan("p", &assignments, &config);
        let rs = &plan.risk_summary;

        prop_assert_eq!(rs.total_steps, plan.steps.len());

        let actual_high = plan.steps.iter().filter(|s| s.risk == StepRisk::High).count();
        let actual_critical = plan.steps.iter().filter(|s| s.risk == StepRisk::Critical).count();
        prop_assert_eq!(rs.high_risk_count, actual_high);
        prop_assert_eq!(rs.critical_risk_count, actual_critical);

        // Verify overall risk.
        if actual_critical > 0 {
            prop_assert_eq!(rs.overall_risk, StepRisk::Critical);
        } else if actual_high > 0 {
            prop_assert_eq!(rs.overall_risk, StepRisk::High);
        } else if plan.steps.iter().any(|s| s.risk == StepRisk::Medium) {
            prop_assert_eq!(rs.overall_risk, StepRisk::Medium);
        } else {
            prop_assert_eq!(rs.overall_risk, StepRisk::Low);
        }
    }

    #[test]
    fn prop_uncompensated_count_correct(
        assignments in arb_assignments(10),
        config in arb_compiler_config(),
    ) {
        let plan = compile_tx_plan("p", &assignments, &config);
        let expected_uncompensated = plan.steps.iter().filter(|s| {
            (s.risk == StepRisk::High || s.risk == StepRisk::Critical)
                && s.compensations.is_empty()
        }).count();
        prop_assert_eq!(plan.risk_summary.uncompensated_steps, expected_uncompensated);
    }

    // ── Rejected edges ───────────────────────────────────────────────────

    #[test]
    fn prop_external_deps_become_rejected_edges(
        assignments in arb_assignments_with_external_deps(8),
        config in arb_compiler_config(),
    ) {
        let plan = compile_tx_plan("p", &assignments, &config);
        let assigned_beads: HashSet<&str> = assignments.iter().map(|a| a.bead_id.as_str()).collect();

        for a in &assignments {
            for dep in &a.dependency_bead_ids {
                if !assigned_beads.contains(dep.as_str()) {
                    // Must appear in rejected_edges.
                    let found = plan.rejected_edges.iter().any(|re| {
                        re.to_step == format!("step-{}", a.bead_id)
                            && re.from_step == format!("step-{}", dep)
                    });
                    prop_assert!(found,
                        "external dep {} -> {} should be in rejected_edges",
                        dep, a.bead_id);
                }
            }
        }
    }

    #[test]
    fn prop_no_external_deps_in_step_depends_on(
        assignments in arb_assignments_with_external_deps(8),
        config in arb_compiler_config(),
    ) {
        let plan = compile_tx_plan("p", &assignments, &config);
        let step_ids: HashSet<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();

        for step in &plan.steps {
            for dep in &step.depends_on {
                prop_assert!(step_ids.contains(dep.as_str()),
                    "step {} depends on {} which is not in the plan", step.id, dep);
            }
        }
    }

    // ── Precondition injection ───────────────────────────────────────────

    #[test]
    fn prop_policy_preflight_injected_when_enabled(
        assignments in arb_assignments(6),
    ) {
        let config = CompilerConfig {
            require_policy_preflight: true,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p", &assignments, &config);
        for step in &plan.steps {
            let has_policy = step.preconditions.iter().any(|p| p.kind == PreconditionKind::PolicyApproved);
            prop_assert!(has_policy,
                "step {} missing PolicyApproved precondition", step.id);
        }
    }

    #[test]
    fn prop_no_policy_preflight_when_disabled(
        assignments in arb_assignments(6),
    ) {
        let config = CompilerConfig {
            require_policy_preflight: false,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p", &assignments, &config);
        for step in &plan.steps {
            let has_policy = step.preconditions.iter().any(|p| p.kind == PreconditionKind::PolicyApproved);
            prop_assert!(!has_policy,
                "step {} should not have PolicyApproved when disabled", step.id);
        }
    }

    #[test]
    fn prop_context_freshness_for_low_score_steps(
        assignments in arb_assignments(8),
        threshold in 0.3..0.8_f64,
    ) {
        let config = CompilerConfig {
            context_freshness_threshold: threshold,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p", &assignments, &config);
        for (step, assignment) in plan.steps.iter().zip(assignments.iter()) {
            let has_freshness = step.preconditions.iter().any(|p| {
                matches!(p.kind, PreconditionKind::ContextFresh { .. })
            });
            if assignment.score < threshold {
                prop_assert!(has_freshness,
                    "step {} with score {:.3} < threshold {:.3} should have ContextFresh",
                    step.id, assignment.score, threshold);
            } else {
                prop_assert!(!has_freshness,
                    "step {} with score {:.3} >= threshold {:.3} should NOT have ContextFresh",
                    step.id, assignment.score, threshold);
            }
        }
    }

    // ── Compensation injection ───────────────────────────────────────────

    #[test]
    fn prop_auto_compensation_for_high_risk_when_enabled(
        assignments in arb_assignments(8),
    ) {
        let config = CompilerConfig {
            auto_compensate_high_risk: true,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p", &assignments, &config);
        for step in &plan.steps {
            if step.risk == StepRisk::High || step.risk == StepRisk::Critical {
                prop_assert!(!step.compensations.is_empty(),
                    "high/critical risk step {} should have auto-compensation", step.id);
            }
        }
    }

    #[test]
    fn prop_no_auto_compensation_when_disabled(
        assignments in arb_assignments(8),
    ) {
        let config = CompilerConfig {
            auto_compensate_high_risk: false,
            require_policy_preflight: false,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p", &assignments, &config);
        for step in &plan.steps {
            // No auto-compensation should be added (the step itself has no manual compensations).
            prop_assert!(step.compensations.is_empty(),
                "step {} should have no compensation when auto_compensate disabled", step.id);
        }
    }

    // ── Serde roundtrip ──────────────────────────────────────────────────

    #[test]
    fn prop_tx_plan_serde_roundtrip(
        assignments in arb_assignments(8),
        config in arb_compiler_config(),
    ) {
        let plan = compile_tx_plan("prop-plan", &assignments, &config);
        let json = serde_json::to_string(&plan).expect("serialize");
        let back: TxPlan = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(back.plan_id, plan.plan_id);
        prop_assert_eq!(back.plan_hash, plan.plan_hash);
        prop_assert_eq!(back.steps.len(), plan.steps.len());
        prop_assert_eq!(&back.execution_order, &plan.execution_order);
        prop_assert_eq!(back.parallel_levels.len(), plan.parallel_levels.len());
        prop_assert_eq!(back.rejected_edges.len(), plan.rejected_edges.len());
    }

    #[test]
    fn prop_compiler_config_serde_roundtrip(
        config in arb_compiler_config(),
    ) {
        let json = serde_json::to_string(&config).expect("serialize");
        let back: CompilerConfig = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(back.require_policy_preflight, config.require_policy_preflight);
        prop_assert_eq!(back.auto_compensate_high_risk, config.auto_compensate_high_risk);
        prop_assert!((back.context_freshness_threshold - config.context_freshness_threshold).abs() < 1e-10);
        prop_assert_eq!(back.context_freshness_max_age_ms, config.context_freshness_max_age_ms);
    }

    // ── Deterministic hash ───────────────────────────────────────────────

    #[test]
    fn prop_plan_hash_deterministic(
        assignments in arb_assignments(8),
        config in arb_compiler_config(),
    ) {
        let plan1 = compile_tx_plan("p", &assignments, &config);
        let plan2 = compile_tx_plan("p", &assignments, &config);
        prop_assert_eq!(plan1.plan_hash, plan2.plan_hash);
    }

    #[test]
    fn prop_different_assignments_different_hash(
        a1 in arb_assignments(4),
        a2 in arb_assignments(4),
    ) {
        // Skip if bead IDs happen to be the same.
        let ids1: HashSet<&str> = a1.iter().map(|a| a.bead_id.as_str()).collect();
        let ids2: HashSet<&str> = a2.iter().map(|a| a.bead_id.as_str()).collect();
        prop_assume!(ids1 != ids2);

        let config = CompilerConfig::default();
        let plan1 = compile_tx_plan("p", &a1, &config);
        let plan2 = compile_tx_plan("p", &a2, &config);
        prop_assert_ne!(plan1.plan_hash, plan2.plan_hash);
    }

    // ── Step structure invariants ────────────────────────────────────────

    #[test]
    fn prop_step_ids_match_bead_ids(
        assignments in arb_assignments(10),
        config in arb_compiler_config(),
    ) {
        let plan = compile_tx_plan("p", &assignments, &config);
        for (step, assignment) in plan.steps.iter().zip(assignments.iter()) {
            prop_assert_eq!(&step.id, &format!("step-{}", assignment.bead_id));
            prop_assert_eq!(&step.bead_id, &assignment.bead_id);
            prop_assert_eq!(&step.agent_id, &assignment.agent_id);
            prop_assert!((step.score - assignment.score).abs() < 1e-10,
                "step score {} != assignment score {}", step.score, assignment.score);
        }
    }

    #[test]
    fn prop_step_count_equals_assignment_count(
        assignments in arb_assignments(12),
        config in arb_compiler_config(),
    ) {
        let plan = compile_tx_plan("p", &assignments, &config);
        prop_assert_eq!(plan.steps.len(), assignments.len());
    }

    // ── Empty plan edge case ─────────────────────────────────────────────

    #[test]
    fn prop_empty_plan_invariants(
        config in arb_compiler_config(),
    ) {
        let plan = compile_tx_plan("empty", &[], &config);
        prop_assert!(plan.steps.is_empty());
        prop_assert!(plan.execution_order.is_empty());
        prop_assert!(plan.parallel_levels.is_empty());
        prop_assert!(plan.rejected_edges.is_empty());
        prop_assert_eq!(plan.risk_summary.total_steps, 0);
        prop_assert_eq!(plan.risk_summary.high_risk_count, 0);
        prop_assert_eq!(plan.risk_summary.critical_risk_count, 0);
        prop_assert_eq!(plan.risk_summary.uncompensated_steps, 0);
        prop_assert_eq!(plan.risk_summary.overall_risk, StepRisk::Low);
    }

    // ── Wide fan-out and fan-in DAGs ─────────────────────────────────────

    #[test]
    fn prop_fan_out_parallel_levels(
        fan_width in 2usize..8,
    ) {
        // One root → N children (fan-out).
        let mut assignments = vec![PlannerAssignment {
            bead_id: "root".to_string(),
            agent_id: "a".to_string(),
            score: 0.9,
            tags: Vec::new(),
            dependency_bead_ids: Vec::new(),
        }];
        for i in 0..fan_width {
            assignments.push(PlannerAssignment {
                bead_id: format!("child{i}"),
                agent_id: "a".to_string(),
                score: 0.8,
                tags: Vec::new(),
                dependency_bead_ids: vec!["root".to_string()],
            });
        }
        let plan = compile_tx_plan("p", &assignments, &CompilerConfig::default());
        prop_assert_eq!(plan.parallel_levels.len(), 2,
            "fan-out should produce 2 levels");
        prop_assert_eq!(plan.parallel_levels[0].len(), 1, "root in level 0");
        prop_assert_eq!(plan.parallel_levels[1].len(), fan_width,
            "all children in level 1");
    }

    #[test]
    fn prop_fan_in_parallel_levels(
        fan_width in 2usize..8,
    ) {
        // N roots → one sink (fan-in).
        let mut assignments: Vec<PlannerAssignment> = (0..fan_width)
            .map(|i| PlannerAssignment {
                bead_id: format!("src{i}"),
                agent_id: "a".to_string(),
                score: 0.9,
                tags: Vec::new(),
                dependency_bead_ids: Vec::new(),
            })
            .collect();
        assignments.push(PlannerAssignment {
            bead_id: "sink".to_string(),
            agent_id: "a".to_string(),
            score: 0.7,
            tags: Vec::new(),
            dependency_bead_ids: (0..fan_width).map(|i| format!("src{i}")).collect(),
        });
        let plan = compile_tx_plan("p", &assignments, &CompilerConfig::default());
        prop_assert_eq!(plan.parallel_levels.len(), 2,
            "fan-in should produce 2 levels");
        prop_assert_eq!(plan.parallel_levels[0].len(), fan_width,
            "all sources in level 0");
        prop_assert_eq!(plan.parallel_levels[1].len(), 1, "sink in level 1");
    }

    // ── Boundary: score exactly at threshold ─────────────────────────────

    #[test]
    fn prop_score_at_exact_threshold_no_freshness(
        threshold in 0.1..0.9_f64,
    ) {
        // Score == threshold should NOT trigger context freshness (< not <=).
        let assignments = vec![PlannerAssignment {
            bead_id: "b0".to_string(),
            agent_id: "a0".to_string(),
            score: threshold,
            tags: Vec::new(),
            dependency_bead_ids: Vec::new(),
        }];
        let config = CompilerConfig {
            context_freshness_threshold: threshold,
            ..CompilerConfig::default()
        };
        let plan = compile_tx_plan("p", &assignments, &config);
        let has_freshness = plan.steps[0].preconditions.iter().any(|p| {
            matches!(p.kind, PreconditionKind::ContextFresh { .. })
        });
        prop_assert!(!has_freshness,
            "score exactly at threshold should NOT trigger ContextFresh");
    }

    // ── PreconditionKind serde roundtrip ─────────────────────────────────

    #[test]
    fn prop_precondition_kind_serde(
        max_age_ms in 1u64..1_000_000,
        path_count in 1usize..5,
    ) {
        let paths: Vec<String> = (0..path_count).map(|i| format!("path{i}.rs")).collect();
        let kinds = vec![
            PreconditionKind::PolicyApproved,
            PreconditionKind::ReservationHeld { paths },
            PreconditionKind::ApprovalRequired { approver: "ops".to_string() },
            PreconditionKind::TargetReachable { target_id: "pane-1".to_string() },
            PreconditionKind::ContextFresh { max_age_ms },
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).expect("serialize");
            let back: PreconditionKind = serde_json::from_str(&json).expect("deserialize");
            prop_assert_eq!(&back, kind);
        }
    }
}
