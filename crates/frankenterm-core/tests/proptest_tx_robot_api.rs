//! Property-based tests for transactional execution robot types (ft-1i2ge.8.8).

use proptest::prelude::*;

use frankenterm_core::robot_types::{
    RobotResponse, TxBundleClassification, TxChainVerificationData, TxCompensatingActionData,
    TxCompensationKind, TxPhaseState, TxPlanData, TxPreconditionData, TxPreconditionKind,
    TxResumeData, TxResumeRecommendation, TxRiskSummaryData, TxRollbackData,
    TxRunData, TxShowData, TxStepData, TxStepOutcome, TxStepRecordData, TxStepRisk,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_step_risk() -> impl Strategy<Value = TxStepRisk> {
    prop_oneof![
        Just(TxStepRisk::Low),
        Just(TxStepRisk::Medium),
        Just(TxStepRisk::High),
        Just(TxStepRisk::Critical),
    ]
}

fn arb_phase() -> impl Strategy<Value = TxPhaseState> {
    prop_oneof![
        Just(TxPhaseState::Planned),
        Just(TxPhaseState::Preparing),
        Just(TxPhaseState::Committing),
        Just(TxPhaseState::Compensating),
        Just(TxPhaseState::Completed),
        Just(TxPhaseState::Aborted),
    ]
}

fn arb_step_outcome() -> impl Strategy<Value = TxStepOutcome> {
    prop_oneof![
        proptest::option::of("[a-z ]{3,15}").prop_map(|r| TxStepOutcome::Success { result: r }),
        ("[A-Z]{2}-[0-9]{4}", "[a-z ]{5,20}", any::<bool>()).prop_map(|(ec, em, comp)| {
            TxStepOutcome::Failed {
                error_code: ec,
                error_message: em,
                compensated: comp,
            }
        }),
        "[a-z ]{5,20}".prop_map(|r| TxStepOutcome::Skipped { reason: r }),
        "[a-z ]{5,20}".prop_map(|r| TxStepOutcome::Compensated {
            compensation_result: r,
        }),
        Just(TxStepOutcome::Pending),
    ]
}

fn arb_resume_rec() -> impl Strategy<Value = TxResumeRecommendation> {
    prop_oneof![
        Just(TxResumeRecommendation::ContinueFromCheckpoint),
        Just(TxResumeRecommendation::RestartFresh),
        Just(TxResumeRecommendation::CompensateAndAbort),
        Just(TxResumeRecommendation::AlreadyComplete),
    ]
}

fn arb_classification() -> impl Strategy<Value = TxBundleClassification> {
    prop_oneof![
        Just(TxBundleClassification::Internal),
        Just(TxBundleClassification::TeamReview),
        Just(TxBundleClassification::ExternalAudit),
    ]
}

fn arb_precondition_kind() -> impl Strategy<Value = TxPreconditionKind> {
    prop_oneof![
        Just(TxPreconditionKind::PolicyApproved),
        proptest::collection::vec("[a-z/_.]{3,15}", 1..3)
            .prop_map(|paths| TxPreconditionKind::ReservationHeld { paths }),
        "[a-z]{3,10}".prop_map(|a| TxPreconditionKind::ApprovalRequired { approver: a }),
        "[a-z0-9-]{3,10}".prop_map(|t| TxPreconditionKind::TargetReachable { target_id: t }),
        (1000..60000u64).prop_map(|m| TxPreconditionKind::ContextFresh { max_age_ms: m }),
    ]
}

fn arb_compensation_kind() -> impl Strategy<Value = TxCompensationKind> {
    prop_oneof![
        Just(TxCompensationKind::Rollback),
        Just(TxCompensationKind::NotifyOperator),
        (1..10u32).prop_map(|m| TxCompensationKind::RetryWithBackoff { max_retries: m }),
        Just(TxCompensationKind::SkipAndContinue),
        "[a-z0-9-]{3,10}".prop_map(|s| TxCompensationKind::Alternative {
            alternative_step_id: s,
        }),
    ]
}

fn arb_chain_verification() -> impl Strategy<Value = TxChainVerificationData> {
    (
        any::<bool>(),
        proptest::option::of(0..100u64),
        proptest::collection::vec(0..100u64, 0..3),
        0..50usize,
    )
        .prop_map(|(intact, first_break, missing, total)| TxChainVerificationData {
            chain_intact: intact,
            first_break_at: first_break,
            missing_ordinals: missing,
            total_records: total,
        })
}

fn arb_step_record() -> impl Strategy<Value = TxStepRecordData> {
    (
        0..100u64,
        "[a-z0-9]{2,8}",
        "[a-z0-9:]{5,15}",
        "[a-z0-9-]{5,15}",
        any::<u64>(),
        arb_step_outcome(),
        arb_step_risk(),
        "[a-z0-9]{5,15}",
        "[a-z0-9-]{3,10}",
    )
        .prop_map(
            |(ordinal, step_id, idem_key, exec_id, ts, outcome, risk, prev_hash, agent_id)| {
                TxStepRecordData {
                    ordinal,
                    step_id,
                    idem_key,
                    execution_id: exec_id,
                    timestamp_ms: ts,
                    outcome,
                    risk,
                    prev_hash,
                    agent_id,
                }
            },
        )
}

// ---------------------------------------------------------------------------
// TRA-1: TxStepRisk serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn tra_1_step_risk_serde(risk in arb_step_risk()) {
        let json = serde_json::to_string(&risk).unwrap();
        let back: TxStepRisk = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, risk);
    }
}

// ---------------------------------------------------------------------------
// TRA-2: TxPhaseState serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn tra_2_phase_serde(phase in arb_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let back: TxPhaseState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, phase);
    }
}

// ---------------------------------------------------------------------------
// TRA-3: TxStepOutcome tagged serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn tra_3_step_outcome_serde(outcome in arb_step_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: TxStepOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, outcome);
    }
}

// ---------------------------------------------------------------------------
// TRA-4: TxPreconditionKind tagged serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn tra_4_precondition_kind_serde(kind in arb_precondition_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: TxPreconditionKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, kind);
    }
}

// ---------------------------------------------------------------------------
// TRA-5: TxCompensationKind tagged serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn tra_5_compensation_kind_serde(kind in arb_compensation_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: TxCompensationKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, kind);
    }
}

// ---------------------------------------------------------------------------
// TRA-6: TxResumeRecommendation serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn tra_6_resume_rec_serde(rec in arb_resume_rec()) {
        let json = serde_json::to_string(&rec).unwrap();
        let back: TxResumeRecommendation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, rec);
    }
}

// ---------------------------------------------------------------------------
// TRA-7: TxBundleClassification serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn tra_7_classification_serde(cls in arb_classification()) {
        let json = serde_json::to_string(&cls).unwrap();
        let back: TxBundleClassification = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, cls);
    }
}

// ---------------------------------------------------------------------------
// TRA-8: TxChainVerificationData serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn tra_8_chain_verification_serde(cv in arb_chain_verification()) {
        let json = serde_json::to_string(&cv).unwrap();
        let back: TxChainVerificationData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.chain_intact, cv.chain_intact);
        prop_assert_eq!(back.total_records, cv.total_records);
    }
}

// ---------------------------------------------------------------------------
// TRA-9: TxStepRecordData serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn tra_9_step_record_serde(record in arb_step_record()) {
        let json = serde_json::to_string(&record).unwrap();
        let back: TxStepRecordData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.ordinal, record.ordinal);
        prop_assert_eq!(&back.step_id, &record.step_id);
        prop_assert_eq!(back.risk, record.risk);
    }
}

// ---------------------------------------------------------------------------
// TRA-10: TxPlanData serde roundtrip with steps
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn tra_10_plan_data_serde(
        risk in arb_step_risk(),
        precond in arb_precondition_kind(),
        comp in arb_compensation_kind(),
    ) {
        let plan = TxPlanData {
            plan_id: "p1".to_string(),
            plan_hash: 123,
            steps: vec![TxStepData {
                id: "s1".to_string(),
                bead_id: "b1".to_string(),
                agent_id: "a1".to_string(),
                description: "test".to_string(),
                depends_on: vec![],
                preconditions: vec![TxPreconditionData {
                    kind: precond,
                    description: "check".to_string(),
                    required: true,
                }],
                compensations: vec![TxCompensatingActionData {
                    step_id: "s1".to_string(),
                    description: "undo".to_string(),
                    action_type: comp,
                }],
                risk,
                score: 0.5,
            }],
            execution_order: vec!["s1".to_string()],
            parallel_levels: vec![vec!["s1".to_string()]],
            risk_summary: TxRiskSummaryData {
                total_steps: 1,
                high_risk_count: 0,
                critical_risk_count: 0,
                uncompensated_steps: 0,
                overall_risk: risk,
            },
            rejected_edges: vec![],
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: TxPlanData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.plan_id, "p1");
        prop_assert_eq!(back.steps.len(), 1);
        prop_assert_eq!(back.risk_summary.overall_risk, risk);
    }
}

// ---------------------------------------------------------------------------
// TRA-11: TxRunData serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn tra_11_run_data_serde(
        phase in arb_phase(),
        records in proptest::collection::vec(arb_step_record(), 0..4),
        cv in arb_chain_verification(),
    ) {
        let data = TxRunData {
            execution_id: "e1".to_string(),
            plan_id: "p1".to_string(),
            plan_hash: 42,
            phase,
            step_count: records.len(),
            completed_count: 0,
            failed_count: 0,
            skipped_count: 0,
            records,
            chain_verification: cv,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxRunData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.phase, phase);
        prop_assert_eq!(back.records.len(), data.records.len());
    }
}

// ---------------------------------------------------------------------------
// TRA-12: TxRunData wraps in RobotResponse envelope
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn tra_12_run_in_envelope(phase in arb_phase()) {
        let data = TxRunData {
            execution_id: "e-env".to_string(),
            plan_id: "p-env".to_string(),
            plan_hash: 99,
            phase,
            step_count: 0,
            completed_count: 0,
            failed_count: 0,
            skipped_count: 0,
            records: vec![],
            chain_verification: TxChainVerificationData {
                chain_intact: true,
                first_break_at: None,
                missing_ordinals: vec![],
                total_records: 0,
            },
        };
        let resp = RobotResponse::success(data, 1);
        let json = serde_json::to_string(&resp).unwrap();
        let back: RobotResponse<TxRunData> = serde_json::from_str(&json).unwrap();
        prop_assert!(back.ok);
        let inner = back.data.unwrap();
        prop_assert_eq!(inner.phase, phase);
    }
}

// ---------------------------------------------------------------------------
// TRA-13: TxRollbackData serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn tra_13_rollback_serde(
        phase in arb_phase(),
        cv in arb_chain_verification(),
    ) {
        let data = TxRollbackData {
            execution_id: "e-rb".to_string(),
            plan_id: "p-rb".to_string(),
            phase,
            compensated_steps: vec!["s1".to_string()],
            failed_compensations: vec![],
            total_compensated: 1,
            total_failed: 0,
            chain_verification: cv,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxRollbackData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.phase, phase);
        prop_assert_eq!(back.total_compensated, 1);
    }
}

// ---------------------------------------------------------------------------
// TRA-14: TxShowData serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn tra_14_show_data_serde(
        phase in arb_phase(),
        cls in arb_classification(),
        risk in arb_step_risk(),
    ) {
        let data = TxShowData {
            execution_id: "e-show".to_string(),
            plan_id: "p-show".to_string(),
            plan_hash: 777,
            phase,
            classification: cls,
            step_count: 2,
            record_count: 2,
            high_risk_count: 0,
            critical_risk_count: 0,
            overall_risk: risk,
            chain_intact: true,
            timeline: vec![],
            resume: None,
            records: vec![],
            redacted_field_count: 0,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxShowData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.phase, phase);
        prop_assert_eq!(back.classification, cls);
        prop_assert_eq!(back.overall_risk, risk);
    }
}

// ---------------------------------------------------------------------------
// TRA-15: TxResumeData serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn tra_15_resume_data_serde(
        phase in arb_phase(),
        rec in arb_resume_rec(),
    ) {
        let data = TxResumeData {
            execution_id: "e-res".to_string(),
            plan_id: "p-res".to_string(),
            interrupted_phase: phase,
            completed_steps: vec!["s1".to_string()],
            failed_steps: vec![],
            remaining_steps: vec!["s2".to_string()],
            compensated_steps: vec![],
            chain_intact: true,
            last_hash: "genesis".to_string(),
            recommendation: rec,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: TxResumeData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.interrupted_phase, phase);
        prop_assert_eq!(back.recommendation, rec);
    }
}

// ---------------------------------------------------------------------------
// TRA-16: All enum variants use snake_case JSON
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn tra_16_enums_snake_case(
        risk in arb_step_risk(),
        phase in arb_phase(),
    ) {
        let risk_json = serde_json::to_string(&risk).unwrap();
        let risk_val = risk_json.trim_matches('"');
        prop_assert_eq!(risk_val, risk_val.to_lowercase(), "risk should be snake_case");

        let phase_json = serde_json::to_string(&phase).unwrap();
        let phase_val = phase_json.trim_matches('"');
        prop_assert_eq!(phase_val, phase_val.to_lowercase(), "phase should be snake_case");
    }
}
