//! Property-based tests for canary_rollout_controller.rs.
//!
//! Covers the Shadow→Canary→Full phase state machine, health check computation,
//! automatic advance/rollback decisions, assignment filtering, canary agent
//! selection, metrics consistency, serde roundtrips, history bounding, and
//! force-transition validation.

#![cfg(feature = "subprocess-bridge")]
#![allow(clippy::manual_range_contains)]

use frankenterm_core::canary_rollout_controller::{
    CanaryAction, CanaryHealthCheck, CanaryMetrics, CanaryPhase, CanaryPhaseTransition,
    CanaryRolloutConfig, CanaryRolloutController,
};
use frankenterm_core::mission_events::{
    MissionEventBuilder, MissionEventKind, MissionEventLog, MissionEventLogConfig,
};
use frankenterm_core::planner_features::{Assignment, AssignmentSet, SolverConfig};
use frankenterm_core::shadow_mode_evaluator::{
    ShadowEvaluationConfig, ShadowModeDiff, ShadowModeEvaluator, ShadowModeMetrics,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_phase() -> impl Strategy<Value = CanaryPhase> {
    prop_oneof![
        Just(CanaryPhase::Shadow),
        Just(CanaryPhase::Canary),
        Just(CanaryPhase::Full),
    ]
}

fn arb_agent_id() -> impl Strategy<Value = String> {
    "[a-z]{1,4}[0-9]{1,3}".prop_map(|s| s)
}

fn arb_bead_id() -> impl Strategy<Value = String> {
    "b-[a-z]{2,4}".prop_map(|s| s)
}

fn arb_config() -> impl Strategy<Value = CanaryRolloutConfig> {
    (
        arb_phase(),
        0.01..=1.0f64, // canary_agent_fraction
        0.1..=0.95f64, // fidelity_threshold
        1..=5u32,      // max_consecutive_unhealthy
        1..=10u32,     // min_healthy_before_advance
        0..=10u64,     // min_warmup_cycles
        1..=20usize,   // max_safety_rejections_per_cycle
        0.05..=0.9f64, // max_conflict_rate
        any::<bool>(), // auto_advance
        any::<bool>(), // auto_rollback
    )
        .prop_map(
            |(
                phase,
                frac,
                fid,
                max_unhl,
                min_hl,
                warmup,
                max_safety,
                max_conf,
                auto_adv,
                auto_rb,
            )| {
                CanaryRolloutConfig {
                    initial_phase: phase,
                    canary_agent_fraction: frac,
                    fidelity_threshold: fid,
                    max_consecutive_unhealthy: max_unhl,
                    min_healthy_before_advance: min_hl,
                    min_warmup_cycles: warmup,
                    max_safety_rejections_per_cycle: max_safety,
                    max_conflict_rate: max_conf,
                    auto_advance: auto_adv,
                    auto_rollback: auto_rb,
                    canary_agent_allowlist: Vec::new(),
                }
            },
        )
}

fn arb_assignment() -> impl Strategy<Value = Assignment> {
    (arb_bead_id(), arb_agent_id(), 0.0..=1.0f64, 1..=100usize).prop_map(
        |(bead_id, agent_id, score, rank)| Assignment {
            bead_id,
            agent_id,
            score,
            rank,
        },
    )
}

fn arb_assignment_set() -> impl Strategy<Value = AssignmentSet> {
    prop::collection::vec(arb_assignment(), 0..=8).prop_map(|assignments| AssignmentSet {
        assignments,
        rejected: Vec::new(),
        solver_config: SolverConfig::default(),
    })
}

/// Build warmed-up ShadowModeMetrics by running `n` perfect evaluation cycles.
fn make_warmed_metrics(n: u64) -> ShadowModeMetrics {
    let mut eval = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..Default::default()
    });
    for i in 1..=n {
        let recs = AssignmentSet {
            assignments: vec![
                Assignment {
                    bead_id: format!("b{i}a"),
                    agent_id: "a1".into(),
                    score: 0.9,
                    rank: 1,
                },
                Assignment {
                    bead_id: format!("b{i}b"),
                    agent_id: "a2".into(),
                    score: 0.8,
                    rank: 2,
                },
            ],
            rejected: Vec::new(),
            solver_config: SolverConfig::default(),
        };
        let mut log = MissionEventLog::new(MissionEventLogConfig {
            max_events: 64,
            enabled: true,
        });
        for a in &recs.assignments {
            log.emit(
                MissionEventBuilder::new(
                    MissionEventKind::AssignmentEmitted,
                    "mission.dispatch.assignment_emitted",
                )
                .cycle(i, i as i64 * 1000)
                .correlation("corr")
                .labels("ws", "track")
                .detail_str("bead_id", &a.bead_id)
                .detail_str("agent_id", &a.agent_id),
            );
        }
        eval.evaluate_cycle(i, i as i64 * 1000, &recs, log.events());
    }
    eval.metrics().clone()
}

fn healthy_diff(cycle_id: u64) -> ShadowModeDiff {
    ShadowModeDiff {
        cycle_id,
        timestamp_ms: cycle_id as i64 * 1000,
        recommendations_count: 3,
        rejections_count: 0,
        emissions_count: 3,
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
    }
}

fn unhealthy_diff(cycle_id: u64) -> ShadowModeDiff {
    ShadowModeDiff {
        cycle_id,
        timestamp_ms: cycle_id as i64 * 1000,
        recommendations_count: 3,
        rejections_count: 0,
        emissions_count: 1,
        execution_rejections_count: 2,
        missing_executions: vec![("b2".into(), "a2".into())],
        unexpected_executions: Vec::new(),
        agent_divergences: Vec::new(),
        score_accuracy: Vec::new(),
        safety_gate_rejections: 0,
        retry_storm_throttles: 0,
        conflicts_detected: 0,
        conflicts_auto_resolved: 0,
        dispatch_rate: 0.33,
        agent_match_rate: 1.0,
        fidelity_score: 0.3,
    }
}

// ── Phase state machine ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 1. Phase.next() progression covers all three phases exactly
    #[test]
    fn phase_next_exhaustive(phase in arb_phase()) {
        match phase {
            CanaryPhase::Shadow => prop_assert_eq!(phase.next(), Some(CanaryPhase::Canary)),
            CanaryPhase::Canary => prop_assert_eq!(phase.next(), Some(CanaryPhase::Full)),
            CanaryPhase::Full   => prop_assert_eq!(phase.next(), None),
        }
    }

    // 2. dispatches() is false only for Shadow
    #[test]
    fn dispatches_only_non_shadow(phase in arb_phase()) {
        prop_assert_eq!(phase.dispatches(), phase != CanaryPhase::Shadow);
    }

    // 3. can_advance_to is never reflexive
    #[test]
    fn advance_not_reflexive(phase in arb_phase()) {
        prop_assert!(!phase.can_advance_to(phase));
    }

    // 4. can_rollback_to is never reflexive
    #[test]
    fn rollback_not_reflexive(phase in arb_phase()) {
        prop_assert!(!phase.can_rollback_to(phase));
    }

    // 5. advance and rollback are mutually exclusive for any pair
    #[test]
    fn advance_rollback_exclusive(a in arb_phase(), b in arb_phase()) {
        if a != b {
            // Can't both advance and rollback between same pair
            prop_assert!(!(a.can_advance_to(b) && a.can_rollback_to(b)));
        }
    }
}

// ── Controller construction ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 6. Controller starts at configured initial_phase
    #[test]
    fn starts_at_configured_phase(config in arb_config()) {
        let expected = config.initial_phase;
        let ctrl = CanaryRolloutController::new(config);
        prop_assert_eq!(ctrl.phase(), expected);
        prop_assert!(ctrl.health_history().is_empty());
        prop_assert!(ctrl.transition_history().is_empty());
    }
}

// ── Health check computation ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 7. Healthy checks have empty failure_reasons
    #[test]
    fn healthy_check_no_failures(cycle_id in 1..=100u64) {
        let mut ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            min_warmup_cycles: 0,
            min_healthy_before_advance: 999, // prevent advance
            ..Default::default()
        });
        let metrics = make_warmed_metrics(10);
        let diff = healthy_diff(cycle_id);
        let decision = ctrl.evaluate_health(cycle_id, cycle_id as i64 * 1000, &diff, &metrics);
        prop_assert!(decision.health_check.healthy);
        prop_assert!(decision.health_check.failure_reasons.is_empty());
    }

    // 8. Low fidelity always triggers LowFidelity reason
    #[test]
    fn low_fidelity_detected(
        fidelity in 0.0..0.5f64,
    ) {
        let mut ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            fidelity_threshold: 0.7,
            min_warmup_cycles: 0,
            min_healthy_before_advance: 999,
            max_consecutive_unhealthy: 999,
            ..Default::default()
        });
        let metrics = make_warmed_metrics(10);
        let mut diff = healthy_diff(1);
        diff.fidelity_score = fidelity;
        let decision = ctrl.evaluate_health(1, 1000, &diff, &metrics);
        prop_assert!(!decision.health_check.healthy);
        let has_low_fidelity = decision.health_check.failure_reasons.iter().any(|r| {
            matches!(r, frankenterm_core::canary_rollout_controller::HealthFailureReason::LowFidelity)
        });
        prop_assert!(has_low_fidelity, "expected LowFidelity for score {}", fidelity);
    }

    // 9. Conflict rate computed correctly: conflicts / max(emissions, 1)
    #[test]
    fn conflict_rate_computation(
        emissions in 0..=10usize,
        conflicts in 0..=10usize,
    ) {
        let mut ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            min_warmup_cycles: 0,
            max_conflict_rate: 2.0, // high threshold so only testing computation
            min_healthy_before_advance: 999,
            ..Default::default()
        });
        let metrics = make_warmed_metrics(10);
        let mut diff = healthy_diff(1);
        diff.emissions_count = emissions;
        diff.conflicts_detected = conflicts;
        let decision = ctrl.evaluate_health(1, 1000, &diff, &metrics);
        let expected_rate = conflicts as f64 / emissions.max(1) as f64;
        let actual = decision.health_check.conflict_rate;
        prop_assert!(
            (actual - expected_rate).abs() < 1e-10,
            "expected {} got {}", expected_rate, actual
        );
    }

    // 10. Safety rejections above threshold produce ExcessiveSafetyRejections
    #[test]
    fn excessive_safety_rejections_detected(
        rejections in 6..=20usize,
    ) {
        let mut ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            max_safety_rejections_per_cycle: 5,
            min_warmup_cycles: 0,
            min_healthy_before_advance: 999,
            max_consecutive_unhealthy: 999,
            ..Default::default()
        });
        let metrics = make_warmed_metrics(10);
        let mut diff = healthy_diff(1);
        diff.safety_gate_rejections = rejections;
        let decision = ctrl.evaluate_health(1, 1000, &diff, &metrics);
        prop_assert!(!decision.health_check.healthy);
        let has_excessive = decision.health_check.failure_reasons.iter().any(|r| {
            matches!(r, frankenterm_core::canary_rollout_controller::HealthFailureReason::ExcessiveSafetyRejections)
        });
        prop_assert!(has_excessive);
    }
}

// ── Advance / rollback decisions ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 11. N consecutive healthy checks in Shadow → Advance to Canary
    #[test]
    fn advance_after_n_healthy(n in 1..=5u32) {
        let config = CanaryRolloutConfig {
            min_healthy_before_advance: n,
            min_warmup_cycles: 0,
            auto_advance: true,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for i in 1..n as u64 {
            let d = ctrl.evaluate_health(i, i as i64 * 1000, &healthy_diff(i), &metrics);
            prop_assert_eq!(d.action, CanaryAction::Hold);
        }
        let final_d = ctrl.evaluate_health(n as u64, n as i64 * 1000, &healthy_diff(n as u64), &metrics);
        prop_assert_eq!(final_d.action, CanaryAction::Advance);
        prop_assert_eq!(final_d.phase, CanaryPhase::Canary);
    }

    // 12. N consecutive unhealthy checks in Canary → Rollback to Shadow
    #[test]
    fn rollback_after_n_unhealthy(n in 1..=5u32) {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            max_consecutive_unhealthy: n,
            min_warmup_cycles: 0,
            auto_rollback: true,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for i in 1..n as u64 {
            let d = ctrl.evaluate_health(i, i as i64 * 1000, &unhealthy_diff(i), &metrics);
            prop_assert_eq!(d.action, CanaryAction::Hold);
        }
        let final_d = ctrl.evaluate_health(n as u64, n as i64 * 1000, &unhealthy_diff(n as u64), &metrics);
        prop_assert_eq!(final_d.action, CanaryAction::Rollback);
        prop_assert_eq!(final_d.phase, CanaryPhase::Shadow);
    }

    // 13. Healthy check resets unhealthy streak
    #[test]
    fn healthy_resets_streak(
        unhealthy_before in 1..=4u32,
    ) {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            max_consecutive_unhealthy: 5,
            min_warmup_cycles: 0,
            min_healthy_before_advance: 999,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for i in 1..=unhealthy_before as u64 {
            ctrl.evaluate_health(i, i as i64 * 1000, &unhealthy_diff(i), &metrics);
        }
        prop_assert_eq!(ctrl.metrics().consecutive_unhealthy, unhealthy_before);

        let next = unhealthy_before as u64 + 1;
        ctrl.evaluate_health(next, next as i64 * 1000, &healthy_diff(next), &metrics);
        prop_assert_eq!(ctrl.metrics().consecutive_unhealthy, 0);
        prop_assert_eq!(ctrl.metrics().consecutive_healthy, 1);
    }

    // 14. auto_advance=false never advances
    #[test]
    fn no_advance_when_disabled(cycles in 1..=10u32) {
        let config = CanaryRolloutConfig {
            auto_advance: false,
            min_healthy_before_advance: 1,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for i in 1..=cycles as u64 {
            let d = ctrl.evaluate_health(i, i as i64 * 1000, &healthy_diff(i), &metrics);
            prop_assert_eq!(d.action, CanaryAction::Hold);
        }
        prop_assert_eq!(ctrl.phase(), CanaryPhase::Shadow);
    }

    // 15. auto_rollback=false never rolls back
    #[test]
    fn no_rollback_when_disabled(cycles in 1..=10u32) {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            auto_rollback: false,
            max_consecutive_unhealthy: 1,
            min_warmup_cycles: 0,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for i in 1..=cycles as u64 {
            let d = ctrl.evaluate_health(i, i as i64 * 1000, &unhealthy_diff(i), &metrics);
            prop_assert_eq!(d.action, CanaryAction::Hold);
        }
        prop_assert_eq!(ctrl.phase(), CanaryPhase::Canary);
    }

    // 16. Cannot rollback from Shadow
    #[test]
    fn no_rollback_from_shadow(cycles in 1..=5u32) {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Shadow,
            max_consecutive_unhealthy: 1,
            min_warmup_cycles: 0,
            auto_rollback: true,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for i in 1..=cycles as u64 {
            let d = ctrl.evaluate_health(i, i as i64 * 1000, &unhealthy_diff(i), &metrics);
            prop_assert_eq!(d.action, CanaryAction::Hold);
        }
        prop_assert_eq!(ctrl.phase(), CanaryPhase::Shadow);
    }
}

// ── Assignment filtering ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 17. Shadow phase: all assignments become rejected
    #[test]
    fn shadow_rejects_all(set in arb_assignment_set()) {
        let ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            initial_phase: CanaryPhase::Shadow,
            ..Default::default()
        });
        let original_count = set.assignments.len();
        let filtered = ctrl.filter_assignments(&set);
        prop_assert!(filtered.assignments.is_empty());
        prop_assert_eq!(filtered.rejected.len(), original_count + set.rejected.len());
    }

    // 18. Full phase: all assignments pass through
    #[test]
    fn full_passes_all(set in arb_assignment_set()) {
        let ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            initial_phase: CanaryPhase::Full,
            ..Default::default()
        });
        let filtered = ctrl.filter_assignments(&set);
        prop_assert_eq!(filtered.assignments.len(), set.assignments.len());
        prop_assert_eq!(filtered.rejected.len(), set.rejected.len());
    }

    // 19. Canary phase: total = passed + rejected (conservation)
    #[test]
    fn canary_conserves_assignments(
        set in arb_assignment_set(),
        agents in prop::collection::vec(arb_agent_id(), 1..=5),
    ) {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            canary_agent_allowlist: agents.clone(),
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&agents);

        let original_assignments = set.assignments.len();
        let original_rejected = set.rejected.len();
        let filtered = ctrl.filter_assignments(&set);

        // passed + new_rejected = original_assignments
        let new_rejected = filtered.rejected.len() - original_rejected;
        prop_assert_eq!(
            filtered.assignments.len() + new_rejected,
            original_assignments,
            "conservation: {} passed + {} new rejected != {} original",
            filtered.assignments.len(),
            new_rejected,
            original_assignments,
        );
    }

    // 20. Canary phase: only canary agents pass
    #[test]
    fn canary_only_allows_canary_agents(
        set in arb_assignment_set(),
        agents in prop::collection::vec(arb_agent_id(), 1..=3),
    ) {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            canary_agent_allowlist: agents.clone(),
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&agents);

        let canary_set: std::collections::HashSet<_> = ctrl.canary_agents().clone();
        let filtered = ctrl.filter_assignments(&set);

        for a in &filtered.assignments {
            prop_assert!(
                canary_set.contains(&a.agent_id),
                "non-canary agent {} passed filter",
                a.agent_id
            );
        }
    }
}

// ── Canary agent selection ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 21. Fraction-based selection: count = ceil(len * fraction), clamped to [1, len]
    #[test]
    fn agent_fraction_count(
        fraction in 0.01..=1.0f64,
        agents in prop::collection::vec(arb_agent_id(), 1..=20),
    ) {
        let config = CanaryRolloutConfig {
            canary_agent_fraction: fraction,
            canary_agent_allowlist: Vec::new(),
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&agents);

        let expected = ((agents.len() as f64 * fraction).ceil() as usize)
            .max(1)
            .min(agents.len());
        prop_assert_eq!(
            ctrl.canary_agents().len(),
            expected,
            "fraction={}, agents={}, expected={}, got={}",
            fraction,
            agents.len(),
            expected,
            ctrl.canary_agents().len(),
        );
    }

    // 22. Allowlist overrides fraction
    #[test]
    fn allowlist_overrides(
        agents in prop::collection::vec(arb_agent_id(), 2..=10),
        take in 1..=3usize,
    ) {
        let allowlist: Vec<String> = agents.iter().take(take.min(agents.len())).cloned().collect();
        let config = CanaryRolloutConfig {
            canary_agent_fraction: 0.01,
            canary_agent_allowlist: allowlist.clone(),
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        ctrl.update_canary_agents(&agents);

        for a in &allowlist {
            prop_assert!(ctrl.canary_agents().contains(a));
        }
        prop_assert_eq!(ctrl.canary_agents().len(), allowlist.len());
    }

    // 23. Empty agent list produces empty canary set
    #[test]
    fn empty_agents_empty_canary(_dummy in 0..=10u32) {
        let mut ctrl = CanaryRolloutController::with_defaults();
        ctrl.update_canary_agents(&[]);
        prop_assert!(ctrl.canary_agents().is_empty());
    }
}

// ── Metrics consistency ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 24. total_checks = healthy + unhealthy
    #[test]
    fn metrics_check_totals(
        healthy_count in 0..=5u32,
        unhealthy_count in 0..=5u32,
    ) {
        let config = CanaryRolloutConfig {
            min_warmup_cycles: 0,
            min_healthy_before_advance: 999,
            max_consecutive_unhealthy: 999,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        let mut cycle = 1u64;
        for _ in 0..healthy_count {
            ctrl.evaluate_health(cycle, cycle as i64 * 1000, &healthy_diff(cycle), &metrics);
            cycle += 1;
        }
        for _ in 0..unhealthy_count {
            ctrl.evaluate_health(cycle, cycle as i64 * 1000, &unhealthy_diff(cycle), &metrics);
            cycle += 1;
        }

        let m = ctrl.metrics();
        prop_assert_eq!(m.total_checks, m.healthy_checks + m.unhealthy_checks);
        prop_assert_eq!(m.total_checks, (healthy_count + unhealthy_count) as u64);
    }

    // 25. total_transitions = total_advances + total_rollbacks
    #[test]
    fn metrics_transition_totals(advances in 0..=3u32) {
        let config = CanaryRolloutConfig {
            min_warmup_cycles: 0,
            min_healthy_before_advance: 1,
            auto_advance: true,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        // Each healthy check in Shadow/Canary triggers one advance (max 2: Shadow→Canary→Full)
        for i in 1..=(advances.min(2)) as u64 {
            ctrl.evaluate_health(i, i as i64 * 1000, &healthy_diff(i), &metrics);
        }

        let m = ctrl.metrics();
        prop_assert_eq!(m.total_transitions, m.total_advances + m.total_rollbacks);
    }

    // 26. max_consecutive_unhealthy >= consecutive_unhealthy
    #[test]
    fn max_tracks_peak(
        pattern in prop::collection::vec(any::<bool>(), 1..=10),
    ) {
        let config = CanaryRolloutConfig {
            initial_phase: CanaryPhase::Canary,
            min_warmup_cycles: 0,
            min_healthy_before_advance: 999,
            max_consecutive_unhealthy: 999,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for (i, is_healthy) in pattern.iter().enumerate() {
            let cycle = (i + 1) as u64;
            let diff = if *is_healthy { healthy_diff(cycle) } else { unhealthy_diff(cycle) };
            ctrl.evaluate_health(cycle, cycle as i64 * 1000, &diff, &metrics);
        }

        let m = ctrl.metrics();
        prop_assert!(
            m.max_consecutive_unhealthy >= m.consecutive_unhealthy,
            "max {} < current {}",
            m.max_consecutive_unhealthy,
            m.consecutive_unhealthy,
        );
    }
}

// ── Force transition ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 27. Force transition to same phase is no-op
    #[test]
    fn force_same_phase_noop(phase in arb_phase()) {
        let mut ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            initial_phase: phase,
            ..Default::default()
        });
        let result = ctrl.force_transition(phase, 1, 1000, "noop");
        prop_assert!(result.is_none());
        prop_assert_eq!(ctrl.phase(), phase);
    }

    // 28. Force transition only works for valid advance or rollback
    #[test]
    fn force_validates_transitions(from in arb_phase(), to in arb_phase()) {
        let mut ctrl = CanaryRolloutController::new(CanaryRolloutConfig {
            initial_phase: from,
            ..Default::default()
        });
        let result = ctrl.force_transition(to, 1, 1000, "test");

        if from == to {
            prop_assert!(result.is_none());
        } else if from.can_advance_to(to) || from.can_rollback_to(to) {
            prop_assert!(result.is_some());
            prop_assert_eq!(ctrl.phase(), to);
            let t = result.unwrap();
            prop_assert_eq!(t.from, from);
            prop_assert_eq!(t.to, to);
        } else {
            prop_assert!(result.is_none());
            prop_assert_eq!(ctrl.phase(), from);
        }
    }
}

// ── History bounding ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    // 29. Health history never exceeds 256
    #[test]
    fn health_history_bounded(cycles in 250..=270u64) {
        let config = CanaryRolloutConfig {
            min_warmup_cycles: 0,
            min_healthy_before_advance: 999,
            max_consecutive_unhealthy: 999,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for i in 1..=cycles {
            ctrl.evaluate_health(i, i as i64 * 1000, &healthy_diff(i), &metrics);
        }

        prop_assert!(
            ctrl.health_history().len() <= 256,
            "history len {} > 256",
            ctrl.health_history().len(),
        );
    }

    // 30. Transition history never exceeds 256
    #[test]
    fn transition_history_bounded(transitions in 250..=270u32) {
        let mut ctrl = CanaryRolloutController::with_defaults();

        // Alternate Shadow↔Canary to generate many transitions
        for i in 0..transitions {
            let cycle = i as u64 + 1;
            let ts = cycle as i64 * 1000;
            if ctrl.phase() == CanaryPhase::Shadow {
                ctrl.force_transition(CanaryPhase::Canary, cycle, ts, "bounce");
            } else {
                ctrl.force_transition(CanaryPhase::Shadow, cycle, ts, "bounce");
            }
        }

        prop_assert!(
            ctrl.transition_history().len() <= 256,
            "transition history len {} > 256",
            ctrl.transition_history().len(),
        );
    }
}

// ── Reset ───────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 31. Reset restores initial phase and clears all state
    #[test]
    fn reset_clears_all(config in arb_config()) {
        let initial = config.initial_phase;
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        // Do some evaluations
        for i in 1..=3u64 {
            ctrl.evaluate_health(i, i as i64 * 1000, &healthy_diff(i), &metrics);
        }

        ctrl.reset();

        prop_assert_eq!(ctrl.phase(), initial);
        prop_assert!(ctrl.health_history().is_empty());
        prop_assert!(ctrl.transition_history().is_empty());
        prop_assert_eq!(ctrl.metrics().total_checks, 0);
        prop_assert_eq!(ctrl.metrics().healthy_checks, 0);
        prop_assert_eq!(ctrl.metrics().unhealthy_checks, 0);
        prop_assert!(ctrl.canary_agents().is_empty());
    }
}

// ── Serde roundtrips ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 32. CanaryPhase serde roundtrip
    #[test]
    fn phase_serde_roundtrip(phase in arb_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let restored: CanaryPhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, phase);
    }

    // 33. CanaryAction serde roundtrip
    #[test]
    fn action_serde_roundtrip(
        action in prop_oneof![
            Just(CanaryAction::Hold),
            Just(CanaryAction::Advance),
            Just(CanaryAction::Rollback),
        ]
    ) {
        let json = serde_json::to_string(&action).unwrap();
        let restored: CanaryAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, action);
    }

    // 34. CanaryRolloutConfig serde roundtrip
    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let restored: CanaryRolloutConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.initial_phase, config.initial_phase);
        prop_assert!((restored.canary_agent_fraction - config.canary_agent_fraction).abs() < 1e-10);
        prop_assert!((restored.fidelity_threshold - config.fidelity_threshold).abs() < 1e-10);
        prop_assert_eq!(restored.max_consecutive_unhealthy, config.max_consecutive_unhealthy);
        prop_assert_eq!(restored.min_healthy_before_advance, config.min_healthy_before_advance);
        prop_assert_eq!(restored.auto_advance, config.auto_advance);
        prop_assert_eq!(restored.auto_rollback, config.auto_rollback);
    }

    // 35. CanaryHealthCheck serde roundtrip
    #[test]
    fn health_check_serde_roundtrip(
        cycle_id in 1..=1000u64,
        fidelity in 0.0..=1.0f64,
        rejections in 0..=10usize,
    ) {
        let check = CanaryHealthCheck {
            cycle_id,
            timestamp_ms: cycle_id as i64 * 1000,
            healthy: fidelity > 0.5,
            fidelity_score: fidelity,
            safety_rejections: rejections,
            conflict_rate: 0.1,
            failure_reasons: Vec::new(),
        };
        let json = serde_json::to_string(&check).unwrap();
        let restored: CanaryHealthCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.cycle_id, cycle_id);
        prop_assert!((restored.fidelity_score - fidelity).abs() < 1e-10);
        prop_assert_eq!(restored.safety_rejections, rejections);
    }

    // 36. CanaryPhaseTransition serde roundtrip
    #[test]
    fn transition_serde_roundtrip(from in arb_phase(), to in arb_phase()) {
        let trans = CanaryPhaseTransition {
            from,
            to,
            cycle_id: 42,
            timestamp_ms: 42000,
            reason: "test_reason".to_string(),
        };
        let json = serde_json::to_string(&trans).unwrap();
        let restored: CanaryPhaseTransition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.from, from);
        prop_assert_eq!(restored.to, to);
        prop_assert_eq!(restored.cycle_id, 42);
    }

    // 37. CanaryMetrics serde roundtrip
    #[test]
    fn metrics_serde_roundtrip(
        total in 0..=100u64,
        healthy in 0..=50u64,
    ) {
        let metrics = CanaryMetrics {
            total_checks: total,
            healthy_checks: healthy,
            unhealthy_checks: total.saturating_sub(healthy),
            total_transitions: 0,
            total_rollbacks: 0,
            total_advances: 0,
            consecutive_healthy: 0,
            consecutive_unhealthy: 0,
            max_consecutive_unhealthy: 0,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let restored: CanaryMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.total_checks, total);
        prop_assert_eq!(restored.healthy_checks, healthy);
    }
}

// ── Full lifecycle ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 38. Full advance-then-rollback lifecycle
    #[test]
    fn full_lifecycle_advance_and_rollback(
        advance_threshold in 1..=3u32,
        rollback_threshold in 1..=3u32,
    ) {
        let config = CanaryRolloutConfig {
            min_healthy_before_advance: advance_threshold,
            max_consecutive_unhealthy: rollback_threshold,
            min_warmup_cycles: 0,
            auto_advance: true,
            auto_rollback: true,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        // Advance Shadow → Canary
        let mut cycle = 1u64;
        for _ in 0..advance_threshold {
            ctrl.evaluate_health(cycle, cycle as i64 * 1000, &healthy_diff(cycle), &metrics);
            cycle += 1;
        }
        prop_assert_eq!(ctrl.phase(), CanaryPhase::Canary);

        // Advance Canary → Full
        for _ in 0..advance_threshold {
            ctrl.evaluate_health(cycle, cycle as i64 * 1000, &healthy_diff(cycle), &metrics);
            cycle += 1;
        }
        prop_assert_eq!(ctrl.phase(), CanaryPhase::Full);

        // Rollback Full → Canary
        for _ in 0..rollback_threshold {
            ctrl.evaluate_health(cycle, cycle as i64 * 1000, &unhealthy_diff(cycle), &metrics);
            cycle += 1;
        }
        prop_assert_eq!(ctrl.phase(), CanaryPhase::Canary);

        // Metrics consistency
        let m = ctrl.metrics();
        prop_assert_eq!(m.total_transitions, m.total_advances + m.total_rollbacks);
        prop_assert!(m.total_advances >= 2, "expected >= 2 advances");
        prop_assert!(m.total_rollbacks >= 1, "expected >= 1 rollback");
    }

    // 39. Random healthy/unhealthy sequence preserves invariants
    #[test]
    fn random_sequence_invariants(
        initial_phase in arb_phase(),
        pattern in prop::collection::vec(any::<bool>(), 1..=20),
    ) {
        let config = CanaryRolloutConfig {
            initial_phase,
            min_warmup_cycles: 0,
            min_healthy_before_advance: 3,
            max_consecutive_unhealthy: 3,
            auto_advance: true,
            auto_rollback: true,
            ..Default::default()
        };
        let mut ctrl = CanaryRolloutController::new(config);
        let metrics = make_warmed_metrics(10);

        for (i, is_healthy) in pattern.iter().enumerate() {
            let cycle = (i + 1) as u64;
            let diff = if *is_healthy { healthy_diff(cycle) } else { unhealthy_diff(cycle) };
            let decision = ctrl.evaluate_health(cycle, cycle as i64 * 1000, &diff, &metrics);

            // Phase must be valid
            let is_valid_phase = matches!(
                decision.phase,
                CanaryPhase::Shadow | CanaryPhase::Canary | CanaryPhase::Full
            );
            prop_assert!(is_valid_phase);

            // Action must be valid
            let is_valid_action = matches!(
                decision.action,
                CanaryAction::Hold | CanaryAction::Advance | CanaryAction::Rollback
            );
            prop_assert!(is_valid_action);
        }

        // Final invariants
        let m = ctrl.metrics();
        prop_assert_eq!(m.total_checks, pattern.len() as u64);
        prop_assert_eq!(m.total_checks, m.healthy_checks + m.unhealthy_checks);
        prop_assert_eq!(m.total_transitions, m.total_advances + m.total_rollbacks);
    }
}

// ── Determinism ─────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 40. Same inputs produce identical outputs
    #[test]
    fn deterministic_evaluation(
        pattern in prop::collection::vec(any::<bool>(), 1..=10),
    ) {
        let config = CanaryRolloutConfig {
            min_warmup_cycles: 0,
            min_healthy_before_advance: 2,
            max_consecutive_unhealthy: 2,
            auto_advance: true,
            auto_rollback: true,
            ..Default::default()
        };
        let metrics = make_warmed_metrics(10);

        let run = |cfg: CanaryRolloutConfig| {
            let mut ctrl = CanaryRolloutController::new(cfg);
            let mut phases = Vec::new();
            for (i, is_healthy) in pattern.iter().enumerate() {
                let cycle = (i + 1) as u64;
                let diff = if *is_healthy { healthy_diff(cycle) } else { unhealthy_diff(cycle) };
                let d = ctrl.evaluate_health(cycle, cycle as i64 * 1000, &diff, &metrics);
                phases.push((d.phase, d.action));
            }
            phases
        };

        let r1 = run(config.clone());
        let r2 = run(config);
        prop_assert_eq!(r1, r2);
    }
}
