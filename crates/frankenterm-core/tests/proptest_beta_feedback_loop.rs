//! Property-based tests for beta_feedback_loop types.
//!
//! Covers serde roundtrip for all serializable types, enum variant
//! properties, config defaults, stage ordering, and snapshot consistency.

use frankenterm_core::beta_feedback_loop::{
    AnomalySeverity, AnomalyStatus, BetaAnomaly, BetaCohort, BetaLoopConfig, BetaLoopController,
    BetaLoopSnapshot, BetaStage, CohortEvaluation, DecisionReason, FeedbackCategory,
    FeedbackSeverity, PromotionDecision, QualitativeFeedback, SmoothnessObservation,
    StageEvaluation,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_beta_stage() -> impl Strategy<Value = BetaStage> {
    prop_oneof![
        Just(BetaStage::Baseline),
        Just(BetaStage::InternalBeta),
        Just(BetaStage::ClosedBeta),
        Just(BetaStage::OpenBeta),
        Just(BetaStage::GeneralAvailability),
    ]
}

fn arb_feedback_category() -> impl Strategy<Value = FeedbackCategory> {
    prop_oneof![
        Just(FeedbackCategory::Smoothness),
        Just(FeedbackCategory::VisualGlitch),
        Just(FeedbackCategory::Confusion),
        Just(FeedbackCategory::WorkflowDisruption),
        Just(FeedbackCategory::PerformanceRegression),
        Just(FeedbackCategory::Positive),
        Just(FeedbackCategory::General),
    ]
}

fn arb_feedback_severity() -> impl Strategy<Value = FeedbackSeverity> {
    prop_oneof![
        Just(FeedbackSeverity::Info),
        Just(FeedbackSeverity::Minor),
        Just(FeedbackSeverity::Moderate),
        Just(FeedbackSeverity::Critical),
    ]
}

fn arb_promotion_decision() -> impl Strategy<Value = PromotionDecision> {
    prop_oneof![
        Just(PromotionDecision::Promote),
        Just(PromotionDecision::Hold),
        Just(PromotionDecision::Rollback),
    ]
}

fn arb_anomaly_severity() -> impl Strategy<Value = AnomalySeverity> {
    prop_oneof![
        Just(AnomalySeverity::Low),
        Just(AnomalySeverity::Medium),
        Just(AnomalySeverity::High),
        Just(AnomalySeverity::Critical),
    ]
}

fn arb_anomaly_status() -> impl Strategy<Value = AnomalyStatus> {
    prop_oneof![
        Just(AnomalyStatus::Open),
        Just(AnomalyStatus::Investigating),
        Just(AnomalyStatus::Mitigated),
        Just(AnomalyStatus::Closed),
    ]
}

fn arb_beta_cohort() -> impl Strategy<Value = BetaCohort> {
    (
        "[a-z]{3,10}",
        arb_beta_stage(),
        prop::collection::vec("[a-z0-9]{2,8}", 0..5),
    )
        .prop_map(|(name, stage, members)| {
            let mut c = BetaCohort::new(name, stage);
            for m in members {
                c.add_member(m);
            }
            c
        })
}

fn arb_qualitative_feedback() -> impl Strategy<Value = QualitativeFeedback> {
    (
        "[a-z]{3,8}",
        arb_feedback_category(),
        arb_feedback_severity(),
        "[a-z ]{5,30}",
        any::<u64>(),
        -100i32..=100,
    )
        .prop_map(
            |(member_id, category, severity, description, timestamp_ms, nps_score)| {
                QualitativeFeedback {
                    member_id,
                    category,
                    severity,
                    description,
                    timestamp_ms,
                    nps_score,
                }
            },
        )
}

fn arb_smoothness_observation() -> impl Strategy<Value = SmoothnessObservation> {
    (
        "[a-z]{3,8}",
        0.0f64..=1.0,
        proptest::option::of(any::<u64>()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(any::<u64>()),
        any::<u64>(),
    )
        .prop_map(
            |(
                member_id,
                smoothness,
                input_to_paint_us,
                frame_jitter_us,
                keystroke_echo_us,
                timestamp_ms,
            )| {
                SmoothnessObservation {
                    member_id,
                    smoothness,
                    input_to_paint_us,
                    frame_jitter_us,
                    keystroke_echo_us,
                    timestamp_ms,
                }
            },
        )
}

fn arb_beta_loop_config() -> impl Strategy<Value = BetaLoopConfig> {
    (
        0.5f64..=1.0,
        0.1f64..=1.0,
        1usize..100,
        1usize..100,
        -50i32..=50,
        -100i32..=-10,
        1usize..10,
        1.0f64..=5.0,
        100usize..50_000,
        100usize..200_000,
    )
        .prop_map(
            |(
                smoothness_target,
                smoothness_percentile,
                min_observations_per_cohort,
                min_feedback_per_cohort,
                promotion_nps_threshold,
                rollback_nps_threshold,
                max_critical_friction,
                rollback_budget_multiplier,
                max_feedback_per_cohort,
                max_observations_per_cohort,
            )| {
                BetaLoopConfig {
                    smoothness_target,
                    smoothness_percentile,
                    min_observations_per_cohort,
                    min_feedback_per_cohort,
                    promotion_nps_threshold,
                    rollback_nps_threshold,
                    max_critical_friction,
                    rollback_budget_multiplier,
                    max_feedback_per_cohort,
                    max_observations_per_cohort,
                }
            },
        )
}

fn arb_decision_reason() -> impl Strategy<Value = DecisionReason> {
    ("[a-z_]{3,15}", "[a-z ]{5,30}")
        .prop_map(|(code, explanation)| DecisionReason { code, explanation })
}

fn arb_beta_anomaly() -> impl Strategy<Value = BetaAnomaly> {
    (
        (
            "[a-z0-9\\-]{3,20}",
            "A[1-9]",
            "[a-z ]{5,30}",
            arb_anomaly_severity(),
            arb_anomaly_status(),
            arb_promotion_decision(),
            "[a-z\\-]{3,20}",
            "[a-z\\-]{3,20}",
        ),
        (
            any::<u64>(),
            any::<u64>(),
            "[a-z ]{5,60}",
            prop::collection::vec("[a-z0-9\\-]{3,20}", 0..4),
            prop::collection::vec("[a-z0-9_./\\-]{3,50}", 0..4),
            "[a-z_]{3,30}",
            prop::collection::vec("[a-z0-9_./\\-]{3,50}", 0..4),
            prop::collection::vec("ft-[a-z0-9.\\-]{3,20}", 0..4),
        ),
    )
        .prop_map(
            |(
                (
                    anomaly_id,
                    category_code,
                    title,
                    severity,
                    status,
                    blocking_decision,
                    triage_owner,
                    remediation_owner,
                ),
                (
                    opened_at_ms,
                    last_updated_at_ms,
                    summary,
                    linked_feedback_ids,
                    linked_artifacts,
                    close_loop_status,
                    close_loop_evidence,
                    tracking_issue_ids,
                ),
            )| BetaAnomaly {
                anomaly_id,
                category_code,
                title,
                severity,
                status,
                blocking_decision,
                triage_owner,
                remediation_owner,
                opened_at_ms,
                last_updated_at_ms,
                summary,
                linked_feedback_ids,
                linked_artifacts,
                close_loop_status,
                close_loop_evidence,
                tracking_issue_ids,
            },
        )
}

fn arb_cohort_evaluation() -> impl Strategy<Value = CohortEvaluation> {
    (
        "[a-z]{3,10}",
        0usize..1000,
        0usize..1000,
        proptest::option::of(0.0f64..=1.0),
        proptest::option::of(-100.0f64..=100.0),
        0usize..20,
        any::<bool>(),
    )
        .prop_map(
            |(
                cohort_name,
                observation_count,
                feedback_count,
                smoothness_at_percentile,
                mean_nps,
                critical_friction_count,
                meets_criteria,
            )| {
                CohortEvaluation {
                    cohort_name,
                    observation_count,
                    feedback_count,
                    smoothness_at_percentile,
                    mean_nps,
                    critical_friction_count,
                    meets_criteria,
                }
            },
        )
}

fn arb_stage_evaluation() -> impl Strategy<Value = StageEvaluation> {
    (
        arb_beta_stage(),
        arb_promotion_decision(),
        prop::collection::vec(arb_decision_reason(), 0..4),
        prop::collection::vec(arb_cohort_evaluation(), 0..3),
        any::<u64>(),
    )
        .prop_map(
            |(stage, decision, reasons, cohort_evaluations, evaluated_at_ms)| StageEvaluation {
                stage,
                decision,
                reasons,
                cohort_evaluations,
                evaluated_at_ms,
            },
        )
}

fn arb_beta_loop_snapshot() -> impl Strategy<Value = BetaLoopSnapshot> {
    (
        arb_beta_stage(),
        any::<u64>(),
        any::<u64>(),
        any::<u32>(),
        any::<u32>(),
        proptest::option::of(arb_promotion_decision()),
        prop::collection::hash_map("[a-z]{3,8}", any::<u64>(), 0..4),
        prop::collection::hash_map("[a-z]{3,8}", any::<u64>(), 0..4),
        any::<u64>(),
        prop::collection::vec(arb_beta_anomaly(), 0..4),
    )
        .prop_map(
            |(
                stage,
                total_feedback,
                total_observations,
                transition_count,
                evaluation_count,
                last_decision,
                cohort_observation_counts,
                cohort_feedback_counts,
                active_anomaly_count,
                anomalies,
            )| {
                BetaLoopSnapshot {
                    stage,
                    total_feedback,
                    total_observations,
                    transition_count,
                    evaluation_count,
                    last_decision,
                    cohort_observation_counts,
                    cohort_feedback_counts,
                    active_anomaly_count,
                    anomalies,
                }
            },
        )
}

// ── Serde Roundtrip Tests ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn proptest_beta_stage_serde_roundtrip(stage in arb_beta_stage()) {
        let json = serde_json::to_string(&stage).unwrap();
        let parsed: BetaStage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stage, parsed);
    }

    #[test]
    fn proptest_feedback_category_serde_roundtrip(cat in arb_feedback_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let parsed: FeedbackCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, parsed);
    }

    #[test]
    fn proptest_feedback_severity_serde_roundtrip(sev in arb_feedback_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let parsed: FeedbackSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, parsed);
    }

    #[test]
    fn proptest_promotion_decision_serde_roundtrip(dec in arb_promotion_decision()) {
        let json = serde_json::to_string(&dec).unwrap();
        let parsed: PromotionDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dec, parsed);
    }

    #[test]
    fn proptest_anomaly_severity_serde_roundtrip(sev in arb_anomaly_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let parsed: AnomalySeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, parsed);
    }

    #[test]
    fn proptest_anomaly_status_serde_roundtrip(status in arb_anomaly_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let parsed: AnomalyStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, parsed);
    }

    #[test]
    fn proptest_beta_cohort_serde_roundtrip(cohort in arb_beta_cohort()) {
        let json = serde_json::to_string(&cohort).unwrap();
        let parsed: BetaCohort = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cohort.name, parsed.name);
        prop_assert_eq!(cohort.activation_stage, parsed.activation_stage);
        prop_assert_eq!(cohort.members, parsed.members);
    }

    #[test]
    fn proptest_qualitative_feedback_serde_roundtrip(fb in arb_qualitative_feedback()) {
        let json = serde_json::to_string(&fb).unwrap();
        let parsed: QualitativeFeedback = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(fb.member_id, parsed.member_id);
        prop_assert_eq!(fb.category, parsed.category);
        prop_assert_eq!(fb.severity, parsed.severity);
        prop_assert_eq!(fb.description, parsed.description);
        prop_assert_eq!(fb.timestamp_ms, parsed.timestamp_ms);
        prop_assert_eq!(fb.nps_score, parsed.nps_score);
    }

    #[test]
    fn proptest_smoothness_observation_serde_roundtrip(obs in arb_smoothness_observation()) {
        let json = serde_json::to_string(&obs).unwrap();
        let parsed: SmoothnessObservation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(obs.member_id, parsed.member_id);
        prop_assert_eq!(obs.input_to_paint_us, parsed.input_to_paint_us);
        prop_assert_eq!(obs.frame_jitter_us, parsed.frame_jitter_us);
        prop_assert_eq!(obs.keystroke_echo_us, parsed.keystroke_echo_us);
        prop_assert_eq!(obs.timestamp_ms, parsed.timestamp_ms);
        // f64 smoothness — check approximate equality
        let diff = (obs.smoothness - parsed.smoothness).abs();
        prop_assert!(diff < 1e-10, "smoothness diverged: {} vs {}", obs.smoothness, parsed.smoothness);
    }

    #[test]
    fn proptest_beta_loop_config_serde_roundtrip(cfg in arb_beta_loop_config()) {
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: BetaLoopConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cfg.min_observations_per_cohort, parsed.min_observations_per_cohort);
        prop_assert_eq!(cfg.min_feedback_per_cohort, parsed.min_feedback_per_cohort);
        prop_assert_eq!(cfg.promotion_nps_threshold, parsed.promotion_nps_threshold);
        prop_assert_eq!(cfg.rollback_nps_threshold, parsed.rollback_nps_threshold);
        prop_assert_eq!(cfg.max_critical_friction, parsed.max_critical_friction);
        prop_assert_eq!(cfg.max_feedback_per_cohort, parsed.max_feedback_per_cohort);
        prop_assert_eq!(cfg.max_observations_per_cohort, parsed.max_observations_per_cohort);
        let st_diff = (cfg.smoothness_target - parsed.smoothness_target).abs();
        prop_assert!(st_diff < 1e-10);
    }

    #[test]
    fn proptest_decision_reason_serde_roundtrip(r in arb_decision_reason()) {
        let json = serde_json::to_string(&r).unwrap();
        let parsed: DecisionReason = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r.code, parsed.code);
        prop_assert_eq!(r.explanation, parsed.explanation);
    }

    #[test]
    fn proptest_beta_anomaly_serde_roundtrip(anomaly in arb_beta_anomaly()) {
        let json = serde_json::to_string(&anomaly).unwrap();
        let parsed: BetaAnomaly = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(anomaly, parsed);
    }

    #[test]
    fn proptest_cohort_evaluation_serde_roundtrip(ce in arb_cohort_evaluation()) {
        let json = serde_json::to_string(&ce).unwrap();
        let parsed: CohortEvaluation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ce.cohort_name, parsed.cohort_name);
        prop_assert_eq!(ce.observation_count, parsed.observation_count);
        prop_assert_eq!(ce.feedback_count, parsed.feedback_count);
        prop_assert_eq!(ce.critical_friction_count, parsed.critical_friction_count);
        prop_assert_eq!(ce.meets_criteria, parsed.meets_criteria);
    }

    #[test]
    fn proptest_stage_evaluation_serde_roundtrip(se in arb_stage_evaluation()) {
        let json = serde_json::to_string(&se).unwrap();
        let parsed: StageEvaluation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(se.stage, parsed.stage);
        prop_assert_eq!(se.decision, parsed.decision);
        prop_assert_eq!(se.reasons.len(), parsed.reasons.len());
        prop_assert_eq!(se.cohort_evaluations.len(), parsed.cohort_evaluations.len());
        prop_assert_eq!(se.evaluated_at_ms, parsed.evaluated_at_ms);
    }

    #[test]
    fn proptest_beta_loop_snapshot_serde_roundtrip(snap in arb_beta_loop_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: BetaLoopSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, parsed);
    }
}

// ── Enum Property Tests ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn proptest_beta_stage_rank_is_monotonic(a in arb_beta_stage(), b in arb_beta_stage()) {
        match a.cmp(&b) {
            std::cmp::Ordering::Less => prop_assert!(a.rank() < b.rank()),
            std::cmp::Ordering::Greater => prop_assert!(a.rank() > b.rank()),
            std::cmp::Ordering::Equal => prop_assert_eq!(a.rank(), b.rank()),
        }
    }

    #[test]
    fn proptest_beta_stage_next_advances_rank(stage in arb_beta_stage()) {
        if let Some(next) = stage.next() {
            prop_assert!(next.rank() > stage.rank());
            prop_assert_eq!(next.rank(), stage.rank() + 1);
        } else {
            // GeneralAvailability has no next
            prop_assert_eq!(stage, BetaStage::GeneralAvailability);
        }
    }

    #[test]
    fn proptest_beta_stage_prev_decreases_rank(stage in arb_beta_stage()) {
        if let Some(prev) = stage.prev() {
            prop_assert!(prev.rank() < stage.rank());
            prop_assert_eq!(prev.rank() + 1, stage.rank());
        } else {
            prop_assert_eq!(stage, BetaStage::Baseline);
        }
    }

    #[test]
    fn proptest_beta_stage_next_then_prev_roundtrip(stage in arb_beta_stage()) {
        if let Some(next) = stage.next() {
            prop_assert_eq!(next.prev(), Some(stage));
        }
    }

    #[test]
    fn proptest_feedback_severity_ordering(a in arb_feedback_severity(), b in arb_feedback_severity()) {
        // Verify Ord is consistent with variant declaration order
        let rank = |s: FeedbackSeverity| match s {
            FeedbackSeverity::Info => 0,
            FeedbackSeverity::Minor => 1,
            FeedbackSeverity::Moderate => 2,
            FeedbackSeverity::Critical => 3,
        };
        prop_assert_eq!(a.cmp(&b), rank(a).cmp(&rank(b)));
    }

    #[test]
    fn proptest_cohort_active_at_monotonic(cohort in arb_beta_cohort(), stage in arb_beta_stage()) {
        // If active at a stage, must be active at all higher stages
        if cohort.is_active_at(stage) {
            if let Some(next) = stage.next() {
                prop_assert!(cohort.is_active_at(next));
            }
        }
    }

    #[test]
    fn proptest_cohort_member_count_matches(cohort in arb_beta_cohort()) {
        prop_assert_eq!(cohort.member_count(), cohort.members.len());
    }
}

// ── Config Default Tests ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn proptest_config_default_serde_roundtrip(_seed in any::<u32>()) {
        let cfg = BetaLoopConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: BetaLoopConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cfg.min_observations_per_cohort, parsed.min_observations_per_cohort);
        prop_assert_eq!(cfg.promotion_nps_threshold, parsed.promotion_nps_threshold);
    }
}

// ── Controller Integration Tests ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn proptest_controller_snapshot_roundtrip(_seed in any::<u32>()) {
        let config = BetaLoopConfig::default();
        let mut cohorts = vec![BetaCohort::new("test", BetaStage::Baseline)];
        cohorts[0].add_member("agent-1");
        let ctrl = BetaLoopController::new(config, cohorts);
        let snap = ctrl.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: BetaLoopSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, parsed);
    }

    #[test]
    fn proptest_controller_initial_stage_is_baseline(_seed in any::<u32>()) {
        let config = BetaLoopConfig::default();
        let cohorts = vec![BetaCohort::new("test", BetaStage::Baseline)];
        let ctrl = BetaLoopController::new(config, cohorts);
        prop_assert_eq!(ctrl.stage(), BetaStage::Baseline);
        prop_assert_eq!(ctrl.snapshot().stage, BetaStage::Baseline);
    }
}
