//! Property-based tests for the policy_dsl module.

use frankenterm_core::config::{PolicyRule, PolicyRuleDecision, PolicyRuleMatch};
use frankenterm_core::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyInput, PolicySurface,
};
use frankenterm_core::policy_dsl::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_action_kind() -> impl Strategy<Value = ActionKind> {
    prop_oneof![
        Just(ActionKind::SendText),
        Just(ActionKind::SendCtrlC),
        Just(ActionKind::SendCtrlD),
        Just(ActionKind::Spawn),
        Just(ActionKind::Split),
        Just(ActionKind::Close),
        Just(ActionKind::ReadOutput),
        Just(ActionKind::SearchOutput),
        Just(ActionKind::WriteFile),
        Just(ActionKind::DeleteFile),
        Just(ActionKind::ExecCommand),
        Just(ActionKind::ConnectorInvoke),
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

fn arb_policy_surface() -> impl Strategy<Value = PolicySurface> {
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

fn arb_dsl_decision() -> impl Strategy<Value = DslDecision> {
    prop_oneof![
        Just(DslDecision::Allow),
        Just(DslDecision::Deny),
        Just(DslDecision::RequireApproval),
    ]
}

/// Generates an atomic predicate (no recursion).
fn arb_atomic_predicate() -> impl Strategy<Value = RulePredicate> {
    prop_oneof![
        prop::collection::vec(arb_action_kind(), 0..=3)
            .prop_map(|a| RulePredicate::ActionIn { actions: a }),
        prop::collection::vec(arb_actor_kind(), 0..=2)
            .prop_map(|a| RulePredicate::ActorIn { actors: a }),
        prop::collection::vec(arb_policy_surface(), 0..=2)
            .prop_map(|s| RulePredicate::SurfaceIn { surfaces: s }),
        prop::collection::vec(any::<u64>(), 0..=3)
            .prop_map(|ids| RulePredicate::PaneIdIn { pane_ids: ids }),
        prop::collection::vec("[a-z*?]{1,10}", 0..=2)
            .prop_map(|p| RulePredicate::PaneTitleGlob { patterns: p }),
        prop::collection::vec("[a-z/.*]{1,15}", 0..=2)
            .prop_map(|p| RulePredicate::PaneCwdGlob { patterns: p }),
        prop::collection::vec("[a-z-]{1,10}", 0..=2)
            .prop_map(|d| RulePredicate::DomainIn { domains: d }),
        prop::collection::vec("[a-z0-9]{1,8}", 0..=2)
            .prop_map(|p| RulePredicate::CommandRegex { patterns: p }),
        prop::collection::vec("[a-z]{1,8}", 0..=2)
            .prop_map(|a| RulePredicate::AgentTypeIn { agent_types: a }),
        Just(RulePredicate::Always),
        Just(RulePredicate::Never),
    ]
}

/// Generates a predicate tree of bounded depth.
fn arb_predicate() -> impl Strategy<Value = RulePredicate> {
    arb_atomic_predicate().prop_recursive(3, 15, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(l, r)| RulePredicate::And {
                left: Box::new(l),
                right: Box::new(r),
            }),
            (inner.clone(), inner.clone()).prop_map(|(l, r)| RulePredicate::Or {
                left: Box::new(l),
                right: Box::new(r),
            }),
            inner.prop_map(|p| RulePredicate::Not { inner: Box::new(p) }),
        ]
    })
}

fn arb_dsl_rule() -> impl Strategy<Value = DslRule> {
    (
        "[a-z-]{1,20}",
        prop::option::of("[a-z ]{1,30}"),
        0..200u32,
        arb_predicate(),
        arb_dsl_decision(),
        prop::option::of("[a-z {}/]{1,40}"),
    )
        .prop_map(
            |(id, desc, priority, predicate, decision, message)| DslRule {
                id,
                description: desc,
                priority,
                predicate,
                decision,
                message,
            },
        )
}

fn arb_predicate_trace() -> impl Strategy<Value = PredicateTrace> {
    (
        "[a-z_()]{1,20}",
        any::<bool>(),
        prop::collection::vec(
            ("[a-z_]{1,10}", any::<bool>()).prop_map(|(d, m)| PredicateTrace {
                description: d,
                matched: m,
                children: vec![],
            }),
            0..=2,
        ),
    )
        .prop_map(|(description, matched, children)| PredicateTrace {
            description,
            matched,
            children,
        })
}

fn arb_dsl_rule_match() -> impl Strategy<Value = DslRuleMatch> {
    (
        "[a-z-]{1,15}",
        arb_dsl_decision(),
        prop::option::of("[a-z ]{1,20}"),
        arb_predicate_trace(),
    )
        .prop_map(|(rule_id, decision, message, trace)| DslRuleMatch {
            rule_id,
            decision,
            message,
            trace,
        })
}

fn arb_dsl_rule_evaluation() -> impl Strategy<Value = DslRuleEvaluation> {
    ("[a-z-]{1,15}", any::<bool>(), arb_dsl_decision(), 0..200u32).prop_map(
        |(rule_id, matched, decision, priority)| DslRuleEvaluation {
            rule_id,
            matched,
            decision,
            priority,
        },
    )
}

fn arb_dsl_eval_result() -> impl Strategy<Value = DslEvalResult> {
    (
        prop::option::of(arb_dsl_rule_match()),
        prop::collection::vec(arb_dsl_rule_evaluation(), 0..=3),
    )
        .prop_map(|(matched_rule, evaluations)| DslEvalResult {
            matched_rule,
            evaluations,
        })
}

fn arb_dsl_telemetry_snapshot() -> impl Strategy<Value = DslTelemetrySnapshot> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(evals, matched, not_matched, deny, allow, require)| DslTelemetrySnapshot {
                evaluations_total: evals,
                rules_matched: matched,
                rules_not_matched: not_matched,
                deny_decisions: deny,
                allow_decisions: allow,
                require_approval_decisions: require,
            },
        )
}

fn arb_policy_input() -> impl Strategy<Value = PolicyInput> {
    (
        arb_action_kind(),
        arb_actor_kind(),
        arb_policy_surface(),
        prop::option::of(any::<u64>()),
        prop::option::of("[a-z-]{1,10}"),
        prop::option::of("[a-z ]{1,15}"),
        prop::option::of("[a-z/]{1,15}"),
        prop::option::of("[a-z]{1,8}"),
        prop::option::of("[a-z ]{1,20}"),
    )
        .prop_map(
            |(action, actor, surface, pane_id, domain, title, cwd, agent, cmd)| PolicyInput {
                action,
                actor,
                surface,
                pane_id,
                domain,
                capabilities: PaneCapabilities::default(),
                text_summary: None,
                workflow_id: None,
                command_text: cmd,
                trauma_decision: None,
                pane_title: title,
                pane_cwd: cwd,
                agent_type: agent,
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn predicate_json_roundtrip(pred in arb_predicate()) {
        let json = serde_json::to_string(&pred).unwrap();
        let back: RulePredicate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(pred, back);
    }

    #[test]
    fn dsl_decision_json_roundtrip(d in arb_dsl_decision()) {
        let json = serde_json::to_string(&d).unwrap();
        let back: DslDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d, back);
    }

    #[test]
    fn dsl_rule_json_roundtrip(rule in arb_dsl_rule()) {
        let json = serde_json::to_string(&rule).unwrap();
        let back: DslRule = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rule, back);
    }

    #[test]
    fn predicate_trace_json_roundtrip(trace in arb_predicate_trace()) {
        let json = serde_json::to_string(&trace).unwrap();
        let back: PredicateTrace = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(trace, back);
    }

    #[test]
    fn dsl_rule_match_json_roundtrip(m in arb_dsl_rule_match()) {
        let json = serde_json::to_string(&m).unwrap();
        let back: DslRuleMatch = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(m, back);
    }

    #[test]
    fn dsl_rule_evaluation_json_roundtrip(e in arb_dsl_rule_evaluation()) {
        let json = serde_json::to_string(&e).unwrap();
        let back: DslRuleEvaluation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(e, back);
    }

    #[test]
    fn dsl_eval_result_json_roundtrip(r in arb_dsl_eval_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: DslEvalResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r, back);
    }

    #[test]
    fn dsl_telemetry_snapshot_json_roundtrip(s in arb_dsl_telemetry_snapshot()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: DslTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    // =========================================================================
    // Behavioral property tests
    // =========================================================================

    #[test]
    fn always_matches_any_input(input in arb_policy_input()) {
        prop_assert!(evaluate_predicate(&RulePredicate::Always, &input));
    }

    #[test]
    fn never_matches_no_input(input in arb_policy_input()) {
        prop_assert!(!evaluate_predicate(&RulePredicate::Never, &input));
    }

    #[test]
    fn not_always_equals_never(input in arb_policy_input()) {
        let pred = RulePredicate::Always.not();
        prop_assert_eq!(
            evaluate_predicate(&pred, &input),
            evaluate_predicate(&RulePredicate::Never, &input)
        );
    }

    #[test]
    fn not_never_equals_always(input in arb_policy_input()) {
        let pred = RulePredicate::Never.not();
        prop_assert_eq!(
            evaluate_predicate(&pred, &input),
            evaluate_predicate(&RulePredicate::Always, &input)
        );
    }

    #[test]
    fn double_negation(pred in arb_predicate(), input in arb_policy_input()) {
        let double_neg = pred.clone().not().not();
        prop_assert_eq!(
            evaluate_predicate(&pred, &input),
            evaluate_predicate(&double_neg, &input)
        );
    }

    #[test]
    fn and_with_always_is_identity(pred in arb_predicate(), input in arb_policy_input()) {
        let with_always = pred.clone().and(RulePredicate::Always);
        prop_assert_eq!(
            evaluate_predicate(&pred, &input),
            evaluate_predicate(&with_always, &input)
        );
    }

    #[test]
    fn and_with_never_is_never(pred in arb_predicate(), input in arb_policy_input()) {
        let with_never = pred.clone().and(RulePredicate::Never);
        prop_assert!(!evaluate_predicate(&with_never, &input));
    }

    #[test]
    fn or_with_always_is_always(pred in arb_predicate(), input in arb_policy_input()) {
        let with_always = pred.clone().or(RulePredicate::Always);
        prop_assert!(evaluate_predicate(&with_always, &input));
    }

    #[test]
    fn or_with_never_is_identity(pred in arb_predicate(), input in arb_policy_input()) {
        let with_never = pred.clone().or(RulePredicate::Never);
        prop_assert_eq!(
            evaluate_predicate(&pred, &input),
            evaluate_predicate(&with_never, &input)
        );
    }

    #[test]
    fn de_morgan_and(
        a in arb_atomic_predicate(),
        b in arb_atomic_predicate(),
        input in arb_policy_input()
    ) {
        // NOT(A AND B) == (NOT A) OR (NOT B)
        let lhs = a.clone().and(b.clone()).not();
        let rhs = a.not().or(b.not());
        prop_assert_eq!(
            evaluate_predicate(&lhs, &input),
            evaluate_predicate(&rhs, &input)
        );
    }

    #[test]
    fn de_morgan_or(
        a in arb_atomic_predicate(),
        b in arb_atomic_predicate(),
        input in arb_policy_input()
    ) {
        // NOT(A OR B) == (NOT A) AND (NOT B)
        let lhs = a.clone().or(b.clone()).not();
        let rhs = a.not().and(b.not());
        prop_assert_eq!(
            evaluate_predicate(&lhs, &input),
            evaluate_predicate(&rhs, &input)
        );
    }

    #[test]
    fn and_is_commutative(
        a in arb_atomic_predicate(),
        b in arb_atomic_predicate(),
        input in arb_policy_input()
    ) {
        let ab = a.clone().and(b.clone());
        let ba = b.and(a);
        prop_assert_eq!(
            evaluate_predicate(&ab, &input),
            evaluate_predicate(&ba, &input)
        );
    }

    #[test]
    fn or_is_commutative(
        a in arb_atomic_predicate(),
        b in arb_atomic_predicate(),
        input in arb_policy_input()
    ) {
        let ab = a.clone().or(b.clone());
        let ba = b.or(a);
        prop_assert_eq!(
            evaluate_predicate(&ab, &input),
            evaluate_predicate(&ba, &input)
        );
    }

    #[test]
    fn trace_matches_evaluate(pred in arb_predicate(), input in arb_policy_input()) {
        let result = evaluate_predicate(&pred, &input);
        let trace = evaluate_with_trace(&pred, &input);
        prop_assert_eq!(result, trace.matched);
    }

    #[test]
    fn depth_non_negative(pred in arb_predicate()) {
        // depth is usize, always >= 0
        let _ = pred.depth();
    }

    #[test]
    fn node_count_at_least_one(pred in arb_predicate()) {
        prop_assert!(pred.node_count() >= 1);
    }

    #[test]
    fn atomic_has_zero_depth(pred in arb_atomic_predicate()) {
        prop_assert_eq!(pred.depth(), 0);
        prop_assert!(pred.is_atomic());
    }

    #[test]
    fn not_increases_depth(pred in arb_atomic_predicate()) {
        let negated = pred.clone().not();
        prop_assert_eq!(negated.depth(), pred.depth() + 1);
    }

    #[test]
    fn specificity_non_negative(pred in arb_predicate()) {
        // specificity returns u32
        let _ = pred.specificity();
    }

    #[test]
    fn action_in_matches_contained_action(
        actions in prop::collection::vec(arb_action_kind(), 1..=5),
        idx in any::<usize>(),
        actor in arb_actor_kind()
    ) {
        let idx = idx % actions.len();
        let action = actions[idx];
        let pred = RulePredicate::action_in(actions);
        let input = PolicyInput::new(action, actor);
        prop_assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn actor_in_matches_contained_actor(
        actors in prop::collection::vec(arb_actor_kind(), 1..=4),
        idx in any::<usize>(),
        action in arb_action_kind()
    ) {
        let idx = idx % actors.len();
        let actor = actors[idx];
        let pred = RulePredicate::actor_in(actors);
        let input = PolicyInput::new(action, actor);
        prop_assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn dsl_eval_no_rules_returns_none(input in arb_policy_input()) {
        let result = evaluate_dsl_rules(&[], &input);
        prop_assert!(result.matched_rule.is_none());
        prop_assert!(result.evaluations.is_empty());
    }

    #[test]
    fn dsl_eval_always_deny_always_matches(input in arb_policy_input()) {
        let rules = vec![DslRule {
            id: "always-deny".to_owned(),
            description: None,
            priority: 1,
            predicate: RulePredicate::Always,
            decision: DslDecision::Deny,
            message: None,
        }];
        let result = evaluate_dsl_rules(&rules, &input);
        prop_assert!(result.matched_rule.is_some());
        let m = result.matched_rule.unwrap();
        prop_assert_eq!(m.decision, DslDecision::Deny);
    }

    #[test]
    fn dsl_eval_never_matches_nothing(input in arb_policy_input()) {
        let rules = vec![DslRule {
            id: "never-deny".to_owned(),
            description: None,
            priority: 1,
            predicate: RulePredicate::Never,
            decision: DslDecision::Deny,
            message: None,
        }];
        let result = evaluate_dsl_rules(&rules, &input);
        prop_assert!(result.matched_rule.is_none());
    }

    #[test]
    fn telemetry_record_increments_total(result in arb_dsl_eval_result()) {
        let mut telem = DslTelemetry::default();
        let before = telem.evaluations_total;
        telem.record(&result);
        prop_assert_eq!(telem.evaluations_total, before + 1);
    }

    #[test]
    fn dsl_decision_severity_ordering(d in arb_dsl_decision()) {
        match d {
            DslDecision::Deny => prop_assert_eq!(d.severity(), 0),
            DslDecision::RequireApproval => prop_assert_eq!(d.severity(), 1),
            DslDecision::Allow => prop_assert_eq!(d.severity(), 2),
        }
    }

    #[test]
    fn deny_wins_over_allow_at_same_priority(input in arb_policy_input()) {
        let rules = vec![
            DslRule {
                id: "allow".to_owned(),
                description: None,
                priority: 10,
                predicate: RulePredicate::Always,
                decision: DslDecision::Allow,
                message: None,
            },
            DslRule {
                id: "deny".to_owned(),
                description: None,
                priority: 10,
                predicate: RulePredicate::Always,
                decision: DslDecision::Deny,
                message: None,
            },
        ];
        let result = evaluate_dsl_rules(&rules, &input);
        let m = result.matched_rule.unwrap();
        prop_assert_eq!(m.decision, DslDecision::Deny);
    }

    #[test]
    fn lower_priority_wins(input in arb_policy_input()) {
        let rules = vec![
            DslRule {
                id: "low-priority-allow".to_owned(),
                description: None,
                priority: 100,
                predicate: RulePredicate::Always,
                decision: DslDecision::Allow,
                message: None,
            },
            DslRule {
                id: "high-priority-deny".to_owned(),
                description: None,
                priority: 1,
                predicate: RulePredicate::Always,
                decision: DslDecision::Deny,
                message: None,
            },
        ];
        let result = evaluate_dsl_rules(&rules, &input);
        let m = result.matched_rule.unwrap();
        prop_assert_eq!(m.rule_id, "high-priority-deny");
    }

    // ---- Bridge compiler property tests ----

    #[test]
    fn compile_empty_match_is_always(_dummy in 0u8..1) {
        let m = PolicyRuleMatch::default();
        let pred = compile_rule_match(&m);
        let check = matches!(pred, RulePredicate::Always);
        prop_assert!(check, "empty match should compile to Always");
    }

    #[test]
    fn compile_decompile_roundtrip_actions(
        actions in prop::collection::vec(arb_action_kind(), 1..=4)
    ) {
        let action_strs: Vec<String> = actions.iter().map(|a| {
            serde_json::to_string(a).unwrap().trim_matches('"').to_owned()
        }).collect();
        let m = PolicyRuleMatch {
            actions: action_strs.clone(),
            ..PolicyRuleMatch::default()
        };
        let pred = compile_rule_match(&m);
        let back = decompile_to_match(&pred).expect("flat AND should decompile");
        prop_assert_eq!(back.actions, action_strs);
    }

    #[test]
    fn compile_decompile_roundtrip_actors(
        actors in prop::collection::vec(arb_actor_kind(), 1..=3)
    ) {
        let actor_strs: Vec<String> = actors.iter().map(|a| {
            serde_json::to_string(a).unwrap().trim_matches('"').to_owned()
        }).collect();
        let m = PolicyRuleMatch {
            actors: actor_strs.clone(),
            ..PolicyRuleMatch::default()
        };
        let pred = compile_rule_match(&m);
        let back = decompile_to_match(&pred).expect("flat AND should decompile");
        prop_assert_eq!(back.actors, actor_strs);
    }

    #[test]
    fn compile_decompile_roundtrip_surfaces(
        surfaces in prop::collection::vec(arb_policy_surface(), 1..=3)
    ) {
        let surface_strs: Vec<String> = surfaces.iter().map(|s| {
            serde_json::to_string(s).unwrap().trim_matches('"').to_owned()
        }).collect();
        let m = PolicyRuleMatch {
            surfaces: surface_strs.clone(),
            ..PolicyRuleMatch::default()
        };
        let pred = compile_rule_match(&m);
        let back = decompile_to_match(&pred).expect("flat AND should decompile");
        prop_assert_eq!(back.surfaces, surface_strs);
    }

    #[test]
    fn compile_decompile_roundtrip_pane_ids(
        pane_ids in prop::collection::vec(any::<u64>(), 1..=5)
    ) {
        let m = PolicyRuleMatch {
            pane_ids: pane_ids.clone(),
            ..PolicyRuleMatch::default()
        };
        let pred = compile_rule_match(&m);
        let back = decompile_to_match(&pred).expect("flat AND should decompile");
        prop_assert_eq!(back.pane_ids, pane_ids);
    }

    #[test]
    fn compile_decompile_roundtrip_multi_field(
        actions in prop::collection::vec(arb_action_kind(), 1..=2),
        actors in prop::collection::vec(arb_actor_kind(), 1..=2),
        pane_ids in prop::collection::vec(any::<u64>(), 0..=3),
    ) {
        let action_strs: Vec<String> = actions.iter().map(|a| {
            serde_json::to_string(a).unwrap().trim_matches('"').to_owned()
        }).collect();
        let actor_strs: Vec<String> = actors.iter().map(|a| {
            serde_json::to_string(a).unwrap().trim_matches('"').to_owned()
        }).collect();
        let m = PolicyRuleMatch {
            actions: action_strs.clone(),
            actors: actor_strs.clone(),
            pane_ids: pane_ids.clone(),
            ..PolicyRuleMatch::default()
        };
        let pred = compile_rule_match(&m);
        let back = decompile_to_match(&pred).expect("multi-field should decompile");
        prop_assert_eq!(back.actions, action_strs);
        prop_assert_eq!(back.actors, actor_strs);
        prop_assert_eq!(back.pane_ids, pane_ids);
    }

    #[test]
    fn compile_policy_rule_preserves_decision(
        decision in prop_oneof![
            Just(PolicyRuleDecision::Allow),
            Just(PolicyRuleDecision::Deny),
            Just(PolicyRuleDecision::RequireApproval),
        ],
        id in "[a-z-]{1,15}",
        priority in 0..200u32,
    ) {
        let rule = PolicyRule {
            id: id.clone(),
            description: None,
            priority,
            match_on: PolicyRuleMatch::default(),
            decision,
            message: None,
        };
        let dsl = compile_policy_rule(&rule);
        prop_assert_eq!(&dsl.id, &id);
        prop_assert_eq!(dsl.priority, priority);
        let expected_decision = match decision {
            PolicyRuleDecision::Allow => DslDecision::Allow,
            PolicyRuleDecision::Deny => DslDecision::Deny,
            PolicyRuleDecision::RequireApproval => DslDecision::RequireApproval,
        };
        prop_assert_eq!(dsl.decision, expected_decision);
    }

    #[test]
    fn or_not_predicates_cannot_decompile(
        left in arb_atomic_predicate(),
        right in arb_atomic_predicate(),
    ) {
        let or_pred = RulePredicate::Or {
            left: Box::new(left),
            right: Box::new(right.clone()),
        };
        prop_assert!(decompile_to_match(&or_pred).is_none(), "OR should not decompile");

        let not_pred = RulePredicate::Not {
            inner: Box::new(right),
        };
        prop_assert!(decompile_to_match(&not_pred).is_none(), "NOT should not decompile");
    }

    #[test]
    fn never_predicate_cannot_decompile(_dummy in 0u8..1) {
        prop_assert!(decompile_to_match(&RulePredicate::Never).is_none());
    }

    #[test]
    fn compiled_predicate_evaluates_consistently(
        actions in prop::collection::vec(arb_action_kind(), 1..=2),
        input in arb_policy_input(),
    ) {
        let action_strs: Vec<String> = actions.iter().map(|a| {
            serde_json::to_string(a).unwrap().trim_matches('"').to_owned()
        }).collect();
        let m = PolicyRuleMatch {
            actions: action_strs,
            ..PolicyRuleMatch::default()
        };
        let pred = compile_rule_match(&m);
        let result = evaluate_predicate(&pred, &input);
        let trace = evaluate_with_trace(&pred, &input);
        // evaluate and evaluate_with_trace must agree
        prop_assert_eq!(result, trace.matched);
    }
}
