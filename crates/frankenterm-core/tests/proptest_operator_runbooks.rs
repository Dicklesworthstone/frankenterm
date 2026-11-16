//! Property tests for operator_runbooks module (ft-3681t.9.6).
//!
//! Covers serde roundtrips, operator role hierarchy, runbook step filtering,
//! registry search/filtering, decision overlay invariants, tutorial step
//! arithmetic, and standard factory validation.

use frankenterm_core::operator_runbooks::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_operator_role() -> impl Strategy<Value = OperatorRole> {
    prop_oneof![
        Just(OperatorRole::Trainee),
        Just(OperatorRole::Operator),
        Just(OperatorRole::SeniorOperator),
        Just(OperatorRole::Admin),
    ]
}

fn arb_step_type() -> impl Strategy<Value = StepType> {
    prop_oneof![
        Just(StepType::Action),
        Just(StepType::Verify),
        Just(StepType::Decision),
        Just(StepType::Escalate),
        Just(StepType::Observe),
        Just(StepType::Wait),
    ]
}

fn arb_decision_option() -> impl Strategy<Value = DecisionOption> {
    (
        "[a-z-]{3,10}",
        "[A-Za-z ]{3,20}",
        ".{1,30}",
        0..100u8,
        any::<bool>(),
    )
        .prop_map(|(id, label, desc, risk, recommended)| DecisionOption {
            option_id: id,
            label,
            description: desc,
            risk_score: risk,
            benefit: String::new(),
            recommended,
            telemetry_refs: Vec::new(),
        })
}

fn arb_runbook_step() -> impl Strategy<Value = RunbookStep> {
    (
        "[a-z-]{3,10}",
        ".{1,30}",
        arb_step_type(),
        arb_operator_role(),
        0..3600u32,
    )
        .prop_map(|(id, instruction, step_type, min_role, est)| RunbookStep {
            step_id: id,
            instruction,
            step_type,
            precondition: None,
            expected_outcome: None,
            decision_support: None,
            min_role,
            caution: None,
            estimated_seconds: est,
        })
}

fn arb_runbook() -> impl Strategy<Value = Runbook> {
    (
        "[a-z-]{3,12}",
        "[A-Za-z ]{3,20}",
        arb_operator_role(),
        prop::collection::vec(arb_runbook_step(), 1..8),
        any::<bool>(),
    )
        .prop_map(|(id, title, min_role, steps, incident)| Runbook {
            runbook_id: id,
            title,
            summary: String::new(),
            version: "1.0".into(),
            applicability: RunbookApplicability {
                workflow_classes: Vec::new(),
                min_role,
                tags: Vec::new(),
                incident_applicable: incident,
            },
            steps,
            related: Vec::new(),
        })
}

fn arb_tutorial_flow() -> impl Strategy<Value = TutorialFlow> {
    (
        "[a-z-]{3,12}",
        "[A-Za-z ]{3,20}",
        arb_operator_role(),
        prop::collection::vec(
            ("[a-z-]{3,8}", ".{1,20}", 0..600u32).prop_map(|(id, instr, est)| TutorialStep {
                step_id: id,
                instruction: instr,
                hint: None,
                validation: String::new(),
                estimated_seconds: est,
            }),
            1..6,
        ),
    )
        .prop_map(|(id, title, role, steps)| TutorialFlow {
            tutorial_id: id,
            title,
            summary: String::new(),
            target_role: role,
            prerequisites: Vec::new(),
            steps,
            tags: Vec::new(),
        })
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_operator_role(role in arb_operator_role()) {
        let json = serde_json::to_string(&role).unwrap();
        let back: OperatorRole = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(role, back);
    }

    #[test]
    fn serde_roundtrip_step_type(st in arb_step_type()) {
        let json = serde_json::to_string(&st).unwrap();
        let back: StepType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(st, back);
    }

    #[test]
    fn serde_roundtrip_runbook(rb in arb_runbook()) {
        let json = serde_json::to_string(&rb).unwrap();
        let back: Runbook = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rb.runbook_id, back.runbook_id);
        prop_assert_eq!(rb.steps.len(), back.steps.len());
    }

    #[test]
    fn serde_roundtrip_tutorial(tut in arb_tutorial_flow()) {
        let json = serde_json::to_string(&tut).unwrap();
        let back: TutorialFlow = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tut.tutorial_id, back.tutorial_id);
        prop_assert_eq!(tut.steps.len(), back.steps.len());
    }
}

// =============================================================================
// Operator role hierarchy
// =============================================================================

proptest! {
    #[test]
    fn role_reflexive(role in arb_operator_role()) {
        prop_assert!(role.has_at_least(&role));
    }

    #[test]
    fn role_admin_has_all(role in arb_operator_role()) {
        prop_assert!(OperatorRole::Admin.has_at_least(&role));
    }

    #[test]
    fn role_ordering_consistent(
        a in arb_operator_role(),
        b in arb_operator_role(),
    ) {
        if a.level() >= b.level() {
            prop_assert!(a.has_at_least(&b));
        } else {
            prop_assert!(!a.has_at_least(&b));
        }
    }
}

#[test]
fn trainee_is_lowest() {
    assert!(!OperatorRole::Trainee.has_at_least(&OperatorRole::Operator));
}

// =============================================================================
// Runbook step filtering
// =============================================================================

proptest! {
    #[test]
    fn steps_for_role_subset(
        rb in arb_runbook(),
        role in arb_operator_role(),
    ) {
        let filtered = rb.steps_for_role(&role);
        prop_assert!(filtered.len() <= rb.steps.len());
    }

    #[test]
    fn decision_steps_are_decisions(rb in arb_runbook()) {
        let decisions = rb.decision_steps();
        for step in &decisions {
            prop_assert_eq!(step.step_type, StepType::Decision);
        }
    }

    #[test]
    fn estimated_total_is_sum(rb in arb_runbook()) {
        let expected: u32 = rb.steps.iter().map(|s| s.estimated_seconds).sum();
        prop_assert_eq!(rb.estimated_total_seconds(), expected);
    }

    #[test]
    fn is_applicable_consistent_with_role(
        rb in arb_runbook(),
        role in arb_operator_role(),
    ) {
        let applicable = rb.is_applicable_for(&role);
        let has_level = role.has_at_least(&rb.applicability.min_role);
        prop_assert_eq!(applicable, has_level,
            "applicability should match role hierarchy");
    }
}

// =============================================================================
// Decision overlay invariants
// =============================================================================

proptest! {
    #[test]
    fn options_by_risk_sorted(
        options in prop::collection::vec(arb_decision_option(), 1..8)
    ) {
        let overlay = DecisionOverlay {
            context: "test".into(),
            options,
            policy_refs: Vec::new(),
            telemetry_fields: Vec::new(),
        };
        let sorted = overlay.options_by_risk();
        for window in sorted.windows(2) {
            prop_assert!(window[0].risk_score <= window[1].risk_score,
                "options_by_risk should be sorted ascending");
        }
    }

    #[test]
    fn recommended_option_if_exists(
        options in prop::collection::vec(arb_decision_option(), 1..5)
    ) {
        let overlay = DecisionOverlay {
            context: "test".into(),
            options: options.clone(),
            policy_refs: Vec::new(),
            telemetry_fields: Vec::new(),
        };
        let rec = overlay.recommended_option();
        if options.iter().any(|o| o.recommended) {
            prop_assert!(rec.is_some(), "should return a recommended option");
            prop_assert!(rec.unwrap().recommended);
        }
    }
}

// =============================================================================
// Tutorial invariants
// =============================================================================

proptest! {
    #[test]
    fn tutorial_estimated_total_is_sum(tut in arb_tutorial_flow()) {
        let expected: u32 = tut.steps.iter().map(|s| s.estimated_seconds).sum();
        prop_assert_eq!(tut.estimated_total_seconds(), expected);
    }

    #[test]
    fn tutorial_step_count_accurate(tut in arb_tutorial_flow()) {
        prop_assert_eq!(tut.step_count(), tut.steps.len());
    }
}

// =============================================================================
// Registry invariants
// =============================================================================

proptest! {
    #[test]
    fn registry_counts_match_additions(
        runbooks in prop::collection::vec(arb_runbook(), 0..5),
        tutorials in prop::collection::vec(arb_tutorial_flow(), 0..5),
    ) {
        let mut reg = RunbookRegistry::new();
        for rb in &runbooks {
            reg.add_runbook(rb.clone());
        }
        for tut in &tutorials {
            reg.add_tutorial(tut.clone());
        }
        prop_assert_eq!(reg.runbook_count(), runbooks.len());
        prop_assert_eq!(reg.tutorial_count(), tutorials.len());
    }

    #[test]
    fn registry_role_filter_subset(
        runbooks in prop::collection::vec(arb_runbook(), 1..5),
        role in arb_operator_role(),
    ) {
        let mut reg = RunbookRegistry::new();
        for rb in &runbooks {
            reg.add_runbook(rb.clone());
        }
        let filtered = reg.runbooks_for_role(&role);
        prop_assert!(filtered.len() <= reg.runbook_count());
    }

    #[test]
    fn registry_incident_filter_subset(
        runbooks in prop::collection::vec(arb_runbook(), 1..5),
    ) {
        let mut reg = RunbookRegistry::new();
        for rb in &runbooks {
            reg.add_runbook(rb.clone());
        }
        let incident = reg.incident_runbooks();
        prop_assert!(incident.len() <= reg.runbook_count());
        for rb in &incident {
            prop_assert!(rb.applicability.incident_applicable);
        }
    }

    #[test]
    fn registry_tutorials_for_role_subset(
        tutorials in prop::collection::vec(arb_tutorial_flow(), 1..5),
        role in arb_operator_role(),
    ) {
        let mut reg = RunbookRegistry::new();
        for tut in &tutorials {
            reg.add_tutorial(tut.clone());
        }
        let filtered = reg.tutorials_for_role(&role);
        prop_assert!(filtered.len() <= reg.tutorial_count());
    }

    #[test]
    fn registry_snapshot_consistent(
        runbooks in prop::collection::vec(arb_runbook(), 0..5),
        tutorials in prop::collection::vec(arb_tutorial_flow(), 0..5),
    ) {
        let mut reg = RunbookRegistry::new();
        for rb in &runbooks {
            reg.add_runbook(rb.clone());
        }
        for tut in &tutorials {
            reg.add_tutorial(tut.clone());
        }
        let snap = reg.snapshot();
        prop_assert_eq!(snap.runbook_count, runbooks.len());
        prop_assert_eq!(snap.tutorial_count, tutorials.len());

        let expected_rb_steps: usize = runbooks.iter().map(|rb| rb.steps.len()).sum();
        prop_assert_eq!(snap.total_runbook_steps, expected_rb_steps);

        let expected_tut_steps: usize = tutorials.iter().map(|t| t.steps.len()).sum();
        prop_assert_eq!(snap.total_tutorial_steps, expected_tut_steps);
    }
}

// =============================================================================
// Snapshot serde
// =============================================================================

#[test]
fn registry_snapshot_serde_roundtrip() {
    let reg = RunbookRegistry::new();
    let snap = reg.snapshot();
    let json = serde_json::to_string(&snap).unwrap();
    let back: RegistrySnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(back.runbook_count, 0);
    assert_eq!(back.tutorial_count, 0);
}
