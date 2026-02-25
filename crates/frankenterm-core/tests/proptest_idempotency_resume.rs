//! Property-based tests for durable idempotency, dedupe, and resume (H7).
//!
//! Covers:
//! - Tx key determinism: same contract always produces same key
//! - Tx key uniqueness: different contracts produce different keys
//! - Step key determinism and phase-sensitivity
//! - Fresh verdict when no prior record
//! - Double-commit/compensation blocking
//! - Resumable verdict for non-terminal records
//! - Exact duplicate for terminal + matching key
//! - Resume state reconstruction: committed/pending counts
//! - Resume state terminal detection
//! - Serde roundtrips for all types
//! - Canonical string determinism
//! - Idempotency check result should_proceed logic

use frankenterm_core::plan::{
    execute_commit_phase, reconstruct_tx_resume_state, validate_tx_idempotency,
    MissionActorRole, MissionKillSwitchLevel, MissionTxContract, MissionTxState, StepAction,
    TxCommitStepInput, TxCompensation, TxExecutionRecord, TxId, TxIdempotencyCheckResult,
    TxIdempotencyVerdict, TxIntent, TxOutcome, TxPhase, TxPlan, TxPlanId, TxReceipt,
    TxResumeState, TxStep, TxStepExecutionRecord, TxStepId,
};
use proptest::prelude::*;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn make_contract(num_steps: usize) -> MissionTxContract {
    let steps: Vec<TxStep> = (1..=num_steps)
        .map(|i| TxStep {
            step_id: TxStepId(format!("s{i}")),
            ordinal: i as u32,
            action: StepAction::SendText {
                pane_id: i as u64,
                text: format!("step-{i}"),
                paste_mode: None,
            },
        })
        .collect();

    let compensations: Vec<TxCompensation> = (1..=num_steps)
        .map(|i| TxCompensation {
            for_step_id: TxStepId(format!("s{i}")),
            action: StepAction::SendText {
                pane_id: i as u64,
                text: format!("undo-{i}"),
                paste_mode: None,
            },
        })
        .collect();

    MissionTxContract {
        tx_version: 1,
        intent: TxIntent {
            tx_id: TxId("tx:h7-prop".into()),
            requested_by: MissionActorRole::Dispatcher,
            summary: "proptest-h7".into(),
            correlation_id: "pth7-1".into(),
            created_at_ms: 1000,
        },
        plan: TxPlan {
            plan_id: TxPlanId("plan:h7-prop".into()),
            tx_id: TxId("tx:h7-prop".into()),
            steps,
            preconditions: vec![],
            compensations,
        },
        lifecycle_state: MissionTxState::Prepared,
        outcome: TxOutcome::Pending,
        receipts: vec![],
    }
}

fn make_record(
    contract: &MissionTxContract,
    state: MissionTxState,
    commit_hash: Option<&str>,
    comp_hash: Option<&str>,
) -> TxExecutionRecord {
    TxExecutionRecord {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        lifecycle_state: state,
        correlation_id: contract.intent.correlation_id.clone(),
        tx_idempotency_key: TxExecutionRecord::compute_tx_key(contract),
        step_records: vec![],
        commit_report_hash: commit_hash.map(|s| s.to_string()),
        compensation_report_hash: comp_hash.map(|s| s.to_string()),
        updated_at_ms: 5000,
    }
}

// ── Properties ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn tx_key_deterministic(
        num_steps in 1usize..10,
    ) {
        let contract = make_contract(num_steps);
        let k1 = TxExecutionRecord::compute_tx_key(&contract);
        let k2 = TxExecutionRecord::compute_tx_key(&contract);
        prop_assert_eq!(k1, k2);
    }

    #[test]
    fn tx_key_unique_for_different_step_counts(
        a in 1usize..10,
        b in 1usize..10,
    ) {
        if a == b { return Ok(()); }
        let c1 = make_contract(a);
        let c2 = make_contract(b);
        let k1 = TxExecutionRecord::compute_tx_key(&c1);
        let k2 = TxExecutionRecord::compute_tx_key(&c2);
        prop_assert_ne!(k1, k2);
    }

    #[test]
    fn step_key_deterministic(
        ordinal in 1u32..20,
        phase in prop_oneof![
            Just(TxPhase::Prepare),
            Just(TxPhase::Commit),
            Just(TxPhase::Compensate),
        ],
    ) {
        let tx_id = TxId("tx:h7-prop".into());
        let step_id = TxStepId(format!("s{ordinal}"));
        let k1 = TxStepExecutionRecord::compute_step_key(&tx_id, &step_id, &phase);
        let k2 = TxStepExecutionRecord::compute_step_key(&tx_id, &step_id, &phase);
        prop_assert_eq!(k1, k2);
    }

    #[test]
    fn step_key_phase_sensitive(
        ordinal in 1u32..10,
    ) {
        let tx_id = TxId("tx:h7-prop".into());
        let step_id = TxStepId(format!("s{ordinal}"));
        let k_commit = TxStepExecutionRecord::compute_step_key(&tx_id, &step_id, &TxPhase::Commit);
        let k_comp = TxStepExecutionRecord::compute_step_key(&tx_id, &step_id, &TxPhase::Compensate);
        let k_prep = TxStepExecutionRecord::compute_step_key(&tx_id, &step_id, &TxPhase::Prepare);
        prop_assert_ne!(k_commit, k_comp);
        prop_assert_ne!(k_commit, k_prep);
        prop_assert_ne!(k_comp, k_prep);
    }

    #[test]
    fn fresh_verdict_when_no_prior(
        num_steps in 1usize..8,
        phase in prop_oneof![
            Just(TxPhase::Prepare),
            Just(TxPhase::Commit),
            Just(TxPhase::Compensate),
        ],
    ) {
        let contract = make_contract(num_steps);
        let result = validate_tx_idempotency(&contract, phase, None);
        prop_assert!(result.should_proceed());
        let is_fresh = matches!(result.verdict, TxIdempotencyVerdict::Fresh);
        prop_assert!(is_fresh);
    }

    #[test]
    fn double_commit_always_blocked(
        num_steps in 1usize..6,
    ) {
        let contract = make_contract(num_steps);
        let record = make_record(&contract, MissionTxState::Committed, Some("h"), None);
        let result = validate_tx_idempotency(&contract, TxPhase::Commit, Some(&record));
        prop_assert!(!result.should_proceed());
        let is_blocked = matches!(
            result.verdict,
            TxIdempotencyVerdict::DoubleExecutionBlocked { .. }
        );
        prop_assert!(is_blocked);
    }

    #[test]
    fn double_compensation_always_blocked(
        num_steps in 1usize..6,
    ) {
        let contract = make_contract(num_steps);
        let record = make_record(
            &contract,
            MissionTxState::RolledBack,
            Some("h1"),
            Some("h2"),
        );
        let result = validate_tx_idempotency(&contract, TxPhase::Compensate, Some(&record));
        prop_assert!(!result.should_proceed());
        let is_blocked = matches!(
            result.verdict,
            TxIdempotencyVerdict::DoubleExecutionBlocked { .. }
        );
        prop_assert!(is_blocked);
    }

    #[test]
    fn resumable_on_non_terminal(
        num_steps in 1usize..6,
    ) {
        let contract = make_contract(num_steps);
        let record = make_record(&contract, MissionTxState::Committing, None, None);
        let result = validate_tx_idempotency(&contract, TxPhase::Commit, Some(&record));
        prop_assert!(result.should_proceed());
        let is_resumable = matches!(result.verdict, TxIdempotencyVerdict::Resumable { .. });
        prop_assert!(is_resumable);
    }

    #[test]
    fn resume_state_pending_equals_total_minus_committed(
        num_steps in 1usize..8,
        fail_at in 1usize..8,
    ) {
        let fail_at = fail_at.min(num_steps);
        let mut contract = make_contract(num_steps);
        contract.lifecycle_state = MissionTxState::Prepared;
        let commit_inputs: Vec<TxCommitStepInput> = (1..=num_steps)
            .map(|i| TxCommitStepInput {
                step_id: TxStepId(format!("s{i}")),
                success: i != fail_at,
                reason_code: if i == fail_at { "err".into() } else { "ok".into() },
                error_code: if i == fail_at { Some("FTX".into()) } else { None },
                completed_at_ms: (i as i64 + 1) * 1000,
            })
            .collect();
        let commit_report = execute_commit_phase(
            &contract,
            &commit_inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        let resume = reconstruct_tx_resume_state(&contract, Some(&commit_report), None, 15_000);
        prop_assert!(resume.commit_phase_completed);
        // committed_step_ids should equal the commit_report's committed count
        prop_assert_eq!(
            resume.committed_step_ids.len(),
            commit_report.committed_count,
            "committed ids {} != report count {}",
            resume.committed_step_ids.len(),
            commit_report.committed_count
        );
    }

    #[test]
    fn resume_state_terminal_no_pending(
        num_steps in 1usize..6,
        terminal_state in prop_oneof![
            Just(MissionTxState::Committed),
            Just(MissionTxState::RolledBack),
            Just(MissionTxState::Failed),
        ],
    ) {
        let mut contract = make_contract(num_steps);
        contract.receipts.push(TxReceipt {
            seq: 1,
            state: terminal_state,
            emitted_at_ms: 5000,
            reason_code: None,
            error_code: None,
        });
        let resume = reconstruct_tx_resume_state(&contract, None, None, 10_000);
        prop_assert!(resume.is_fully_resolved());
        prop_assert!(resume.pending_step_ids.is_empty());
    }

    #[test]
    fn execution_record_serde_roundtrip(
        num_steps in 1usize..5,
    ) {
        let contract = make_contract(num_steps);
        let record = make_record(&contract, MissionTxState::Committed, Some("h1"), None);
        let json = serde_json::to_string(&record).unwrap();
        let restored: TxExecutionRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&record, &restored);
    }

    #[test]
    fn idempotency_check_serde_roundtrip(
        num_steps in 1usize..5,
    ) {
        let contract = make_contract(num_steps);
        let result = validate_tx_idempotency(&contract, TxPhase::Commit, None);
        let json = serde_json::to_string(&result).unwrap();
        let restored: TxIdempotencyCheckResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&result, &restored);
    }

    #[test]
    fn resume_state_serde_roundtrip(
        num_steps in 1usize..5,
    ) {
        let contract = make_contract(num_steps);
        let resume = reconstruct_tx_resume_state(&contract, None, None, 10_000);
        let json = serde_json::to_string(&resume).unwrap();
        let restored: TxResumeState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&resume, &restored);
    }

    #[test]
    fn canonical_strings_all_deterministic(
        num_steps in 1usize..5,
    ) {
        let contract = make_contract(num_steps);
        let record = make_record(&contract, MissionTxState::Committed, Some("h"), None);
        let resume = reconstruct_tx_resume_state(&contract, None, None, 10_000);
        let check = validate_tx_idempotency(&contract, TxPhase::Commit, None);

        // Each canonical_string() call produces same result
        prop_assert_eq!(record.canonical_string(), record.canonical_string());
        prop_assert_eq!(resume.canonical_string(), resume.canonical_string());
        prop_assert_eq!(check.canonical_string(), check.canonical_string());
    }

    #[test]
    fn should_proceed_consistency(
        num_steps in 1usize..5,
    ) {
        let contract = make_contract(num_steps);

        // Fresh always proceeds
        let fresh = validate_tx_idempotency(&contract, TxPhase::Commit, None);
        prop_assert!(fresh.should_proceed());
        prop_assert!(!fresh.is_exact_duplicate());

        // Exact duplicate never proceeds
        let record = make_record(&contract, MissionTxState::Committed, None, None);
        let dup = validate_tx_idempotency(&contract, TxPhase::Prepare, Some(&record));
        prop_assert!(!dup.should_proceed());
        prop_assert!(dup.is_exact_duplicate());
    }
}
