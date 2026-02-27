//! Property-based tests for tx_observability (ft-1i2ge.8.9).
//!
//! Tests invariants of event taxonomy, timeline construction, redaction policies,
//! plan/ledger snapshots, forensic bundles, and reason code conventions.

#![cfg(feature = "subprocess-bridge")]

use frankenterm_core::tx_idempotency::*;
use frankenterm_core::tx_observability::reason_codes;
use frankenterm_core::tx_observability::*;
use frankenterm_core::tx_plan_compiler::*;
use proptest::prelude::*;
use std::collections::HashMap;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn make_test_plan(n_steps: usize) -> TxPlan {
    let assignments: Vec<PlannerAssignment> = (0..n_steps)
        .map(|i| PlannerAssignment {
            bead_id: format!("bead-{}", i),
            agent_id: format!("agent-{}", i % 3),
            score: 0.8,
            tags: Vec::new(),
            dependency_bead_ids: if i > 0 {
                vec![format!("bead-{}", i - 1)]
            } else {
                Vec::new()
            },
        })
        .collect();
    compile_tx_plan("test-plan", &assignments, &CompilerConfig::default())
}

fn make_test_ledger(plan: &TxPlan) -> TxExecutionLedger {
    let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
    store.create_ledger("exec-001", plan).unwrap();
    let ledger = store.get_ledger_mut("exec-001").unwrap();
    ledger.transition_phase(TxPhase::Preparing).unwrap();
    ledger.transition_phase(TxPhase::Committing).unwrap();
    // Record one step
    if let Some(step) = plan.steps.first() {
        let key = IdempotencyKey::new(&plan.plan_id, &step.id, "exec");
        ledger
            .append(
                key,
                StepOutcome::Success {
                    result: Some("ok".into()),
                },
                step.risk,
                "agent-0",
                1000,
            )
            .unwrap();
    }
    ledger.transition_phase(TxPhase::Aborted).unwrap();
    store.archive_ledger("exec-001").unwrap()
}

fn make_populated_ledger(plan: &TxPlan, n_records: usize) -> TxExecutionLedger {
    let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
    store.create_ledger("exec-pop", plan).unwrap();
    let ledger = store.get_ledger_mut("exec-pop").unwrap();
    ledger.transition_phase(TxPhase::Preparing).unwrap();
    ledger.transition_phase(TxPhase::Committing).unwrap();
    let count = n_records.min(plan.steps.len());
    for (i, step) in plan.steps.iter().take(count).enumerate() {
        let key = IdempotencyKey::new(&plan.plan_id, &step.id, "exec");
        let ts = (i as u64 + 1) * 100;
        ledger
            .append(
                key,
                StepOutcome::Success {
                    result: Some(format!("result-{}", i)),
                },
                step.risk,
                &step.agent_id,
                ts,
            )
            .unwrap();
    }
    ledger.transition_phase(TxPhase::Aborted).unwrap();
    store.archive_ledger("exec-pop").unwrap()
}

fn all_event_kinds() -> Vec<TxEventKind> {
    vec![
        TxEventKind::PlanCompiled,
        TxEventKind::RiskAssessed,
        TxEventKind::PrepareStarted,
        TxEventKind::PreconditionValidated,
        TxEventKind::PreconditionFailed,
        TxEventKind::PrepareCompleted,
        TxEventKind::CommitStarted,
        TxEventKind::StepCommitted,
        TxEventKind::StepFailed,
        TxEventKind::CommitCompleted,
        TxEventKind::CompensationStarted,
        TxEventKind::StepCompensated,
        TxEventKind::CompensationCompleted,
        TxEventKind::ResumeContextBuilt,
        TxEventKind::ResumeExecuted,
        TxEventKind::ExecutionRecorded,
        TxEventKind::ChainVerified,
        TxEventKind::BundleExported,
    ]
}

fn make_observability_event(
    seq: u64,
    ts: u64,
    kind: TxEventKind,
    reason_code: &str,
) -> TxObservabilityEvent {
    let phase = kind.phase();
    TxObservabilityEvent {
        sequence: seq,
        timestamp_ms: ts,
        kind,
        reason_code: reason_code.to_string(),
        phase,
        execution_id: "exec-001".to_string(),
        plan_id: "test-plan".to_string(),
        plan_hash: 0xCAFE,
        step_id: String::new(),
        idem_key: String::new(),
        tx_phase: TxPhase::Committing,
        chain_hash: String::new(),
        agent_id: "agent-0".to_string(),
        details: HashMap::new(),
    }
}

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_step_count() -> impl Strategy<Value = usize> {
    1_usize..=8
}

fn arb_step_outcome() -> impl Strategy<Value = StepOutcome> {
    prop_oneof![
        Just(StepOutcome::Success { result: None }),
        "[a-z]{3,12}".prop_map(|r| StepOutcome::Success { result: Some(r) }),
        Just(StepOutcome::Failed {
            error_code: "E001".to_string(),
            error_message: "test failure".to_string(),
            compensated: false,
        }),
        Just(StepOutcome::Skipped {
            reason: "not needed".to_string(),
        }),
        Just(StepOutcome::Pending),
    ]
}

// ── TO-P01: All TxEventKind variants map to a valid phase ───────────────────

#[test]
fn to_p01_all_event_kinds_map_to_phase() {
    let kinds = all_event_kinds();
    assert_eq!(kinds.len(), 18, "Expected 18 TxEventKind variants");

    for kind in &kinds {
        // Calling .phase() must not panic and must return a known phase.
        let phase = kind.phase();
        let is_valid = matches!(
            phase,
            TxObservabilityPhase::Plan
                | TxObservabilityPhase::Prepare
                | TxObservabilityPhase::Commit
                | TxObservabilityPhase::Compensate
                | TxObservabilityPhase::Resume
                | TxObservabilityPhase::Observability
        );
        assert!(
            is_valid,
            "Kind {:?} mapped to invalid phase {:?}",
            kind, phase
        );
    }
}

// ── TO-P02: TxEventKind serde roundtrip ─────────────────────────────────────

#[test]
fn to_p02_event_kind_serde_roundtrip_all() {
    for kind in all_event_kinds() {
        let json = serde_json::to_string(&kind).unwrap();
        let restored: TxEventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored, kind,
            "Serde roundtrip failed for {:?}, json: {}",
            kind, json
        );
    }
}

// ── TO-P03: TxObservabilityPhase serde roundtrip ────────────────────────────

#[test]
fn to_p03_phase_serde_roundtrip() {
    let phases = [
        TxObservabilityPhase::Plan,
        TxObservabilityPhase::Prepare,
        TxObservabilityPhase::Commit,
        TxObservabilityPhase::Compensate,
        TxObservabilityPhase::Resume,
        TxObservabilityPhase::Observability,
    ];
    for phase in &phases {
        let json = serde_json::to_string(phase).unwrap();
        let restored: TxObservabilityPhase = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored, *phase,
            "Phase serde roundtrip failed for {:?}",
            phase
        );
    }
}

// ── TO-P04: RedactionPolicy::none() has all booleans false ──────────────────

#[test]
fn to_p04_redaction_none_all_false() {
    let policy = RedactionPolicy::none();
    assert!(
        !policy.redact_command_text,
        "none() should not redact command_text"
    );
    assert!(
        !policy.redact_error_messages,
        "none() should not redact error_messages"
    );
    assert!(!policy.redact_results, "none() should not redact results");
    assert!(
        !policy.redact_approval_codes,
        "none() should not redact approval_codes"
    );
    assert!(!policy.redact_labels, "none() should not redact labels");
}

// ── TO-P05: RedactionPolicy::maximum() has all booleans true ────────────────

#[test]
fn to_p05_redaction_maximum_all_true() {
    let policy = RedactionPolicy::maximum();
    assert!(
        policy.redact_command_text,
        "maximum() should redact command_text"
    );
    assert!(
        policy.redact_error_messages,
        "maximum() should redact error_messages"
    );
    assert!(policy.redact_results, "maximum() should redact results");
    assert!(
        policy.redact_approval_codes,
        "maximum() should redact approval_codes"
    );
    assert!(policy.redact_labels, "maximum() should redact labels");
}

// ── TO-P06: redact_outcome with none() preserves original ───────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn to_p06_redact_none_preserves(outcome in arb_step_outcome()) {
        let policy = RedactionPolicy::none();
        let redacted = redact_outcome(&outcome, &policy);
        let orig_json = serde_json::to_string(&outcome).unwrap();
        let redacted_json = serde_json::to_string(&redacted).unwrap();
        prop_assert_eq!(
            &orig_json, &redacted_json,
            "Policy::none() should not alter outcome"
        );
    }
}

// ── TO-P07: redact_outcome with maximum() redacts result in Success ─────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn to_p07_redact_max_success(result_str in "[a-z]{3,20}") {
        let outcome = StepOutcome::Success {
            result: Some(result_str.clone()),
        };
        let policy = RedactionPolicy::maximum();
        let redacted = redact_outcome(&outcome, &policy);
        if let StepOutcome::Success { result } = &redacted {
            let r = result.as_deref().unwrap_or("");
            prop_assert_eq!(r, "[REDACTED]", "Maximum policy should redact result");
        } else {
            prop_assert!(false, "Redacted Success should remain Success");
        }
    }
}

// ── TO-P08: redact_outcome on Pending is identity ───────────────────────────

#[test]
fn to_p08_redact_pending_identity() {
    let policies = [
        RedactionPolicy::none(),
        RedactionPolicy::default(),
        RedactionPolicy::maximum(),
    ];
    for policy in &policies {
        let redacted = redact_outcome(&StepOutcome::Pending, policy);
        let is_pending = matches!(redacted, StepOutcome::Pending);
        assert!(is_pending, "Redacting Pending must always produce Pending");
    }
}

// ── TO-P09: redact_outcome on Compensated recursively redacts ───────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn to_p09_redact_compensated_recursive(
        err_msg in "[a-z]{5,20}",
        comp_result in "[a-z]{5,20}",
    ) {
        let inner = StepOutcome::Failed {
            error_code: "TX-E001".to_string(),
            error_message: err_msg.clone(),
            compensated: true,
        };
        let outcome = StepOutcome::Compensated {
            original_outcome: Box::new(inner),
            compensation_result: comp_result.clone(),
        };
        let policy = RedactionPolicy::maximum();
        let redacted = redact_outcome(&outcome, &policy);
        if let StepOutcome::Compensated {
            original_outcome,
            compensation_result,
        } = &redacted
        {
            prop_assert_eq!(compensation_result.as_str(), "[REDACTED]",
                "compensation_result should be redacted");
            if let StepOutcome::Failed {
                error_code,
                error_message,
                ..
            } = original_outcome.as_ref()
            {
                prop_assert_eq!(error_code.as_str(), "TX-E001",
                    "error_code should be preserved");
                prop_assert_eq!(error_message.as_str(), "[REDACTED]",
                    "error_message should be redacted recursively");
            } else {
                prop_assert!(false, "Inner outcome should remain Failed");
            }
        } else {
            prop_assert!(false, "Redacted Compensated should remain Compensated");
        }
    }
}

// ── TO-P10: build_timeline returns entries sorted by timestamp_ms ────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn to_p10_timeline_sorted(
        n_events in 0_usize..=10,
        base_ts in 0_u64..10000,
    ) {
        let plan = make_test_plan(3);
        let ledger = make_test_ledger(&plan);

        // Create events with various timestamps
        let events: Vec<TxObservabilityEvent> = (0..n_events)
            .map(|i| {
                let ts = base_ts.wrapping_add((i as u64).wrapping_mul(137));
                make_observability_event(
                    i as u64,
                    ts,
                    TxEventKind::ExecutionRecorded,
                    reason_codes::EXECUTION_RECORDED,
                )
            })
            .collect();

        let timeline = build_timeline(&ledger, &events);

        for window in timeline.windows(2) {
            prop_assert!(
                window[0].timestamp_ms <= window[1].timestamp_ms,
                "Timeline must be sorted by timestamp_ms: {} > {}",
                window[0].timestamp_ms, window[1].timestamp_ms
            );
        }
    }
}

// ── TO-P11: PlanSnapshot::from_plan preserves step_count and plan_hash ──────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn to_p11_plan_snapshot_preserves(n_steps in arb_step_count()) {
        let plan = make_test_plan(n_steps);
        let snapshot = PlanSnapshot::from_plan(&plan);

        prop_assert_eq!(
            snapshot.step_count, plan.steps.len(),
            "step_count must match plan.steps.len()"
        );
        prop_assert_eq!(
            snapshot.plan_hash, plan.plan_hash,
            "plan_hash must be preserved in snapshot"
        );
        prop_assert_eq!(
            &snapshot.plan_id, &plan.plan_id,
            "plan_id must be preserved"
        );
        prop_assert_eq!(
            snapshot.execution_order.len(), plan.execution_order.len(),
            "execution_order length must match"
        );
    }
}

// ── TO-P12: PlanSnapshot serde roundtrip ────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn to_p12_plan_snapshot_serde(n_steps in arb_step_count()) {
        let plan = make_test_plan(n_steps);
        let snapshot = PlanSnapshot::from_plan(&plan);

        let json = serde_json::to_string(&snapshot).unwrap();
        let restored: PlanSnapshot = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(&restored.plan_id, &snapshot.plan_id);
        prop_assert_eq!(restored.plan_hash, snapshot.plan_hash);
        prop_assert_eq!(restored.step_count, snapshot.step_count);
        prop_assert_eq!(restored.high_risk_count, snapshot.high_risk_count);
        prop_assert_eq!(restored.critical_risk_count, snapshot.critical_risk_count);
        prop_assert_eq!(restored.uncompensated_steps, snapshot.uncompensated_steps);
    }
}

// ── TO-P13: LedgerSnapshot::from_ledger preserves execution_id and record_count

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn to_p13_ledger_snapshot_preserves(n_steps in 1_usize..=6) {
        let plan = make_test_plan(n_steps);
        let ledger = make_populated_ledger(&plan, n_steps);
        let snapshot = LedgerSnapshot::from_ledger(&ledger);

        prop_assert_eq!(
            &snapshot.execution_id,
            ledger.execution_id(),
            "execution_id must be preserved"
        );
        prop_assert_eq!(
            snapshot.record_count,
            ledger.records().len(),
            "record_count must match records().len()"
        );
        prop_assert_eq!(
            &snapshot.plan_id,
            ledger.plan_id(),
            "plan_id must be preserved"
        );
    }
}

// ── TO-P14: LedgerSnapshot serde roundtrip ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn to_p14_ledger_snapshot_serde(n_steps in 1_usize..=5) {
        let plan = make_test_plan(n_steps);
        let ledger = make_populated_ledger(&plan, n_steps);
        let snapshot = LedgerSnapshot::from_ledger(&ledger);

        let json = serde_json::to_string(&snapshot).unwrap();
        let restored: LedgerSnapshot = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(&restored.execution_id, &snapshot.execution_id);
        prop_assert_eq!(restored.record_count, snapshot.record_count);
        prop_assert_eq!(restored.plan_hash, snapshot.plan_hash);
        prop_assert_eq!(&restored.last_hash, &snapshot.last_hash);
    }
}

// ── TO-P15: TxForensicBundle serde roundtrip ────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn to_p15_forensic_bundle_serde(n_steps in 1_usize..=5) {
        let plan = make_test_plan(n_steps);
        let ledger = make_populated_ledger(&plan, n_steps);
        let config = TxObservabilityConfig::default();

        let bundle = build_forensic_bundle(
            &plan,
            &ledger,
            &[],
            None,
            "proptest-gen",
            "INC-PROP",
            5000,
            &config,
        );

        let json = serde_json::to_string(&bundle).unwrap();
        let restored: TxForensicBundle = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(&restored.metadata.incident_id, &bundle.metadata.incident_id);
        prop_assert_eq!(restored.metadata.version, bundle.metadata.version);
        prop_assert_eq!(restored.plan.plan_hash, bundle.plan.plan_hash);
        prop_assert_eq!(restored.ledger.record_count, bundle.ledger.record_count);
        prop_assert_eq!(
            restored.chain_verification.chain_intact,
            bundle.chain_verification.chain_intact
        );
        prop_assert_eq!(restored.timeline.len(), bundle.timeline.len());
    }
}

// ── TO-P16: build_forensic_bundle with no events produces valid bundle ──────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn to_p16_bundle_no_events(n_steps in 1_usize..=6) {
        let plan = make_test_plan(n_steps);
        let ledger = make_test_ledger(&plan);
        let config = TxObservabilityConfig::default();

        let bundle = build_forensic_bundle(
            &plan,
            &ledger,
            &[],
            None,
            "proptest",
            "INC-EMPTY",
            10000,
            &config,
        );

        // Bundle should still be valid with metadata
        prop_assert_eq!(bundle.metadata.version, 1_u32);
        prop_assert_eq!(&bundle.metadata.generator, "proptest");
        prop_assert_eq!(&bundle.metadata.incident_id, "INC-EMPTY");
        // Timeline comes only from ledger records (no events added)
        // The ledger has 1 record from make_test_ledger
        prop_assert!(
            bundle.timeline.len() <= bundle.ledger.record_count,
            "Timeline from ledger-only should have at most record_count entries, got {} vs {}",
            bundle.timeline.len(), bundle.ledger.record_count
        );
        prop_assert!(bundle.resume.is_none(), "No resume context was provided");
    }
}

// ── TO-P17: All reason_codes start with "tx." ───────────────────────────────

#[test]
fn to_p17_all_reason_codes_start_with_tx() {
    let codes = [
        reason_codes::PLAN_COMPILED,
        reason_codes::PLAN_RISK_ASSESSED,
        reason_codes::PLAN_RISK_HIGH,
        reason_codes::PLAN_RISK_CRITICAL,
        reason_codes::PREPARE_STARTED,
        reason_codes::PRECONDITION_PASS,
        reason_codes::PRECONDITION_FAIL,
        reason_codes::PREPARE_COMPLETED,
        reason_codes::COMMIT_STARTED,
        reason_codes::STEP_COMMITTED,
        reason_codes::STEP_FAILED,
        reason_codes::COMMIT_COMPLETED,
        reason_codes::COMMIT_PARTIAL,
        reason_codes::COMPENSATE_STARTED,
        reason_codes::STEP_COMPENSATED,
        reason_codes::COMPENSATE_COMPLETED,
        reason_codes::RESUME_CONTEXT_BUILT,
        reason_codes::RESUME_CONTINUE,
        reason_codes::RESUME_RESTART,
        reason_codes::RESUME_ABORT,
        reason_codes::RESUME_ALREADY_DONE,
        reason_codes::EXECUTION_RECORDED,
        reason_codes::CHAIN_VERIFIED,
        reason_codes::CHAIN_BROKEN,
        reason_codes::BUNDLE_EXPORTED,
    ];
    for code in &codes {
        assert!(
            code.starts_with("tx."),
            "Reason code must start with \"tx.\": {}",
            code
        );
    }
}

// ── TO-P18: BundleClassification serde roundtrip for all variants ───────────

#[test]
fn to_p18_bundle_classification_serde_all() {
    let variants = [
        BundleClassification::Internal,
        BundleClassification::TeamReview,
        BundleClassification::ExternalAudit,
    ];
    for variant in &variants {
        let json = serde_json::to_string(variant).unwrap();
        let restored: BundleClassification = serde_json::from_str(&json).unwrap();
        assert_eq!(
            &restored, variant,
            "BundleClassification serde roundtrip failed for {:?}",
            variant
        );
    }
}

// ── TO-P19: TxObservabilityConfig default has reasonable values ─────────────

#[test]
fn to_p19_config_default_reasonable() {
    let config = TxObservabilityConfig::default();
    assert!(
        config.max_timeline_entries > 0,
        "max_timeline_entries should be > 0, got {}",
        config.max_timeline_entries
    );
    assert!(
        config.max_events > 0,
        "max_events should be > 0, got {}",
        config.max_events
    );
    // Verify the default classification is a sensible value
    let is_known = matches!(
        config.default_classification,
        BundleClassification::Internal
            | BundleClassification::TeamReview
            | BundleClassification::ExternalAudit
    );
    assert!(is_known, "Default classification should be a known variant");
}

// ── TO-P20: Timeline entries from ledger have monotonically non-decreasing timestamps

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn to_p20_timeline_ledger_monotonic(n_steps in 1_usize..=6) {
        let plan = make_test_plan(n_steps);
        let ledger = make_populated_ledger(&plan, n_steps);

        // Build timeline with no external events — just ledger records
        let timeline = build_timeline(&ledger, &[]);

        for window in timeline.windows(2) {
            prop_assert!(
                window[0].timestamp_ms <= window[1].timestamp_ms,
                "Ledger-derived timeline must be monotonically non-decreasing: {} > {}",
                window[0].timestamp_ms, window[1].timestamp_ms
            );
        }

        // Also verify each entry has a non-empty step_id (from ledger records)
        for entry in &timeline {
            prop_assert!(
                !entry.step_id.is_empty(),
                "Ledger-derived timeline entries should have non-empty step_id"
            );
        }
    }
}
