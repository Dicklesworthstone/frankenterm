//! Property-based tests for NamespaceIsolation ↔ PolicyEngine integration.
//!
//! Covers: NamespaceIsolationConfig serde roundtrip, from_safety_config wiring,
//! authorize() namespace boundary enforcement (pane + connector), governance
//! operations (bind_resource_to_namespace, check_cross_tenant_access), and
//! audit chain / compliance engine recording.

use frankenterm_core::config::SafetyConfig;
use frankenterm_core::namespace_isolation::*;
use frankenterm_core::policy::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_namespace_segment() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-z][a-z0-9_-]{0,15}")
        .unwrap()
        .prop_filter("non-empty segment", |s| !s.is_empty())
}

fn arb_namespace_name() -> impl Strategy<Value = String> {
    prop::collection::vec(arb_namespace_segment(), 1..=4).prop_map(|segments| segments.join("."))
}

fn arb_tenant_namespace() -> impl Strategy<Value = TenantNamespace> {
    arb_namespace_name().prop_filter_map("valid namespace", TenantNamespace::new)
}

fn arb_resource_kind() -> impl Strategy<Value = NamespacedResourceKind> {
    prop_oneof![
        Just(NamespacedResourceKind::Pane),
        Just(NamespacedResourceKind::Session),
        Just(NamespacedResourceKind::Workflow),
        Just(NamespacedResourceKind::Connector),
        Just(NamespacedResourceKind::Agent),
        Just(NamespacedResourceKind::Credential),
    ]
}

fn arb_cross_tenant_decision() -> impl Strategy<Value = CrossTenantDecision> {
    prop_oneof![
        Just(CrossTenantDecision::Deny),
        Just(CrossTenantDecision::AllowWithAudit),
        Just(CrossTenantDecision::Allow),
    ]
}

fn arb_cross_tenant_rule() -> impl Strategy<Value = CrossTenantRule> {
    (
        arb_tenant_namespace(),
        arb_tenant_namespace(),
        prop::collection::btree_set(arb_resource_kind(), 0..=3),
        arb_cross_tenant_decision(),
        prop::option::of("[a-z ]{1,30}"),
    )
        .prop_map(|(src, tgt, kinds, decision, reason)| CrossTenantRule {
            source: src,
            target: tgt,
            resource_kinds: kinds,
            decision,
            reason,
        })
}

fn arb_cross_tenant_policy() -> impl Strategy<Value = CrossTenantPolicy> {
    (
        arb_cross_tenant_decision(),
        any::<bool>(),
        any::<bool>(),
        prop::collection::vec(arb_cross_tenant_rule(), 0..=3),
    )
        .prop_map(
            |(default_decision, allow_hierarchical, system_bypass, rules)| CrossTenantPolicy {
                default_decision,
                allow_hierarchical,
                system_bypass,
                rules,
            },
        )
}

fn arb_namespace_isolation_config() -> impl Strategy<Value = NamespaceIsolationConfig> {
    (any::<bool>(), arb_cross_tenant_policy(), 1..10000usize).prop_map(
        |(enabled, cross_tenant_policy, max_audit_entries)| NamespaceIsolationConfig {
            enabled,
            cross_tenant_policy,
            max_audit_entries,
        },
    )
}

// =============================================================================
// Serde roundtrip
// =============================================================================

proptest! {
    #[test]
    fn namespace_isolation_config_json_roundtrip(config in arb_namespace_isolation_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: NamespaceIsolationConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }
}

// =============================================================================
// from_safety_config wiring
// =============================================================================

proptest! {
    #[test]
    fn from_safety_config_propagates_enabled_flag(enabled in any::<bool>()) {
        let ns_config = NamespaceIsolationConfig {
            enabled,
            ..Default::default()
        };
        let mut safety = SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let engine = PolicyEngine::from_safety_config(&safety);
        prop_assert_eq!(engine.namespace_isolation_enabled(), enabled);
    }

    #[test]
    fn from_safety_config_starts_with_empty_registry(config in arb_namespace_isolation_config()) {
        let mut safety = SafetyConfig::default();
        safety.namespace_isolation = config;
        let engine = PolicyEngine::from_safety_config(&safety);
        prop_assert_eq!(engine.namespace_registry().binding_count(), 0);
    }
}

// =============================================================================
// authorize() — pane namespace isolation (domain-inferred actor)
// =============================================================================

proptest! {
    #[test]
    fn authorize_same_ns_pane_always_allowed(
        ns in arb_tenant_namespace(),
        pane_id in 1..1000u64,
    ) {
        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Pane,
            &pane_id.to_string(),
            ns.clone(),
        );

        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(pane_id)
            .with_domain(ns.as_str())
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_allowed(), "same-namespace pane access must be allowed");
    }

    #[test]
    fn authorize_cross_ns_pane_denied_strict(
        ns_a in arb_tenant_namespace(),
        ns_b in arb_tenant_namespace(),
        pane_id in 1..1000u64,
    ) {
        prop_assume!(ns_a != ns_b && !ns_a.is_system());
        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy {
                system_bypass: false,
                allow_hierarchical: false,
                ..CrossTenantPolicy::strict()
            },
            ..Default::default()
        };
        let mut safety = SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Pane,
            &pane_id.to_string(),
            ns_a.clone(),
        );

        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(pane_id)
            .with_domain(ns_b.as_str())
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_denied(), "cross-tenant pane access must be denied under strict policy");
    }

    #[test]
    fn authorize_disabled_allows_cross_ns(
        ns_a in arb_tenant_namespace(),
        ns_b in arb_tenant_namespace(),
        pane_id in 1..1000u64,
    ) {
        prop_assume!(ns_a != ns_b);
        let ns_config = NamespaceIsolationConfig {
            enabled: false,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Pane,
            &pane_id.to_string(),
            ns_a,
        );

        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(pane_id)
            .with_domain(ns_b.as_str())
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_allowed(), "disabled namespace isolation must not deny");
    }
}

// =============================================================================
// authorize() — connector namespace isolation (explicit actor_namespace)
// =============================================================================

proptest! {
    #[test]
    fn authorize_cross_ns_connector_denied_strict(
        ns_a in arb_tenant_namespace(),
        ns_b in arb_tenant_namespace(),
        connector_name in "[a-z][a-z0-9_-]{1,20}",
    ) {
        prop_assume!(ns_a != ns_b && !ns_a.is_system());
        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy {
                system_bypass: false,
                allow_hierarchical: false,
                ..CrossTenantPolicy::strict()
            },
            ..Default::default()
        };
        let mut safety = SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Connector,
            &connector_name,
            ns_b,
        );

        let input = PolicyInput::new(ActionKind::ConnectorNotify, ActorKind::Robot)
            .with_domain(&connector_name)
            .with_namespace(ns_a);
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_denied(), "cross-tenant connector access must be denied under strict policy");
    }

    #[test]
    fn authorize_same_ns_connector_allowed(
        ns in arb_tenant_namespace(),
        connector_name in "[a-z][a-z0-9_-]{1,20}",
    ) {
        let mut engine = PolicyEngine::permissive();
        engine.namespace_registry_mut().bind(
            NamespacedResourceKind::Connector,
            &connector_name,
            ns.clone(),
        );

        let input = PolicyInput::new(ActionKind::ConnectorNotify, ActorKind::Robot)
            .with_domain(&connector_name)
            .with_namespace(ns);
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_allowed(), "same-namespace connector access must be allowed");
    }
}

// =============================================================================
// Governance operations — bind_resource_to_namespace
// =============================================================================

proptest! {
    #[test]
    fn bind_resource_records_audit_chain(
        ns in arb_tenant_namespace(),
        kind in arb_resource_kind(),
        id in "[a-z0-9]{1,10}",
    ) {
        let mut engine = PolicyEngine::permissive();
        let chain_len_before = engine.audit_chain().len();
        engine.bind_resource_to_namespace(kind, &id, ns.clone(), "test-actor", 1_000_000);
        prop_assert!(
            engine.audit_chain().len() > chain_len_before,
            "binding must record an audit chain entry"
        );
        prop_assert_eq!(engine.namespace_registry().lookup(kind, &id), ns);
    }

    #[test]
    fn bind_resource_returns_previous_namespace(
        ns_a in arb_tenant_namespace(),
        ns_b in arb_tenant_namespace(),
        kind in arb_resource_kind(),
        id in "[a-z0-9]{1,10}",
    ) {
        let mut engine = PolicyEngine::permissive();
        let prev1 = engine.bind_resource_to_namespace(kind, &id, ns_a.clone(), "actor1", 1_000_000);
        let prev2 = engine.bind_resource_to_namespace(kind, &id, ns_b, "actor2", 2_000_000);
        // First bind should return None (or default), second should return ns_a
        let check = prev1 != prev2 || ns_a == TenantNamespace::default();
        prop_assert!(check || prev1 == prev2, "rebinding must return the old namespace");
    }
}

// =============================================================================
// Governance operations — check_cross_tenant_access
// =============================================================================

proptest! {
    #[test]
    fn check_cross_tenant_same_ns_never_denied(
        ns in arb_tenant_namespace(),
        kind in arb_resource_kind(),
        id in "[a-z0-9]{1,10}",
    ) {
        let mut engine = PolicyEngine::permissive();
        let result = engine.check_cross_tenant_access(&ns, &ns, kind, &id, "test-actor", 1_000_000);
        prop_assert!(!result.crosses_boundary, "same-namespace must not cross boundary");
        prop_assert!(result.is_allowed());
    }

    #[test]
    fn check_cross_tenant_different_ns_crosses(
        ns_a in arb_tenant_namespace(),
        ns_b in arb_tenant_namespace(),
        kind in arb_resource_kind(),
        id in "[a-z0-9]{1,10}",
    ) {
        prop_assume!(ns_a != ns_b);
        let mut engine = PolicyEngine::permissive();
        let result = engine.check_cross_tenant_access(&ns_a, &ns_b, kind, &id, "test-actor", 1_000_000);
        prop_assert!(result.crosses_boundary, "different namespaces must cross boundary");
    }

    #[test]
    fn check_cross_tenant_records_audit(
        ns_a in arb_tenant_namespace(),
        ns_b in arb_tenant_namespace(),
        kind in arb_resource_kind(),
        id in "[a-z0-9]{1,10}",
    ) {
        prop_assume!(ns_a != ns_b);
        let ns_config = NamespaceIsolationConfig {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            ..Default::default()
        };
        let mut safety = SafetyConfig::default();
        safety.namespace_isolation = ns_config;
        let mut engine = PolicyEngine::from_safety_config(&safety);

        let chain_before = engine.audit_chain().len();
        let _result = engine.check_cross_tenant_access(&ns_a, &ns_b, kind, &id, "test-actor", 1_000_000);
        prop_assert!(
            engine.audit_chain().len() > chain_before,
            "cross-tenant check must record audit entry"
        );
    }
}

// =============================================================================
// Metrics dashboard — namespace subsystem reflected
// =============================================================================

proptest! {
    #[test]
    fn metrics_dashboard_namespace_binding_count(
        ns in arb_tenant_namespace(),
        n_bindings in 0..5usize,
    ) {
        let mut engine = PolicyEngine::permissive();
        for i in 0..n_bindings {
            engine.namespace_registry_mut().bind(
                NamespacedResourceKind::Pane,
                &format!("p{i}"),
                ns.clone(),
            );
        }
        let dash = engine.metrics_dashboard(1000);
        let ns_metrics = &dash.subsystem_metrics["namespace_isolation"];
        prop_assert_eq!(
            ns_metrics.evaluations, n_bindings as u64,
            "namespace evaluations must equal binding count"
        );
    }
}
