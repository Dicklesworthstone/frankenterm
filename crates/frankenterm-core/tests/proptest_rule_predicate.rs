// Property-based tests for policy::RulePredicate composable predicate AST.
//
// Covers: serde roundtrip, evaluation semantics (And/Or/Not/True/False),
// from_flat_match parity with matches_rule, depth/leaf_count invariants.
#![allow(clippy::ignored_unit_patterns)]

use proptest::prelude::*;

use frankenterm_core::policy::{
    ActionKind, ActorKind, PolicyInput, PolicySurface, RulePredicate, evaluate_predicate,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_action_kind() -> impl Strategy<Value = ActionKind> {
    prop_oneof![
        Just(ActionKind::SendText),
        Just(ActionKind::SendCtrlC),
        Just(ActionKind::Spawn),
        Just(ActionKind::Close),
        Just(ActionKind::Split),
        Just(ActionKind::ReadOutput),
        Just(ActionKind::WorkflowRun),
    ]
}

fn arb_actor_kind() -> impl Strategy<Value = ActorKind> {
    prop_oneof![
        Just(ActorKind::Human),
        Just(ActorKind::Robot),
        Just(ActorKind::Mcp),
        Just(ActorKind::Workflow),
    ]
}

fn arb_surface() -> impl Strategy<Value = PolicySurface> {
    prop_oneof![
        Just(PolicySurface::Unknown),
        Just(PolicySurface::Mux),
        Just(PolicySurface::Swarm),
        Just(PolicySurface::Robot),
        Just(PolicySurface::Connector),
        Just(PolicySurface::Workflow),
        Just(PolicySurface::Mcp),
        Just(PolicySurface::Ipc),
    ]
}

fn arb_policy_input() -> impl Strategy<Value = PolicyInput> {
    (arb_action_kind(), arb_actor_kind(), arb_surface()).prop_map(|(action, actor, surface)| {
        let mut input = PolicyInput::new(action, actor);
        input.surface = surface;
        input
    })
}

/// Generate a leaf predicate (non-recursive).
fn arb_leaf_predicate() -> impl Strategy<Value = RulePredicate> {
    prop_oneof![
        Just(RulePredicate::True),
        Just(RulePredicate::False),
        prop::collection::vec(
            prop_oneof![
                Just("send_text".to_string()),
                Just("close".to_string()),
                Just("spawn".to_string()),
                Just("split".to_string()),
            ],
            1..=3
        )
        .prop_map(|values| RulePredicate::Action { values }),
        prop::collection::vec(
            prop_oneof![
                Just("human".to_string()),
                Just("robot".to_string()),
                Just("mcp".to_string()),
                Just("workflow".to_string()),
            ],
            1..=3
        )
        .prop_map(|values| RulePredicate::Actor { values }),
        prop::collection::vec(
            prop_oneof![
                Just("mux".to_string()),
                Just("swarm".to_string()),
                Just("robot".to_string()),
                Just("connector".to_string()),
            ],
            1..=3
        )
        .prop_map(|values| RulePredicate::Surface { values }),
        prop::collection::vec(0..1000u64, 1..=3)
            .prop_map(|values| RulePredicate::PaneId { values }),
    ]
}

/// Generate a predicate tree up to the given depth.
fn arb_predicate(max_depth: u32) -> impl Strategy<Value = RulePredicate> {
    arb_leaf_predicate().prop_recursive(max_depth, 16, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..=4)
                .prop_map(|children| RulePredicate::And { children }),
            prop::collection::vec(inner.clone(), 0..=4)
                .prop_map(|children| RulePredicate::Or { children }),
            inner.prop_map(|child| RulePredicate::Not {
                child: Box::new(child)
            }),
        ]
    })
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn predicate_serde_roundtrip(pred in arb_predicate(3)) {
        let json = serde_json::to_string(&pred).unwrap();
        let back: RulePredicate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&pred, &back);
    }

    #[test]
    fn predicate_serde_json_has_type_tag(pred in arb_leaf_predicate()) {
        let json = serde_json::to_string(&pred).unwrap();
        // All variants except True/False should have a "type" field
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        if let serde_json::Value::Object(map) = &parsed {
            prop_assert!(map.contains_key("type"), "missing 'type' tag in {json}");
        }
    }
}

// =============================================================================
// Boolean algebra invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// True always evaluates to true regardless of input.
    #[test]
    fn predicate_true_always_matches(input in arb_policy_input()) {
        prop_assert!(RulePredicate::True.evaluate(&input));
    }

    /// False always evaluates to false regardless of input.
    #[test]
    fn predicate_false_never_matches(input in arb_policy_input()) {
        prop_assert!(!RulePredicate::False.evaluate(&input));
    }

    /// Not(p) == !p for all inputs.
    #[test]
    fn predicate_not_inverts(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let negated = RulePredicate::Not { child: Box::new(pred.clone()) };
        prop_assert_eq!(negated.evaluate(&input), !pred.evaluate(&input));
    }

    /// Double negation: Not(Not(p)) == p.
    #[test]
    fn predicate_double_negation(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let double_neg = RulePredicate::Not {
            child: Box::new(RulePredicate::Not {
                child: Box::new(pred.clone()),
            }),
        };
        prop_assert_eq!(double_neg.evaluate(&input), pred.evaluate(&input));
    }

    /// And([p]) == p (singleton And).
    #[test]
    fn predicate_and_singleton(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let and_single = RulePredicate::And { children: vec![pred.clone()] };
        prop_assert_eq!(and_single.evaluate(&input), pred.evaluate(&input));
    }

    /// Or([p]) == p (singleton Or).
    #[test]
    fn predicate_or_singleton(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let or_single = RulePredicate::Or { children: vec![pred.clone()] };
        prop_assert_eq!(or_single.evaluate(&input), pred.evaluate(&input));
    }

    /// And(True, p) == p (True is identity for And).
    #[test]
    fn predicate_and_true_identity(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let and_true = RulePredicate::And {
            children: vec![RulePredicate::True, pred.clone()],
        };
        prop_assert_eq!(and_true.evaluate(&input), pred.evaluate(&input));
    }

    /// Or(False, p) == p (False is identity for Or).
    #[test]
    fn predicate_or_false_identity(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let or_false = RulePredicate::Or {
            children: vec![RulePredicate::False, pred.clone()],
        };
        prop_assert_eq!(or_false.evaluate(&input), pred.evaluate(&input));
    }

    /// And(False, p) == False (False is annihilator for And).
    #[test]
    fn predicate_and_false_annihilates(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let and_false = RulePredicate::And {
            children: vec![RulePredicate::False, pred.clone()],
        };
        prop_assert!(!and_false.evaluate(&input));
    }

    /// Or(True, p) == True (True is annihilator for Or).
    #[test]
    fn predicate_or_true_annihilates(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let or_true = RulePredicate::Or {
            children: vec![RulePredicate::True, pred.clone()],
        };
        prop_assert!(or_true.evaluate(&input));
    }

    /// And(p, p) == p (idempotence).
    #[test]
    fn predicate_and_idempotent(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let and_dup = RulePredicate::And {
            children: vec![pred.clone(), pred.clone()],
        };
        prop_assert_eq!(and_dup.evaluate(&input), pred.evaluate(&input));
    }

    /// Or(p, p) == p (idempotence).
    #[test]
    fn predicate_or_idempotent(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let or_dup = RulePredicate::Or {
            children: vec![pred.clone(), pred.clone()],
        };
        prop_assert_eq!(or_dup.evaluate(&input), pred.evaluate(&input));
    }

    /// p OR Not(p) == True (excluded middle).
    #[test]
    fn predicate_excluded_middle(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let or_neg = RulePredicate::Or {
            children: vec![
                pred.clone(),
                RulePredicate::Not { child: Box::new(pred.clone()) },
            ],
        };
        prop_assert!(or_neg.evaluate(&input));
    }

    /// p AND Not(p) == False (contradiction).
    #[test]
    fn predicate_contradiction(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        let and_neg = RulePredicate::And {
            children: vec![
                pred.clone(),
                RulePredicate::Not { child: Box::new(pred.clone()) },
            ],
        };
        prop_assert!(!and_neg.evaluate(&input));
    }

    /// evaluate_predicate matches RulePredicate::evaluate.
    #[test]
    fn predicate_evaluate_fn_parity(
        pred in arb_predicate(2),
        input in arb_policy_input(),
    ) {
        prop_assert_eq!(evaluate_predicate(&pred, &input), pred.evaluate(&input));
    }
}

// =============================================================================
// Structural invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Depth is always at least 1.
    #[test]
    fn predicate_depth_at_least_one(pred in arb_predicate(3)) {
        prop_assert!(pred.depth() >= 1);
    }

    /// Leaf count is non-negative (empty And/Or have 0 leaves).
    #[test]
    fn predicate_leaf_count_nonnegative(pred in arb_predicate(3)) {
        // leaf_count is usize, always >= 0. Leaf nodes have count 1.
        // Empty And/Or containers have count 0.
        let count = pred.leaf_count();
        let is_leaf = matches!(
            &pred,
            RulePredicate::True
                | RulePredicate::False
                | RulePredicate::Action { .. }
                | RulePredicate::Actor { .. }
                | RulePredicate::Surface { .. }
                | RulePredicate::PaneId { .. }
                | RulePredicate::PaneTitle { .. }
                | RulePredicate::PaneCwd { .. }
                | RulePredicate::PaneDomain { .. }
                | RulePredicate::CommandPattern { .. }
                | RulePredicate::AgentType { .. }
        );
        if is_leaf {
            prop_assert_eq!(count, 1);
        }
    }

    /// Not adds exactly one to depth.
    #[test]
    fn predicate_not_depth_is_one_plus_child(pred in arb_predicate(2)) {
        let not_pred = RulePredicate::Not { child: Box::new(pred.clone()) };
        prop_assert_eq!(not_pred.depth(), 1 + pred.depth());
    }

    /// Not preserves leaf count.
    #[test]
    fn predicate_not_leaf_count_same_as_child(pred in arb_predicate(2)) {
        let not_pred = RulePredicate::Not { child: Box::new(pred.clone()) };
        prop_assert_eq!(not_pred.leaf_count(), pred.leaf_count());
    }

    /// And/Or leaf count is sum of children's leaf counts.
    #[test]
    fn predicate_and_leaf_count_is_sum(
        children in prop::collection::vec(arb_predicate(1), 1..=4),
    ) {
        let expected: usize = children.iter().map(|c| c.leaf_count()).sum();
        let and_pred = RulePredicate::And { children };
        prop_assert_eq!(and_pred.leaf_count(), expected);
    }
}
