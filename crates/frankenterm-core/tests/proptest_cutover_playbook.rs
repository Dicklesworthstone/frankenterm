//! Property tests for cutover_playbook module (ft-3681t.8.4).
//!
//! Covers serde roundtrips, stage ordering invariants, gate pass/fail
//! logic, advance preconditions, rollback mechanics, telemetry counter
//! consistency, and standard factory validation.

use frankenterm_core::cutover_playbook::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_cutover_stage() -> impl Strategy<Value = CutoverStage> {
    prop_oneof![
        Just(CutoverStage::Preflight),
        Just(CutoverStage::Shadow),
        Just(CutoverStage::Canary),
        Just(CutoverStage::Progressive),
        Just(CutoverStage::Default),
    ]
}

fn arb_gate_category() -> impl Strategy<Value = GateCategory> {
    prop_oneof![
        Just(GateCategory::Parity),
        Just(GateCategory::Contract),
        Just(GateCategory::Divergence),
        Just(GateCategory::PolicySafety),
        Just(GateCategory::RollbackReadiness),
        Just(GateCategory::Performance),
        Just(GateCategory::Approval),
        Just(GateCategory::SoakConfidence),
    ]
}

fn arb_approver_role() -> impl Strategy<Value = ApproverRole> {
    prop_oneof![
        Just(ApproverRole::MigrationLead),
        Just(ApproverRole::Operations),
        Just(ApproverRole::PolicyOwner),
    ]
}

fn arb_stage_gate() -> impl Strategy<Value = StageGate> {
    (
        "[A-Z]-[0-9]{1,4}",
        arb_gate_category(),
        ".{1,30}",
        arb_cutover_stage(),
        any::<bool>(),
    )
        .prop_map(|(id, cat, desc, stage, blocking)| {
            let mut gate = StageGate::new(&id, cat, &desc).for_stage(stage);
            if !blocking {
                gate = gate.advisory();
            }
            gate
        })
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_cutover_stage(stage in arb_cutover_stage()) {
        let json = serde_json::to_string(&stage).unwrap();
        let back: CutoverStage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stage, back);
    }

    #[test]
    fn serde_roundtrip_gate_category(cat in arb_gate_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: GateCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    #[test]
    fn serde_roundtrip_approver_role(role in arb_approver_role()) {
        let json = serde_json::to_string(&role).unwrap();
        let back: ApproverRole = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(role, back);
    }

    #[test]
    fn serde_roundtrip_stage_gate(gate in arb_stage_gate()) {
        let json = serde_json::to_string(&gate).unwrap();
        let back: StageGate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(gate.gate_id, back.gate_id);
        prop_assert_eq!(gate.category, back.category);
        prop_assert_eq!(gate.stage, back.stage);
        prop_assert_eq!(gate.blocking, back.blocking);
    }

    #[test]
    fn serde_roundtrip_playbook_snapshot(stage in arb_cutover_stage()) {
        let pb = CutoverPlaybook::new("test", 1);
        let snap = pb.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: PlaybookSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.migration_id, back.migration_id);
        let _ = stage; // use the param to satisfy proptest
    }
}

// =============================================================================
// Stage ordering invariants
// =============================================================================

proptest! {
    #[test]
    fn stage_index_monotonic(stage in arb_cutover_stage()) {
        if let Some(next) = stage.next() {
            prop_assert!(next.index() > stage.index(),
                "next stage index {} should be > current {}", next.index(), stage.index());
        }
        if let Some(prev) = stage.previous() {
            prop_assert!(prev.index() < stage.index(),
                "previous stage index {} should be < current {}", prev.index(), stage.index());
        }
    }

    #[test]
    fn stage_next_previous_inverse(stage in arb_cutover_stage()) {
        if let Some(next) = stage.next() {
            prop_assert_eq!(next.previous(), Some(stage),
                "next.previous should return original stage");
        }
        if let Some(prev) = stage.previous() {
            prop_assert_eq!(prev.next(), Some(stage),
                "previous.next should return original stage");
        }
    }

    #[test]
    fn stage_label_nonempty(stage in arb_cutover_stage()) {
        prop_assert!(!stage.label().is_empty());
    }

    #[test]
    fn gate_category_label_nonempty(cat in arb_gate_category()) {
        prop_assert!(!cat.label().is_empty());
    }

    #[test]
    fn approver_role_label_nonempty(role in arb_approver_role()) {
        prop_assert!(!role.label().is_empty());
    }
}

#[test]
fn stage_first_has_no_previous() {
    assert_eq!(CutoverStage::Preflight.previous(), None);
}

#[test]
fn stage_last_has_no_next() {
    assert_eq!(CutoverStage::Default.next(), None);
}

// =============================================================================
// Gate pass/fail invariants
// =============================================================================

proptest! {
    #[test]
    fn pass_gate_sets_passed(
        gate_id in "[A-Z]-[0-9]{1,3}",
        cat in arb_gate_category(),
        stage in arb_cutover_stage(),
    ) {
        let mut pb = CutoverPlaybook::new("test", 1);
        let gate = StageGate::new(&gate_id, cat, "test gate").for_stage(stage);
        pb.register_gate(gate);
        pb.pass_gate(&gate_id, "evidence");

        let gates = pb.current_gates();
        // If the gate is for the current stage (Preflight), it should be passed
        if stage == CutoverStage::Preflight {
            let found = gates.iter().find(|g| g.gate_id == gate_id);
            if let Some(g) = found {
                prop_assert!(g.passed, "gate should be passed after pass_gate");
            }
        }
    }

    #[test]
    fn fail_gate_clears_passed(
        gate_id in "[A-Z]-[0-9]{1,3}",
        cat in arb_gate_category(),
    ) {
        let mut pb = CutoverPlaybook::new("test", 1);
        let gate = StageGate::new(&gate_id, cat, "test").for_stage(CutoverStage::Preflight);
        pb.register_gate(gate);

        pb.pass_gate(&gate_id, "first");
        pb.fail_gate(&gate_id, "regression");

        let failures = pb.blocking_failures();
        if cat != GateCategory::Approval {
            // Default gates are blocking
            let is_failed = failures.iter().any(|g| g.gate_id == gate_id);
            // Gate should show in blocking failures (unless it was advisory)
            prop_assert!(is_failed || !pb.current_gates().iter().any(|g| g.gate_id == gate_id && g.blocking),
                "failed blocking gate should appear in blocking_failures");
        }
    }
}

// =============================================================================
// Advance preconditions
// =============================================================================

proptest! {
    #[test]
    fn advance_fails_without_gates_passing(
        n_gates in 1..5usize,
        stage in Just(CutoverStage::Preflight),
    ) {
        let mut pb = CutoverPlaybook::new("test", 1);
        for i in 0..n_gates {
            let gate = StageGate::new(format!("G-{i}"), GateCategory::Parity, "test")
                .for_stage(stage);
            pb.register_gate(gate);
        }

        let result = pb.try_advance(1000, "operator");
        prop_assert!(!result.advanced, "should not advance with unpassed gates");
        prop_assert_eq!(result.from, CutoverStage::Preflight);
        prop_assert_eq!(result.to, CutoverStage::Preflight);
    }

    #[test]
    fn advance_succeeds_when_all_blocking_pass(
        n_gates in 1..4usize,
    ) {
        let mut pb = CutoverPlaybook::new("test", 1);
        for i in 0..n_gates {
            let gate = StageGate::new(format!("G-{i}"), GateCategory::Parity, "test")
                .for_stage(CutoverStage::Preflight);
            pb.register_gate(gate);
        }

        for i in 0..n_gates {
            pb.pass_gate(&format!("G-{i}"), "evidence");
        }

        // Preflight stage requires MigrationLead approval
        pb.record_approval(ApprovalRecord {
            approver: "test-lead".into(),
            role: ApproverRole::MigrationLead,
            stage: CutoverStage::Preflight,
            approved_at_ms: 999,
            notes: String::new(),
        });

        let result = pb.try_advance(1000, "operator");
        prop_assert!(result.advanced, "should advance when all blocking gates pass");
        prop_assert_eq!(result.from, CutoverStage::Preflight);
        prop_assert_eq!(result.to, CutoverStage::Shadow);
    }
}

// =============================================================================
// Rollback mechanics
// =============================================================================

proptest! {
    #[test]
    fn rollback_halts_playbook(
        reason in ".{1,30}",
    ) {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.initiate_rollback(&reason, 1000, "operator");

        prop_assert!(pb.halted, "playbook should be halted after rollback");
        prop_assert!(!pb.halt_reason.is_empty());
    }

    #[test]
    fn recovery_unhalts_playbook(
        reason in ".{1,30}",
    ) {
        let mut pb = CutoverPlaybook::new("test", 1);
        pb.initiate_rollback(&reason, 1000, "operator");
        prop_assert!(pb.halted);

        pb.confirm_recovery(2000, "recovery notes");
        prop_assert!(!pb.halted, "playbook should be unhalted after recovery");
    }
}

// =============================================================================
// Telemetry counter consistency
// =============================================================================

proptest! {
    #[test]
    fn telemetry_gate_counts_consistent(
        n_pass in 0..5usize,
        n_fail in 0..5usize,
    ) {
        let mut pb = CutoverPlaybook::new("test", 1);

        for i in 0..n_pass {
            let gate = StageGate::new(format!("P-{i}"), GateCategory::Parity, "pass")
                .for_stage(CutoverStage::Preflight);
            pb.register_gate(gate);
            pb.pass_gate(&format!("P-{i}"), "ok");
        }

        for i in 0..n_fail {
            let gate = StageGate::new(format!("F-{i}"), GateCategory::Contract, "fail")
                .for_stage(CutoverStage::Preflight);
            pb.register_gate(gate);
            pb.fail_gate(&format!("F-{i}"), "not ok");
        }

        let telem = &pb.telemetry;
        let total_evals = telem.gates_passed + telem.gates_failed;
        prop_assert_eq!(total_evals, telem.gate_evaluations,
            "passed ({}) + failed ({}) should equal evaluations ({})",
            telem.gates_passed, telem.gates_failed, telem.gate_evaluations);
    }
}

// =============================================================================
// Standard factory
// =============================================================================

#[test]
fn standard_playbook_starts_at_preflight() {
    let pb = standard_playbook("migration-1");
    assert_eq!(pb.current_stage, CutoverStage::Preflight);
    assert!(!pb.halted);
    assert!(!pb.is_complete());
}

#[test]
fn standard_playbook_has_gates_and_triggers() {
    let pb = standard_playbook("migration-1");
    assert!(!pb.gates.is_empty(), "should have gates");
    assert!(!pb.rollback_triggers.is_empty(), "should have triggers");
}

#[test]
fn standard_playbook_snapshot_serializes() {
    let pb = standard_playbook("migration-1");
    let snap = pb.snapshot();
    let json = serde_json::to_string(&snap).unwrap();
    let back: PlaybookSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(back.migration_id, "migration-1");
    assert_eq!(back.current_stage, CutoverStage::Preflight);
}

#[test]
fn standard_playbook_summary_has_content() {
    let pb = standard_playbook("migration-1");
    let summary = pb.render_summary();
    assert!(summary.contains("migration-1"));
    assert!(summary.contains("Preflight"));
}
