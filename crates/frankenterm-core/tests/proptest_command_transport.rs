//! Property-based tests for command_transport (ft-3681t.2.3).
//!
//! Coverage: CommandScope, CommandKind, CommandDeduplicator, serde roundtrips,
//! routing invariants, deduplication TTL semantics, and CommandPolicyTrace
//! field preservation.

use proptest::prelude::*;

use frankenterm_core::command_transport::{
    AckOutcome, CommandContext, CommandDeduplicator, CommandKind, CommandPolicyTrace,
    CommandRequest, CommandResult, CommandRouter, CommandScope, DeliveryStatus, InterruptSignal,
};
use frankenterm_core::policy::{
    ActionKind, ActorKind, DecisionContext, PolicyDecision, PolicySurface,
};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleRegistry, LifecycleState,
    MuxPaneLifecycleState, SessionLifecycleState, WindowLifecycleState,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn pane_id(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws", "local", id, 1)
}

fn window_id(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Window, "ws", "local", id, 1)
}

fn session_id(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Session, "ws", "local", id, 1)
}

fn test_ctx(ts: u64) -> CommandContext {
    CommandContext {
        timestamp_ms: ts,
        component: "proptest".to_string(),
        correlation_id: "corr-pt".to_string(),
        caller_identity: "agent-pt".to_string(),
        reason: None,
        policy_trace: None,
    }
}

fn seed_registry() -> LifecycleRegistry {
    let mut reg = LifecycleRegistry::new();
    reg.register_entity(
        session_id(1),
        LifecycleState::Session(SessionLifecycleState::Active),
        0,
    )
    .ok();
    reg.register_entity(
        window_id(10),
        LifecycleState::Window(WindowLifecycleState::Active),
        0,
    )
    .ok();
    for pid in [100, 101, 102] {
        reg.register_entity(
            pane_id(pid),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            0,
        )
        .ok();
    }
    reg
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_interrupt_signal() -> impl Strategy<Value = InterruptSignal> {
    prop_oneof![
        Just(InterruptSignal::CtrlC),
        Just(InterruptSignal::CtrlD),
        Just(InterruptSignal::CtrlZ),
        Just(InterruptSignal::CtrlBackslash),
    ]
}

fn arb_ack_outcome() -> impl Strategy<Value = AckOutcome> {
    prop_oneof![
        Just(AckOutcome::Delivered),
        Just(AckOutcome::Timeout),
        "[a-z]{3,20}".prop_map(|r| AckOutcome::Failed { reason: r }),
    ]
}

fn arb_command_kind() -> impl Strategy<Value = CommandKind> {
    prop_oneof![
        ("[a-z ]{1,20}", any::<bool>(), any::<bool>()).prop_map(|(text, paste, nl)| {
            CommandKind::SendInput {
                text,
                paste_mode: paste,
                append_newline: nl,
            }
        }),
        arb_interrupt_signal().prop_map(|sig| CommandKind::Interrupt { signal: sig }),
        (0u32..100, any::<bool>()).prop_map(|(lines, esc)| {
            CommandKind::Capture {
                tail_lines: lines,
                include_escapes: esc,
            }
        }),
        ("[a-z ]{1,20}", any::<bool>()).prop_map(|(text, paste)| {
            CommandKind::Broadcast {
                text,
                paste_mode: paste,
            }
        }),
        ("[a-z0-9-]{5,15}", arb_ack_outcome()).prop_map(|(id, outcome)| {
            CommandKind::Acknowledge {
                command_id: id,
                outcome,
            }
        }),
    ]
}

fn arb_delivery_status() -> impl Strategy<Value = DeliveryStatus> {
    prop_oneof![
        Just(DeliveryStatus::Delivered),
        "[a-z]{3,20}".prop_map(|r| DeliveryStatus::Skipped { reason: r }),
        "[a-z]{3,20}".prop_map(|r| DeliveryStatus::PolicyDenied { reason: r }),
        "[a-z]{3,20}".prop_map(|r| DeliveryStatus::RoutingError { reason: r }),
    ]
}

// ---------------------------------------------------------------------------
// Serde roundtrips
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn command_kind_serde_roundtrip(kind in arb_command_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let decoded: CommandKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, decoded);
    }

    #[test]
    fn interrupt_signal_serde_roundtrip(signal in arb_interrupt_signal()) {
        let json = serde_json::to_string(&signal).unwrap();
        let decoded: InterruptSignal = serde_json::from_str(&json).unwrap();
        assert_eq!(signal, decoded);
    }

    #[test]
    fn ack_outcome_serde_roundtrip(outcome in arb_ack_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let decoded: AckOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, decoded);
    }

    #[test]
    fn delivery_status_serde_roundtrip(status in arb_delivery_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let decoded: DeliveryStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, decoded);
    }

    /// CommandScope serde roundtrip for all variants.
    #[test]
    fn command_scope_serde_roundtrip(pid in 1u64..1000) {
        let scopes = vec![
            CommandScope::pane(pane_id(pid)),
            CommandScope::window(window_id(pid)),
            CommandScope::session(session_id(pid)),
            CommandScope::fleet(),
        ];
        for scope in scopes {
            let json = serde_json::to_string(&scope).unwrap();
            let decoded: CommandScope = serde_json::from_str(&json).unwrap();
            assert_eq!(scope, decoded);
        }
    }

    /// CommandContext serde roundtrip.
    #[test]
    fn command_context_serde_roundtrip(ts in 0u64..u64::MAX / 2) {
        let ctx = test_ctx(ts);
        let json = serde_json::to_string(&ctx).unwrap();
        let decoded: CommandContext = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx, decoded);
    }
}

// ---------------------------------------------------------------------------
// Deduplicator properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// First submission of any ID is never a duplicate.
    #[test]
    fn first_submission_is_not_duplicate(
        ttl in 100u64..10_000,
        ids in prop::collection::vec("[a-z0-9]{4,12}", 1..20),
    ) {
        let mut dedup = CommandDeduplicator::new(ttl);
        for (i, id) in ids.iter().enumerate() {
            let is_dup = dedup.is_duplicate(id, i as u64 * 10);
            // Only true if this ID appeared before (since IDs might repeat in generated vec)
            if ids[..i].contains(id) {
                // It's a repeat — should be duplicate
                assert!(is_dup, "repeated ID '{}' should be duplicate", id);
            } else {
                assert!(!is_dup, "first-seen ID '{}' should NOT be duplicate", id);
            }
        }
    }

    /// Second submission within TTL is a duplicate.
    #[test]
    fn resubmission_within_ttl_is_duplicate(ttl in 100u64..10_000) {
        let mut dedup = CommandDeduplicator::new(ttl);
        assert!(!dedup.is_duplicate("cmd-1", 0));
        assert!(dedup.is_duplicate("cmd-1", ttl / 2));
    }

    /// Submission after TTL expires is not a duplicate.
    #[test]
    fn submission_after_ttl_is_not_duplicate(ttl in 100u64..10_000) {
        let mut dedup = CommandDeduplicator::new(ttl);
        assert!(!dedup.is_duplicate("cmd-1", 0));
        assert!(!dedup.is_duplicate("cmd-1", ttl + 1));
    }

    /// Deduplicator evicts expired entries on each call.
    #[test]
    fn eviction_cleans_expired(
        ttl in 100u64..1000,
        n in 5u32..30,
    ) {
        let mut dedup = CommandDeduplicator::new(ttl);
        // Insert n commands at time 0
        for i in 0..n {
            dedup.is_duplicate(&format!("cmd-{i}"), 0);
        }
        assert_eq!(dedup.len(), n as usize);

        // After TTL, a new call should evict all previous
        dedup.is_duplicate("new-cmd", ttl + 1);
        // Only "new-cmd" should remain
        assert_eq!(dedup.len(), 1);
    }
}

// ---------------------------------------------------------------------------
// Router invariants
// ---------------------------------------------------------------------------

#[test]
fn route_pane_scope_to_running_pane_delivers() {
    let reg = seed_registry();
    let mut router = CommandRouter::new();

    let req = CommandRequest {
        command_id: "cmd-1".to_string(),
        scope: CommandScope::pane(pane_id(100)),
        command: CommandKind::SendInput {
            text: "hello".to_string(),
            paste_mode: false,
            append_newline: true,
        },
        context: test_ctx(1000),
        dry_run: false,
    };

    let result = router.route(&req, &reg).unwrap();
    assert_eq!(result.command_id, "cmd-1");
    assert_eq!(result.delivered_count(), 1);
    assert!(result.all_delivered());
}

#[test]
fn route_fleet_scope_delivers_to_all_running_panes() {
    let reg = seed_registry();
    let mut router = CommandRouter::new();

    let req = CommandRequest {
        command_id: "cmd-fleet".to_string(),
        scope: CommandScope::fleet(),
        command: CommandKind::Broadcast {
            text: "alert".to_string(),
            paste_mode: false,
        },
        context: test_ctx(1000),
        dry_run: false,
    };

    let result = router.route(&req, &reg).unwrap();
    // Should deliver to all 3 running panes
    assert_eq!(result.delivered_count(), 3);
    assert!(result.all_delivered());
}

#[test]
fn route_skips_non_running_panes() {
    let mut reg = seed_registry();
    // Mark pane 101 as closed
    reg.register_entity(
        pane_id(101),
        LifecycleState::Pane(MuxPaneLifecycleState::Closed),
        100,
    )
    .ok();

    let mut router = CommandRouter::new();
    let req = CommandRequest {
        command_id: "cmd-2".to_string(),
        scope: CommandScope::fleet(),
        command: CommandKind::SendInput {
            text: "test".to_string(),
            paste_mode: false,
            append_newline: true,
        },
        context: test_ctx(1000),
        dry_run: false,
    };

    let result = router.route(&req, &reg).unwrap();
    assert_eq!(result.delivered_count(), 2); // 100, 102
    assert_eq!(result.skipped_count(), 1); // 101 (closed)
    assert!(!result.all_delivered());
}

#[test]
fn dry_run_does_not_change_delivered_count() {
    let reg = seed_registry();
    let mut router = CommandRouter::new();

    let req = CommandRequest {
        command_id: "cmd-dry".to_string(),
        scope: CommandScope::pane(pane_id(100)),
        command: CommandKind::SendInput {
            text: "test".to_string(),
            paste_mode: false,
            append_newline: true,
        },
        context: test_ctx(1000),
        dry_run: true,
    };

    let result = router.route(&req, &reg).unwrap();
    assert!(result.dry_run);
    assert_eq!(result.delivered_count(), 1);
}

#[test]
fn audit_log_grows_with_routes() {
    let reg = seed_registry();
    let mut router = CommandRouter::new();

    assert!(router.audit_log().is_empty());

    for i in 0..5 {
        let req = CommandRequest {
            command_id: format!("cmd-{i}"),
            scope: CommandScope::pane(pane_id(100)),
            command: CommandKind::SendInput {
                text: "x".to_string(),
                paste_mode: false,
                append_newline: true,
            },
            context: test_ctx(1000 + i),
            dry_run: false,
        };
        router.route(&req, &reg).unwrap();
    }

    assert_eq!(router.audit_log().len(), 5);
}

#[test]
fn audit_log_serializes_to_json() {
    let reg = seed_registry();
    let mut router = CommandRouter::new();

    let req = CommandRequest {
        command_id: "cmd-audit".to_string(),
        scope: CommandScope::pane(pane_id(100)),
        command: CommandKind::SendInput {
            text: "hello".to_string(),
            paste_mode: false,
            append_newline: true,
        },
        context: test_ctx(1000),
        dry_run: false,
    };
    router.route(&req, &reg).unwrap();

    let json = router.audit_log_json().unwrap();
    assert!(json.contains("cmd-audit"));
    assert!(json.contains("proptest"));
}

// ---------------------------------------------------------------------------
// InterruptSignal coverage
// ---------------------------------------------------------------------------

#[test]
fn interrupt_signal_bytes_nonempty() {
    let signals = [
        InterruptSignal::CtrlC,
        InterruptSignal::CtrlD,
        InterruptSignal::CtrlZ,
        InterruptSignal::CtrlBackslash,
    ];
    for sig in signals {
        assert!(!sig.as_bytes().is_empty());
        assert!(!sig.label().is_empty());
    }
}

// ---------------------------------------------------------------------------
// CommandScope label coverage
// ---------------------------------------------------------------------------

#[test]
fn scope_labels_include_type_prefix() {
    assert!(CommandScope::pane(pane_id(1)).label().starts_with("pane:"));
    assert!(
        CommandScope::window(window_id(1))
            .label()
            .starts_with("window:")
    );
    assert!(
        CommandScope::session(session_id(1))
            .label()
            .starts_with("session:")
    );
    assert_eq!(CommandScope::fleet().label(), "fleet:*");
}

// ---------------------------------------------------------------------------
// DeliveryStatus predicates
// ---------------------------------------------------------------------------

#[test]
fn delivery_status_predicates() {
    assert!(DeliveryStatus::Delivered.is_delivered());
    assert!(!DeliveryStatus::Delivered.is_skipped());

    assert!(DeliveryStatus::Skipped { reason: "x".into() }.is_skipped());
    assert!(!DeliveryStatus::Skipped { reason: "x".into() }.is_delivered());

    assert!(!DeliveryStatus::PolicyDenied { reason: "x".into() }.is_delivered());
    assert!(!DeliveryStatus::RoutingError { reason: "x".into() }.is_delivered());
}

// ---------------------------------------------------------------------------
// CommandResult predicates
// ---------------------------------------------------------------------------

#[test]
fn command_result_all_delivered_requires_nonempty() {
    let empty = CommandResult {
        command_id: "x".into(),
        deliveries: vec![],
        dry_run: false,
        elapsed_us: 0,
    };
    assert!(
        !empty.all_delivered(),
        "empty deliveries → not all_delivered"
    );
}

// ---------------------------------------------------------------------------
// CommandPolicyTrace strategies
// ---------------------------------------------------------------------------

fn arb_actor_kind() -> impl Strategy<Value = ActorKind> {
    prop_oneof![
        Just(ActorKind::Human),
        Just(ActorKind::Robot),
        Just(ActorKind::Mcp),
        Just(ActorKind::Workflow),
    ]
}

fn arb_action_kind() -> impl Strategy<Value = ActionKind> {
    prop_oneof![
        Just(ActionKind::SendText),
        Just(ActionKind::SendCtrlC),
        Just(ActionKind::SendCtrlD),
        Just(ActionKind::SendCtrlZ),
        Just(ActionKind::SendControl),
        Just(ActionKind::Spawn),
        Just(ActionKind::Split),
        Just(ActionKind::Activate),
        Just(ActionKind::Close),
        Just(ActionKind::BrowserAuth),
        Just(ActionKind::WorkflowRun),
        Just(ActionKind::ReservePane),
        Just(ActionKind::ReleasePane),
        Just(ActionKind::ReadOutput),
        Just(ActionKind::SearchOutput),
        Just(ActionKind::ConnectorNotify),
        Just(ActionKind::ConnectorInvoke),
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

fn arb_decision_context(
    action: ActionKind,
    actor: ActorKind,
    surface: PolicySurface,
    pane_id: Option<u64>,
    domain: Option<String>,
    workflow_id: Option<String>,
    determining_rule: Option<String>,
) -> DecisionContext {
    let mut ctx = DecisionContext::new_audit(
        1000,
        action,
        actor,
        surface,
        pane_id,
        domain,
        None,
        workflow_id,
    );
    if let Some(rule) = determining_rule {
        ctx.set_determining_rule(rule);
    }
    ctx
}

// ---------------------------------------------------------------------------
// CommandPolicyTrace serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn policy_trace_serde_roundtrip_with_all_fields(
        actor in arb_actor_kind(),
        action in arb_action_kind(),
        surface in arb_policy_surface(),
        pane_id in prop::option::of(1u64..1000),
        domain in prop::option::of("[a-z]{3,10}"),
        workflow_id in prop::option::of("[a-z0-9-]{5,15}"),
        rule_id in prop::option::of("[a-z.]{5,20}"),
        reason in prop::option::of("[a-z ]{5,30}"),
        determining_rule in prop::option::of("[a-z.]{5,20}"),
    ) {
        let trace = CommandPolicyTrace {
            decision: "allow".to_string(),
            surface,
            actor: Some(actor),
            action: Some(action),
            reason,
            rule_id,
            determining_rule,
            pane_id,
            domain,
            workflow_id,
        };
        let json = serde_json::to_string(&trace).unwrap();
        let decoded: CommandPolicyTrace = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(trace, decoded);
    }

    #[test]
    fn policy_trace_serde_roundtrip_minimal(
        surface in arb_policy_surface(),
        decision in prop_oneof![Just("allow"), Just("deny"), Just("require_approval")],
    ) {
        let trace = CommandPolicyTrace {
            decision: decision.to_string(),
            surface,
            actor: None,
            action: None,
            reason: None,
            rule_id: None,
            determining_rule: None,
            pane_id: None,
            domain: None,
            workflow_id: None,
        };
        let json = serde_json::to_string(&trace).unwrap();
        let decoded: CommandPolicyTrace = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(trace, decoded);
    }
}

// ---------------------------------------------------------------------------
// from_surface_and_decision property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn from_allow_decision_preserves_decision_string(
        surface in arb_policy_surface(),
        rule_id in prop::option::of("[a-z.]{5,20}"),
    ) {
        let decision = match rule_id {
            Some(ref r) => PolicyDecision::allow_with_rule(r.clone()),
            None => PolicyDecision::allow(),
        };
        let trace = CommandPolicyTrace::from_surface_and_decision(surface, &decision);
        prop_assert_eq!(&trace.decision, "allow");
        prop_assert_eq!(trace.rule_id.as_deref(), rule_id.as_deref());
        prop_assert!(trace.reason.is_none(), "allow has no reason");
        prop_assert!(trace.actor.is_none(), "no context → no actor");
        prop_assert!(trace.action.is_none(), "no context → no action");
    }

    #[test]
    fn from_deny_decision_preserves_reason_and_rule(
        surface in arb_policy_surface(),
        reason in "[a-z ]{3,30}",
        rule_id in prop::option::of("[a-z.]{5,20}"),
    ) {
        let decision = match rule_id {
            Some(ref r) => PolicyDecision::deny_with_rule(reason.clone(), r.clone()),
            None => PolicyDecision::deny(reason.clone()),
        };
        let trace = CommandPolicyTrace::from_surface_and_decision(surface, &decision);
        prop_assert_eq!(&trace.decision, "deny");
        prop_assert_eq!(trace.reason.as_deref(), Some(reason.as_str()));
        prop_assert_eq!(trace.rule_id.as_deref(), rule_id.as_deref());
    }

    #[test]
    fn from_require_approval_preserves_reason(
        surface in arb_policy_surface(),
        reason in "[a-z ]{3,30}",
        rule_id in prop::option::of("[a-z.]{5,20}"),
    ) {
        let decision = match rule_id {
            Some(ref r) => PolicyDecision::require_approval_with_rule(reason.clone(), r.clone()),
            None => PolicyDecision::require_approval(reason.clone()),
        };
        let trace = CommandPolicyTrace::from_surface_and_decision(surface, &decision);
        prop_assert_eq!(&trace.decision, "require_approval");
        prop_assert_eq!(trace.reason.as_deref(), Some(reason.as_str()));
        prop_assert_eq!(trace.rule_id.as_deref(), rule_id.as_deref());
    }

    #[test]
    fn context_surface_overrides_explicit_when_not_unknown(
        explicit_surface in arb_policy_surface(),
        context_surface in arb_policy_surface(),
        actor in arb_actor_kind(),
        action in arb_action_kind(),
    ) {
        let ctx = arb_decision_context(action, actor, context_surface, None, None, None, None);
        let decision = PolicyDecision::allow().with_context(ctx);
        let trace = CommandPolicyTrace::from_surface_and_decision(explicit_surface, &decision);

        if context_surface == PolicySurface::Unknown {
            prop_assert_eq!(trace.surface, explicit_surface,
                "Unknown context surface falls back to explicit");
        } else {
            prop_assert_eq!(trace.surface, context_surface,
                "Non-unknown context surface overrides explicit");
        }
    }

    #[test]
    fn context_actor_and_action_preserved(
        surface in arb_policy_surface(),
        actor in arb_actor_kind(),
        action in arb_action_kind(),
    ) {
        let ctx = arb_decision_context(action, actor, PolicySurface::Unknown, None, None, None, None);
        let decision = PolicyDecision::allow().with_context(ctx);
        let trace = CommandPolicyTrace::from_surface_and_decision(surface, &decision);

        prop_assert_eq!(trace.actor, Some(actor));
        prop_assert_eq!(trace.action, Some(action));
    }

    #[test]
    fn context_pane_id_domain_workflow_preserved(
        surface in arb_policy_surface(),
        pane_id in prop::option::of(1u64..1000),
        domain in prop::option::of("[a-z]{3,10}"),
        workflow_id in prop::option::of("[a-z0-9-]{5,15}"),
    ) {
        let ctx = arb_decision_context(
            ActionKind::SendText,
            ActorKind::Robot,
            PolicySurface::Unknown,
            pane_id,
            domain.clone(),
            workflow_id.clone(),
            None,
        );
        let decision = PolicyDecision::allow().with_context(ctx);
        let trace = CommandPolicyTrace::from_surface_and_decision(surface, &decision);

        prop_assert_eq!(trace.pane_id, pane_id);
        prop_assert_eq!(trace.domain, domain);
        prop_assert_eq!(trace.workflow_id, workflow_id);
    }

    #[test]
    fn context_determining_rule_preserved(
        surface in arb_policy_surface(),
        determining_rule in prop::option::of("[a-z.]{5,20}"),
    ) {
        let ctx = arb_decision_context(
            ActionKind::SendText,
            ActorKind::Human,
            PolicySurface::Unknown,
            None,
            None,
            None,
            determining_rule.clone(),
        );
        let decision = PolicyDecision::allow().with_context(ctx);
        let trace = CommandPolicyTrace::from_surface_and_decision(surface, &decision);
        prop_assert_eq!(trace.determining_rule, determining_rule);
    }

    #[test]
    fn command_context_with_policy_trace_roundtrips(
        ts in 0u64..u64::MAX / 2,
        surface in arb_policy_surface(),
        actor in arb_actor_kind(),
        action in arb_action_kind(),
        pane_id in prop::option::of(1u64..1000),
    ) {
        let ctx_decision = arb_decision_context(
            action, actor, surface, pane_id, None, None, None,
        );
        let decision = PolicyDecision::deny("test").with_context(ctx_decision);
        let ctx = test_ctx(ts).with_policy_decision(surface, &decision);

        let json = serde_json::to_string(&ctx).unwrap();
        let decoded: CommandContext = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(&ctx, &decoded);
        let trace = decoded.policy_trace.expect("trace must survive roundtrip");
        prop_assert_eq!(&trace.decision, "deny");
        prop_assert_eq!(trace.actor, Some(actor));
        prop_assert_eq!(trace.action, Some(action));
        prop_assert_eq!(trace.pane_id, pane_id);
    }
}
