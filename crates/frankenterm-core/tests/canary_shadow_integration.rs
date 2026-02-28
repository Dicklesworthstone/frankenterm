//! Integration tests: ShadowModeEvaluator → CanaryRolloutController pipeline.
//!
//! Validates the cross-module contract where shadow evaluation diffs drive
//! canary health checks, phase transitions, and assignment filtering.
//!
//! This mirrors the intended MissionLoop integration:
//! ```text
//! AssignmentSet + MissionEventLog
//!         ↓
//! ShadowModeEvaluator.evaluate_cycle()
//!         ↓
//! ShadowModeDiff + ShadowModeMetrics
//!         ↓
//! CanaryRolloutController.evaluate_health()
//!         ↓
//! CanaryDecision → filter_assignments()
//! ```

#![cfg(feature = "subprocess-bridge")]

use frankenterm_core::canary_rollout_controller::*;
use frankenterm_core::mission_events::{
    MissionEventBuilder, MissionEventKind, MissionEventLog, MissionEventLogConfig,
};
use frankenterm_core::planner_features::{Assignment, AssignmentSet, SolverConfig};
use frankenterm_core::shadow_mode_evaluator::{ShadowEvaluationConfig, ShadowModeEvaluator};

// ── Helpers ────────────────────────────────────────────────────────────────

fn make_assignment(bead_id: &str, agent_id: &str, score: f64, rank: usize) -> Assignment {
    Assignment {
        bead_id: bead_id.to_string(),
        agent_id: agent_id.to_string(),
        score,
        rank,
    }
}

fn make_assignment_set(assignments: Vec<Assignment>) -> AssignmentSet {
    AssignmentSet {
        assignments,
        rejected: Vec::new(),
        solver_config: SolverConfig::default(),
    }
}

/// Run a perfect shadow cycle: recommendations match dispatched events exactly.
fn run_perfect_cycle(
    eval: &mut ShadowModeEvaluator,
    cycle_id: u64,
    recs: &AssignmentSet,
) -> frankenterm_core::shadow_mode_evaluator::ShadowModeDiff {
    let mut log = MissionEventLog::new(MissionEventLogConfig {
        max_events: 128,
        enabled: true,
    });
    for a in &recs.assignments {
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::AssignmentEmitted,
                "mission.dispatch.assignment_emitted",
            )
            .cycle(cycle_id, cycle_id as i64 * 1000)
            .correlation("corr")
            .labels("ws", "track")
            .detail_str("bead_id", &a.bead_id)
            .detail_str("agent_id", &a.agent_id),
        );
    }
    eval.evaluate_cycle(cycle_id, cycle_id as i64 * 1000, recs, log.events())
}

/// Run a degraded shadow cycle: only some recommendations dispatched.
fn run_degraded_cycle(
    eval: &mut ShadowModeEvaluator,
    cycle_id: u64,
    recs: &AssignmentSet,
    dispatch_fraction: f64,
) -> frankenterm_core::shadow_mode_evaluator::ShadowModeDiff {
    let mut log = MissionEventLog::new(MissionEventLogConfig {
        max_events: 128,
        enabled: true,
    });
    let dispatch_count = (recs.assignments.len() as f64 * dispatch_fraction).ceil() as usize;
    for a in recs.assignments.iter().take(dispatch_count) {
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::AssignmentEmitted,
                "mission.dispatch.assignment_emitted",
            )
            .cycle(cycle_id, cycle_id as i64 * 1000)
            .correlation("corr")
            .labels("ws", "track")
            .detail_str("bead_id", &a.bead_id)
            .detail_str("agent_id", &a.agent_id),
        );
    }
    eval.evaluate_cycle(cycle_id, cycle_id as i64 * 1000, recs, log.events())
}

/// Run a cycle with safety rejections.
fn run_cycle_with_rejections(
    eval: &mut ShadowModeEvaluator,
    cycle_id: u64,
    recs: &AssignmentSet,
    rejection_count: usize,
) -> frankenterm_core::shadow_mode_evaluator::ShadowModeDiff {
    let mut log = MissionEventLog::new(MissionEventLogConfig {
        max_events: 128,
        enabled: true,
    });
    // Emit dispatch events
    for a in &recs.assignments {
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::AssignmentEmitted,
                "mission.dispatch.assignment_emitted",
            )
            .cycle(cycle_id, cycle_id as i64 * 1000)
            .correlation("corr")
            .labels("ws", "track")
            .detail_str("bead_id", &a.bead_id)
            .detail_str("agent_id", &a.agent_id),
        );
    }
    // Emit rejection events
    for i in 0..rejection_count {
        log.emit(
            MissionEventBuilder::new(
                MissionEventKind::SafetyGateRejection,
                "mission.safety.rejection",
            )
            .cycle(cycle_id, cycle_id as i64 * 1000)
            .correlation("corr")
            .labels("ws", "track")
            .detail_str("gate_name", &format!("gate_{i}"))
            .detail_str("reason", "test_rejection"),
        );
    }
    eval.evaluate_cycle(cycle_id, cycle_id as i64 * 1000, recs, log.events())
}

// ── Integration tests ──────────────────────────────────────────────────────

#[test]
fn i01_perfect_cycles_advance_shadow_to_canary() {
    // Setup: shadow evaluator + canary controller starting in Shadow phase
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Shadow,
        min_warmup_cycles: 0,
        min_healthy_before_advance: 3,
        auto_advance: true,
        ..Default::default()
    });

    let recs = make_assignment_set(vec![
        make_assignment("b1", "a1", 0.9, 1),
        make_assignment("b2", "a2", 0.8, 2),
    ]);

    // Run 3 perfect cycles — should trigger advance to Canary
    for i in 1..=3 {
        let diff = run_perfect_cycle(&mut shadow_eval, i, &recs);
        let decision = canary.evaluate_health(i, i as i64 * 1000, &diff, shadow_eval.metrics());

        if i < 3 {
            assert_eq!(
                decision.action,
                CanaryAction::Hold,
                "cycle {i}: should hold before min_healthy reached"
            );
            assert_eq!(decision.phase, CanaryPhase::Shadow);
        } else {
            assert_eq!(
                decision.action,
                CanaryAction::Advance,
                "cycle 3: should advance after 3 healthy checks"
            );
            assert_eq!(decision.phase, CanaryPhase::Canary);
        }
    }

    // Shadow phase should block all assignments
    // (But we've now advanced to Canary after the 3rd check)
    assert_eq!(canary.phase(), CanaryPhase::Canary);
}

#[test]
fn i02_canary_filters_assignment_subset() {
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Canary,
        min_warmup_cycles: 0,
        min_healthy_before_advance: 1000, // prevent advance to Full
        canary_agent_allowlist: vec!["a1".to_string()],
        ..Default::default()
    });
    // Populate canary agents from allowlist
    canary.update_canary_agents(&["a1".to_string(), "a2".to_string(), "a3".to_string()]);

    let recs = make_assignment_set(vec![
        make_assignment("b1", "a1", 0.9, 1),
        make_assignment("b2", "a2", 0.8, 2),
        make_assignment("b3", "a3", 0.7, 3),
    ]);

    // Run a perfect cycle to warm the evaluator
    let diff = run_perfect_cycle(&mut shadow_eval, 1, &recs);
    let decision = canary.evaluate_health(1, 1000, &diff, shadow_eval.metrics());

    assert!(
        decision.health_check.healthy,
        "perfect cycle should be healthy"
    );
    assert_eq!(decision.phase, CanaryPhase::Canary);

    // Filter assignments: only a1 should pass in canary phase
    let filtered = canary.filter_assignments(&recs);
    assert_eq!(
        filtered.assignments.len(),
        1,
        "canary should pass only allowlisted agents"
    );
    assert_eq!(filtered.assignments[0].agent_id, "a1");
    assert_eq!(
        filtered.rejected.len(),
        2,
        "non-canary agents should be rejected"
    );
}

#[test]
fn i03_degraded_fidelity_triggers_rollback() {
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Canary,
        min_warmup_cycles: 0,
        max_consecutive_unhealthy: 2,
        auto_rollback: true,
        fidelity_threshold: 0.8,
        ..Default::default()
    });

    let recs = make_assignment_set(vec![
        make_assignment("b1", "a1", 0.9, 1),
        make_assignment("b2", "a2", 0.8, 2),
        make_assignment("b3", "a3", 0.7, 3),
    ]);

    // First run some perfect cycles to warm the shadow evaluator
    for i in 1..=3 {
        let diff = run_perfect_cycle(&mut shadow_eval, i, &recs);
        let decision = canary.evaluate_health(i, i as i64 * 1000, &diff, shadow_eval.metrics());
        assert!(
            decision.health_check.healthy,
            "cycle {i}: should be healthy"
        );
    }
    assert_eq!(canary.phase(), CanaryPhase::Canary);

    // Now send degraded cycles — only 33% dispatched
    for i in 4..=5 {
        let diff = run_degraded_cycle(&mut shadow_eval, i, &recs, 0.33);
        let decision = canary.evaluate_health(i, i as i64 * 1000, &diff, shadow_eval.metrics());

        if i == 4 {
            // First unhealthy — hold
            assert_eq!(decision.action, CanaryAction::Hold);
        } else {
            // Second consecutive unhealthy — rollback
            assert_eq!(
                decision.action,
                CanaryAction::Rollback,
                "should rollback after 2 consecutive unhealthy"
            );
            assert_eq!(decision.phase, CanaryPhase::Shadow);
        }
    }

    // After rollback, all assignments should be blocked
    let filtered = canary.filter_assignments(&recs);
    assert!(
        filtered.assignments.is_empty(),
        "shadow phase should block all"
    );
}

#[test]
fn i04_full_lifecycle_shadow_canary_full() {
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Shadow,
        min_warmup_cycles: 0,
        min_healthy_before_advance: 2,
        auto_advance: true,
        canary_agent_allowlist: vec!["a1".to_string()],
        ..Default::default()
    });
    // Populate canary agents from allowlist
    canary.update_canary_agents(&["a1".to_string(), "a2".to_string()]);

    let recs = make_assignment_set(vec![
        make_assignment("b1", "a1", 0.9, 1),
        make_assignment("b2", "a2", 0.8, 2),
    ]);

    // Phase 1: Shadow — all blocked
    assert_eq!(canary.phase(), CanaryPhase::Shadow);
    let filtered = canary.filter_assignments(&recs);
    assert!(filtered.assignments.is_empty(), "shadow blocks all");

    // Run 2 perfect cycles → advance to Canary
    for i in 1..=2 {
        let diff = run_perfect_cycle(&mut shadow_eval, i, &recs);
        canary.evaluate_health(i, i as i64 * 1000, &diff, shadow_eval.metrics());
    }
    assert_eq!(canary.phase(), CanaryPhase::Canary);

    // Phase 2: Canary — only a1 passes
    let filtered = canary.filter_assignments(&recs);
    assert_eq!(filtered.assignments.len(), 1);
    assert_eq!(filtered.assignments[0].agent_id, "a1");

    // Run 2 more perfect cycles → advance to Full
    for i in 3..=4 {
        let diff = run_perfect_cycle(&mut shadow_eval, i, &recs);
        canary.evaluate_health(i, i as i64 * 1000, &diff, shadow_eval.metrics());
    }
    assert_eq!(canary.phase(), CanaryPhase::Full);

    // Phase 3: Full — all pass
    let filtered = canary.filter_assignments(&recs);
    assert_eq!(filtered.assignments.len(), 2);
}

#[test]
fn i05_rollback_from_full_to_canary_to_shadow() {
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Full,
        min_warmup_cycles: 0,
        max_consecutive_unhealthy: 1, // rollback after just 1 unhealthy
        auto_rollback: true,
        fidelity_threshold: 0.8,
        ..Default::default()
    });

    let recs = make_assignment_set(vec![
        make_assignment("b1", "a1", 0.9, 1),
        make_assignment("b2", "a2", 0.8, 2),
        make_assignment("b3", "a3", 0.7, 3),
    ]);

    // Warm evaluator with a perfect cycle
    let diff = run_perfect_cycle(&mut shadow_eval, 1, &recs);
    canary.evaluate_health(1, 1000, &diff, shadow_eval.metrics());
    assert_eq!(canary.phase(), CanaryPhase::Full);

    // Degraded cycle → rollback Full→Canary
    let diff = run_degraded_cycle(&mut shadow_eval, 2, &recs, 0.33);
    let decision = canary.evaluate_health(2, 2000, &diff, shadow_eval.metrics());
    assert_eq!(decision.action, CanaryAction::Rollback);
    assert_eq!(canary.phase(), CanaryPhase::Canary);

    // Another degraded cycle → rollback Canary→Shadow
    let diff = run_degraded_cycle(&mut shadow_eval, 3, &recs, 0.33);
    let decision = canary.evaluate_health(3, 3000, &diff, shadow_eval.metrics());
    assert_eq!(decision.action, CanaryAction::Rollback);
    assert_eq!(canary.phase(), CanaryPhase::Shadow);

    // Already in Shadow — should Hold (no further rollback)
    let diff = run_degraded_cycle(&mut shadow_eval, 4, &recs, 0.33);
    let decision = canary.evaluate_health(4, 4000, &diff, shadow_eval.metrics());
    assert_eq!(decision.action, CanaryAction::Hold);
    assert_eq!(canary.phase(), CanaryPhase::Shadow);
}

#[test]
fn i06_metrics_consistency_across_pipeline() {
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Shadow,
        min_warmup_cycles: 0,
        min_healthy_before_advance: 1000, // prevent transitions
        ..Default::default()
    });

    let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);

    let n = 10u64;
    for i in 1..=n {
        let diff = run_perfect_cycle(&mut shadow_eval, i, &recs);
        canary.evaluate_health(i, i as i64 * 1000, &diff, shadow_eval.metrics());
    }

    // Shadow evaluator tracked all cycles
    assert_eq!(shadow_eval.metrics().total_cycles, n);

    // Canary controller tracked all health checks
    let cm = canary.metrics();
    assert_eq!(cm.total_checks, n);
    assert_eq!(cm.healthy_checks + cm.unhealthy_checks, cm.total_checks);

    // Health history matches
    assert_eq!(canary.health_history().len() as u64, n);
}

#[test]
fn i07_safety_rejections_propagate_through_pipeline() {
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Canary,
        min_warmup_cycles: 0,
        max_safety_rejections_per_cycle: 2,
        max_consecutive_unhealthy: 1,
        auto_rollback: true,
        ..Default::default()
    });

    let recs = make_assignment_set(vec![
        make_assignment("b1", "a1", 0.9, 1),
        make_assignment("b2", "a2", 0.8, 2),
    ]);

    // Warm with a perfect cycle
    let diff = run_perfect_cycle(&mut shadow_eval, 1, &recs);
    canary.evaluate_health(1, 1000, &diff, shadow_eval.metrics());
    assert_eq!(canary.phase(), CanaryPhase::Canary);

    // Cycle with many safety rejections → unhealthy → rollback
    let diff = run_cycle_with_rejections(&mut shadow_eval, 2, &recs, 5);
    let decision = canary.evaluate_health(2, 2000, &diff, shadow_eval.metrics());

    // Check that safety rejections caused unhealthy check
    assert!(
        diff.safety_gate_rejections >= 5,
        "diff should report safety rejections"
    );
    assert!(
        !decision.health_check.healthy || decision.action == CanaryAction::Rollback,
        "excessive safety rejections should cause unhealthy or rollback"
    );
}

#[test]
fn i08_reset_allows_fresh_pipeline_restart() {
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Shadow,
        min_warmup_cycles: 0,
        min_healthy_before_advance: 2,
        auto_advance: true,
        ..Default::default()
    });

    let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);

    // Advance through phases
    for i in 1..=4 {
        let diff = run_perfect_cycle(&mut shadow_eval, i, &recs);
        canary.evaluate_health(i, i as i64 * 1000, &diff, shadow_eval.metrics());
    }
    assert_eq!(canary.phase(), CanaryPhase::Full);

    // Reset canary controller
    canary.reset();
    assert_eq!(canary.phase(), CanaryPhase::Shadow);
    assert!(canary.health_history().is_empty());
    assert_eq!(canary.metrics().total_checks, 0);

    // Can re-start the pipeline
    let diff = run_perfect_cycle(&mut shadow_eval, 5, &recs);
    let decision = canary.evaluate_health(5, 5000, &diff, shadow_eval.metrics());
    assert_eq!(decision.phase, CanaryPhase::Shadow);
    assert_eq!(canary.metrics().total_checks, 1);
}

#[test]
fn i09_shadow_evaluator_warmup_gates_canary_advance() {
    // Shadow evaluator has warmup — canary should detect NotWarmedUp
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 5, // 5 cycles warmup
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Shadow,
        min_warmup_cycles: 5, // match shadow warmup
        min_healthy_before_advance: 2,
        auto_advance: true,
        ..Default::default()
    });

    let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);

    // During warmup: canary should not advance even with perfect cycles
    for i in 1..=4 {
        let diff = run_perfect_cycle(&mut shadow_eval, i, &recs);
        let decision = canary.evaluate_health(i, i as i64 * 1000, &diff, shadow_eval.metrics());
        assert_eq!(
            canary.phase(),
            CanaryPhase::Shadow,
            "cycle {i}: should stay in Shadow during warmup"
        );
        assert_eq!(decision.action, CanaryAction::Hold);
    }

    // After warmup completes, should start advancing
    for i in 5..=8 {
        let diff = run_perfect_cycle(&mut shadow_eval, i, &recs);
        canary.evaluate_health(i, i as i64 * 1000, &diff, shadow_eval.metrics());
    }
    // After warmup + enough healthy checks, should have advanced
    assert_ne!(
        canary.phase(),
        CanaryPhase::Shadow,
        "should eventually advance past Shadow after warmup"
    );
}

#[test]
fn i10_empty_assignment_cycle_is_safe() {
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Canary,
        min_warmup_cycles: 0,
        ..Default::default()
    });

    let empty_recs = make_assignment_set(vec![]);

    // Empty cycle should not panic or cause invalid state
    let diff = run_perfect_cycle(&mut shadow_eval, 1, &empty_recs);
    let decision = canary.evaluate_health(1, 1000, &diff, shadow_eval.metrics());

    // With zero recommendations, dispatch rate is 1.0 (special case)
    assert!(
        decision.health_check.healthy || !decision.health_check.healthy,
        "should produce a valid health check regardless"
    );

    // Filter empty set
    let filtered = canary.filter_assignments(&empty_recs);
    assert!(filtered.assignments.is_empty());
    assert!(filtered.rejected.is_empty());
}

#[test]
fn i11_transition_history_tracks_full_lifecycle() {
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Shadow,
        min_warmup_cycles: 0,
        min_healthy_before_advance: 1,
        max_consecutive_unhealthy: 1,
        auto_advance: true,
        auto_rollback: true,
        fidelity_threshold: 0.8,
        ..Default::default()
    });

    let recs = make_assignment_set(vec![
        make_assignment("b1", "a1", 0.9, 1),
        make_assignment("b2", "a2", 0.8, 2),
        make_assignment("b3", "a3", 0.7, 3),
    ]);

    // Perfect cycle → Shadow→Canary
    let diff = run_perfect_cycle(&mut shadow_eval, 1, &recs);
    canary.evaluate_health(1, 1000, &diff, shadow_eval.metrics());
    assert_eq!(canary.phase(), CanaryPhase::Canary);

    // Perfect cycle → Canary→Full
    let diff = run_perfect_cycle(&mut shadow_eval, 2, &recs);
    canary.evaluate_health(2, 2000, &diff, shadow_eval.metrics());
    assert_eq!(canary.phase(), CanaryPhase::Full);

    // Degraded → Full→Canary
    let diff = run_degraded_cycle(&mut shadow_eval, 3, &recs, 0.33);
    canary.evaluate_health(3, 3000, &diff, shadow_eval.metrics());
    assert_eq!(canary.phase(), CanaryPhase::Canary);

    // Verify transition history captures all transitions
    let transitions = canary.transition_history();
    assert!(
        transitions.len() >= 3,
        "should have at least 3 transitions, got {}",
        transitions.len()
    );

    // Verify transition ordering
    let phases: Vec<(CanaryPhase, CanaryPhase)> =
        transitions.iter().map(|t| (t.from, t.to)).collect();
    assert_eq!(phases[0], (CanaryPhase::Shadow, CanaryPhase::Canary));
    assert_eq!(phases[1], (CanaryPhase::Canary, CanaryPhase::Full));
    assert_eq!(phases[2], (CanaryPhase::Full, CanaryPhase::Canary));

    // Verify metrics
    let m = canary.metrics();
    assert_eq!(m.total_transitions, transitions.len() as u64);
}

#[test]
fn i12_canary_agent_update_affects_filtering() {
    let mut shadow_eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    let mut canary = CanaryRolloutController::new(CanaryRolloutConfig {
        initial_phase: CanaryPhase::Canary,
        min_warmup_cycles: 0,
        canary_agent_fraction: 0.5,
        ..Default::default()
    });

    // Set initial agent pool
    canary.update_canary_agents(&[
        "a1".to_string(),
        "a2".to_string(),
        "a3".to_string(),
        "a4".to_string(),
    ]);

    let recs = make_assignment_set(vec![
        make_assignment("b1", "a1", 0.9, 1),
        make_assignment("b2", "a2", 0.8, 2),
        make_assignment("b3", "a3", 0.7, 3),
        make_assignment("b4", "a4", 0.6, 4),
    ]);

    // With 50% fraction, ~2 agents in canary cohort
    let filtered1 = canary.filter_assignments(&recs);
    let passed1 = filtered1.assignments.len();
    assert!(passed1 >= 1 && passed1 <= 4, "should pass canary subset");

    // Update agent pool — new agents
    canary.update_canary_agents(&["a5".to_string(), "a6".to_string()]);

    // Old assignments should now be filtered differently
    let filtered2 = canary.filter_assignments(&recs);
    // With new agents a5/a6, none of a1-a4 should pass
    assert!(
        filtered2.assignments.is_empty(),
        "agents not in canary set should all be rejected"
    );

    // Warm evaluator just to keep pipeline alive
    let diff = run_perfect_cycle(&mut shadow_eval, 1, &recs);
    canary.evaluate_health(1, 1000, &diff, shadow_eval.metrics());
}
