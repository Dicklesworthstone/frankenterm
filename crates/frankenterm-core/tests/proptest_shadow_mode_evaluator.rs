//! Property-based tests for shadow_mode_evaluator (ft-1i2ge.6.3).
//!
//! Tests invariants of the shadow-mode evaluator: fidelity scoring, dispatch rate,
//! agent match rate, divergence tracking, history bounding, metrics accumulation,
//! serde roundtrip, warmup detection, and configuration toggles.

#![cfg(feature = "subprocess-bridge")]

use frankenterm_core::mission_events::*;
use frankenterm_core::planner_features::{Assignment as PlannerAssignment, AssignmentSet, SolverConfig};
use frankenterm_core::shadow_mode_evaluator::*;
use proptest::prelude::*;
use std::collections::HashSet;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_log() -> MissionEventLog {
    MissionEventLog::new(MissionEventLogConfig {
        max_events: 1024,
        enabled: true,
    })
}

fn emit_dispatch(log: &mut MissionEventLog, cycle_id: u64, bead_id: &str, agent_id: &str) {
    log.emit(
        MissionEventBuilder::new(
            MissionEventKind::AssignmentEmitted,
            "mission.dispatch.assignment_emitted",
        )
        .cycle(cycle_id, 1000)
        .correlation("corr-1")
        .labels("workspace", "track")
        .detail_str("bead_id", bead_id)
        .detail_str("agent_id", agent_id),
    );
}

fn emit_rejection(log: &mut MissionEventLog, cycle_id: u64, bead_id: &str) {
    log.emit(
        MissionEventBuilder::new(
            MissionEventKind::AssignmentRejected,
            "mission.dispatch.assignment_rejected",
        )
        .cycle(cycle_id, 1000)
        .correlation("corr-1")
        .labels("workspace", "track")
        .detail_str("bead_id", bead_id),
    );
}

fn emit_safety(log: &mut MissionEventLog, cycle_id: u64) {
    log.emit(
        MissionEventBuilder::new(
            MissionEventKind::SafetyGateRejection,
            "mission.safety.gate_rejection",
        )
        .cycle(cycle_id, 1000)
        .correlation("corr-1")
        .labels("workspace", "track"),
    );
}

fn emit_conflict(log: &mut MissionEventLog, cycle_id: u64, kind: MissionEventKind) {
    log.emit(
        MissionEventBuilder::new(kind, "mission.conflict")
            .cycle(cycle_id, 1000)
            .correlation("corr-1")
            .labels("workspace", "track"),
    );
}

fn make_assignment(bead_id: &str, agent_id: &str, score: f64, rank: usize) -> PlannerAssignment {
    PlannerAssignment {
        bead_id: bead_id.to_string(),
        agent_id: agent_id.to_string(),
        score,
        rank,
    }
}

fn make_assignment_set(assignments: Vec<PlannerAssignment>) -> AssignmentSet {
    AssignmentSet {
        assignments,
        rejected: Vec::new(),
        solver_config: SolverConfig::default(),
    }
}

// ── Strategies ───────────────────────────────────────────────────────────────

fn arb_bead_id() -> impl Strategy<Value = String> {
    "[a-z]{2,5}-[0-9]{1,3}".prop_map(|s| s)
}

fn arb_agent_id() -> impl Strategy<Value = String> {
    "agent-[a-z]{2,5}".prop_map(|s| s)
}

fn arb_score() -> impl Strategy<Value = f64> {
    0.0f64..=1.0f64
}

/// Generate a vec of unique (bead_id, agent_id, score) tuples for assignments.
fn arb_unique_assignments(
    max_count: usize,
) -> impl Strategy<Value = Vec<(String, String, f64)>> {
    proptest::collection::vec(
        (arb_bead_id(), arb_agent_id(), arb_score()),
        1..=max_count,
    )
    .prop_map(|tuples| {
        // Deduplicate by bead_id to avoid HashMap collisions in the evaluator
        let mut seen = HashSet::new();
        tuples
            .into_iter()
            .filter(|(b, _, _)| seen.insert(b.clone()))
            .collect()
    })
}

// ── SE-P01: Fidelity score is always in [0.0, 1.0], never NaN ────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p01_fidelity_in_range(
        assignments in arb_unique_assignments(10),
        emit_count in 0usize..10,
    ) {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(
            assignments.iter().enumerate().map(|(i, (b, a, s))| {
                make_assignment(b, a, *s, i + 1)
            }).collect(),
        );
        let mut log = make_log();
        // Emit dispatches for a subset of recommendations
        for (b, a, _) in assignments.iter().take(emit_count) {
            emit_dispatch(&mut log, 1, b, a);
        }
        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        prop_assert!(
            diff.fidelity_score >= 0.0 && diff.fidelity_score <= 1.0,
            "fidelity_score {} out of [0,1]", diff.fidelity_score
        );
        prop_assert!(
            !diff.fidelity_score.is_nan(),
            "fidelity_score must not be NaN"
        );
    }
}

// ── SE-P02: Dispatch rate is always in [0.0, 1.0], never NaN ─────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p02_dispatch_rate_in_range(
        assignments in arb_unique_assignments(10),
        emit_count in 0usize..10,
    ) {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(
            assignments.iter().enumerate().map(|(i, (b, a, s))| {
                make_assignment(b, a, *s, i + 1)
            }).collect(),
        );
        let mut log = make_log();
        for (b, a, _) in assignments.iter().take(emit_count) {
            emit_dispatch(&mut log, 1, b, a);
        }
        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        prop_assert!(
            diff.dispatch_rate >= 0.0 && diff.dispatch_rate <= 1.0,
            "dispatch_rate {} out of [0,1]", diff.dispatch_rate
        );
        prop_assert!(
            !diff.dispatch_rate.is_nan(),
            "dispatch_rate must not be NaN"
        );
    }
}

// ── SE-P03: Agent match rate is always in [0.0, 1.0], never NaN ──────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p03_agent_match_rate_in_range(
        assignments in arb_unique_assignments(10),
        emit_count in 0usize..10,
    ) {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(
            assignments.iter().enumerate().map(|(i, (b, a, s))| {
                make_assignment(b, a, *s, i + 1)
            }).collect(),
        );
        let mut log = make_log();
        for (b, a, _) in assignments.iter().take(emit_count) {
            emit_dispatch(&mut log, 1, b, a);
        }
        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        prop_assert!(
            diff.agent_match_rate >= 0.0 && diff.agent_match_rate <= 1.0,
            "agent_match_rate {} out of [0,1]", diff.agent_match_rate
        );
        prop_assert!(
            !diff.agent_match_rate.is_nan(),
            "agent_match_rate must not be NaN"
        );
    }
}

// ── SE-P04: Empty inputs yield fidelity=1.0 and is_healthy=true ──────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p04_empty_inputs_perfect(
        cycle_id in 1u64..1000,
        ts in 0i64..100_000,
    ) {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(Vec::new());
        let empty: Vec<MissionEvent> = Vec::new();

        let diff = eval.evaluate_cycle(cycle_id, ts, &recs, &empty);

        prop_assert!(
            (diff.fidelity_score - 1.0).abs() < 1e-10,
            "empty inputs should have fidelity=1.0, got {}", diff.fidelity_score
        );
        prop_assert!(
            diff.is_healthy(),
            "empty inputs should be healthy"
        );
    }
}

// ── SE-P05: Perfect match yields fidelity >= 0.95 ────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p05_perfect_match_high_fidelity(
        assignments in arb_unique_assignments(8),
    ) {
        prop_assume!(!assignments.is_empty());
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(
            assignments.iter().enumerate().map(|(i, (b, a, s))| {
                make_assignment(b, a, *s, i + 1)
            }).collect(),
        );
        let mut log = make_log();
        // Emit exactly the same bead+agent pairs
        for (b, a, _) in &assignments {
            emit_dispatch(&mut log, 1, b, a);
        }
        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        prop_assert!(
            diff.fidelity_score >= 0.95,
            "perfect match should have fidelity >= 0.95, got {}", diff.fidelity_score
        );
    }
}

// ── SE-P06: total_divergences = missing + unexpected + agent divergences ──────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p06_total_divergences_sum(
        assignments in arb_unique_assignments(6),
        emit_count in 0usize..8,
        extra_beads in 0usize..4,
    ) {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(
            assignments.iter().enumerate().map(|(i, (b, a, s))| {
                make_assignment(b, a, *s, i + 1)
            }).collect(),
        );
        let mut log = make_log();
        // Emit some of the recommendations
        for (b, a, _) in assignments.iter().take(emit_count) {
            emit_dispatch(&mut log, 1, b, a);
        }
        // Add extra unexpected dispatches
        for i in 0..extra_beads {
            emit_dispatch(&mut log, 1, &format!("extra-{}", i), "extra-agent");
        }
        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        let expected = diff.missing_executions.len()
            + diff.unexpected_executions.len()
            + diff.agent_divergences.len();
        prop_assert_eq!(
            diff.total_divergences(),
            expected,
            "total_divergences mismatch: got {}, expected {}", diff.total_divergences(), expected
        );
    }
}

// ── SE-P07: History length never exceeds max_history ─────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p07_history_bounded(
        max_history in 1usize..20,
        num_cycles in 1usize..40,
    ) {
        let config = ShadowEvaluationConfig {
            max_history,
            ..Default::default()
        };
        let mut eval = ShadowModeEvaluator::new(config);
        let recs = make_assignment_set(Vec::new());
        let empty: Vec<MissionEvent> = Vec::new();

        for i in 0..num_cycles {
            eval.evaluate_cycle(i as u64, (i * 1000) as i64, &recs, &empty);
            prop_assert!(
                eval.history().len() <= max_history,
                "history len {} exceeds max_history {} at cycle {}",
                eval.history().len(), max_history, i
            );
        }
    }
}

// ── SE-P08: total_cycles monotonically increases by 1 per call ───────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p08_total_cycles_monotonic(num_cycles in 1usize..25) {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(Vec::new());
        let empty: Vec<MissionEvent> = Vec::new();

        for i in 0..num_cycles {
            let before = eval.metrics().total_cycles;
            eval.evaluate_cycle(i as u64, (i * 1000) as i64, &recs, &empty);
            let after = eval.metrics().total_cycles;
            prop_assert_eq!(
                after,
                before + 1,
                "total_cycles should increment by 1 at cycle {}", i
            );
        }
    }
}

// ── SE-P09: is_warmed_up iff total_cycles >= warmup_cycles ───────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p09_warmup_iff_enough_cycles(
        warmup in 0usize..15,
        num_cycles in 0usize..20,
    ) {
        let config = ShadowEvaluationConfig {
            warmup_cycles: warmup,
            ..Default::default()
        };
        let mut eval = ShadowModeEvaluator::new(config);
        let recs = make_assignment_set(Vec::new());
        let empty: Vec<MissionEvent> = Vec::new();

        for i in 0..num_cycles {
            eval.evaluate_cycle(i as u64, (i * 1000) as i64, &recs, &empty);
        }

        let expected_warmed = num_cycles >= warmup;
        prop_assert_eq!(
            eval.is_warmed_up(),
            expected_warmed,
            "is_warmed_up should be {} after {} cycles with warmup={}",
            expected_warmed, num_cycles, warmup
        );
    }
}

// ── SE-P10: reset() clears history and resets metrics to 0 ───────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p10_reset_clears_state(num_cycles in 1usize..15) {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a1");

        for i in 0..num_cycles {
            eval.evaluate_cycle(i as u64, (i * 1000) as i64, &recs, log.events());
        }

        prop_assert!(eval.metrics().total_cycles > 0);
        prop_assert!(!eval.history().is_empty());

        eval.reset();

        prop_assert_eq!(eval.metrics().total_cycles, 0, "total_cycles should be 0 after reset");
        prop_assert!(eval.history().is_empty(), "history should be empty after reset");
        prop_assert_eq!(eval.metrics().total_recommendations, 0);
        prop_assert_eq!(eval.metrics().total_dispatches, 0);
    }
}

// ── SE-P11: ShadowModeDiff serde roundtrip preserves all fields ──────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p11_diff_serde_roundtrip(
        assignments in arb_unique_assignments(6),
        emit_count in 0usize..6,
    ) {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(
            assignments.iter().enumerate().map(|(i, (b, a, s))| {
                make_assignment(b, a, *s, i + 1)
            }).collect(),
        );
        let mut log = make_log();
        for (b, a, _) in assignments.iter().take(emit_count) {
            emit_dispatch(&mut log, 1, b, a);
        }
        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        let json = serde_json::to_string(&diff).unwrap();
        let restored: ShadowModeDiff = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.cycle_id, diff.cycle_id);
        prop_assert_eq!(restored.timestamp_ms, diff.timestamp_ms);
        prop_assert_eq!(restored.recommendations_count, diff.recommendations_count);
        prop_assert_eq!(restored.emissions_count, diff.emissions_count);
        prop_assert_eq!(restored.rejections_count, diff.rejections_count);
        prop_assert_eq!(restored.execution_rejections_count, diff.execution_rejections_count);
        prop_assert_eq!(restored.missing_executions.len(), diff.missing_executions.len());
        prop_assert_eq!(restored.unexpected_executions.len(), diff.unexpected_executions.len());
        prop_assert_eq!(restored.agent_divergences.len(), diff.agent_divergences.len());
        prop_assert_eq!(restored.safety_gate_rejections, diff.safety_gate_rejections);
        prop_assert_eq!(restored.conflicts_detected, diff.conflicts_detected);
        // f64 fields: use tolerance
        prop_assert!(
            (restored.fidelity_score - diff.fidelity_score).abs() < 1e-10,
            "fidelity_score mismatch: {} vs {}", restored.fidelity_score, diff.fidelity_score
        );
        prop_assert!(
            (restored.dispatch_rate - diff.dispatch_rate).abs() < 1e-10,
            "dispatch_rate mismatch: {} vs {}", restored.dispatch_rate, diff.dispatch_rate
        );
        prop_assert!(
            (restored.agent_match_rate - diff.agent_match_rate).abs() < 1e-10,
            "agent_match_rate mismatch: {} vs {}", restored.agent_match_rate, diff.agent_match_rate
        );
    }
}

// ── SE-P12: ShadowModeMetrics serde roundtrip ────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p12_metrics_serde_roundtrip(num_cycles in 1usize..10) {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(vec![make_assignment("b1", "a1", 0.9, 1)]);
        let mut log = make_log();
        emit_dispatch(&mut log, 1, "b1", "a1");

        for i in 0..num_cycles {
            eval.evaluate_cycle(i as u64, (i * 1000) as i64, &recs, log.events());
        }

        let json = serde_json::to_string(eval.metrics()).unwrap();
        let restored: ShadowModeMetrics = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.total_cycles, eval.metrics().total_cycles);
        prop_assert_eq!(restored.total_recommendations, eval.metrics().total_recommendations);
        prop_assert_eq!(restored.total_dispatches, eval.metrics().total_dispatches);
        prop_assert_eq!(restored.low_fidelity_count, eval.metrics().low_fidelity_count);
        prop_assert_eq!(restored.max_consecutive_low_fidelity, eval.metrics().max_consecutive_low_fidelity);
        prop_assert!(
            (restored.mean_dispatch_rate - eval.metrics().mean_dispatch_rate).abs() < 1e-10,
            "mean_dispatch_rate mismatch: {} vs {}",
            restored.mean_dispatch_rate, eval.metrics().mean_dispatch_rate
        );
        prop_assert!(
            (restored.mean_fidelity_score - eval.metrics().mean_fidelity_score).abs() < 1e-10,
            "mean_fidelity_score mismatch: {} vs {}",
            restored.mean_fidelity_score, eval.metrics().mean_fidelity_score
        );
    }
}

// ── SE-P13: ShadowEvaluationConfig serde roundtrip ───────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p13_config_serde_roundtrip(
        max_history in 1usize..1000,
        low_thresh in 0.0f64..1.0,
        track_agent in proptest::bool::ANY,
        track_score in proptest::bool::ANY,
        warmup in 0usize..50,
    ) {
        let config = ShadowEvaluationConfig {
            max_history,
            low_confidence_threshold: low_thresh,
            track_agent_divergence: track_agent,
            track_score_accuracy: track_score,
            warmup_cycles: warmup,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: ShadowEvaluationConfig = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.max_history, config.max_history);
        prop_assert_eq!(restored.track_agent_divergence, config.track_agent_divergence);
        prop_assert_eq!(restored.track_score_accuracy, config.track_score_accuracy);
        prop_assert_eq!(restored.warmup_cycles, config.warmup_cycles);
        prop_assert!(
            (restored.low_confidence_threshold - config.low_confidence_threshold).abs() < 1e-10,
            "low_confidence_threshold mismatch: {} vs {}",
            restored.low_confidence_threshold, config.low_confidence_threshold
        );
    }
}

// ── SE-P14: dispatch_rate = emissions_count / recommendations_count ───────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p14_dispatch_rate_formula(
        assignments in arb_unique_assignments(8),
        emit_count in 0usize..8,
    ) {
        prop_assume!(!assignments.is_empty());
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(
            assignments.iter().enumerate().map(|(i, (b, a, s))| {
                make_assignment(b, a, *s, i + 1)
            }).collect(),
        );
        let mut log = make_log();
        let actual_emit = emit_count.min(assignments.len());
        for (b, a, _) in assignments.iter().take(actual_emit) {
            emit_dispatch(&mut log, 1, b, a);
        }
        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        let expected_rate = actual_emit as f64 / assignments.len() as f64;
        prop_assert!(
            (diff.dispatch_rate - expected_rate).abs() < 1e-10,
            "dispatch_rate mismatch: got {}, expected {}", diff.dispatch_rate, expected_rate
        );
    }
}

// ── SE-P15: Deterministic — same inputs produce same outputs ─────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p15_deterministic(
        assignments in arb_unique_assignments(6),
        emit_count in 0usize..6,
    ) {
        let recs = make_assignment_set(
            assignments.iter().enumerate().map(|(i, (b, a, s))| {
                make_assignment(b, a, *s, i + 1)
            }).collect(),
        );
        let mut log = make_log();
        for (b, a, _) in assignments.iter().take(emit_count) {
            emit_dispatch(&mut log, 1, b, a);
        }
        let events = log.events().to_vec();

        let mut eval1 = ShadowModeEvaluator::with_defaults();
        let diff1 = eval1.evaluate_cycle(1, 1000, &recs, &events);

        let mut eval2 = ShadowModeEvaluator::with_defaults();
        let diff2 = eval2.evaluate_cycle(1, 1000, &recs, &events);

        prop_assert_eq!(diff1.recommendations_count, diff2.recommendations_count);
        prop_assert_eq!(diff1.emissions_count, diff2.emissions_count);
        prop_assert_eq!(diff1.missing_executions.len(), diff2.missing_executions.len());
        prop_assert_eq!(diff1.unexpected_executions.len(), diff2.unexpected_executions.len());
        prop_assert_eq!(diff1.agent_divergences.len(), diff2.agent_divergences.len());
        prop_assert!(
            (diff1.fidelity_score - diff2.fidelity_score).abs() < 1e-10,
            "fidelity_score not deterministic: {} vs {}", diff1.fidelity_score, diff2.fidelity_score
        );
        prop_assert!(
            (diff1.dispatch_rate - diff2.dispatch_rate).abs() < 1e-10,
            "dispatch_rate not deterministic: {} vs {}", diff1.dispatch_rate, diff2.dispatch_rate
        );
        prop_assert!(
            (diff1.agent_match_rate - diff2.agent_match_rate).abs() < 1e-10,
            "agent_match_rate not deterministic: {} vs {}", diff1.agent_match_rate, diff2.agent_match_rate
        );
    }
}

// ── SE-P16: Low-fidelity streak incremented/reset correctly ──────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p16_low_fidelity_streak(
        low_count in 0usize..8,
        high_count in 0usize..5,
        final_low in 0usize..5,
    ) {
        let config = ShadowEvaluationConfig {
            warmup_cycles: 0,
            ..Default::default()
        };
        let mut eval = ShadowModeEvaluator::new(config);

        // Generate low-fidelity cycles: no recommendations but unexpected dispatches
        let empty_recs = make_assignment_set(Vec::new());
        let mut bad_log = make_log();
        emit_dispatch(&mut bad_log, 1, "unexpected", "unknown-agent");
        let bad_events = bad_log.events().to_vec();

        // Phase 1: low-fidelity cycles
        for i in 0..low_count {
            eval.evaluate_cycle(i as u64, (i * 1000) as i64, &empty_recs, &bad_events);
        }

        // Phase 2: high-fidelity cycles (empty, fidelity=1.0)
        let good_events: Vec<MissionEvent> = Vec::new();
        for i in 0..high_count {
            let cycle = (low_count + i) as u64;
            eval.evaluate_cycle(cycle, (cycle * 1000) as i64, &empty_recs, &good_events);
        }

        // Phase 3: more low-fidelity cycles
        for i in 0..final_low {
            let cycle = (low_count + high_count + i) as u64;
            eval.evaluate_cycle(cycle, (cycle * 1000) as i64, &empty_recs, &bad_events);
        }

        let metrics = eval.metrics();

        // Total low-fidelity count should equal all bad cycles
        prop_assert_eq!(
            metrics.low_fidelity_count,
            (low_count + final_low) as u64,
            "low_fidelity_count mismatch: expected {}, got {}",
            low_count + final_low, metrics.low_fidelity_count
        );

        // Max streak should be the longer of the two bad runs
        let expected_max = if high_count > 0 || low_count == 0 {
            low_count.max(final_low) as u64
        } else {
            // No high cycles to break the streak, so it's continuous
            (low_count + final_low) as u64
        };
        prop_assert_eq!(
            metrics.max_consecutive_low_fidelity,
            expected_max,
            "max streak mismatch: expected {}, got {}",
            expected_max, metrics.max_consecutive_low_fidelity
        );
    }
}

// ── SE-P17: track_agent_divergence=false → agent_divergences always empty ────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p17_no_agent_divergence_tracking(
        assignments in arb_unique_assignments(6),
    ) {
        prop_assume!(!assignments.is_empty());
        let config = ShadowEvaluationConfig {
            track_agent_divergence: false,
            ..Default::default()
        };
        let mut eval = ShadowModeEvaluator::new(config);
        let recs = make_assignment_set(
            assignments.iter().enumerate().map(|(i, (b, _a, s))| {
                make_assignment(b, "original-agent", *s, i + 1)
            }).collect(),
        );
        // Dispatch all beads but to different agents
        let mut log = make_log();
        for (b, _, _) in &assignments {
            emit_dispatch(&mut log, 1, b, "different-agent");
        }
        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        prop_assert!(
            diff.agent_divergences.is_empty(),
            "agent_divergences should be empty when tracking disabled, got {}",
            diff.agent_divergences.len()
        );
    }
}

// ── SE-P18: track_score_accuracy=false → score_accuracy always empty ─────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p18_no_score_accuracy_tracking(
        assignments in arb_unique_assignments(6),
    ) {
        prop_assume!(!assignments.is_empty());
        let config = ShadowEvaluationConfig {
            track_score_accuracy: false,
            ..Default::default()
        };
        let mut eval = ShadowModeEvaluator::new(config);
        let recs = make_assignment_set(
            assignments.iter().enumerate().map(|(i, (b, a, s))| {
                make_assignment(b, a, *s, i + 1)
            }).collect(),
        );
        let mut log = make_log();
        for (b, a, _) in &assignments {
            emit_dispatch(&mut log, 1, b, a);
        }
        let diff = eval.evaluate_cycle(1, 1000, &recs, log.events());

        prop_assert!(
            diff.score_accuracy.is_empty(),
            "score_accuracy should be empty when tracking disabled, got {}",
            diff.score_accuracy.len()
        );
    }
}

// ── SE-P19: All recommendations missing → dispatch_rate = 0.0 ────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p19_all_missing_zero_dispatch(
        assignments in arb_unique_assignments(8),
    ) {
        prop_assume!(!assignments.is_empty());
        let mut eval = ShadowModeEvaluator::with_defaults();
        let recs = make_assignment_set(
            assignments.iter().enumerate().map(|(i, (b, a, s))| {
                make_assignment(b, a, *s, i + 1)
            }).collect(),
        );
        // No emissions at all
        let empty: Vec<MissionEvent> = Vec::new();
        let diff = eval.evaluate_cycle(1, 1000, &recs, &empty);

        prop_assert!(
            (diff.dispatch_rate - 0.0).abs() < 1e-10,
            "dispatch_rate should be 0.0 when all missing, got {}", diff.dispatch_rate
        );
    }
}

// ── SE-P20: Multiple cycles accumulate total_recommendations ─────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn se_p20_total_recommendations_accumulates(
        cycle_sizes in proptest::collection::vec(1usize..8, 1..10),
    ) {
        let mut eval = ShadowModeEvaluator::with_defaults();
        let empty: Vec<MissionEvent> = Vec::new();
        let mut expected_total: u64 = 0;

        for (cycle_idx, &size) in cycle_sizes.iter().enumerate() {
            let assignments: Vec<PlannerAssignment> = (0..size)
                .map(|j| make_assignment(
                    &format!("b{}-{}", cycle_idx, j),
                    &format!("a{}", j),
                    0.5,
                    j + 1,
                ))
                .collect();
            let recs = make_assignment_set(assignments);
            expected_total += size as u64;

            eval.evaluate_cycle(cycle_idx as u64, (cycle_idx * 1000) as i64, &recs, &empty);
        }

        prop_assert_eq!(
            eval.metrics().total_recommendations,
            expected_total,
            "total_recommendations mismatch: expected {}, got {}",
            expected_total, eval.metrics().total_recommendations
        );
        prop_assert_eq!(
            eval.metrics().total_cycles,
            cycle_sizes.len() as u64,
            "total_cycles mismatch: expected {}, got {}",
            cycle_sizes.len(), eval.metrics().total_cycles
        );
    }
}
