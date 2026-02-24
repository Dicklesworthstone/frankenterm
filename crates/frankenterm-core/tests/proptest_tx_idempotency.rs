//! Property-based tests for tx_idempotency (ft-1i2ge.8.7).
//!
//! Tests invariants of idempotency keys, execution ledgers, dedup guards,
//! phase state machines, and resume context reconstruction.

#![cfg(feature = "subprocess-bridge")]

use frankenterm_core::tx_idempotency::*;
use frankenterm_core::tx_plan_compiler::*;
use proptest::prelude::*;

// ── Strategies ───────────────────────────────────────────────────────────────

fn arb_plan_id() -> impl Strategy<Value = String> {
    "[a-z]{3,8}-[0-9]{1,4}".prop_map(|s| s)
}

fn arb_step_id() -> impl Strategy<Value = String> {
    "step-[a-z]{2,6}".prop_map(|s| s)
}

fn arb_action_fingerprint() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_]{5,20}".prop_map(|s| s)
}

fn arb_step_outcome() -> impl Strategy<Value = StepOutcome> {
    prop_oneof![
        Just(StepOutcome::Success { result: None }),
        Just(StepOutcome::Success {
            result: Some("ok".to_string()),
        }),
        Just(StepOutcome::Failed {
            error_code: "E001".to_string(),
            error_message: "test failure".to_string(),
            compensated: false,
        }),
        Just(StepOutcome::Failed {
            error_code: "E002".to_string(),
            error_message: "compensated failure".to_string(),
            compensated: true,
        }),
        Just(StepOutcome::Skipped {
            reason: "already done".to_string(),
        }),
        Just(StepOutcome::Pending),
    ]
}

fn arb_step_risk() -> impl Strategy<Value = StepRisk> {
    prop_oneof![
        Just(StepRisk::Low),
        Just(StepRisk::Medium),
        Just(StepRisk::High),
        Just(StepRisk::Critical),
    ]
}

fn arb_tx_phase() -> impl Strategy<Value = TxPhase> {
    prop_oneof![
        Just(TxPhase::Planned),
        Just(TxPhase::Preparing),
        Just(TxPhase::Committing),
        Just(TxPhase::Compensating),
        Just(TxPhase::Completed),
        Just(TxPhase::Aborted),
    ]
}

fn make_plan(n: usize) -> TxPlan {
    let assignments: Vec<PlannerAssignment> = (0..n)
        .map(|i| PlannerAssignment {
            bead_id: format!("b{i}"),
            agent_id: format!("a{}", i % 3),
            score: 0.8,
            tags: Vec::new(),
            dependency_bead_ids: Vec::new(),
        })
        .collect();
    compile_tx_plan("test-plan", &assignments, &CompilerConfig::default())
}

// ── TI-01: Idempotency key determinism ───────────────────────────────────────

proptest! {
    #[test]
    fn ti_01_key_deterministic(
        plan_id in arb_plan_id(),
        step_id in arb_step_id(),
        action in arb_action_fingerprint(),
    ) {
        let k1 = IdempotencyKey::new(&plan_id, &step_id, &action);
        let k2 = IdempotencyKey::new(&plan_id, &step_id, &action);
        prop_assert_eq!(&k1, &k2, "Same inputs must produce same key");
        prop_assert_eq!(k1.as_str(), k2.as_str());
    }
}

// ── TI-02: Different inputs produce different keys ───────────────────────────

proptest! {
    #[test]
    fn ti_02_key_uniqueness(
        plan1 in arb_plan_id(),
        plan2 in arb_plan_id(),
        step in arb_step_id(),
        action in arb_action_fingerprint(),
    ) {
        prop_assume!(plan1 != plan2);
        let k1 = IdempotencyKey::new(&plan1, &step, &action);
        let k2 = IdempotencyKey::new(&plan2, &step, &action);
        prop_assert_ne!(&k1, &k2, "Different plan IDs must produce different keys");
    }
}

// ── TI-03: Key format invariant ──────────────────────────────────────────────

proptest! {
    #[test]
    fn ti_03_key_format(
        plan_id in arb_plan_id(),
        step_id in arb_step_id(),
        action in arb_action_fingerprint(),
    ) {
        let k = IdempotencyKey::new(&plan_id, &step_id, &action);
        prop_assert!(k.as_str().starts_with("txk:"), "Key must start with txk: prefix");
        prop_assert!(k.as_str().len() > 4, "Key must have content after prefix");
        prop_assert_eq!(k.plan_id(), &plan_id);
        prop_assert_eq!(k.step_id(), &step_id);
    }
}

// ── TI-04: Compensation keys differ from normal keys ─────────────────────────

proptest! {
    #[test]
    fn ti_04_compensation_key_distinct(
        plan_id in arb_plan_id(),
        step_id in arb_step_id(),
        action in arb_action_fingerprint(),
    ) {
        let normal = IdempotencyKey::new(&plan_id, &step_id, &action);
        let comp = IdempotencyKey::for_compensation(&plan_id, &step_id, &action);
        prop_assert_ne!(&normal, &comp, "Compensation key must differ from normal key");
    }
}

// ── TI-05: Key serde roundtrip ───────────────────────────────────────────────

proptest! {
    #[test]
    fn ti_05_key_serde(
        plan_id in arb_plan_id(),
        step_id in arb_step_id(),
        action in arb_action_fingerprint(),
    ) {
        let k = IdempotencyKey::new(&plan_id, &step_id, &action);
        let json = serde_json::to_string(&k).unwrap();
        let back: IdempotencyKey = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&k, &back, "Serde roundtrip must preserve key");
    }
}

// ── TI-06: StepOutcome terminal/failure predicates ───────────────────────────

proptest! {
    #[test]
    fn ti_06_outcome_predicates(outcome in arb_step_outcome()) {
        // An outcome cannot be both terminal and pending.
        if outcome.is_pending() {
            prop_assert!(!outcome.is_terminal(), "Pending must not be terminal");
            prop_assert!(!outcome.is_failure(), "Pending must not be failure");
        }
        // A failure is never terminal.
        if outcome.is_failure() {
            prop_assert!(!outcome.is_terminal(), "Failure must not be terminal");
        }
    }
}

// ── TI-07: StepOutcome serde roundtrip ───────────────────────────────────────

proptest! {
    #[test]
    fn ti_07_outcome_serde(outcome in arb_step_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: StepOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&outcome, &back, "Outcome serde roundtrip must preserve value");
    }
}

// ── TI-08: Phase state machine: terminal phases have no transitions ──────────

proptest! {
    #[test]
    fn ti_08_terminal_no_transitions(phase in arb_tx_phase()) {
        if phase.is_terminal() {
            prop_assert!(
                phase.valid_transitions().is_empty(),
                "Terminal phase {:?} must have no valid transitions",
                phase
            );
        }
    }
}

// ── TI-09: Phase transitions are asymmetric ──────────────────────────────────

proptest! {
    #[test]
    fn ti_09_phase_no_self_transition(phase in arb_tx_phase()) {
        prop_assert!(
            !phase.can_transition_to(phase),
            "Phase {:?} must not transition to itself",
            phase
        );
    }
}

// ── TI-10: Planned never reachable from any other phase ──────────────────────

proptest! {
    #[test]
    fn ti_10_planned_unreachable(phase in arb_tx_phase()) {
        if phase != TxPhase::Planned {
            prop_assert!(
                !phase.can_transition_to(TxPhase::Planned),
                "{:?} must not transition to Planned",
                phase
            );
        }
    }
}

// ── TI-11: Phase serde roundtrip ─────────────────────────────────────────────

proptest! {
    #[test]
    fn ti_11_phase_serde(phase in arb_tx_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let back: TxPhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&phase, &back);
    }
}

// ── TI-12: Ledger ordinals are monotonic ─────────────────────────────────────

proptest! {
    #[test]
    fn ti_12_ledger_ordinals_monotonic(n in 1usize..20) {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();

        for i in 0..n {
            let key = IdempotencyKey::new("plan-1", &format!("step-{i}"), "act");
            ledger.append(key, StepOutcome::Success { result: None }, StepRisk::Low, "a", i as u64 * 1000).unwrap();
        }

        let records = ledger.records();
        for i in 1..records.len() {
            prop_assert!(
                records[i].ordinal > records[i - 1].ordinal,
                "Ordinals must be strictly monotonic"
            );
        }
    }
}

// ── TI-13: Ledger hash chain integrity after N appends ───────────────────────

proptest! {
    #[test]
    fn ti_13_hash_chain_intact(n in 1usize..30) {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();

        for i in 0..n {
            let key = IdempotencyKey::new("plan-1", &format!("step-{i}"), "act");
            ledger.append(key, StepOutcome::Success { result: None }, StepRisk::Low, "a", i as u64).unwrap();
        }

        let v = ledger.verify_chain();
        prop_assert!(v.chain_intact, "Hash chain must be intact after {} appends", n);
        prop_assert_eq!(v.total_records, n);
        prop_assert!(v.missing_ordinals.is_empty());
    }
}

// ── TI-14: Duplicate append always rejected ──────────────────────────────────

proptest! {
    #[test]
    fn ti_14_duplicate_rejected(
        plan_id in arb_plan_id(),
        step_id in arb_step_id(),
    ) {
        let mut ledger = TxExecutionLedger::new("exec-1", &plan_id, 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();

        let key = IdempotencyKey::new(&plan_id, &step_id, "act");
        ledger.append(key.clone(), StepOutcome::Success { result: None }, StepRisk::Low, "a", 1000).unwrap();

        let result = ledger.append(key, StepOutcome::Pending, StepRisk::Low, "a", 2000);
        let is_dup = matches!(result, Err(IdempotencyError::DuplicateExecution { .. }));
        prop_assert!(is_dup, "Duplicate key must be rejected");
    }
}

// ── TI-15: Sealed ledger rejects all appends ────────────────────────────────

proptest! {
    #[test]
    fn ti_15_sealed_rejects(
        plan_id in arb_plan_id(),
        step_id in arb_step_id(),
    ) {
        let mut ledger = TxExecutionLedger::new("exec-1", &plan_id, 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();
        ledger.transition_phase(TxPhase::Completed).unwrap();

        let key = IdempotencyKey::new(&plan_id, &step_id, "act");
        let result = ledger.append(key, StepOutcome::Pending, StepRisk::Low, "a", 1000);
        let is_sealed = matches!(result, Err(IdempotencyError::LedgerSealed { .. }));
        prop_assert!(is_sealed, "Sealed ledger must reject appends");
    }
}

// ── TI-16: Ledger serde + rebuild_index preserves lookups ────────────────────

proptest! {
    #[test]
    fn ti_16_ledger_serde_index(n in 1usize..15) {
        let mut ledger = TxExecutionLedger::new("exec-1", "plan-1", 0);
        ledger.transition_phase(TxPhase::Preparing).unwrap();

        let mut keys = Vec::new();
        for i in 0..n {
            let key = IdempotencyKey::new("plan-1", &format!("step-{i}"), "act");
            ledger.append(key.clone(), StepOutcome::Success { result: None }, StepRisk::Low, "a", i as u64).unwrap();
            keys.push(key);
        }

        let json = serde_json::to_string(&ledger).unwrap();
        let mut restored: TxExecutionLedger = serde_json::from_str(&json).unwrap();
        restored.rebuild_index();

        for key in &keys {
            prop_assert!(restored.is_executed(key), "Restored ledger must find key {}", key);
        }
        prop_assert_eq!(restored.record_count(), n);
    }
}

// ── TI-17: Dedup guard capacity invariant ────────────────────────────────────

proptest! {
    #[test]
    fn ti_17_dedup_capacity(cap in 1usize..50, inserts in 1usize..100) {
        let mut guard = DeduplicationGuard::new(cap);
        for i in 0..inserts {
            let key = IdempotencyKey::new("p1", &format!("s{i}"), "act");
            guard.record(&key, "exec-1", StepOutcome::Success { result: None }, i as u64);
        }
        prop_assert!(guard.len() <= cap, "Guard size {} exceeds capacity {}", guard.len(), cap);
    }
}

// ── TI-18: Dedup evict_before preserves newer entries ────────────────────────

proptest! {
    #[test]
    fn ti_18_dedup_evict_preserves_newer(n in 2usize..30, cutoff_idx in 0usize..30) {
        let mut guard = DeduplicationGuard::new(1000);
        for i in 0..n {
            let key = IdempotencyKey::new("p1", &format!("s{i}"), "act");
            guard.record(&key, "exec-1", StepOutcome::Success { result: None }, i as u64 * 1000);
        }
        let cutoff_ms = (cutoff_idx.min(n) as u64) * 1000;
        guard.evict_before(cutoff_ms);

        // All remaining entries should have timestamp >= cutoff.
        for i in 0..n {
            let key = IdempotencyKey::new("p1", &format!("s{i}"), "act");
            if let Some(entry) = guard.check(&key) {
                prop_assert!(
                    entry.timestamp_ms >= cutoff_ms,
                    "Entry at ts {} should not survive evict cutoff {}",
                    entry.timestamp_ms, cutoff_ms
                );
            }
        }
    }
}

// ── TI-19: ResumeContext completed_steps + failed_steps are disjoint ─────────

proptest! {
    #[test]
    fn ti_19_resume_steps_disjoint(n in 1usize..10) {
        let plan = make_plan(n);
        let mut ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        for (i, step) in plan.steps.iter().enumerate() {
            let key = IdempotencyKey::new("test-plan", &step.id, "act");
            let outcome = if i % 3 == 0 {
                StepOutcome::Failed {
                    error_code: "E1".into(),
                    error_message: "fail".into(),
                    compensated: false,
                }
            } else {
                StepOutcome::Success { result: None }
            };
            ledger.append(key, outcome, step.risk, "a", i as u64 * 1000).unwrap();
        }

        let ctx = ResumeContext::from_ledger(&ledger, &plan);
        let completed: std::collections::HashSet<_> = ctx.completed_steps.iter().collect();
        let failed: std::collections::HashSet<_> = ctx.failed_steps.iter().collect();
        let overlap: Vec<_> = completed.intersection(&failed).collect();
        prop_assert!(overlap.is_empty(), "Completed and failed must be disjoint, got overlap: {:?}", overlap);
    }
}

// ── TI-20: ResumeContext remaining + completed + failed covers plan ──────────

proptest! {
    #[test]
    fn ti_20_resume_covers_plan(n in 1usize..8, execute_count in 0usize..8) {
        let plan = make_plan(n);
        let mut ledger = TxExecutionLedger::new("exec-1", "test-plan", plan.plan_hash);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        let to_execute = execute_count.min(n);
        for i in 0..to_execute {
            let step = &plan.steps[i];
            let key = IdempotencyKey::new("test-plan", &step.id, "act");
            ledger.append(key, StepOutcome::Success { result: None }, StepRisk::Low, "a", i as u64 * 1000).unwrap();
        }

        let ctx = ResumeContext::from_ledger(&ledger, &plan);

        // Every plan step should appear in exactly one of: completed, failed, remaining.
        let all_plan_steps: std::collections::HashSet<_> = plan.steps.iter().map(|s| &s.id).collect();
        let mut accounted = std::collections::HashSet::new();
        for s in &ctx.completed_steps {
            accounted.insert(s.clone());
        }
        for s in &ctx.failed_steps {
            accounted.insert(s.clone());
        }
        for s in &ctx.remaining_steps {
            accounted.insert(s.clone());
        }

        for step_id in &all_plan_steps {
            prop_assert!(
                accounted.contains(*step_id),
                "Step {} not accounted for in resume context",
                step_id
            );
        }
    }
}

// ── TI-21: IdempotencyPolicy serde roundtrip ─────────────────────────────────

proptest! {
    #[test]
    fn ti_21_policy_serde(
        capacity in 1usize..100_000,
        skip in proptest::bool::ANY,
        ttl in 1000u64..10_000_000,
        integrity in proptest::bool::ANY,
        max_ledgers in 1usize..1000,
    ) {
        let policy = IdempotencyPolicy {
            dedup_capacity: capacity,
            skip_completed_on_resume: skip,
            dedup_ttl_ms: ttl,
            require_chain_integrity: integrity,
            max_active_ledgers: max_ledgers,
        };
        let json = serde_json::to_string(&policy).unwrap();
        let back: IdempotencyPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.dedup_capacity, capacity);
        prop_assert_eq!(back.skip_completed_on_resume, skip);
        prop_assert_eq!(back.dedup_ttl_ms, ttl);
        prop_assert_eq!(back.require_chain_integrity, integrity);
        prop_assert_eq!(back.max_active_ledgers, max_ledgers);
    }
}

// ── TI-22: IdempotencyError serde roundtrip ──────────────────────────────────

proptest! {
    #[test]
    fn ti_22_error_serde(phase in arb_tx_phase()) {
        let errors = vec![
            IdempotencyError::DuplicateExecution { key: "k1".to_string() },
            IdempotencyError::InvalidPhaseTransition { from: phase, to: TxPhase::Completed },
            IdempotencyError::LedgerSealed { phase },
            IdempotencyError::LedgerNotFound { execution_id: "e1".to_string() },
            IdempotencyError::ChainIntegrityViolation { ordinal: 42 },
        ];
        for err in &errors {
            let json = serde_json::to_string(err).unwrap();
            let back: IdempotencyError = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(&back, err);
        }
    }
}

// ── TI-23: Store cross-instance dedup ────────────────────────────────────────

proptest! {
    #[test]
    fn ti_23_cross_instance_dedup(n in 1usize..5) {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(n);

        store.create_ledger("exec-1", &plan).unwrap();
        store.get_ledger_mut("exec-1").unwrap().transition_phase(TxPhase::Preparing).unwrap();

        // Record in first execution.
        for (i, step) in plan.steps.iter().enumerate() {
            let key = IdempotencyKey::new("test-plan", &step.id, "act");
            store.record_execution(
                "exec-1",
                key,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a",
                i as u64 * 1000,
            ).unwrap();
        }

        // Second execution should see dedup hits.
        store.create_ledger("exec-2", &plan).unwrap();
        for step in &plan.steps {
            let key = IdempotencyKey::new("test-plan", &step.id, "act");
            let dedup = store.check_dedup(&key);
            prop_assert!(dedup.is_some(), "Cross-instance dedup must find step {}", step.id);
        }
    }
}

// ── TI-24: Store rejects duplicate ledger creation ───────────────────────────

proptest! {
    #[test]
    fn ti_24_store_no_dup_ledger(
        exec_id in "[a-z]{5,10}",
        n in 1usize..5,
    ) {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(n);
        store.create_ledger(&exec_id, &plan).unwrap();
        let result = store.create_ledger(&exec_id, &plan);
        let is_dup = matches!(result, Err(IdempotencyError::DuplicateExecution { .. }));
        prop_assert!(is_dup, "Duplicate ledger creation must fail");
    }
}

// ── TI-25: Full lifecycle: plan → prepare → commit → complete ────────────────

proptest! {
    #[test]
    fn ti_25_full_lifecycle(n in 1usize..8) {
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let plan = make_plan(n);
        store.create_ledger("exec-1", &plan).unwrap();

        // Phase transitions.
        let ledger = store.get_ledger_mut("exec-1").unwrap();
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();

        // Execute all steps.
        for (i, step) in plan.steps.iter().enumerate() {
            let key = IdempotencyKey::new("test-plan", &step.id, &step.description);
            store.record_execution(
                "exec-1",
                key,
                StepOutcome::Success { result: None },
                step.risk,
                &step.agent_id,
                i as u64 * 1000,
            ).unwrap();
        }

        // Complete.
        store.get_ledger_mut("exec-1").unwrap().transition_phase(TxPhase::Completed).unwrap();

        // Verify.
        let ledger = store.get_ledger("exec-1").unwrap();
        let v = ledger.verify_chain();
        prop_assert!(v.chain_intact, "Chain must be intact after full lifecycle");
        prop_assert_eq!(v.total_records, n);

        let ctx = store.resume_context("exec-1", &plan).unwrap();
        prop_assert_eq!(ctx.recommendation, ResumeRecommendation::AlreadyComplete);
    }
}
