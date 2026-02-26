//! Property-based tests for canary rollout controller (ft-1i2ge.6.4).
//!
//! Tests invariants across randomly generated health scenarios, phase
//! transitions, and assignment filtering.

#![cfg(feature = "subprocess-bridge")]

use frankenterm_core::canary_rollout_controller::*;
use frankenterm_core::planner_features::{Assignment, AssignmentSet, SolverConfig};
use frankenterm_core::shadow_mode_evaluator::ShadowModeDiff;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_phase() -> impl Strategy<Value = CanaryPhase> {
    prop_oneof![
        Just(CanaryPhase::Shadow),
        Just(CanaryPhase::Canary),
        Just(CanaryPhase::Full),
    ]
}

fn arb_fidelity() -> impl Strategy<Value = f64> {
    prop::num::f64::NORMAL.prop_map(|f| f.abs().fract()) // 0.0..1.0
}

fn arb_diff(cycle_id: u64) -> impl Strategy<Value = ShadowModeDiff> {
    (
        0usize..20,     // recommendations
        0usize..20,     // emissions
        0usize..5,      // safety rejections
        0usize..5,      // conflicts
        arb_fidelity(), // fidelity
    )
        .prop_map(
            move |(recs, emis, safety, conflicts, fidelity)| ShadowModeDiff {
                cycle_id,
                timestamp_ms: cycle_id as i64 * 1000,
                recommendations_count: recs,
                rejections_count: 0,
                emissions_count: emis,
                execution_rejections_count: 0,
                missing_executions: Vec::new(),
                unexpected_executions: Vec::new(),
                agent_divergences: Vec::new(),
                score_accuracy: Vec::new(),
                safety_gate_rejections: safety,
                retry_storm_throttles: 0,
                conflicts_detected: conflicts,
                conflicts_auto_resolved: 0,
                dispatch_rate: if recs > 0 {
                    (emis as f64 / recs as f64).min(1.0)
                } else {
                    1.0
                },
                agent_match_rate: 1.0,
                fidelity_score: fidelity,
            },
        )
}

fn arb_assignment_set() -> impl Strategy<Value = AssignmentSet> {
    prop::collection::vec(0usize..100, 0..10).prop_map(|ids| {
        let assignments: Vec<Assignment> = ids
            .iter()
            .map(|&id| Assignment {
                bead_id: format!("b{id}"),
                agent_id: format!("a{}", id % 5),
                score: 0.5 + (id as f64 * 0.01),
                rank: id,
            })
            .collect();
        AssignmentSet {
            assignments,
            rejected: Vec::new(),
            solver_config: SolverConfig::default(),
        }
    })
}

/// Build warmed shadow metrics by running evaluator through cycles.
fn build_warmed_metrics(
    cycles: u64,
) -> frankenterm_core::shadow_mode_evaluator::ShadowModeMetrics {
    use frankenterm_core::mission_events::{
        MissionEventBuilder, MissionEventKind, MissionEventLog, MissionEventLogConfig,
    };
    use frankenterm_core::shadow_mode_evaluator::{ShadowEvaluationConfig, ShadowModeEvaluator};

    let mut eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });

    for i in 1..=cycles {
        let recs = AssignmentSet {
            assignments: vec![Assignment {
                bead_id: format!("b{i}"),
                agent_id: "a1".to_string(),
                score: 0.9,
                rank: 1,
            }],
            rejected: Vec::new(),
            solver_config: SolverConfig::default(),
        };
        let mut log = MissionEventLog::new(MissionEventLogConfig {
            max_events: 64,
            enabled: true,
        });
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::AssignmentEmitted,
                "mission.dispatch.assignment_emitted",
            )
            .cycle(i, i as i64 * 1000)
            .correlation("corr")
            .labels("ws", "track")
            .detail_str("bead_id", &format!("b{i}"))
            .detail_str("agent_id", "a1"),
        );
        eval.evaluate_cycle(i, i as i64 * 1000, &recs, log.events());
    }

    eval.metrics().clone()
}

// ── Property tests ──────────────────────────────────────────────────────────

proptest! {
    /// Phase progression is monotonic: Shadow < Canary < Full.
    #[test]
    fn phase_next_is_monotonic(phase in arb_phase()) {
        if let Some(next) = phase.next() {
            let is_forward = matches!(
                (phase, next),
                (CanaryPhase::Shadow, CanaryPhase::Canary)
                    | (CanaryPhase::Canary, CanaryPhase::Full)
            );
            prop_assert!(is_forward, "next() must advance forward");
        }
    }

    /// Rollback target is strictly before current phase.
    #[test]
    fn rollback_is_strictly_backward(
        from in arb_phase(),
        to in arb_phase(),
    ) {
        if from.can_rollback_to(to) {
            // Numeric ordering: Shadow=0, Canary=1, Full=2
            let from_ord = match from {
                CanaryPhase::Shadow => 0,
                CanaryPhase::Canary => 1,
                CanaryPhase::Full => 2,
            };
            let to_ord = match to {
                CanaryPhase::Shadow => 0,
                CanaryPhase::Canary => 1,
                CanaryPhase::Full => 2,
            };
            prop_assert!(to_ord < from_ord, "rollback target must be before current");
        }
    }

    /// Shadow phase always blocks all assignments.
    #[test]
    fn shadow_blocks_everything(set in arb_assignment_set()) {
        let ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            initial_phase: CanaryPhase::Shadow,
            ..Default::default()
        });
        let filtered = ctrl.filter_assignments(&set);
        prop_assert!(
            filtered.assignments.is_empty(),
            "shadow must block all, got {}",
            filtered.assignments.len()
        );
        prop_assert_eq!(
            filtered.rejected.len(),
            set.assignments.len() + set.rejected.len()
        );
    }

    /// Full phase passes all assignments unchanged.
    #[test]
    fn full_passes_everything(set in arb_assignment_set()) {
        let ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            initial_phase: CanaryPhase::Full,
            ..Default::default()
        });
        let filtered = ctrl.filter_assignments(&set);
        prop_assert_eq!(filtered.assignments.len(), set.assignments.len());
        prop_assert_eq!(filtered.rejected.len(), set.rejected.len());
    }

    /// Canary filtering preserves total count (passed + rejected = original).
    #[test]
    fn canary_filter_preserves_total(set in arb_assignment_set()) {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            canary_agent_allowlist: vec!["a0".to_string(), "a1".to_string()],
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&["a0".to_string(), "a1".to_string(), "a2".to_string()]);

        let filtered = ctrl.filter_assignments(&set);
        let total_original = set.assignments.len() + set.rejected.len();
        let total_filtered = filtered.assignments.len() + filtered.rejected.len();
        prop_assert_eq!(total_filtered, total_original);
    }

    /// Health check always has valid failure reasons when unhealthy.
    #[test]
    fn unhealthy_check_has_reasons(
        cycle_id in 1u64..1000,
        diff in arb_diff(1),
    ) {
        let mut ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            min_warmup_cycles: 0,
            min_healthy_before_advance: 1000, // prevent advance
            ..Default::default()
        });
        let metrics = build_warmed_metrics(10);

        let decision = ctrl.evaluate_health(cycle_id, cycle_id as i64 * 1000, &diff, &metrics);

        if !decision.health_check.healthy {
            prop_assert!(
                !decision.health_check.failure_reasons.is_empty(),
                "unhealthy check must have reasons"
            );
        }
    }

    /// Metrics are internally consistent after N evaluations.
    #[test]
    fn metrics_consistency(n in 1u64..50) {
        let mut ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            min_warmup_cycles: 0,
            min_healthy_before_advance: 1000, // prevent transitions
            auto_rollback: false,
            ..Default::default()
        });
        let metrics = build_warmed_metrics(10);

        for i in 1..=n {
            let diff = ShadowModeDiff {
                cycle_id: i,
                timestamp_ms: i as i64 * 1000,
                recommendations_count: 1,
                rejections_count: 0,
                emissions_count: 1,
                execution_rejections_count: 0,
                missing_executions: Vec::new(),
                unexpected_executions: Vec::new(),
                agent_divergences: Vec::new(),
                score_accuracy: Vec::new(),
                safety_gate_rejections: 0,
                retry_storm_throttles: 0,
                conflicts_detected: 0,
                conflicts_auto_resolved: 0,
                dispatch_rate: 1.0,
                agent_match_rate: 1.0,
                fidelity_score: 0.95,
            };
            ctrl.evaluate_health(i, i as i64 * 1000, &diff, &metrics);
        }

        let m = ctrl.metrics();
        prop_assert_eq!(m.total_checks, n);
        prop_assert_eq!(m.healthy_checks + m.unhealthy_checks, m.total_checks);
    }

    /// Canary agent fraction produces at least 1 and at most N agents.
    #[test]
    fn canary_fraction_bounds(
        fraction in 0.01f64..1.0,
        agent_count in 1usize..50,
    ) {
        let config = CanaryRolloutConfig {
            canary_agent_fraction: fraction,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let agents: Vec<String> = (0..agent_count).map(|i| format!("a{i}")).collect();
        ctrl.update_canary_agents(&agents);

        let selected = ctrl.canary_agents().len();
        prop_assert!(selected >= 1, "must select at least 1 agent");
        prop_assert!(selected <= agent_count, "must not exceed available agents");
    }

    /// Config serde roundtrip preserves all fields.
    #[test]
    fn config_roundtrip(
        fidelity in 0.0f64..1.0,
        max_unhealthy in 1u32..20,
        min_healthy in 1u32..20,
    ) {
        let config = CanaryRolloutConfig {
            fidelity_threshold: fidelity,
            max_consecutive_unhealthy: max_unhealthy,
            min_healthy_before_advance: min_healthy,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: CanaryRolloutConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((restored.fidelity_threshold - config.fidelity_threshold).abs() < 1e-10);
        prop_assert_eq!(restored.max_consecutive_unhealthy, config.max_consecutive_unhealthy);
        prop_assert_eq!(restored.min_healthy_before_advance, config.min_healthy_before_advance);
    }

    /// Reset brings controller back to initial state.
    #[test]
    fn reset_restores_initial(
        initial in arb_phase(),
        n in 1u64..20,
    ) {
        let config = CanaryRolloutConfig {
            initial_phase: initial,
            min_warmup_cycles: 0,
            min_healthy_before_advance: 1,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = build_warmed_metrics(10);

        // Run some cycles (may cause transitions)
        for i in 1..=n {
            let diff = ShadowModeDiff {
                cycle_id: i,
                timestamp_ms: i as i64 * 1000,
                recommendations_count: 1,
                rejections_count: 0,
                emissions_count: 1,
                execution_rejections_count: 0,
                missing_executions: Vec::new(),
                unexpected_executions: Vec::new(),
                agent_divergences: Vec::new(),
                score_accuracy: Vec::new(),
                safety_gate_rejections: 0,
                retry_storm_throttles: 0,
                conflicts_detected: 0,
                conflicts_auto_resolved: 0,
                dispatch_rate: 1.0,
                agent_match_rate: 1.0,
                fidelity_score: 0.95,
            };
            ctrl.evaluate_health(i, i as i64 * 1000, &diff, &metrics);
        }

        ctrl.reset();
        prop_assert_eq!(ctrl.phase(), initial);
        prop_assert!(ctrl.health_history().is_empty());
        prop_assert!(ctrl.transition_history().is_empty());
        prop_assert_eq!(ctrl.metrics().total_checks, 0);
    }
}
